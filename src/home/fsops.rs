//! The one module of the home scope that mutates the filesystem (design call D4, `LD-100`).
//!
//! Two types carry the whole containment argument:
//!
//! - [`Root`] is a directory [`crate::roots::Roots`] computed. Its field is private and its only
//!   constructors are that struct's three methods, so nothing can invent one.
//! - [`RelPath`] is a relative path with no `..`, no `~`, no empty component, no `.`, no NUL and
//!   no component longer than 255 bytes. Its constructor is the only way to make one.
//!
//! Every entry point below takes both, and **none takes an absolute path**, so "write outside the
//! root" is not expressible in this API. Before a destination is traversed every component of it
//! is checked with `symlink_metadata`, and a symlink anywhere along it — the final component
//! included — is `E_PATH_ESCAPE`, exactly as `lodi init` already refuses a symlinked
//! `.gitignore`. Writes are atomic: a temporary file named from the sha256 of the content
//! (never a timestamp, a pid or a counter — design call D13, `LD-109`) is written, synced,
//! chmod-ed and renamed over the destination, and the directory is synced.
//!
//! When `LODI_FS_LEDGER` names an absolute path, every mutating call appends one
//! `<op>\t<absolute path>` line to it **before** it acts, so a crash in the middle of a write is
//! still visible. The ledger is a test and debugging facility (`AGENTS.md` §5), inert when the
//! variable is unset, and it is the one path this module touches outside a root.
//!
//! **Limitation, stated rather than papered over** (`docs/ERRORS.md`): the check-then-use of the
//! component scan is a TOCTOU window. Closing it needs `openat2(RESOLVE_BENEATH)`, which not
//! every kernel this project supports has. The window is narrow — the scan and the rename are
//! adjacent — and the root itself is trusted as the environment gave it, so a symlink *above* a
//! root is the user's own arrangement and is not refused.
//!
//! The lock of an apply comes through here too ([`lock`], [`try_lock`], [`lock_if_present`]),
//! bounded by the same [`Root`] and [`RelPath`] as every write: since M-0.5 `validator-repair-1`
//! there is **no** module of the home scope that opens a file itself, and no exception in the
//! containment lint to carry (`LD-185`, refining `LD-175`). The `flock` implementation is still
//! the store's one [`crate::store::FileLock`]; this module only decides *which path* may be
//! locked, and records the creation of a lock file in the ledger like any other mutation.
//!
//! The entry points take the root as a value and assume nothing about it being a home directory,
//! because M-0.6's host-scope files engine may later call this module.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::diag::Diagnostic;
use crate::util::sha256_hex;

pub use crate::roots::Root;

/// The lock a caller of this module gets back: the store's own [`crate::store::FileLock`], whose
/// `flock` is the one implementation in the binary. It is re-exported here so that no module of
/// the home scope has to name the store's type — the lint in `tests/home_containment.rs` holds
/// this file to being the only one that does (`LD-185`).
pub use crate::store::FileLock as Lock;

/// The longest single path component this module will create or touch.
const MAX_COMPONENT: usize = 255;

/// A path relative to a [`Root`] that cannot leave it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RelPath(String);

