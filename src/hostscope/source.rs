//! A host is a directory you own (LD-379, W3 of `docs/design/HOST_WORKFLOW.md`).
//!
//! `lodi host plan`, `apply` and `import` take an optional positional `SOURCE`:
//!
//! - if `SOURCE/host.toml` exists, `SOURCE` is the host, and `--host` beside it is a usage error;
//! - otherwise the host is `SOURCE/<name>/`, where the name is `--host NAME` or the root's
//!   hostname — `<root>/etc/hostname`, and `gethostname` only when the root is `/` — and must be
//!   one safe path component. There is no `#` selector: zsh's `EXTENDED_GLOB` reads `#` as a
//!   glob operator;
//! - a host directory that is not there is `E_NO_MANIFEST`, whose hint lists the hosts that are.
//!
//! With no `SOURCE` the host is `<root>/etc/lodi`, read by the rules it always was.
//!
//! # Applying a directory someone else could write is refused, by rule
//!
//! The trusted owners are root, and the invoking user when unprivileged, or `SUDO_UID` when root
//! runs under `sudo` with a `SUDO_UID` that is not 0 ([`trusted_owners`]). Every directory from
//! the root — `/` itself, or the `--root` under one, which is why a `SOURCE` outside a `--root` is
//! refused — down to the host directory, and every directory and file the apply reads below it,
//! must not be a symbolic link (never followed: `O_NOFOLLOW`), must belong to a trusted owner and
//! must have `mode & 0o022 == 0`, with no exception for a sticky bit ([`untrusted`]). A file must
//! be a regular one, judged on its open descriptor without blocking. A host directory on NFS or
//! SMB/CIFS is refused ([`refused_filesystem`]). Every refusal is `E_PATH_ESCAPE` (exit 3),
//! naming the path and the rule; there is no bypass flag. Arming (`/etc/lodi/host-allowed`) and
//! LD-333's rule for what an apply changes are unchanged.
//!
//! The walk is made on **descriptors**, not on names (validator-repair-1): each directory is
//! opened below the descriptor of the one above it with `O_NOFOLLOW | O_DIRECTORY`, and judged by
//! `fstat` on the descriptor it opened, so what is judged is what the next step opens below.
//! Every read of a file walks again from the root, so a directory on the way swapped for a link
//! after an earlier walk is refused at the read, never followed ([`open_walk`]).
//!
//! # Read once
//!
//! The manifest and every `source` it declares are read into memory, each under a size cap,
//! before the first plan ([`Input`]); the plan hashes those bytes, the re-plan under the apply
//! lock uses the same bytes, and the apply writes them. Nothing the apply acts on is opened twice.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use crate::diag::Diagnostic;

use super::landing;
use super::safety::{self, Gate, Options};

/// The rule every refusal of this module states in its hint.
const RULE: &str = "lodi reads a host directory only when every directory from / down to it, and \
                    every file it reads there, is not a symbolic link, belongs to root or to you \
                    (under sudo, the user who ran sudo) and is not writable by group or others, \
                    and only off NFS and SMB; fix what the message names (chmod go-w, or move \
                    it). Nothing was read or changed";

/// The owners a host directory, and everything the apply reads in it, may have.
///
/// - an unprivileged user (`euid` not 0): that user, and root;
/// - root under `sudo`, with `SUDO_UID` a user id that is not 0: root and that user;
/// - otherwise: root only.
///
/// Root is in every row: it owns `/` and can write any path whatever the path's owner, so trusting
/// it trusts nothing new. `SUDO_UID` counts only when the process is root — an unprivileged
/// process cannot become anyone, so the variable can only ever narrow what root trusts — and
/// only when it is plain decimal digits.
pub fn trusted_owners(euid: u32, sudo_uid: Option<&OsStr>) -> Vec<u32> {
    owners_for(euid, sudo_uid.and_then(parse_id))
}

