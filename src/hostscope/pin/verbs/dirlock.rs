//! The advisory lock the pin verbs hold on the host directory (V1): an exclusive `flock` on the
//! directory itself, so it adds no file to a directory its owner commits to git. It is not the
//! machine's apply lock and needs no root. A second pin or unpin of the same directory is
//! refused by name rather than interleaved; an apply never takes it, because an apply reads the
//! directory once (LD-379) and a concurrent pin, which replaces whole files by rename, cannot
//! tear what it read.

use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use crate::diag::Diagnostic;

#[derive(Debug)]
pub struct HostDirLock {
    dir: fs::File,
}

impl HostDirLock {
    /// Take the lock on `dir` without waiting.
    pub fn take(dir: &Path) -> Result<HostDirLock, Diagnostic> {
        let io = |e: io::Error| Diagnostic::new("E_STORE_IO", format!("{}: {e}", dir.display()));
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(dir)
            .map_err(io)?;
        loop {
            // SAFETY: flock on a file descriptor this function owns.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(HostDirLock { dir: file });
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(Diagnostic::new(
                    "E_SYSTEM_BUSY",
                    format!(
                        "another `lodi host pin` or `lodi host unpin` is changing {}",
                        dir.display()
                    ),
                )
                .hint("run it again when that command has finished"));
            }
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(io(e));
            }
        }
    }
}

impl Drop for HostDirLock {
    fn drop(&mut self) {
        // SAFETY: unlocking the descriptor this struct owns; closing would unlock as well.
        unsafe { libc::flock(self.dir.as_raw_fd(), libc::LOCK_UN) };
    }
}
