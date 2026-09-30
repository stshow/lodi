//! Filesystem boundary for host-scope file actions.
//!
//! Every destination is relative to an already-armed root.  Existing components are inspected
//! with `symlink_metadata`, final files are opened with `O_NOFOLLOW`, and parents are created one
//! component at a time. No caller mutates a manifest-managed path directly. A managed path is
//! read only through [`open_regular`]: opened without blocking and refused unless the descriptor
//! is a regular file, so that nothing found there can hold an apply still.
//!
//! A directory on the way to anything a host apply changes is **trusted only if nobody else can
//! write it** (LD-333): the root itself and every existing directory below it must be owned by
//! root — by the invoking user under a `--root` — and be neither group- nor world-writable, or
//! the path is refused before anything is changed ([`untrusted_directory`]). A directory this
//! module creates on the way is [`PARENT_MODE`]; only the last one a caller asks for takes the
//! mode the caller gives. The owner and mode of an existing file are changed only when it has a
//! single link. Walking by directory descriptor (`openat`) rather than by path string is the
//! structural fix that remains; `docs/ERRORS.md` states the window it would close.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};

use crate::diag::Diagnostic;

use super::plan::{Content, Desired};

fn escape(path: &str, why: impl std::fmt::Display) -> Diagnostic {
    Diagnostic::new("E_PATH_ESCAPE", format!("{path}: {why}"))
        .hint("lodi never follows a symbolic link in a host-scope destination")
}

/// The mode of a directory created on the way to a path, as `docs/scopes/host.md` states it.
pub const PARENT_MODE: u32 = 0o755;

/// The one owner a trusted directory may have: root on the running system's `/`, and the
/// invoking user under a `--root`, as the arming marker is judged.
pub fn trusted_owner(system_root: bool, euid: u32) -> u32 {
    if system_root { 0 } else { euid }
}

/// Why a directory with this owner and mode is not one the host scope may change anything
/// below, or `None` when it is: owned by `trusted` and neither group- nor world-writable, so
/// that no other principal can replace an entry in it. A sticky bit does not make a writable
/// directory trusted.
pub fn untrusted_directory(owner: u32, mode: u32, trusted: u32) -> Option<String> {
    if owner != trusted {
        let who = if trusted == 0 {
            "root".to_string()
        } else {
            format!("the invoking user (uid {trusted})")
        };
        return Some(format!("it is owned by uid {owner}, not by {who}"));
    }
    if mode & 0o022 != 0 {
        return Some(format!(
            "it is mode {:04o}: group- or world-writable",
            mode & 0o7777
        ));
    }
    None
}

/// The trusted owner for `root`, which the gate has already canonicalised.
fn trusted_for(root: &Path) -> u32 {
    trusted_owner(root == Path::new("/"), super::safety::current_euid())
}

fn check_trusted(
    path: &str,
    dir: &Path,
    meta: &fs::Metadata,
    trusted: u32,
) -> Result<(), Diagnostic> {
    use std::os::unix::fs::MetadataExt;
    match untrusted_directory(meta.uid(), meta.mode(), trusted) {
        None => Ok(()),
        Some(why) => Err(Diagnostic::new(
            "E_PATH_ESCAPE",
            format!(
                "{path}: {} is not a trusted directory: {why}",
                dir.display()
            ),
        )
        .hint(
            "lodi changes a host path only below directories that nobody but their owner can \
             write; fix that directory's owner or mode, or move the path elsewhere. Nothing was \
             changed",
        )),
    }
}

/// The refusal for a directory on the host state path that this process could not inspect: its
/// metadata or its listing failed for a reason other than its absence. The scan never treats
/// such a directory as missing or empty.
pub fn uninspectable(path: &str, dir: &Path, error: &io::Error) -> Diagnostic {
    Diagnostic::new(
        "E_PATH_ESCAPE",
        format!(
            "{path}: {} is not a trusted directory: it could not be inspected ({error})",
            dir.display()
        ),
    )
    .hint(
        "lodi reads the host state only from directories it can inspect, and never assumes one it \
         could not look into holds no journal; run it as the root's owner (sudo on /), or give \
         that directory back its owner's read and search bits. Nothing was changed",
    )
}