impl RelPath {
    /// Every refusal here is `E_PATH_ESCAPE` (exit 3) and names what it refused.
    pub fn new(text: &str) -> Result<RelPath, Diagnostic> {
        let refuse = |why: String| {
            Err(Diagnostic::new(
                "E_PATH_ESCAPE",
                format!(
                    "`{}` is not a path inside the root: {why}",
                    text.escape_debug()
                ),
            )
            .hint("paths in the home scope are relative to the root and may not leave it"))
        };
        if text.is_empty() {
            return refuse("it is empty".into());
        }
        if text.contains('\0') {
            return refuse("it has an embedded NUL byte".into());
        }
        if text.starts_with('/') {
            return refuse("it is absolute".into());
        }
        if text.contains('\\') {
            return refuse("it has a backslash".into());
        }
        let mut parts = Vec::new();
        for component in text.split('/') {
            match component {
                "" => return refuse("it has an empty component".into()),
                "." => return refuse("it has a `.` component".into()),
                ".." => return refuse("it has a `..` component".into()),
                _ => {}
            }
            if component.starts_with('~') {
                return refuse(format!("the component `{component}` starts with `~`"));
            }
            if component.len() > MAX_COMPONENT {
                let head: String = component.chars().take(16).collect();
                return refuse(format!(
                    "the component `{head}…` is {} bytes, more than {MAX_COMPONENT}",
                    component.len()
                ));
            }
            parts.push(component.to_string());
        }
        Ok(RelPath(parts.join("/")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The components, in order. There is always at least one.
    pub fn components(&self) -> std::str::Split<'_, char> {
        self.0.split('/')
    }
}

impl std::fmt::Display for RelPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The absolute path of `rel` inside `root`, with every component below the root proven not to be
/// a symlink. A component that does not exist yet is fine; one that exists and is a symlink is
/// `E_PATH_ESCAPE`, and so is one that exists, is not a directory and is not the last.
fn resolve_with_final_link(
    root: &Root,
    rel: &RelPath,
    allow_final_link: bool,
) -> Result<PathBuf, Diagnostic> {
    let mut at = root.path().to_path_buf();
    let components: Vec<&str> = rel.components().collect();
    let last = components.len() - 1;
    for (i, component) in components.into_iter().enumerate() {
        at.push(component);
        match fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() && !(allow_final_link && i == last) => {
                return Err(Diagnostic::new(
                    "E_PATH_ESCAPE",
                    format!(
                        "`{}` of `{rel}` is a symbolic link, and lodi does not follow one inside \
                         a root",
                        component
                    ),
                )
                .hint("remove the link, or point the entry at the file it names"));
            }
            Ok(meta) if i < last && !meta.is_dir() => {
                return Err(Diagnostic::new(
                    "E_PATH_ESCAPE",
                    format!("`{component}` of `{rel}` is not a directory"),
                )
                .hint("move what is in the way, or choose another path"));
            }
            _ => {}
        }
    }
    Ok(at)
}

fn resolve(root: &Root, rel: &RelPath) -> Result<PathBuf, Diagnostic> {
    resolve_with_final_link(root, rel, false)
}

/// Append one `<op>\t<absolute path>` line to the ledger, before the operation runs. A ledger
/// that cannot be written is a hard error: a containment record that silently stops recording is
/// worse than none (design call D4).
fn ledger(op: &str, path: &Path) -> Result<(), Diagnostic> {
    let Some(ledger) = crate::roots::ledger_path() else {
        return Ok(());
    };
    let line = format!("{op}\t{}\n", path.display());
    let write = || -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&ledger)?;
        file.write_all(line.as_bytes())?;
        file.sync_all()
    };
    write().map_err(|e| {
        Diagnostic::new(
            "E_STORE_IO",
            format!(
                "cannot append to the write ledger {}: {e}",
                ledger.display()
            ),
        )
        .hint("unset LODI_FS_LEDGER, or point it at a file lodi can append to")
    })
}

fn io(path: &Path, what: &str, e: std::io::Error) -> Diagnostic {
    Diagnostic::new(
        "E_STORE_IO",
        format!("cannot {what} {}: {e}", path.display()),
    )
}

/// Read a file inside `root`. The only entry point here that changes nothing, so it writes no
/// ledger line.
pub fn read(root: &Root, rel: &RelPath) -> Result<Vec<u8>, Diagnostic> {
    let path = resolve(root, rel)?;
    fs::read(&path).map_err(|e| io(&path, "read", e))
}

/// Whether `rel` is an existing directory, without following a final symbolic link.
pub fn is_dir(root: &Root, rel: &RelPath) -> bool {
    resolve(root, rel)
        .ok()
        .and_then(|path| fs::symlink_metadata(path).ok())
        .is_some_and(|meta| meta.is_dir())
}

/// Whether anything is at `path` — a file, a directory, a symbolic link, a dangling one included
/// — asked of `lstat` alone: nothing is opened or followed, and nothing is written. What
/// `lodi import --home` knows about a file of the user's (LD-341).
pub fn lexists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// List one contained directory without following a symbolic link in any component. A missing
/// directory is `None`; every other read error is reported.
pub fn entries(
    root: &Root,
    rel: &RelPath,
) -> Result<Option<Vec<(std::ffi::OsString, fs::FileType)>>, Diagnostic> {
    let path = resolve(root, rel)?;
    let read = match fs::read_dir(&path) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(&path, "read", e)),
    };
    let mut entries = Vec::new();
    for entry in read {
        let entry = entry.map_err(|e| io(&path, "read", e))?;
        let kind = entry
            .file_type()
            .map_err(|e| io(&entry.path(), "inspect", e))?;
        entries.push((entry.file_name(), kind));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Some(entries))
}

/// Create every missing directory of `rel` inside `root`.
pub fn mkdir_p(root: &Root, rel: &RelPath) -> Result<(), Diagnostic> {
    let path = resolve(root, rel)?;
    ledger("mkdir", &path)?;
    fs::create_dir_all(&path).map_err(|e| io(&path, "create", e))
}