/// [`trusted_owners`] with `SUDO_UID` already read.
pub fn owners_for(euid: u32, sudo_uid: Option<u32>) -> Vec<u32> {
    if euid != 0 {
        return vec![0, euid];
    }
    match sudo_uid {
        Some(uid) if uid != 0 => vec![0, uid],
        _ => vec![0],
    }
}

/// A user or group id as `sudo` writes one: decimal digits and nothing else.
pub fn parse_id(text: &OsStr) -> Option<u32> {
    let text = text.to_str()?;
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Why an entry with this owner and mode is not one lodi reads a host from, or `None` when it is.
pub fn untrusted(owner: u32, mode: u32, owners: &[u32]) -> Option<String> {
    if !owners.contains(&owner) {
        let named: Vec<String> = owners
            .iter()
            .map(|id| {
                if *id == 0 {
                    "root".to_string()
                } else {
                    format!("uid {id}")
                }
            })
            .collect();
        return Some(format!(
            "it is owned by uid {owner}, and only {} may own it",
            named.join(" or ")
        ));
    }
    if mode & 0o022 != 0 {
        let sticky = if mode & 0o1000 != 0 {
            " (a sticky bit is no exception)"
        } else {
            ""
        };
        return Some(format!(
            "it is mode {:04o}: writable by group or others{sticky}",
            mode & 0o7777
        ));
    }
    None
}

/// Why a filesystem of this `statfs` type is refused, or `None`: a network filesystem's owners
/// and modes are the server's word, not this kernel's.
pub fn refused_filesystem(f_type: i64) -> Option<&'static str> {
    match f_type {
        0x6969 => Some("NFS"),
        0x517B => Some("SMB"),
        0xFF53_4D42 => Some("SMB/CIFS"),
        0xFE53_4D42 => Some("SMB2"),
        _ => None,
    }
}

fn escape(path: &Path, why: &str) -> Diagnostic {
    Diagnostic::new(
        "E_PATH_ESCAPE",
        format!("{} is not trusted as a host: {why}", path.display()),
    )
    .hint(RULE)
}

/// The flags every directory of the walk is opened with: never through a link, and only a
/// directory.
const DIR_FLAGS: i32 = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
/// The flags a file is opened with: never through a link, and without blocking on a FIFO or a
/// device before it is judged.
const FILE_FLAGS: i32 =
    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY;