fn io_error(path: &str, error: io::Error) -> Diagnostic {
    Diagnostic::new("E_STORE_IO", format!("{path}: {error}"))
}

fn relative(path: &str) -> Result<&Path, Diagnostic> {
    let stripped = path
        .strip_prefix('/')
        .ok_or_else(|| escape(path, "the path is not absolute inside the selected root"))?;
    let rel = Path::new(stripped);
    if rel.as_os_str().is_empty()
        || rel
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(escape(
            path,
            "the path does not stay inside the selected root",
        ));
    }
    Ok(rel)
}

/// Check every existing component without following a symlink.  `final_file` controls whether
/// an existing last component must be a regular file. This is the check for a path that is
/// read; a path that is changed goes through [`inspect_trusted`].
pub fn inspect(root: &Path, path: &str, final_file: bool) -> Result<PathBuf, Diagnostic> {
    walk(root, path, final_file, None)
}

/// [`inspect`], and the root and every existing directory on the path — the last component
/// included when it is one — must also be trusted ([`untrusted_directory`]).
pub fn inspect_trusted(root: &Path, path: &str, final_file: bool) -> Result<PathBuf, Diagnostic> {
    walk(root, path, final_file, Some(trusted_for(root)))
}

/// The host state path (`<root>/var/lib/lodi/host/journal`), checked like [`inspect_trusted`]
/// before any journal in it is listed or read. Only a missing component ends the walk early; one
/// this process cannot look into is refused ([`uninspectable`]), because what it hides — a
/// pending journal — is exactly what the scan exists to find.
pub fn check_state_path(root: &Path, path: &str) -> Result<(), Diagnostic> {
    let trusted = trusted_for(root);
    let rel = relative(path)?;
    let mut cursor = root.to_path_buf();
    let mut meta = fs::symlink_metadata(&cursor).map_err(|e| uninspectable(path, &cursor, &e))?;
    check_trusted(path, &cursor, &meta, trusted)?;
    for part in rel.components() {
        cursor.push(part.as_os_str());
        meta = match fs::symlink_metadata(&cursor) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(uninspectable(path, &cursor, &error)),
        };
        if meta.file_type().is_symlink() {
            return Err(escape(
                path,
                format!("{} is a symbolic link", cursor.display()),
            ));
        }
        if !meta.is_dir() {
            return Err(escape(
                path,
                format!("{} is not a directory", cursor.display()),
            ));
        }
        check_trusted(path, &cursor, &meta, trusted)?;
    }
    Ok(())
}

fn walk(
    root: &Path,
    path: &str,
    final_file: bool,
    trusted: Option<u32>,
) -> Result<PathBuf, Diagnostic> {
    let rel = relative(path)?;
    if let Some(trusted) = trusted {
        let meta = fs::symlink_metadata(root).map_err(|e| io_error(path, e))?;
        check_trusted(path, root, &meta, trusted)?;
    }
    let mut cursor = root.to_path_buf();
    let parts: Vec<_> = rel.components().collect();
    for (index, part) in parts.iter().enumerate() {
        cursor.push(part.as_os_str());
        match fs::symlink_metadata(&cursor) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(escape(
                        path,
                        format!("{} is a symbolic link", cursor.display()),
                    ));
                }
                let last = index + 1 == parts.len();
                if !last && !meta.is_dir() {
                    return Err(escape(
                        path,
                        format!("{} is not a directory", cursor.display()),
                    ));
                }
                if last && final_file && !meta.is_file() {
                    return Err(escape(path, "the final component is not a regular file"));
                }
                if let Some(trusted) = trusted
                    && meta.is_dir()
                {
                    check_trusted(path, &cursor, &meta, trusted)?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(io_error(path, error)),
        }
    }
    Ok(root.join(rel))
}