/// Create every missing directory of `rel` inside `root` at mode 0700, set explicitly after each
/// creation so the umask plays no part (LD-361). A component that is already there is left as it
/// is. This is how the home scope creates its own directories below the data root, which hold
/// the digests of the user's private originals and the originals themselves.
pub fn mkdir_private(root: &Root, rel: &RelPath) -> Result<(), Diagnostic> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let path = resolve(root, rel)?;
    if !root.path().is_dir() {
        ledger("mkdir", root.path())?;
        fs::create_dir_all(root.path()).map_err(|e| io(root.path(), "create", e))?;
    }
    let mut at = root.path().to_path_buf();
    for component in rel.components() {
        at.push(component);
        if fs::symlink_metadata(&at).is_ok() {
            continue;
        }
        ledger("mkdir", &at)?;
        match fs::DirBuilder::new().mode(0o700).create(&at) {
            Ok(()) => fs::set_permissions(&at, fs::Permissions::from_mode(0o700))
                .map_err(|e| io(&at, "create", e))?,
            // Another process created it between the look and the create: its mode is its own.
            Err(e)
                if e.kind() == std::io::ErrorKind::AlreadyExists
                    && fs::symlink_metadata(&at).is_ok_and(|m| m.is_dir()) => {}
            Err(e) => return Err(io(&at, "create", e)),
        }
    }
    debug_assert_eq!(at, path);
    Ok(())
}

/// Whether a regular file is at `rel`, asked of `lstat` alone and with every component checked
/// as [`write`] checks it: a symbolic link there is `E_PATH_ESCAPE` and is never followed, a
/// missing path is `false`, and anything else that is not a regular file is `E_STORE_IO`.
pub fn is_file(root: &Root, rel: &RelPath) -> Result<bool, Diagnostic> {
    let path = resolve(root, rel)?;
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => Ok(true),
        Ok(_) => Err(Diagnostic::new(
            "E_STORE_IO",
            format!("{} is not a regular file", path.display()),
        )
        .hint("move what is in the way; lodi only ever writes a regular file there")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io(&path, "inspect", e)),
    }
}

/// Create the root directory itself when it is not there yet, as `lodi import --home` does for
/// configuration root no command has written before. An existing root is left as it is: the
/// root is trusted as the environment gave it.
pub fn mkdir_root(root: &Root) -> Result<(), Diagnostic> {
    let path = root.path();
    if fs::symlink_metadata(path).is_ok() {
        return Ok(());
    }
    ledger("mkdir", path)?;
    fs::create_dir_all(path).map_err(|e| io(path, "create", e))
}

/// Replace `rel` inside `root` with `bytes` at `mode`, atomically. The parent directories are
/// created first. On any failure the destination keeps its previous content and the temporary
/// file is removed.
pub fn write(root: &Root, rel: &RelPath, bytes: &[u8], mode: u32) -> Result<(), Diagnostic> {
    let path = resolve(root, rel)?;
    if let Some(parent) = parent_rel(rel) {
        mkdir_p(root, &parent)?;
    }
    ledger("write", &path)?;
    let dir = path.parent().expect("a resolved path has a parent");
    let tmp = dir.join(format!(".lodi-tmp-{}", &sha256_hex(bytes)[..32]));
    let result = (|| -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;
        // Created owner-only and chmod-ed to `mode` below, so no umask ever widens it.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        crate::util::keep_label(&path, &file)?;
        drop(file);
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        fs::rename(&tmp, &path)?;
        fs::File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| io(&path, "write", e))
}

/// Copy `from` to `rel` inside `root`, through the same atomic write. `from` is read, not linked,
/// so the destination never becomes a link to a file outside the root.
pub fn copy(root: &Root, rel: &RelPath, from: &Path, mode: u32) -> Result<(), Diagnostic> {
    let bytes = fs::read(from).map_err(|e| io(from, "read", e))?;
    write(root, rel, &bytes, mode)
}

/// Remove a regular file at `rel` inside `root`. A path that is not there is not an error. A
/// directory or any other non-regular target is refused before the ledger is touched: the home
/// scope never removes directories.
pub fn remove(root: &Root, rel: &RelPath) -> Result<(), Diagnostic> {
    let path = resolve(root, rel)?;
    let Ok(meta) = fs::symlink_metadata(&path) else {
        return Ok(());
    };
    if !meta.is_file() {
        return Err(Diagnostic::new(
            "E_STORE_IO",
            format!(
                "cannot remove {}: the target is not a regular file",
                path.display()
            ),
        )
        .hint("remove the directory or special file yourself if that is intended"));
    }
    ledger("remove", &path)?;
    fs::remove_file(&path).map_err(|e| io(&path, "remove", e))
}