/// Open `name` below the directory `parent` — or the absolute `name` itself when there is no
/// parent — with `flags`, as a file.
fn open_at(parent: Option<&fs::File>, name: &OsStr, flags: i32) -> io::Result<fs::File> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in a path"))?;
    let at = parent.map_or(libc::AT_FDCWD, AsRawFd::as_raw_fd);
    // SAFETY: `name` is a NUL-terminated string that outlives the call; `at` is open for it.
    let fd = unsafe { libc::openat(at, name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened here and is owned by nothing else.
    Ok(fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
}

/// Why an open of a walk's step failed, in the module's terms.
fn refused_open(path: &Path, error: &io::Error) -> Diagnostic {
    let link = fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink());
    if link || error.raw_os_error() == Some(libc::ELOOP) {
        return escape(path, "it is a symbolic link");
    }
    if error.raw_os_error() == Some(libc::ENOTDIR) {
        return escape(path, "it is not a directory");
    }
    escape(path, &format!("it could not be opened ({error})"))
}

/// Judge an open directory by the descriptor it was opened on.
fn judge_open_dir(path: &Path, handle: &fs::File, owners: &[u32]) -> Result<(), Diagnostic> {
    let meta = handle
        .metadata()
        .map_err(|e| escape(path, &format!("it could not be inspected ({e})")))?;
    if !meta.is_dir() {
        return Err(escape(path, "it is not a directory"));
    }
    if let Some(why) = untrusted(meta.uid(), meta.mode(), owners) {
        return Err(escape(path, &why));
    }
    Ok(())
}

/// Open one directory below `parent` and judge it; `Ok(None)` when it is not there.
fn step(
    parent: &fs::File,
    path: &Path,
    name: &OsStr,
    owners: &[u32],
) -> Result<Option<fs::File>, Diagnostic> {
    match open_at(Some(parent), name, DIR_FLAGS) {
        Ok(handle) => {
            judge_open_dir(path, &handle, owners)?;
            Ok(Some(handle))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(refused_open(path, &e)),
    }
}

/// Open `dir` by walking to it from `anchor` one component at a time, each opened below the
/// descriptor of the one above and judged on its own descriptor, `anchor` and `dir` included.
/// A step that is not there is refused as well: the walk leads to a directory that must exist.
fn open_walk(anchor: &Path, dir: &Path, owners: &[u32]) -> Result<fs::File, Diagnostic> {
    let below = dir.strip_prefix(anchor).map_err(|_| {
        escape(
            dir,
            &format!(
                "it is not below the root {}, where a host must live",
                anchor.display()
            ),
        )
    })?;
    let mut handle =
        open_at(None, anchor.as_os_str(), DIR_FLAGS).map_err(|e| refused_open(anchor, &e))?;
    judge_open_dir(anchor, &handle, owners)?;
    let mut cursor = anchor.to_path_buf();
    for part in below.components() {
        cursor.push(part.as_os_str());
        handle = step(&handle, &cursor, part.as_os_str(), owners)?
            .ok_or_else(|| escape(&cursor, "it is not there"))?;
    }
    Ok(handle)
}

/// Whether `name` exists below the open directory `dir`, a link counting as existing and never
/// followed.
fn exists_at(dir: &fs::File, name: &str) -> bool {
    let Ok(name) = CString::new(name) else {
        return false;
    };
    // SAFETY: `stat` is plain data the kernel fills; `dir` and `name` outlive the call.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        ) == 0
    }
}

/// Refuse a network filesystem under `fd`.
fn judge_filesystem(path: &Path, fd: i32) -> Result<(), Diagnostic> {
    // SAFETY: `stat` is plain data the kernel fills; `fd` is open for the whole call.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(fd, &mut stat) } != 0 {
        return Err(escape(
            path,
            &format!(
                "its filesystem could not be read ({})",
                io::Error::last_os_error()
            ),
        ));
    }
    #[allow(clippy::unnecessary_cast)]
    match refused_filesystem(stat.f_type as i64) {
        Some(kind) => Err(escape(
            path,
            &format!("it is on {kind}, a network filesystem"),
        )),
        None => Ok(()),
    }
}

/// An absolute, lexically normal form of `path`: relative to the working directory, with `.`
/// dropped and `..` taken back. Nothing is resolved through a link: the walk that follows refuses
/// every link on the result, so what is judged is what is opened.
fn absolute(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::from("/");
    for part in joined.components() {
        match part {
            Component::Normal(name) => out.push(name),
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}

/// Whether `name` is one safe path component.
pub fn safe_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\0')
        && name.len() <= 255
}

/// Where a command's host is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    /// The directory holding `host.toml` and `files/`.
    pub dir: PathBuf,
    /// Where every walk to `dir` begins: the root for a host a `SOURCE` chose, `dir` itself for
    /// the manifest in place, whose own directories the host scope's other rules judge.
    pub anchor: PathBuf,
    /// What the record names as the source of the last sync: `/etc/lodi` for the manifest in
    /// place, and otherwise the host directory as the root sees it (`~/hosts/box` spelled out, or
    /// `/hosts/box` under a `--root` whose `hosts/box` it is).
    pub source: String,
    /// Whether this is `<root>/etc/lodi`, read by the rules it always was.
    pub in_place: bool,
    /// The owners everything read here may have.
    pub owners: Vec<u32>,
    /// The directory the positional `SOURCE` named, when there was one: the owner of this is who
    /// an import under `sudo` writes as.
    pub named: Option<PathBuf>,
}

