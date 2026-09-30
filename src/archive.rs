//! Bounded, checked extraction of tool archives into a store staging directory (M-Spike S-3).
//!
//! The `tar` crate only parses; every file system change is made here, under these rules:
//!
//! - an entry path that is absolute or contains `..` is refused (`E_ARCHIVE_UNSAFE`);
//! - nothing is written through a symlink: every parent of an entry must be a real directory;
//! - a symlink target must be relative and, resolved physically after extraction, stay inside
//!   the extracted tree; a hard link must name a regular file extracted earlier;
//! - device nodes, FIFOs, sparse files and unknown entry types are refused, as is a second
//!   entry for a path that already exists (except repeated directories);
//! - the number of entries, the unpacked bytes and the path length are bounded ([`Limits`]);
//! - modes are reduced to `0644`/`0755` from the owner-execute bit: no setuid, setgid or
//!   sticky bit, no group or world write;
//! - an archive that declares exact post-strip regular-file exclusions skips them before any
//!   filesystem write
//!   and fails when a named entry is missing or is not a regular file;
//! - `strip_components` and `subdir` are applied as the lock records them; an archive that
//!   leaves nothing is `E_ARCHIVE_EMPTY`; unreadable compression or tar data is
//!   `E_ARCHIVE_FORMAT`.

use std::fs;
use std::io::{self, BufReader, Read, Seek, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use crate::diag::Diagnostic;

/// Upper bounds for one archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_entries: u64,
    /// Total bytes of regular-file content, and of the decompressed tar stream.
    pub max_bytes: u64,
    pub max_path_bytes: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_entries: 262_144,
            max_bytes: 4 << 30,
            max_path_bytes: 4096,
        }
    }
}

/// What was extracted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub hardlinks: u64,
    pub bytes: u64,
}

/// The exact paths one catalogue asset deliberately leaves out of its realized tree.
pub(crate) struct Excludes<'a> {
    pub paths: &'a [String],
    pub recipe_file: &'a str,
}

fn unsafe_entry(what: impl Into<String>) -> Diagnostic {
    Diagnostic::new("E_ARCHIVE_UNSAFE", what)
}

fn format_error(what: impl std::fmt::Display) -> Diagnostic {
    Diagnostic::new("E_ARCHIVE_FORMAT", format!("unreadable archive: {what}"))
}

fn store_io(path: &Path, e: io::Error) -> Diagnostic {
    Diagnostic::new("E_STORE_IO", format!("{}: {e}", path.display()))
}

