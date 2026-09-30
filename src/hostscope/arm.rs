//! `lodi host arm [--root DIR]`: write the arming marker the safety gate requires (LD-320).
//!
//! Arming stays a deliberate root action, and this is the one command that performs it. It is
//! never a side effect of an install, a `plan`, an `apply` or an `import`, and no package arms a
//! machine on install. What it writes is exactly what [`super::safety`]'s step 2 reads:
//! `<root>/etc/lodi/host-allowed`, an **empty regular file of mode 0644**, owned by root on `/`
//! and by the invoking user under a `--root` (LD-114). Its parent is created if absent.
//!
//! The order is fixed, and each step is refused before the next is attempted:
//!
//! 1. resolve the root as the gate does — `--root DIR` when given, else `/`;
//! 2. **privilege**, before anything is created or even looked at below the root: arming `/`
//!    needs euid 0 (`E_NEED_ROOT`, exit 9). That decision is [`may_arm`], a pure function of the
//!    root and the effective uid, so it is proved without ever pointing this command at `/`;
//! 3. `<root>/etc` and `<root>/etc/lodi` are created one checked component at a time through
//!    [`super::files::ensure_dir`], so a symbolic link on the way is `E_PATH_ESCAPE` (exit 3);
//! 4. the marker is created `O_CREAT | O_EXCL | O_NOFOLLOW` at 0644, with the SELinux type the
//!    policy gives its path (`crate::util::policy_label`). If one is already there it
//!    is judged, never repaired: a valid marker is left exactly as found — mode, owner and mtime —
//!    and the command says the root is already armed; a symbolic link is `E_PATH_ESCAPE`, and
//!    anything else that is not a valid marker is `E_EXISTS` (exit 3), also left as found.
//!
//! To disarm, delete the file. There is no `lodi host disarm`: deleting one file is already one
//! command, and a second privileged verb would be a second surface for the same thing.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::diag::Diagnostic;

use super::HostError;
use super::files;
use super::safety::{self, MARKER, Options};

/// The mode the marker is created with, whatever the umask.
pub const MARKER_MODE: u32 = 0o644;

/// Step 2, decided without touching the filesystem: may `euid` arm `root`?
///
/// `root` is the root as the command resolved it. Arming `/` is a root action; under a `--root`
/// the invoking user arms a tree of their own, as LD-114 says for every host command.
pub fn may_arm(root: &Path, euid: u32) -> Result<(), Diagnostic> {
    if root == Path::new("/") && euid != 0 {
        return Err(Diagnostic::new(
            "E_NEED_ROOT",
            format!(
                "arming / writes the root-owned /{MARKER}, and this process runs as uid {euid}"
            ),
        )
        .hint("run it as root: sudo lodi host arm; nothing was created"));
    }
    Ok(())
}

/// Whether an existing marker is one the gate accepts and this command would have written:
/// a regular file, owned by root on `/` and by `euid` under a `--root`, neither group- nor
/// world-writable. `Err` carries the reason it is not.
fn judge(marker: &Path, system_root: bool, euid: u32) -> Result<(), Diagnostic> {
    let meta = fs::symlink_metadata(marker).map_err(|e| store_io(marker, e))?;
    if meta.file_type().is_symlink() {
        return Err(Diagnostic::new(
            "E_PATH_ESCAPE",
            format!("{} is a symbolic link", marker.display()),
        )
        .hint(
            "lodi never follows a symbolic link in a host-scope destination; it was left as found",
        ));
    }
    let owner = if system_root { 0 } else { euid };
    let mode = meta.permissions().mode() & 0o7777;
    let wrong = if !meta.is_file() {
        Some("is not a regular file".to_string())
    } else if meta.uid() != owner {
        Some(format!("is owned by uid {}, not uid {owner}", meta.uid()))
    } else if mode & 0o022 != 0 {
        Some(format!("is mode {mode:04o}: group- or world-writable"))
    } else {
        None
    };
    match wrong {
        None => Ok(()),
        Some(why) => Err(Diagnostic::new(
            "E_EXISTS",
            format!(
                "{} {why}, so it is not a valid arming marker",
                marker.display()
            ),
        )
        .hint(
            "lodi never repairs a marker: it was left as found. Remove it, then run \
             lodi host arm again",
        )),
    }
}

fn store_io(path: &Path, error: io::Error) -> Diagnostic {
    Diagnostic::new("E_STORE_IO", format!("{}: {error}", path.display()))
}

/// The one sentence a success prints: what is now enabled, and how to take it back.
fn armed_sentence(root: &Path, marker: &Path, already: bool) -> String {
    if already {
        format!(
            "{} is already armed: {} was left as it is; delete that file to disarm it",
            root.display(),
            marker.display()
        )
    } else {
        format!(
            "armed {}: lodi host plan, apply and import may now act on it; delete {} to disarm it",
            root.display(),
            marker.display()
        )
    }
}

/// `lodi host arm [--root DIR]`.
pub fn run(options: &Options) -> Result<String, HostError> {
    let requested = options.root.clone().unwrap_or_else(|| PathBuf::from("/"));
    // As the gate does: aliases such as `/tmp/..` resolve before deciding whether this is `/`.
    let root = fs::canonicalize(&requested).unwrap_or(requested);
    let system_root = root == Path::new("/");
    let euid = safety::current_euid();

    // 2. Privilege, before anything below the root is looked at.
    may_arm(&root, euid)?;

    if !fs::metadata(&root).is_ok_and(|meta| meta.is_dir()) {
        return Err(Diagnostic::new(
            "E_STORE_IO",
            format!("{} is not a directory", root.display()),
        )
        .hint("--root names an existing directory; nothing was created")
        .into());
    }

    // 3. The parent, one checked component at a time.
    let parent = format!(
        "/{}",
        Path::new(MARKER)
            .parent()
            .expect("the marker has a parent")
            .display()
    );
    files::ensure_dir(&root, &parent, 0o755)?;

    // 4. The marker itself, created new and never through a link. Losing a race to another
    //    creator is the same as finding the file there: it is judged, not overwritten.
    let marker = root.join(MARKER);
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(MARKER_MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&marker)
    {
        Ok(handle) => {
            // The umask may have narrowed the mode `open` was given; the descriptor is ours, so
            // this cannot be redirected to another file.
            handle
                .set_permissions(fs::Permissions::from_mode(MARKER_MODE))
                .map_err(|e| store_io(&marker, e))?;
            crate::util::policy_label(&marker, &handle).map_err(|e| store_io(&marker, e))?;
            handle.sync_all().map_err(|e| store_io(&marker, e))?;
            drop(handle);
            files::sync_parent(&marker);
            Ok(armed_sentence(&root, &marker, false))
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            judge(&marker, system_root, euid)?;
            Ok(armed_sentence(&root, &marker, true))
        }
        Err(e) => Err(store_io(&marker, e).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_root_may_arm_the_system_root() {
        let refused = may_arm(Path::new("/"), 1000).unwrap_err();
        assert_eq!(refused.code, "E_NEED_ROOT");
        assert_eq!(crate::diag::exit_status(refused.code), 9);
        assert!(may_arm(Path::new("/"), 0).is_ok());
        assert!(may_arm(Path::new("/srv/scratch"), 1000).is_ok());
    }
}