impl Host {
    /// The manifest in place, `<root>/etc/lodi`.
    pub fn in_place(gate: &Gate) -> Host {
        let dir = gate.root.join(&landing::IN_PLACE[1..]);
        Host {
            anchor: dir.clone(),
            dir,
            source: super::lock::IN_PLACE_SOURCE.to_string(),
            in_place: true,
            owners: vec![super::files::trusted_owner(gate.system_root, gate.euid)],
            named: None,
        }
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join(landing::MANIFEST)
    }
}

/// The usage error of `--host` beside a `SOURCE` that is itself a host, or `None`. `src/main.rs`
/// asks this before it runs the verb, and a usage error exits 2 with no `E_` code.
pub fn usage(options: &Options) -> Option<String> {
    let (Some(source), Some(host)) = (&options.source, &options.host) else {
        return None;
    };
    source
        .join(landing::MANIFEST)
        .symlink_metadata()
        .is_ok()
        .then(|| {
            format!(
                "--host {host} chooses a host in a directory of hosts, and {} is itself a host \
                 (it holds host.toml); drop --host, or name the directory of hosts",
                source.display()
            )
        })
}

/// The root's hostname: the first line of `<root>/etc/hostname`, and `gethostname` only when the
/// root is `/` and that file says nothing.
pub fn hostname(root: &Path, system_root: bool) -> Result<String, Diagnostic> {
    let path = root.join("etc/hostname");
    let mut text = String::new();
    if let Ok(file) = fs::File::open(&path) {
        let _ = file.take(4096).read_to_string(&mut text);
    }
    let named = text.lines().next().unwrap_or("").trim().to_string();
    if !named.is_empty() {
        return Ok(named);
    }
    if system_root {
        let mut buffer = [0u8; 256];
        // SAFETY: the buffer is ours and its length is passed with it.
        if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0 {
            let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
            let named = String::from_utf8_lossy(&buffer[..end]).trim().to_string();
            if !named.is_empty() {
                return Ok(named);
            }
        }
    }
    Err(Diagnostic::new(
        "E_NO_MANIFEST",
        format!(
            "{} names no hostname, so no host of the directory can be chosen by it",
            path.display()
        ),
    )
    .hint("name the host with --host NAME"))
}

/// The hosts a directory of hosts holds: its sub-directories with a `host.toml`, sorted.
fn hosts_in(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    entry.file_type().is_ok_and(|kind| kind.is_dir())
                        && entry.path().join(landing::MANIFEST).is_file()
                })
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

/// Choose and judge the host a command reads. `owners` are [`trusted_owners`] for the process.
/// For an import (`create`), a host directory that is not there yet is not a refusal: it is
/// where the import will write.
pub fn select(
    gate: &Gate,
    options: &Options,
    owners: &[u32],
    create: bool,
) -> Result<Host, Diagnostic> {
    let Some(named) = &options.source else {
        return Ok(Host::in_place(gate));
    };
    let named = absolute(named);
    if fs::symlink_metadata(&named).is_err() {
        return Err(Diagnostic::new(
            "E_NO_MANIFEST",
            format!("{} does not exist", named.display()),
        )
        .hint("SOURCE names an existing host directory, or a directory of hosts"));
    }
    let named_handle = open_walk(&gate.root, &named, owners)?;
    let (dir, handle) = if exists_at(&named_handle, landing::MANIFEST) {
        if options.host.is_some() {
            return Err(Diagnostic::new(
                "E_UNSUPPORTED",
                usage(options).unwrap_or_default(),
            ));
        }
        (named.clone(), named_handle)
    } else {
        let name = match &options.host {
            Some(name) => name.clone(),
            None => hostname(&gate.root, gate.system_root)?,
        };
        if !safe_component(&name) {
            return Err(escape(
                &named.join(&name),
                &format!("the host name {name:?} is not one path component"),
            ));
        }
        let dir = named.join(&name);
        match step(&named_handle, &dir, OsStr::new(&name), owners)? {
            Some(handle) => (dir, handle),
            None if create => (dir, named_handle),
            None => {
                let found = hosts_in(&named);
                let listed = if found.is_empty() {
                    "it holds no host".to_string()
                } else {
                    format!("the hosts it holds: {}", found.join(", "))
                };
                return Err(Diagnostic::new(
                    "E_NO_MANIFEST",
                    format!("no host {name:?} in {}", named.display()),
                )
                .hint(format!("{listed}; choose one with --host NAME")));
            }
        }
    };
    judge_filesystem(&dir, handle.as_raw_fd())?;
    let source = if gate.system_root {
        dir.display().to_string()
    } else {
        format!(
            "/{}",
            dir.strip_prefix(&gate.root).unwrap_or(&dir).display()
        )
    };
    Ok(Host {
        dir,
        anchor: gate.root.clone(),
        source,
        in_place: false,
        owners: owners.to_vec(),
        named: Some(named),
    })
}