/// Remove a regular file or symbolic link at `rel`, without following the final link. This is
/// reserved for Lodi-owned directories such as the generated home profile's bin farm. A
/// directory or special file is still refused: the home scope never removes directories.
pub fn remove_owned(root: &Root, rel: &RelPath) -> Result<(), Diagnostic> {
    let path = resolve_with_final_link(root, rel, true)?;
    let Ok(meta) = fs::symlink_metadata(&path) else {
        return Ok(());
    };
    if !meta.is_file() && !meta.file_type().is_symlink() {
        return Err(Diagnostic::new(
            "E_STORE_IO",
            format!(
                "cannot remove {}: the target is not a regular file or symbolic link",
                path.display()
            ),
        )
        .hint("remove the directory or special file yourself if that is intended"));
    }
    ledger("remove", &path)?;
    fs::remove_file(&path).map_err(|e| io(&path, "remove", e))
}

/// Create a symbolic link at `rel` inside `root`. The destination is contained by the same
/// component checks as every other home-scope mutation; `target` is stored verbatim and is not
/// traversed by this operation.
pub fn link(root: &Root, rel: &RelPath, target: &Path) -> Result<(), Diagnostic> {
    let path = resolve(root, rel)?;
    if let Some(parent) = parent_rel(rel)
        && !root.path().join(parent.as_str()).is_dir()
    {
        mkdir_p(root, &parent)?;
    }
    ledger("symlink", &path)?;
    std::os::unix::fs::symlink(target, &path).map_err(|e| io(&path, "create link", e))
}

/// Set the mode of `rel` inside `root`.
pub fn set_mode(root: &Root, rel: &RelPath, mode: u32) -> Result<(), Diagnostic> {
    use std::os::unix::fs::PermissionsExt;
    let path = resolve(root, rel)?;
    ledger("chmod", &path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(mode))
        .map_err(|e| io(&path, "set the mode of", e))
}

// ------------------------------------------------------------------------------- locking ---

/// Take the lock file at `rel` inside `root`, waiting for it. The file is created when it is not
/// there — that creation is the mutation, and it is recorded in the ledger — and it is never
/// read, written or truncated, only locked; the lock is released when the value is dropped.
///
/// The parent directory has to exist already: a lock is taken inside a scope directory the
/// caller created through [`mkdir_p`], never by creating one as a side effect.
pub fn lock(root: &Root, rel: &RelPath, exclusive: bool) -> Result<Lock, Diagnostic> {
    let path = open_path(root, rel)?;
    Lock::at(&path, exclusive)
}

/// [`lock`] without waiting: `Ok(None)` means another process holds it, which is what lets a
/// command say `waiting for the home lock` once and then block on [`lock`].
pub fn try_lock(root: &Root, rel: &RelPath, exclusive: bool) -> Result<Option<Lock>, Diagnostic> {
    let path = open_path(root, rel)?;
    Lock::try_at(&path, exclusive)
}

/// [`lock`] **only when the lock file is already there**: `Ok(None)` otherwise, and nothing is
/// created. This is what a report takes, because `plan` and `status` write nothing at all.
pub fn lock_if_present(
    root: &Root,
    rel: &RelPath,
    exclusive: bool,
) -> Result<Option<Lock>, Diagnostic> {
    let path = resolve(root, rel)?;
    if !path.exists() {
        return Ok(None);
    }
    Lock::at(&path, exclusive).map(Some)
}

/// The resolved path of a lock, with its creation announced in the ledger when the file is not
/// there yet. A lock file that already exists is opened without a ledger line: opening it changes
/// nothing, which is why `status` can share one and still touch nothing.
fn open_path(root: &Root, rel: &RelPath) -> Result<PathBuf, Diagnostic> {
    let path = resolve(root, rel)?;
    if !path.exists() {
        ledger("lock", &path)?;
    }
    Ok(path)
}

/// The parent of `rel`, or `None` when `rel` is a single component.
fn parent_rel(rel: &RelPath) -> Option<RelPath> {
    let at = rel.as_str().rfind('/')?;
    Some(RelPath(rel.as_str()[..at].to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_of_plain_components_is_accepted() {
        for good in [".config/git/ignore", "a", "a/b/c", ".inputrc", "x~y"] {
            assert!(RelPath::new(good).is_ok(), "{good}");
        }
    }

    #[test]
    fn the_parent_of_a_single_component_is_nothing() {
        assert_eq!(parent_rel(&RelPath::new("a").unwrap()), None);
        assert_eq!(
            parent_rel(&RelPath::new("a/b/c").unwrap()),
            Some(RelPath::new("a/b").unwrap())
        );
    }
}
