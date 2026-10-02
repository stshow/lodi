//! A home's work in a child process that becomes its user for good (#695): `lodi switch`'s
//! home part under `sudo` or `doas`, and a config fetched as the person. The child drops to the
//! passwd uid and gid (no other group) before it opens anything, so nothing it writes is root's,
//! and reports its lines or its failure back over a pipe.

use std::io::{Read, Write};
use std::os::fd::FromRawFd;

use crate::diag::Diagnostic;

/// What one home's child reported: its lines, or its failure's code and text.
enum Child {
    Done(String),
    Failed { code: &'static str, text: String },
}

/// The code `name` as the catalogue spells it, for a failure carried back from a child.
fn code_of(name: &str) -> &'static str {
    crate::diag::CODES
        .iter()
        .map(|(code, _)| *code)
        .find(|code| *code == name)
        .unwrap_or("E_APPLY")
}

fn encode(child: &Child) -> Vec<u8> {
    let (tag, a, b) = match child {
        Child::Done(lines) => (0u8, "", lines.as_str()),
        Child::Failed { code, text } => (1u8, *code, text.as_str()),
    };
    let mut out = vec![tag];
    for part in [a, b] {
        out.extend_from_slice(&(part.len() as u64).to_le_bytes());
        out.extend_from_slice(part.as_bytes());
    }
    out
}

fn decode(bytes: &[u8]) -> Option<Child> {
    let (&tag, mut rest) = bytes.split_first()?;
    let mut parts = Vec::new();
    for _ in 0..2 {
        let (len, tail) = rest.split_at_checked(8)?;
        let len = usize::try_from(u64::from_le_bytes(len.try_into().ok()?)).ok()?;
        let (part, tail) = tail.split_at_checked(len)?;
        parts.push(String::from_utf8(part.to_vec()).ok()?);
        rest = tail;
    }
    let text = parts.pop()?;
    let code = parts.pop()?;
    match tag {
        0 => Some(Child::Done(text)),
        1 => Some(Child::Failed {
            code: code_of(&code),
            text,
        }),
        _ => None,
    }
}

fn failed(d: &Diagnostic) -> Child {
    Child::Failed {
        code: d.code,
        text: d.to_string(),
    }
}

/// `work` in a child that first becomes `uid`:`gid` for good, with no other group: `lodi
/// switch`'s home part under `sudo` or `doas` (#695). Its lines, or its failure's code and text.
pub fn as_person(
    uid: u32,
    gid: u32,
    name: &str,
    work: impl FnOnce() -> Result<String, (&'static str, String)>,
) -> Result<String, (&'static str, String)> {
    as_person_reporting(uid, gid, name, &mut crate::progress::Silent, |_| work())
}

/// [`as_person`] with the child's progress: `work` reports into a relay on a pipe of its own,
/// and this process passes each event on to `sink` as it comes (#694 story 17).
pub fn as_person_reporting(
    uid: u32,
    gid: u32,
    name: &str,
    sink: &mut dyn crate::progress::Sink,
    work: impl FnOnce(&mut dyn crate::progress::Sink) -> Result<String, (&'static str, String)>,
) -> Result<String, (&'static str, String)> {
    let mut events = [0 as libc::c_int; 2];
    // SAFETY: `events` has room for the two descriptors pipe2 fills in.
    if unsafe { libc::pipe2(events.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        let error = std::io::Error::last_os_error();
        return Err(("E_STORE_IO", format!("no pipe for home {name}: {error}")));
    }
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors pipe2 fills in.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        let error = std::io::Error::last_os_error();
        return Err(("E_STORE_IO", format!("no pipe for home {name}: {error}")));
    }
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // SAFETY: the child only drops privileges, does the home's work, writes its report to the
    // pipe and leaves with `_exit`; nothing it inherits is used across the fork by both sides.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let error = std::io::Error::last_os_error();
        // SAFETY: closing the four descriptors pipe2 returned.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
            libc::close(events[0]);
            libc::close(events[1]);
        }
        return Err(("E_APPLY", format!("could not start home {name}: {error}")));
    }
    if pid == 0 {
        // SAFETY: the child's own copies of the read ends.
        unsafe {
            libc::close(fds[0]);
            libc::close(events[0]);
        }
        // SAFETY: the events' write end this child owns.
        let relay = unsafe { std::fs::File::from_raw_fd(events[1]) };
        let mut relay = crate::progress::Relay::new(relay);
        use super::privilege::Privilege;
        // Already that uid and gid (a home root owns), there is nothing to become.
        // SAFETY: geteuid and getegid cannot fail and take no arguments.
        let same = unsafe { libc::geteuid() == uid && libc::getegid() == gid };
        let became = if same {
            Ok(())
        } else {
            super::privilege::System.become_user(uid, gid)
        };
        let report = match became {
            Err(d) => failed(&d),
            Ok(()) => match work(&mut relay) {
                Ok(lines) => Child::Done(lines),
                Err((code, text)) => Child::Failed { code, text },
            },
        };
        // SAFETY: the write end this child owns.
        let mut pipe = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        let _ = pipe.write_all(&encode(&report));
        drop(pipe);
        drop(relay);
        // SAFETY: leaving the child without running the parent's exit handlers.
        unsafe { libc::_exit(0) };
    }
    // SAFETY: the parent's copies of the write ends, so that the reads below end with the child.
    unsafe {
        libc::close(fds[1]);
        libc::close(events[1]);
    }
    // SAFETY: the read ends this process owns from here on.
    let mut pipe = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let relayed = unsafe { std::fs::File::from_raw_fd(events[0]) };
    // The report on a thread of its own, so a long one never holds the child while its events
    // are read here.
    let report = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    });
    use std::io::BufRead;
    for line in std::io::BufReader::new(relayed).lines() {
        let Ok(line) = line else { break };
        crate::progress::relayed(line, sink);
    }
    let bytes = report.join().unwrap_or_default();
    let mut status = 0;
    // SAFETY: waiting for the child this function started.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    match decode(&bytes) {
        Some(Child::Done(lines)) => Ok(lines),
        Some(Child::Failed { code, text }) => Err((code, text)),
        None => Err((
            "E_APPLY",
            format!("the process of home {name} ended without a report (wait status {status})"),
        )),
    }
}
