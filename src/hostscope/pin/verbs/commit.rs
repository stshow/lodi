//! The pin verbs' one write (V1, P13): `pins.lock` first, then `host.toml`, each replaced
//! atomically by rename so a reader sees the old file or the new one and never a part. The
//! replacement keeps the mode, the owner and the group of the file it replaces; a new
//! `pins.lock` is mode 0644. A crash between the two writes leaves `host.toml` as it was beside
//! a `pins.lock` that already records the change, which plan reads as a record no request
//! names (ignored, and dropped by the next verb) or a request with no record (unrecorded).

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use crate::diag::Diagnostic;

/// What happens to `pins.lock`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockChange {
    /// Replace it with these bytes (m140-core's renderer), unless it already holds them.
    Write(Vec<u8>),
    /// Remove it (`lodi host unpin --all`), if it is there.
    Remove,
    /// Leave it exactly as it is.
    Keep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// Whether either file changed.
    pub changed: bool,
    /// Whether `pins.lock` was written or removed.
    pub lock: bool,
    /// Whether `host.toml` was replaced.
    pub host: bool,
}

/// Apply `lock` to `<dir>/pins.lock`, then call `between` (a fault seam for tests; `|| Ok(())`
/// otherwise), then replace `<dir>/host.toml` with `host` when it is `Some` and differs.
pub fn write(
    dir: &Path,
    lock: LockChange,
    host: Option<&str>,
    between: impl FnOnce() -> io::Result<()>,
) -> Result<Outcome, Diagnostic> {
    let lock_path = dir.join("pins.lock");
    let host_path = dir.join("host.toml");
    let io_at = |path: &Path| {
        let path = path.display().to_string();
        move |e: io::Error| Diagnostic::new("E_STORE_IO", format!("{path}: {e}"))
    };
    let mut lock_changed = false;
    match lock {
        LockChange::Write(bytes) => {
            if fs::read(&lock_path).ok().as_deref() != Some(bytes.as_slice()) {
                replace(&lock_path, &bytes, 0o644).map_err(io_at(&lock_path))?;
                lock_changed = true;
            }
        }
        LockChange::Remove => match fs::remove_file(&lock_path) {
            Ok(()) => lock_changed = true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_at(&lock_path)(e)),
        },
        LockChange::Keep => {}
    }
    between().map_err(io_at(&lock_path))?;
    let mut host_changed = false;
    if let Some(text) = host
        && fs::read(&host_path).map_err(io_at(&host_path))? != text.as_bytes()
    {
        replace(&host_path, text.as_bytes(), 0o644).map_err(io_at(&host_path))?;
        host_changed = true;
    }
    Ok(Outcome {
        changed: lock_changed || host_changed,
        lock: lock_changed,
        host: host_changed,
    })
}

/// Replace `path` by `bytes` through a synced temporary file and a rename, giving the new file
/// the mode, owner and group of the one it replaces (`default_mode` for a new file).
fn replace(path: &Path, bytes: &[u8], default_mode: u32) -> io::Result<()> {
    let old = match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(io::Error::other(
                "is a symbolic link; refusing to replace it",
            ));
        }
        Ok(m) => Some(m),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    crate::lock::write_atomic_with(path, bytes, |tmp| {
        let mode = old.as_ref().map_or(default_mode, |m| m.mode() & 0o7777);
        fs::set_permissions(tmp, fs::Permissions::from_mode(mode))?;
        if let Some(m) = &old {
            let here = fs::metadata(tmp)?;
            if (here.uid(), here.gid()) != (m.uid(), m.gid()) {
                std::os::unix::fs::lchown(tmp, Some(m.uid()), Some(m.gid()))?;
            }
        }
        Ok(())
    })
}