/// Create an in-root directory path, one checked component at a time. A directory created on
/// the way is [`PARENT_MODE`], and the last component, when it is created, is `mode`. This is
/// the form for a destination outside a host root (`lodi host import --out`, `lodi host arm`);
/// what the host apply changes goes through [`ensure_dir_trusted`].
pub fn ensure_dir(root: &Path, path: &str, mode: u32) -> Result<PathBuf, Diagnostic> {
    make_dir(root, path, mode, None)
}

/// [`ensure_dir`], and the root and every directory that already exists on the way must also be
/// trusted ([`untrusted_directory`]).
pub fn ensure_dir_trusted(root: &Path, path: &str, mode: u32) -> Result<PathBuf, Diagnostic> {
    make_dir(root, path, mode, Some(trusted_for(root)))
}

fn make_dir(
    root: &Path,
    path: &str,
    mode: u32,
    trusted: Option<u32>,
) -> Result<PathBuf, Diagnostic> {
    let rel = relative(path)?;
    if let Some(trusted) = trusted {
        let meta = fs::symlink_metadata(root).map_err(|e| io_error(path, e))?;
        check_trusted(path, root, &meta, trusted)?;
    }
    let mut cursor = root.to_path_buf();
    let parts: Vec<_> = rel.components().collect();
    for (index, part) in parts.iter().enumerate() {
        cursor.push(part.as_os_str());
        match fs::symlink_metadata(&cursor) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(escape(
                    path,
                    format!("{} is a symbolic link", cursor.display()),
                ));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(escape(
                    path,
                    format!("{} is not a directory", cursor.display()),
                ));
            }
            Ok(meta) => {
                if let Some(trusted) = trusted {
                    check_trusted(path, &cursor, &meta, trusted)?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let created = if index + 1 == parts.len() {
                    mode
                } else {
                    PARENT_MODE
                };
                create_dir_mode(&cursor, created).map_err(|e| io_error(path, e))?;
                // The umask can only have removed bits from `created`; give them back.
                fs::set_permissions(&cursor, fs::Permissions::from_mode(created))
                    .map_err(|e| io_error(path, e))?;
                sync_parent(&cursor);
            }
            Err(error) => return Err(io_error(path, error)),
        }
    }
    Ok(cursor)
}

/// Create one directory born with at most `mode`: the umask can remove bits from it but never
/// add any, so no directory this module creates is ever wider than its final mode, not even for
/// the instant before its mode is set.
fn create_dir_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(mode).create(path)
}

/// Create the parent directories of an in-root path at `mode`, one checked component at a
/// time, and return the path itself with every existing component proven not to be a link.
pub fn ensure_parent(root: &Path, path: &str, mode: u32) -> Result<PathBuf, Diagnostic> {
    make_parent(root, path, mode, None)
}

/// [`ensure_parent`] for a path the host apply changes: every directory on the way is trusted.
pub fn ensure_parent_trusted(root: &Path, path: &str, mode: u32) -> Result<PathBuf, Diagnostic> {
    make_parent(root, path, mode, Some(trusted_for(root)))
}

fn make_parent(
    root: &Path,
    path: &str,
    mode: u32,
    trusted: Option<u32>,
) -> Result<PathBuf, Diagnostic> {
    let rel = relative(path)?;
    let parent = rel
        .parent()
        .ok_or_else(|| escape(path, "the path has no parent"))?;
    if !parent.as_os_str().is_empty() {
        let shown = format!("/{}", parent.display());
        make_dir(root, &shown, mode, trusted)?;
    }
    walk(root, path, false, trusted)
}

fn open_error(shown: &str, error: io::Error) -> Diagnostic {
    if error.raw_os_error() == Some(libc::ELOOP) {
        escape(shown, "the final component is a symbolic link")
    } else {
        io_error(shown, error)
    }
}