/// A reader that fails once more than `limit` bytes have been read.
struct Bounded<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Bounded<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n as u64 > self.left {
            return Err(io::Error::other(
                "the decompressed archive exceeds the size limit",
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// Split an archive path into its normal components, refusing absolute paths and `..`.
fn components(raw: &[u8], what: &str) -> Result<Vec<Vec<u8>>, Diagnostic> {
    if raw.contains(&0) {
        return Err(unsafe_entry(format!("{what} contains a NUL byte")));
    }
    let path = Path::new(std::ffi::OsStr::from_bytes(raw));
    let mut out = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(n) => out.push(n.as_bytes().to_vec()),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => {
                return Err(unsafe_entry(format!(
                    "{what} `{}` is absolute",
                    String::from_utf8_lossy(raw)
                )));
            }
            Component::ParentDir => {
                return Err(unsafe_entry(format!(
                    "{what} `{}` contains `..`",
                    String::from_utf8_lossy(raw)
                )));
            }
        }
    }
    Ok(out)
}

/// Apply `strip_components` and `subdir`; `None` when the entry falls outside what is kept.
fn relocate(mut parts: Vec<Vec<u8>>, strip: usize, subdir: &[Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    if parts.len() <= strip {
        return None;
    }
    parts.drain(..strip);
    if !parts.starts_with(subdir) {
        return None;
    }
    parts.drain(..subdir.len());
    (!parts.is_empty()).then_some(parts)
}

fn join(root: &Path, parts: &[Vec<u8>]) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in parts {
        p.push(std::ffi::OsStr::from_bytes(part));
    }
    p
}

/// Make sure every parent of `parts` under `root` is a real directory, creating missing ones.
fn ensure_parents(root: &Path, parts: &[Vec<u8>], stats: &mut Stats) -> Result<(), Diagnostic> {
    let mut p = root.to_path_buf();
    for part in &parts[..parts.len() - 1] {
        p.push(std::ffi::OsStr::from_bytes(part));
        match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => {
                return Err(unsafe_entry(format!(
                    "an entry would be written through `{}`, which is not a directory",
                    p.strip_prefix(root).unwrap_or(&p).display()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&p).map_err(|e| store_io(&p, e))?;
                fs::set_permissions(&p, fs::Permissions::from_mode(0o755))
                    .map_err(|e| store_io(&p, e))?;
                stats.dirs += 1;
            }
            Err(e) => return Err(store_io(&p, e)),
        }
    }
    Ok(())
}

/// The destination of an extraction: created, or reused when it is already a directory. A tool
/// with several artifacts extracts them in order over one another into the same staging tree
/// (`spec/01` §3.8); the "appears twice" rule still refuses two artifacts that collide.
fn ensure_dest(dest: &Path) -> Result<(), Diagnostic> {
    match fs::create_dir(dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dest.is_dir() => Ok(()),
        Err(e) => Err(store_io(dest, e)),
    }
}

/// Install a downloaded file that is the tool itself (`format = "binary"`) as `bin/<name>`,
/// mode 0755, under `dest`. Nothing is unpacked: the bytes are the program.
pub fn install_binary(file: &Path, name: &str, dest: &Path) -> Result<Stats, Diagnostic> {
    let parts = components(name.as_bytes(), "binary name")?;
    if parts.len() != 1 {
        return Err(unsafe_entry(format!(
            "`{name}` is not a single file name for a binary artifact"
        )));
    }
    ensure_dest(dest)?;
    let bin = dest.join("bin");
    match fs::create_dir(&bin) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && bin.is_dir() => {}
        Err(e) => return Err(store_io(&bin, e)),
    }
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).map_err(|e| store_io(&bin, e))?;
    let target = join(&bin, &parts);
    if exists(&target) {
        return Err(unsafe_entry(format!("`bin/{name}` appears twice")));
    }
    let bytes = fs::copy(file, &target).map_err(|e| store_io(&target, e))?;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
        .map_err(|e| store_io(&target, e))?;
    Ok(Stats {
        files: 1,
        dirs: 1,
        symlinks: 0,
        hardlinks: 0,
        bytes,
    })
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// The decompressed tar stream of `archive`: gzip is streamed, xz is decompressed into a
/// temporary file next to `dest` (lzma-rs writes, it does not offer a reader).
fn open_tar(
    archive: &Path,
    format: &str,
    dest: &Path,
    limits: &Limits,
) -> Result<(Box<dyn Read>, Option<PathBuf>), Diagnostic> {
    let file = fs::File::open(archive).map_err(|e| store_io(archive, e))?;
    match format {
        "tar.gz" => Ok((
            Box::new(Bounded {
                inner: flate2::read::MultiGzDecoder::new(BufReader::new(file)),
                left: limits.max_bytes + limits.max_entries.saturating_mul(1536),
            }),
            None,
        )),
        "tar.xz" => {
            let name = dest
                .file_name()
                .map_or_else(Default::default, |n| n.to_string_lossy().into_owned());
            let tmp = dest.with_file_name(format!("{name}.tar"));
            let mut out = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(|e| store_io(&tmp, e))?;
            struct Capped<'a> {
                out: &'a mut fs::File,
                left: u64,
            }
            impl Write for Capped<'_> {
                fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                    if buf.len() as u64 > self.left {
                        return Err(io::Error::other(
                            "the decompressed archive exceeds the size limit",
                        ));
                    }
                    self.left -= buf.len() as u64;
                    self.out.write_all(buf)?;
                    Ok(buf.len())
                }
                fn flush(&mut self) -> io::Result<()> {
                    self.out.flush()
                }
            }
            let mut capped = Capped {
                out: &mut out,
                left: limits.max_bytes + limits.max_entries.saturating_mul(1536),
            };
            let mut writer = io::BufWriter::with_capacity(1 << 20, &mut capped);
            let result = lzma_rs::xz_decompress(&mut BufReader::new(file), &mut writer)
                .map_err(|e| format!("{e:?}"))
                .and_then(|()| writer.flush().map_err(|e| e.to_string()));
            drop(writer);
            if let Err(e) = result {
                let _ = fs::remove_file(&tmp);
                return Err(format_error(e));
            }
            out.rewind().map_err(|e| store_io(&tmp, e))?;
            Ok((Box::new(BufReader::new(out)), Some(tmp)))
        }
        other => Err(Diagnostic::new(
            "E_ARCHIVE_FORMAT",
            format!("archive format `{other}` is not supported (tar.gz, tar.xz)"),
        )),
    }
}