/// Open one file below a host directory and read it, at most `limit` bytes: every directory
/// between the host directory and the file is judged, the file is opened without following a
/// link and without blocking, and its descriptor must be a regular file with a trusted owner and
/// no group or world write, off a network filesystem. `Ok(None)` is a file that is not there.
pub fn read_file(host: &Host, relative: &str, limit: usize) -> Result<Option<Vec<u8>>, Diagnostic> {
    let Some((handle, path, name)) = open_parent(host, relative)? else {
        return Ok(None);
    };
    let file = match open_at(Some(&handle), &name, FILE_FLAGS) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(escape(&path, "it is a symbolic link"));
        }
        Err(e) => return Err(escape(&path, &format!("it could not be opened ({e})"))),
    };
    let meta = file
        .metadata()
        .map_err(|e| escape(&path, &format!("it could not be inspected ({e})")))?;
    if !meta.is_file() {
        return Err(escape(&path, "it is not a regular file"));
    }
    if !host.in_place
        && let Some(why) = untrusted(meta.uid(), meta.mode(), &host.owners)
    {
        return Err(escape(&path, &why));
    }
    judge_filesystem(&path, file.as_raw_fd())?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| {
            Diagnostic::new("E_STORE_IO", format!("cannot read {}: {e}", path.display()))
        })?;
    Ok(Some(bytes))
}

fn open_parent(
    host: &Host,
    relative: &str,
) -> Result<Option<(fs::File, PathBuf, OsString)>, Diagnostic> {
    let rel = Path::new(relative);
    if rel
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(escape(
            &host.dir.join(rel),
            "it does not stay inside the host directory",
        ));
    }
    let path = host.dir.join(rel);
    // Walked again from the root on every read, on descriptors: nothing judged by an earlier walk
    // is trusted by name.
    let mut handle = open_walk(&host.anchor, &host.dir, &host.owners)?;
    let mut cursor = host.dir.clone();
    let parts: Vec<_> = rel.components().collect();
    let Some((last, between)) = parts.split_last() else {
        return Err(escape(&path, "it names no file"));
    };
    for part in between {
        cursor.push(part.as_os_str());
        match step(&handle, &cursor, part.as_os_str(), &host.owners)? {
            Some(next) => handle = next,
            None => return Ok(None),
        }
    }
    Ok(Some((handle, path, last.as_os_str().to_os_string())))
}

pub fn inspect(host: &Host, relative: &str) -> Result<Option<fs::FileType>, Diagnostic> {
    let Some((parent, path, name)) = open_parent(host, relative)? else {
        return Ok(None);
    };
    let file = match open_at(Some(&parent), &name, FILE_FLAGS) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(refused_open(&path, &e)),
    };
    let meta = file
        .metadata()
        .map_err(|e| escape(&path, &format!("it could not be inspected ({e})")))?;
    if let Some(why) = untrusted(meta.uid(), meta.mode(), &host.owners) {
        return Err(escape(&path, &why));
    }
    judge_filesystem(&path, file.as_raw_fd())?;
    Ok(Some(meta.file_type()))
}