/// Open a managed path — a file below the root that the host scope observes, backs up or
/// restores from — for reading. It is opened without following a final symbolic link and
/// **without blocking**, and the descriptor is kept only if it is a regular file: a FIFO, a
/// device or anything else found there is refused at once, so nothing at a managed path can
/// hold the process still while it holds the apply lock. The descriptor reads normally
/// afterwards.
pub fn open_regular(path: &Path, shown: &str) -> Result<File, Diagnostic> {
    let handle = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| open_error(shown, e))?;
    // Judged on the descriptor itself, so nothing swapped in after an earlier check can pass.
    let meta = handle.metadata().map_err(|e| io_error(shown, e))?;
    if !meta.file_type().is_file() {
        return Err(escape(shown, "the final component is not a regular file"));
    }
    // SAFETY: the descriptor belongs to `handle`; these only read and set its status flags.
    let flags = unsafe { libc::fcntl(handle.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(handle.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0
    {
        return Err(io_error(shown, io::Error::last_os_error()));
    }
    Ok(handle)
}

/// How a temporary file is opened: created fresh, never through a link, and at 0600 from its
/// first instant, so that the bytes of a managed file are never observable above the mode they
/// are declared with — the declared mode is applied afterwards, and a process killed before
/// that leaves a private file behind, not a world-readable one. `copy()` (backups) goes through
/// the same open.
fn temp_open_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    options
}

fn temp_file(
    parent: &Path,
    name: &std::ffi::OsStr,
    shown: &str,
) -> Result<(PathBuf, File), Diagnostic> {
    for attempt in 0..64u32 {
        let temp = parent.join(format!(
            ".{}.lodi-{}-{attempt}",
            String::from_utf8_lossy(name.as_bytes()),
            std::process::id()
        ));
        match temp_open_options().open(&temp) {
            Ok(file) => return Ok((temp, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(shown, error)),
        }
    }
    Err(io_error(
        shown,
        io::Error::new(io::ErrorKind::AlreadyExists, "no temporary name was free"),
    ))
}

fn chown(handle: &File, uid: u32, gid: u32, shown: &str, owner: &str) -> Result<(), Diagnostic> {
    // SAFETY: the descriptor belongs to `handle`; `fchown` cannot be redirected by replacing a
    // path between validation and this call.
    let rc = unsafe { libc::fchown(handle.as_raw_fd(), uid, gid) };
    if rc == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EPERM) {
        return Err(Diagnostic::new(
            "E_NEED_ROOT",
            format!("{shown} declares owner {owner}, which this process cannot set"),
        )
        .hint("run the command as that user or as root"));
    }
    Err(io_error(shown, error))
}

fn write_atomic(
    root: &Path,
    path: &str,
    bytes: &[u8],
    mode: u32,
    uid: u32,
    gid: u32,
    owner: &str,
) -> Result<(), Diagnostic> {
    let target = ensure_parent_trusted(root, path, PARENT_MODE)?;
    if let Ok(meta) = fs::symlink_metadata(&target)
        && (!meta.is_file() || meta.file_type().is_symlink())
    {
        return Err(escape(path, "the final component is not a regular file"));
    }
    let parent = target
        .parent()
        .ok_or_else(|| escape(path, "the path has no parent"))?;
    let name = target
        .file_name()
        .ok_or_else(|| escape(path, "the path has no file name"))?;
    let (temp, mut handle) = temp_file(parent, name, path)?;
    let result = (|| {
        handle.write_all(bytes).map_err(|e| io_error(path, e))?;
        chown(&handle, uid, gid, path, owner)?;
        // Ownership changes may clear set-id bits, so the declared mode is applied afterwards.
        handle
            .set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|e| io_error(path, e))?;
        handle.sync_all().map_err(|e| io_error(path, e))?;
        // A file system that takes no label, such as the EFI system partition's FAT under
        // SELinux, keeps the one its mount gives every file.
        match crate::util::keep_label(&target, &handle) {
            Err(e) if e.raw_os_error() == Some(libc::EOPNOTSUPP) => {}
            other => other.map_err(|e| io_error(path, e))?,
        }
        drop(handle);
        inspect_trusted(root, path, false)?;
        fs::rename(&temp, &target).map_err(|e| io_error(path, e))?;
        sync_parent(&target);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

pub fn materialise(root: &Path, path: &str, desired: &Desired) -> Result<(), Diagnostic> {
    let bytes = match &desired.content {
        Content::Inline(bytes) => bytes.clone(),
        Content::Kept(kept) => {
            let mut handle = open_regular(kept, path)?;
            let mut bytes = Vec::new();
            handle
                .read_to_end(&mut bytes)
                .map_err(|e| io_error(path, e))?;
            bytes
        }
    };
    write_atomic(
        root,
        path,
        &bytes,
        desired.mode,
        desired.uid,
        desired.gid,
        &desired.owner,
    )
}

/// Write one of lodi's own files (sd-1): owned by root on `/`, by the invoking user under a
/// `--root`, at `mode`.
pub fn write_owned(root: &Path, path: &str, bytes: &[u8], mode: u32) -> Result<(), Diagnostic> {
    let uid = trusted_for(root);
    // A group of `-1` leaves the new file's group as it is: root's on `/`, the user's own else.
    let gid = if uid == 0 { 0 } else { u32::MAX };
    write_atomic(root, path, bytes, mode, uid, gid, &uid.to_string())
}

pub fn copy(root: &Path, from: &str, to: &str) -> Result<(), Diagnostic> {
    let source = inspect_trusted(root, from, true)?;
    let destination = inspect_trusted(root, to, false)?;
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            return Err(Diagnostic::new(
                "E_APPLY",
                format!("refusing to overwrite the existing backup {to}"),
            )
            .hint("move that backup aside and plan again"));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(to, error)),
    }
    let mut handle = open_regular(&source, from)?;
    let meta = handle.metadata().map_err(|e| io_error(from, e))?;
    let mut bytes = Vec::new();
    handle
        .read_to_end(&mut bytes)
        .map_err(|e| io_error(from, e))?;
    write_atomic(
        root,
        to,
        &bytes,
        meta.permissions().mode() & 0o7777,
        std::os::unix::fs::MetadataExt::uid(&meta),
        std::os::unix::fs::MetadataExt::gid(&meta),
        &std::os::unix::fs::MetadataExt::uid(&meta).to_string(),
    )
}

pub fn remove(root: &Path, path: &str) -> Result<(), Diagnostic> {
    let target = inspect_trusted(root, path, true)?;
    fs::remove_file(&target).map_err(|e| io_error(path, e))?;
    sync_parent(&target);
    Ok(())
}

pub fn metadata(root: &Path, path: &str, desired: &Desired) -> Result<(), Diagnostic> {
    let target = inspect_trusted(root, path, true)?;
    let handle = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&target)
        .map_err(|e| io_error(path, e))?;
    // Judged on the descriptor that is about to be changed, so no other name can stand in.
    let links =
        std::os::unix::fs::MetadataExt::nlink(&handle.metadata().map_err(|e| io_error(path, e))?);
    if links > 1 {
        return Err(hard_linked(path, links));
    }
    // SAFETY: the descriptor belongs to `handle`; `fchown` cannot follow a replaced path.
    let rc = unsafe { libc::fchown(handle.as_raw_fd(), desired.uid, desired.gid) };
    if rc != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EPERM) {
            return Err(Diagnostic::new(
                "E_NEED_ROOT",
                format!(
                    "{path} declares owner {}, which this process cannot set",
                    desired.owner
                ),
            )
            .hint("run the command as that user or as root"));
        }
        return Err(io_error(path, error));
    }
    // As in the atomic writer, make mode the last metadata operation because chown may clear
    // set-id bits.
    handle
        .set_permissions(fs::Permissions::from_mode(desired.mode))
        .map_err(|e| io_error(path, e))
}