/// Extract `archive` (`tar.gz` or `tar.xz`) into the new directory `dest` under the rules in
/// the module documentation. `dest` must not exist; on failure it may hold a partial tree,
/// which the caller discards.
pub fn extract(
    archive: &Path,
    format: &str,
    strip_components: u32,
    subdir: &str,
    dest: &Path,
    limits: &Limits,
) -> Result<Stats, Diagnostic> {
    extract_with_excludes(
        archive,
        format,
        strip_components,
        subdir,
        Excludes {
            paths: &[],
            recipe_file: "<lock>",
        },
        dest,
        limits,
    )
}

/// Extract an archive while omitting exact regular-file paths named by its recipe. Exclusions
/// are compared after `strip_components` and before `subdir`; a missing or non-file entry means
/// the recipe no longer describes upstream and is refused rather than becoming a silent no-op.
pub(crate) fn extract_with_excludes(
    archive: &Path,
    format: &str,
    strip_components: u32,
    subdir: &str,
    excludes: Excludes<'_>,
    dest: &Path,
    limits: &Limits,
) -> Result<Stats, Diagnostic> {
    let subdir = components(subdir.as_bytes(), "subdir")?;
    let excluded = excludes
        .paths
        .iter()
        .map(|path| {
            components(path.as_bytes(), "exclude path").map(|parts| (parts, false, path.clone()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    ensure_dest(dest)?;
    let (reader, tmp) = open_tar(archive, format, dest, limits)?;
    let result = extract_tar(
        reader,
        strip_components as usize,
        &subdir,
        excluded,
        excludes.recipe_file,
        dest,
        limits,
    );
    if let Some(tmp) = tmp {
        let _ = fs::remove_file(tmp);
    }
    let stats = result?;
    if stats.files + stats.symlinks + stats.hardlinks + stats.dirs == 0 {
        return Err(Diagnostic::new(
            "E_ARCHIVE_EMPTY",
            format!(
                "the archive has no entries after strip_components = {strip_components}{}",
                if subdir.is_empty() {
                    String::new()
                } else {
                    " and subdir".into()
                }
            ),
        ));
    }
    check_symlinks(dest, dest)?;
    Ok(stats)
}

fn extract_tar(
    reader: Box<dyn Read>,
    strip: usize,
    subdir: &[Vec<u8>],
    mut excluded: Vec<(Vec<Vec<u8>>, bool, String)>,
    recipe_file: &str,
    root: &Path,
    limits: &Limits,
) -> Result<Stats, Diagnostic> {
    let mut archive = tar::Archive::new(reader);
    let mut stats = Stats::default();
    let mut seen = 0u64;
    for entry in archive.entries().map_err(format_error)? {
        let mut entry = entry.map_err(format_error)?;
        seen += 1;
        if seen > limits.max_entries {
            return Err(unsafe_entry(format!(
                "the archive has more than {} entries",
                limits.max_entries
            )));
        }
        let kind = entry.header().entry_type();
        if matches!(kind, tar::EntryType::XGlobalHeader) {
            continue;
        }
        let raw = entry.path_bytes().into_owned();
        if raw.len() > limits.max_path_bytes {
            return Err(unsafe_entry(format!(
                "an entry path is longer than {} bytes",
                limits.max_path_bytes
            )));
        }
        let parts = components(&raw, "entry path")?;
        let Some(stripped) = relocate(parts, strip, &[]) else {
            continue;
        };
        let shown = String::from_utf8_lossy(&raw).into_owned();
        if let Some((_, found, path)) = excluded.iter_mut().find(|(parts, _, _)| *parts == stripped)
        {
            if !matches!(kind, tar::EntryType::Regular | tar::EntryType::Continuous) {
                return Err(Diagnostic::new(
                    "E_RECIPE_INVALID",
                    format!(
                        "catalogue recipe {recipe_file}: excluded path `{path}` is not a regular file"
                    ),
                ));
            }
            *found = true;
            continue;
        }
        let Some(parts) = relocate(stripped, 0, subdir) else {
            continue;
        };
        let target = join(root, &parts);
        match kind {
            tar::EntryType::Directory => {
                ensure_parents(root, &parts, &mut stats)?;
                match fs::symlink_metadata(&target) {
                    Ok(m) if m.file_type().is_dir() => {}
                    Ok(_) => {
                        return Err(unsafe_entry(format!(
                            "directory `{shown}` replaces an earlier entry"
                        )));
                    }
                    Err(_) => {
                        fs::create_dir(&target).map_err(|e| store_io(&target, e))?;
                        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
                            .map_err(|e| store_io(&target, e))?;
                        stats.dirs += 1;
                    }
                }
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                ensure_parents(root, &parts, &mut stats)?;
                if exists(&target) {
                    return Err(unsafe_entry(format!("`{shown}` appears twice")));
                }
                let size = entry.header().size().map_err(format_error)?;
                if stats.bytes.saturating_add(size) > limits.max_bytes {
                    return Err(unsafe_entry(format!(
                        "the archive unpacks to more than {} bytes",
                        limits.max_bytes
                    )));
                }
                let mode = entry.header().mode().map_err(format_error)?;
                let exec = mode & 0o100 != 0;
                let mut out = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&target)
                    .map_err(|e| store_io(&target, e))?;
                let copied = io::copy(&mut (&mut entry).take(size), &mut out)
                    .map_err(|e| format_error(format!("{shown}: {e}")))?;
                if copied != size {
                    return Err(format_error(format!("`{shown}` is truncated")));
                }
                drop(out);
                fs::set_permissions(
                    &target,
                    fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }),
                )
                .map_err(|e| store_io(&target, e))?;
                stats.bytes += size;
                stats.files += 1;
            }
            tar::EntryType::Symlink => {
                ensure_parents(root, &parts, &mut stats)?;
                if exists(&target) {
                    return Err(unsafe_entry(format!("`{shown}` appears twice")));
                }
                let link = entry
                    .link_name_bytes()
                    .ok_or_else(|| format_error(format!("symlink `{shown}` has no target")))?
                    .into_owned();
                check_link_text(&parts, &link, &shown)?;
                std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&link), &target)
                    .map_err(|e| store_io(&target, e))?;
                stats.symlinks += 1;
            }
            tar::EntryType::Link => {
                ensure_parents(root, &parts, &mut stats)?;
                if exists(&target) {
                    return Err(unsafe_entry(format!("`{shown}` appears twice")));
                }
                let link = entry
                    .link_name_bytes()
                    .ok_or_else(|| format_error(format!("hard link `{shown}` has no target")))?
                    .into_owned();
                let link_parts = components(&link, "hard link target")?;
                let Some(link_parts) = relocate(link_parts, strip, subdir) else {
                    return Err(unsafe_entry(format!(
                        "hard link `{shown}` names a file outside the extracted tree"
                    )));
                };
                ensure_parents_exist(root, &link_parts, &shown)?;
                let source = join(root, &link_parts);
                match fs::symlink_metadata(&source) {
                    Ok(m) if m.file_type().is_file() => {}
                    _ => {
                        return Err(unsafe_entry(format!(
                            "hard link `{shown}` does not name a regular file extracted before it"
                        )));
                    }
                }
                fs::hard_link(&source, &target).map_err(|e| store_io(&target, e))?;
                stats.hardlinks += 1;
            }
            other => {
                return Err(unsafe_entry(format!(
                    "`{shown}` has an unsupported entry type ({other:?}); only files, \
                     directories, symlinks and hard links are extracted"
                )));
            }
        }
    }
    if let Some((_, _, path)) = excluded.iter().find(|(_, found, _)| !found) {
        return Err(Diagnostic::new(
            "E_RECIPE_INVALID",
            format!(
                "catalogue recipe {recipe_file}: excluded path `{path}` is not in the artifact"
            ),
        ));
    }
    Ok(stats)
}