pub fn read_dir(
    host: &Host,
    relative: &str,
) -> Result<Option<Vec<(OsString, fs::FileType)>>, Diagnostic> {
    let Some((parent, path, name)) = open_parent(host, relative)? else {
        return Ok(None);
    };
    let Some(dir) = step(&parent, &path, &name, &host.owners)? else {
        return Ok(None);
    };
    judge_filesystem(&path, dir.as_raw_fd())?;
    let fd_path = format!("/proc/self/fd/{}", dir.as_raw_fd());
    let entries = fs::read_dir(fd_path)
        .map_err(|e| escape(&path, &format!("it could not be listed ({e})")))?;
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| escape(&path, &format!("it could not be listed ({e})")))?;
        let kind = entry
            .file_type()
            .map_err(|e| escape(&path, &format!("it could not be inspected ({e})")))?;
        found.push((entry.file_name(), kind));
    }
    found.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(Some(found))
}

/// What a plan and an apply act on, read once (LD-379): the manifest's bytes and those of every
/// `source` it declares, keyed by the `source` as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub host: Host,
    pub manifest: Vec<u8>,
    pub sources: BTreeMap<String, Vec<u8>>,
    /// The git revision `host` was fetched at, for a host read from a URL (LD-401).
    pub git: Option<super::lock::GitRecord>,
}

/// Read the manifest of `host`, whole, under the manifest's cap. The manifest in place is read
/// the way it always was ([`safety::read_config`]); a host directory's by [`read_file`].
pub fn read_manifest(gate: &Gate, host: &Host) -> Result<Option<Vec<u8>>, Diagnostic> {
    let limit = crate::manifest::MAX_MANIFEST_BYTES;
    if host.in_place {
        safety::read_config(
            &gate.root,
            safety::MANIFEST,
            gate.system_root,
            gate.euid,
            limit,
        )
    } else {
        read_file(host, landing::MANIFEST, limit)
    }
}

/// Read every `source` of `manifest` below `host`, each under [`super::MAX_SOURCE_BYTES`].
pub fn read_sources(
    host: &Host,
    manifest: &super::manifest::HostManifest,
) -> Result<BTreeMap<String, Vec<u8>>, Diagnostic> {
    let limit = super::MAX_SOURCE_BYTES;
    let mut sources = BTreeMap::new();
    for (path, entry) in &manifest.files {
        let Some(source) = &entry.source else {
            continue;
        };
        if entry.state != super::manifest::FileState::Present || sources.contains_key(source) {
            continue;
        }
        let bytes = read_file(host, source, limit)?.ok_or_else(|| {
            Diagnostic::new(
                "E_STORE_IO",
                format!(
                    "{path}: source {source:?} is not there in {}",
                    host.dir.display()
                ),
            )
        })?;
        if bytes.len() > limit {
            return Err(Diagnostic::new(
                "E_SYNTAX",
                format!(
                    "{path}: source {source:?} is over {limit} bytes, the most one source may have"
                ),
            ));
        }
        sources.insert(source.clone(), bytes);
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_one_safe_path_component() {
        for good in ["box", "box.example", "b-o_x", "#box", "a#b"] {
            assert!(safe_component(good), "{good}");
        }
        for bad in ["", ".", "..", "/", "a/b", "../box", "a\0b"] {
            assert!(!safe_component(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_path_is_made_absolute_without_following_anything() {
        assert_eq!(absolute(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(absolute(Path::new("/../a")), PathBuf::from("/a"));
    }

    #[test]
    fn sudo_uid_is_digits_or_nothing() {
        assert_eq!(parse_id(OsStr::new("1000")), Some(1000));
        for bad in ["", "+1000", "-1", "1000 ", "x", "99999999999"] {
            assert_eq!(parse_id(OsStr::new(bad)), None, "{bad:?}");
        }
    }
}
