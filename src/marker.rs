//! The "may manage" marker (#705, LD-503): an empty file whose presence says the owner agreed,
//! once, that lodi may manage this host. `lodi import` writes it after its one-time question and
//! `lodi switch` reads it before it touches the host.
//!
//! Its name is new in 2.0, so a 1.x `host-allowed` never counts (#688). *Read* needs no root and
//! answers yes only for a regular file, owned by the root's owner, that nobody else can write,
//! reached through folders that are not links and that nobody else can write. *Write* runs as
//! root (through `crate::elevate`), follows no link, creates the file exclusively at 0644 and
//! leaves a valid marker exactly as found. Under a scratch `--root` the invoking user stands for
//! root, as for every host verb (LD-114).

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use crate::diag::Diagnostic;
use crate::hostscope::files;

/// The marker, relative to the root.
pub const MARKER: &str = "etc/lodi/may-manage";
/// Its mode, whatever the umask.
pub const MODE: u32 = 0o644;

/// The owner a marker of `root` must have: root on `/`, else the process's own `euid`.
fn owner(root: &Path, euid: u32) -> u32 {
    files::trusted_owner(root == Path::new("/"), euid)
}

/// Whether lodi may manage the host at `root`, as a process of `euid` sees it.
pub fn may_manage(root: &Path, euid: u32) -> bool {
    let owner = owner(root, euid);
    // The marker is an empty file (#705): one with anything in it is not one lodi wrote.
    let trusted = |path: &Path, dir: bool| {
        fs::symlink_metadata(path).is_ok_and(|meta| {
            let kind = if dir {
                meta.is_dir()
            } else {
                meta.is_file() && meta.len() == 0
            };
            kind && meta.uid() == owner && meta.mode() & 0o022 == 0
        })
    };
    trusted(root, true)
        && trusted(&root.join("etc"), true)
        && trusted(&root.join("etc/lodi"), true)
        && trusted(&root.join(MARKER), false)
}

/// The refusal of a host lodi may not manage yet (`E_HOST_NOT_ARMED`, exit 9), in one wording
/// for `lodi switch` and the host part's gate.
pub fn refusal(root: &Path) -> Diagnostic {
    Diagnostic::new(
        "E_HOST_NOT_ARMED",
        format!(
            "lodi may not manage {} yet: {} is missing or not trusted",
            root.display(),
            root.join(MARKER).display()
        ),
    )
    .hint(
        "run `lodi import` once, and answer yes to managing this host; `lodi switch --home` \
         works without it",
    )
}

fn io_error(path: &Path, error: io::Error) -> Diagnostic {
    Diagnostic::new("E_STORE_IO", format!("{}: {error}", path.display()))
}

/// Write the marker of `root` as a process of `euid`: `Ok(true)` when it was created,
/// `Ok(false)` when a valid one was there and was left as found.
pub fn write(root: &Path, euid: u32) -> Result<bool, Diagnostic> {
    files::ensure_dir(root, "/etc/lodi", 0o755)?;
    let path = root.join(MARKER);
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
    {
        Ok(handle) => {
            handle
                .set_permissions(fs::Permissions::from_mode(MODE))
                .map_err(|e| io_error(&path, e))?;
            crate::util::policy_label(&path, &handle).map_err(|e| io_error(&path, e))?;
            handle.sync_all().map_err(|e| io_error(&path, e))?;
            drop(handle);
            files::sync_parent(&path);
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(&path).map_err(|e| io_error(&path, e))?;
            if meta.file_type().is_symlink() {
                return Err(Diagnostic::new(
                    "E_PATH_ESCAPE",
                    format!("{} is a symbolic link", path.display()),
                )
                .hint("lodi never follows a link there; remove it, nothing was written"));
            }
            if may_manage(root, euid) {
                return Ok(false);
            }
            Err(Diagnostic::new(
                "E_EXISTS",
                format!(
                    "{} is there and is not a valid marker (an empty regular file owned by \
                     uid {}, writable by no one else)",
                    path.display(),
                    owner(root, euid)
                ),
            )
            .hint("lodi never repairs it: remove it, then run lodi import again"))
        }
        Err(e) => Err(io_error(&path, e)),
    }
}