/// Every parent of an existing entry must be a real directory (hard-link sources).
fn ensure_parents_exist(root: &Path, parts: &[Vec<u8>], shown: &str) -> Result<(), Diagnostic> {
    let mut p = root.to_path_buf();
    for part in &parts[..parts.len() - 1] {
        p.push(std::ffi::OsStr::from_bytes(part));
        if !fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_dir()) {
            return Err(unsafe_entry(format!(
                "hard link `{shown}` resolves through something that is not a directory"
            )));
        }
    }
    Ok(())
}

/// The lexical check at extraction time: relative, and inside the tree when read literally.
fn check_link_text(parts: &[Vec<u8>], link: &[u8], shown: &str) -> Result<(), Diagnostic> {
    if link.is_empty() || link.contains(&0) {
        return Err(unsafe_entry(format!(
            "symlink `{shown}` has an empty target"
        )));
    }
    if link.starts_with(b"/") {
        return Err(unsafe_entry(format!(
            "symlink `{shown}` points to the absolute path `{}`",
            String::from_utf8_lossy(link)
        )));
    }
    let mut depth = parts.len() as i64 - 1;
    for c in Path::new(std::ffi::OsStr::from_bytes(link)).components() {
        match c {
            Component::ParentDir => depth -= 1,
            Component::Normal(_) => depth += 1,
            _ => {}
        }
        if depth < 0 {
            return Err(unsafe_entry(format!(
                "symlink `{shown}` -> `{}` escapes the extracted tree",
                String::from_utf8_lossy(link)
            )));
        }
    }
    Ok(())
}