/// The refusal for a managed file whose owner or mode would change while another name reaches
/// the same file.
pub fn hard_linked(path: &str, links: u64) -> Diagnostic {
    Diagnostic::new(
        "E_PATH_ESCAPE",
        format!("{path} has {links} hard links, and lodi changes the owner or mode only of a file with one"),
    )
    .hint("remove the other links, or replace the file with a copy of itself, and plan again. Nothing was changed")
}

pub fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(handle) = File::open(parent)
    {
        let _ = handle.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A managed file's bytes are never observable above 0600 while they are being written: the
    /// temporary file is created at 0600 whatever the umask, and the declared mode is applied
    /// afterwards. This is what `write_atomic` — and through it `copy()` — opens.
    #[test]
    fn a_temporary_file_is_private_from_its_first_instant() {
        // Scratch lives under the build directory, never in the temporary directory under an
        // invented name (`AGENTS.md` §6): a unit test has no `CARGO_TARGET_TMPDIR`, and what is
        // left behind by a panicking run goes with `cargo clean` rather than staying in `/tmp`.
        let dir = Path::new(env!("OUT_DIR")).join("hostscope-files-temp");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (temp, handle) =
            temp_file(&dir, std::ffi::OsStr::new("secret"), "/etc/secret").unwrap();
        let mode = handle.metadata().unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600, "{} was created at {mode:04o}", temp.display());
        assert_eq!(
            fs::symlink_metadata(&temp).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        drop(handle);
        let _ = fs::remove_dir_all(&dir);
    }
    /// The rule a directory on the way to a changed path is judged by, with injected owners and
    /// modes, since a second uid is not available to an unprivileged test: owned by the one
    /// trusted uid and neither group- nor world-writable, a sticky bit changing nothing.
    #[test]
    fn a_directory_is_trusted_only_if_nobody_but_its_owner_can_write_it() {
        // On `/`, the trusted owner is root; under a `--root`, the invoking user.
        assert_eq!(trusted_owner(true, 1000), 0);
        assert_eq!(trusted_owner(true, 0), 0);
        assert_eq!(trusted_owner(false, 1000), 1000);
        for mode in [0o755, 0o700, 0o555, 0o40755, 0o1755] {
            assert_eq!(untrusted_directory(0, mode, 0), None, "{mode:o}");
            assert_eq!(untrusted_directory(1000, mode, 1000), None, "{mode:o}");
        }
        for (owner, mode, trusted, says) in [
            (1000, 0o755, 0, "owned by uid 1000, not by root"),
            (
                0,
                0o755,
                1000,
                "owned by uid 0, not by the invoking user (uid 1000)",
            ),
            (1001, 0o700, 1000, "owned by uid 1001"),
            (0, 0o775, 0, "mode 0775: group- or world-writable"),
            (0, 0o757, 0, "mode 0757"),
            (0, 0o1777, 0, "mode 1777"),
            (1000, 0o40770, 1000, "mode 0770"),
        ] {
            let why = untrusted_directory(owner, mode, trusted)
                .unwrap_or_else(|| panic!("{owner} {mode:o} {trusted} was trusted"));
            assert!(why.contains(says), "{why}");
        }
    }

    /// The owner and mode of a file are changed only through a descriptor whose file has one
    /// link, judged on that descriptor, so a second name is refused even when the plan's own
    /// check was passed before it appeared.
    #[test]
    fn metadata_refuses_a_file_with_a_second_link_at_the_descriptor() {
        use std::os::unix::fs::MetadataExt;
        let dir = Path::new(env!("OUT_DIR")).join("hostscope-files-links");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("managed"), b"bytes").unwrap();
        fs::set_permissions(dir.join("managed"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(dir.join("managed"), dir.join("second")).unwrap();
        let meta = fs::metadata(dir.join("managed")).unwrap();
        let desired = Desired {
            digest: String::new(),
            mode: 0o644,
            uid: meta.uid(),
            gid: meta.gid(),
            owner: meta.uid().to_string(),
            group: meta.gid().to_string(),
            content: Content::Inline(Vec::new()),
        };
        let error = metadata(&dir, "/managed", &desired).unwrap_err();
        assert_eq!(error.code, "E_PATH_ESCAPE");
        assert_eq!(
            fs::metadata(dir.join("second"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
        fs::remove_file(dir.join("second")).unwrap();
        metadata(&dir, "/managed", &desired).unwrap();
        assert_eq!(
            fs::metadata(dir.join("managed"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o644
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A FIFO at a managed path — observed, backed up or restored from — is refused at once
    /// rather than read: the open never waits for a writer, and what it found is judged on the
    /// descriptor. The open runs on its own thread with a bounded wait, so a regression fails
    /// this test instead of hanging it; a regular file at the same place still reads normally.
    #[test]
    fn a_fifo_at_a_managed_path_is_refused_without_blocking() {
        let dir = Path::new(env!("OUT_DIR")).join("hostscope-files-fifo");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let fifo = dir.join("kept");
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let desired = Desired {
            digest: String::new(),
            mode: 0o644,
            uid: super::super::safety::current_euid(),
            gid: super::super::safety::current_egid(),
            owner: String::new(),
            group: String::new(),
            content: Content::Kept(fifo.clone()),
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        let (opened, restored) = (fifo.clone(), dir.clone());
        std::thread::spawn(move || {
            let direct = open_regular(&opened, "/kept").map(|_| ());
            let restore = materialise(&restored, "/restored", &desired);
            let _ = sender.send((direct, restore));
        });
        let (direct, restore) = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("opening a FIFO at a managed path blocked instead of being refused");
        for error in [direct.unwrap_err(), restore.unwrap_err()] {
            assert_eq!(error.code, "E_PATH_ESCAPE", "{}", error.message);
            assert!(
                error.message.contains("not a regular file"),
                "{}",
                error.message
            );
        }
        assert!(!dir.join("restored").exists(), "nothing was written");
        fs::remove_file(&fifo).unwrap();
        fs::write(&fifo, b"kept bytes").unwrap();
        let mut bytes = Vec::new();
        open_regular(&fifo, "/kept")
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"kept bytes");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A directory created on the way is born with its final mode, so under an empty umask it
    /// is never group- or world-writable, not even before its mode is set; and a umask that
    /// strips bits still leaves the declared mode. The umask belongs to the whole process and
    /// the other tests of this binary run on its other threads, so the check runs in a child of
    /// this binary that runs this test alone.
    #[test]
    fn a_created_directory_is_born_with_its_mode_whatever_the_umask() {
        const NAME: &str =
            "hostscope::files::tests::a_created_directory_is_born_with_its_mode_whatever_the_umask";
        const CHILD: &str = "LODI_TEST_UMASK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--test-threads=1", "--nocapture"])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success(), "the umask child failed: {status}");
            return;
        }
        let mode = |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777;
        let dir = Path::new(env!("OUT_DIR")).join("hostscope-files-birth-mode");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: `umask` cannot fail; this child process runs no other test.
        let previous = unsafe { libc::umask(0) };
        // The birth itself, before any `set_permissions`: `fs::create_dir` would give 0777.
        for (name, wanted) in [("born-parent", PARENT_MODE), ("born-private", 0o700)] {
            create_dir_mode(&dir.join(name), wanted).unwrap();
            assert_eq!(mode(&dir.join(name)), wanted, "{name} at birth");
        }
        ensure_dir(&dir, "/open/deeper/leaf", 0o700).unwrap();
        for (rel, wanted) in [
            ("open", PARENT_MODE),
            ("open/deeper", PARENT_MODE),
            ("open/deeper/leaf", 0o700),
        ] {
            assert_eq!(mode(&dir.join(rel)), wanted, "{rel} under umask 0000");
        }
        // SAFETY: as above.
        unsafe { libc::umask(0o077) };
        ensure_dir_trusted(&dir, "/narrow/leaf", PARENT_MODE).unwrap();
        for rel in ["narrow", "narrow/leaf"] {
            assert_eq!(mode(&dir.join(rel)), PARENT_MODE, "{rel} under umask 0077");
        }
        // SAFETY: as above.
        unsafe { libc::umask(previous) };
        let _ = fs::remove_dir_all(&dir);
    }
}
