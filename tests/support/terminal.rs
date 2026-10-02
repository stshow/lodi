//! The real binary on a pseudo-terminal: standard input and standard error are the terminal,
//! whose input already holds the answer a person would type.

use std::fs;
use std::io::Write as _;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Output, Stdio};

/// Run `start`, handing it the terminal as standard input and as standard error, with `answer`
/// already typed. Returns its output and every byte the terminal was shown.
pub fn on_terminal(answer: &str, start: impl FnOnce(Stdio, Stdio) -> Output) -> (Output, Vec<u8>) {
    // SAFETY: plain pseudo-terminal calls on descriptors this function owns.
    let (master, slave) = unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        assert!(master >= 0, "no pseudo-terminal");
        assert_eq!(libc::grantpt(master), 0);
        assert_eq!(libc::unlockpt(master), 0);
        let mut name = [0 as libc::c_char; 128];
        assert_eq!(libc::ptsname_r(master, name.as_mut_ptr(), name.len()), 0);
        let slave = libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        );
        assert!(slave >= 0, "the terminal's other end");
        (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave))
    };
    let mut writer = fs::File::from(master.try_clone().unwrap());
    writer.write_all(answer.as_bytes()).unwrap();
    let output = start(Stdio::from(slave.try_clone().unwrap()), Stdio::from(slave));
    // The child has exited and every copy of the terminal's other end is closed, so the master
    // reads all the child wrote and then fails. One non-blocking read can run ahead of the
    // terminal's buffer under load; poll bounds each wait. A wait that ends with nothing to read
    // is a copy still open after the child ended (a `Command` that outlived `start` holds two),
    // which would cost every run the whole bound: a broken caller, never a pass (LD-515).
    let fd = master.as_raw_fd();
    let mut shown = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let mut ready = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: a poll of, then a read from, the master end this function owns.
        let n = unsafe {
            let polled = libc::poll(&mut ready, 1, 10_000);
            assert!(
                polled != 0,
                "the terminal stayed open after the command ended: move the Command into `start`"
            );
            if polled < 0 {
                break;
            }
            libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len())
        };
        let Ok(n) = usize::try_from(n) else { break };
        if n == 0 {
            break;
        }
        shown.extend_from_slice(&chunk[..n]);
    }
    (output, shown)
}