/// The physical check after extraction: every symlink, resolved through the links around it,
/// stays inside `root`. Dangling links are resolved as far as they exist and the rest read
/// literally.
fn check_symlinks(root: &Path, dir: &Path) -> Result<(), Diagnostic> {
    for entry in fs::read_dir(dir).map_err(|e| store_io(dir, e))? {
        let entry = entry.map_err(|e| store_io(dir, e))?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|e| store_io(&path, e))?;
        if kind.is_dir() {
            check_symlinks(root, &path)?;
        } else if kind.is_symlink() && !resolves_inside(root, &path)? {
            let target = fs::read_link(&path).unwrap_or_default();
            return Err(unsafe_entry(format!(
                "symlink `{}` -> `{}` resolves outside the extracted tree",
                path.strip_prefix(root).unwrap_or(&path).display(),
                target.display()
            )));
        }
    }
    Ok(())
}

/// Resolve `link` component by component without leaving `root`: `..` above `root` or a
/// chain longer than 40 links is outside.
fn resolves_inside(root: &Path, link: &Path) -> Result<bool, Diagnostic> {
    // The current position as components below root; pending components to walk.
    let rel = link.strip_prefix(root).unwrap_or(link);
    let mut pending: Vec<std::ffi::OsString> = rel
        .components()
        .map(|c| c.as_os_str().to_os_string())
        .collect();
    pending.reverse();
    let mut here: Vec<std::ffi::OsString> = Vec::new();
    let mut hops = 0;
    while let Some(part) = pending.pop() {
        if part == ".." {
            if here.pop().is_none() {
                return Ok(false);
            }
            continue;
        }
        if part == "." || part.is_empty() {
            continue;
        }
        let mut candidate = root.to_path_buf();
        candidate.extend(&here);
        candidate.push(&part);
        match fs::symlink_metadata(&candidate) {
            Ok(m) if m.file_type().is_symlink() => {
                hops += 1;
                if hops > 40 {
                    return Ok(false);
                }
                let target = fs::read_link(&candidate).map_err(|e| store_io(&candidate, e))?;
                if target.is_absolute() {
                    return Ok(false);
                }
                let mut parts: Vec<std::ffi::OsString> = target
                    .components()
                    .map(|c| c.as_os_str().to_os_string())
                    .collect();
                parts.reverse();
                pending.extend(parts);
            }
            _ => here.push(part),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_refuse_absolute_and_parent_paths() {
        assert_eq!(
            components(b"./a/b/", "p").unwrap(),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
        for bad in [&b"/etc/passwd"[..], b"a/../../b", b"..", b"a\0b"] {
            assert_eq!(components(bad, "p").unwrap_err().code, "E_ARCHIVE_UNSAFE");
        }
    }

    #[test]
    fn relocate_strips_and_selects_subdir() {
        let p = |s: &str| components(s.as_bytes(), "p").unwrap();
        assert_eq!(relocate(p("top/bin/x"), 1, &[]), Some(p("bin/x")));
        assert_eq!(relocate(p("top"), 1, &[]), None);
        assert_eq!(relocate(p("top/sub/bin"), 1, &p("sub")), Some(p("bin")));
        assert_eq!(relocate(p("top/other/bin"), 1, &p("sub")), None);
    }

    #[test]
    fn link_text_is_checked_lexically() {
        let parts = vec![b"bin".to_vec(), b"python3".to_vec()];
        assert!(check_link_text(&parts, b"python3.12", "x").is_ok());
        assert!(check_link_text(&parts, b"../lib/x", "x").is_ok());
        for bad in [&b"../../x"[..], b"/usr/bin/python3", b""] {
            assert!(check_link_text(&parts, bad, "x").is_err());
        }
    }
}
