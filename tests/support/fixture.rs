//! Long-lived children that end with their test (LD-372).
//!
//! A test that starts a child which waits for the test to release it — a `lodi develop` session
//! looping until a `STOP` file appears, a package-manager shim held on a fifo — leaves that child
//! running for ever when the test fails before it releases it: a failed assertion unwinds the
//! test, a plain `std::process::Child` is dropped without being killed, and the child and the
//! processes it started are reparented and live on. Two rules close that:
//!
//! - [`Fixture`] owns such a child. Its `Drop`, which runs on a pass, a failed assertion and a
//!   panic alike, stops the child and every process descended from it, kills them, reaps the
//!   child, and waits — with the suite's ceiling, never a fixed sleep — until each of those pids
//!   is gone. It signals only pids this test started: the child and its own descendants.
//! - [`hold`] builds the shell loop such a child waits in, bounded: it exits on its own with
//!   [`HOLD_EXPIRED`] once its deadline passes, even if nothing ever releases it, so a fixture
//!   whose test process was killed outright (no `Drop` runs then) still ends.
//!
//! Included by path, like `wait.rs`, by each test binary that needs it; the including binary also
//! declares `mod wait`.
#![allow(dead_code)]

use std::fs;
use std::io;
use std::process::{Child, Command, ExitStatus, Output};

/// The exit status of a [`hold`] loop whose deadline passed before it was released.
pub const HOLD_EXPIRED: i32 = 124;

/// How long a [`hold`] loop waits at most by default: the suite's wait ceiling plus a margin, so
/// that no wait of a test that is still running gives up after the fixture it waits for.
pub fn hold_seconds() -> u64 {
    crate::wait::ceiling().as_secs() + 60
}

/// A shell loop that waits while `condition` holds, for at most `seconds`, then exits
/// [`HOLD_EXPIRED`]. `condition` is a shell test such as `[ ! -f "$STOP" ]`.
pub fn hold_within(condition: &str, seconds: u64) -> String {
    format!(
        "lodi_hold_deadline=$(( $(date +%s) + {seconds} )); \
         while {condition}; do \
         [ \"$(date +%s)\" -lt \"$lodi_hold_deadline\" ] || exit {HOLD_EXPIRED}; \
         sleep 0.05; done"
    )
}

/// [`hold_within`] with the default bound, [`hold_seconds`].
pub fn hold(condition: &str) -> String {
    hold_within(condition, hold_seconds())
}

/// A child that is killed and reaped, with every process it started, when it is dropped.
pub struct Fixture {
    child: Option<Child>,
    what: String,
}

impl Fixture {
    /// Start `command` as a fixture named `what` (the name appears in a failed wait).
    pub fn spawn(what: &str, command: &mut Command) -> Fixture {
        let child = command
            .spawn()
            .unwrap_or_else(|e| panic!("{what} starts: {e}"));
        Fixture {
            child: Some(child),
            what: what.to_string(),
        }
    }

    pub fn id(&self) -> u32 {
        self.child.as_ref().expect("a live fixture").id()
    }

    /// The child, for signalling it or polling it; the fixture still owns it.
    pub fn child(&mut self) -> &mut Child {
        self.child.as_mut().expect("a live fixture")
    }

    /// Wait for the child to exit by itself and collect its output. What it started is still
    /// ended if it is left behind.
    pub fn wait_with_output(mut self) -> io::Result<Output> {
        let child = self.child.take().expect("a live fixture");
        let pid = child.id() as i32;
        let before = descendants(&[pid]);
        let output = child.wait_with_output();
        end_all(&self.what, &before);
        output
    }

    /// Wait for the child to exit by itself.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child().wait()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            end(&self.what, &mut child);
        }
    }
}

/// Kill `child` and every process descended from it, reap it, and wait until each is gone.
pub fn end(what: &str, child: &mut Child) {
    // A child already reaped has no pid of its own any more, and what it started has been
    // reparented out of this walk's reach: signalling that number could reach a stranger.
    if !matches!(child.try_wait(), Ok(None)) {
        return;
    }
    let root = child.id() as i32;
    // Freeze the tree before it is killed, so nothing in it forks a process this walk misses
    // and a killed parent's children are not reparented out of reach first.
    let mut tree = vec![root];
    loop {
        for &pid in &tree {
            // SAFETY: signalling a pid of this test's own child or of one of its descendants.
            unsafe { libc::kill(pid, libc::SIGSTOP) };
        }
        let grown = descendants(&tree);
        if grown.len() == tree.len() {
            break;
        }
        tree = grown;
    }
    for &pid in &tree {
        // SAFETY: as above.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let _ = child.wait();
    for &pid in &tree {
        wait_gone(&format!("{what} (pid {pid})"), pid);
    }
}

/// Kill whatever of `pids` is still running and wait until each is gone.
fn end_all(what: &str, pids: &[i32]) {
    for &pid in pids {
        if !gone(pid) {
            // SAFETY: a pid of this test's own child or of one of its descendants.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
    for &pid in pids {
        wait_gone(&format!("{what} (pid {pid})"), pid);
    }
}

/// `roots` and every live process whose parent chain leads to one of them.
pub fn descendants(roots: &[i32]) -> Vec<i32> {
    let table = process_table();
    let mut found: Vec<i32> = roots.to_vec();
    let mut grew = true;
    while grew {
        grew = false;
        for &(pid, ppid) in &table {
            if found.contains(&ppid) && !found.contains(&pid) {
                found.push(pid);
                grew = true;
            }
        }
    }
    found
}

/// Every process as `(pid, parent pid)`, from `/proc`.
fn process_table() -> Vec<(i32, i32)> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter_map(|pid| Some((pid, stat(pid)?.1)))
        .collect()
}

/// A process's state letter and parent pid, or `None` when it no longer exists.
fn stat(pid: i32) -> Option<(char, i32)> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is in parentheses and may itself hold spaces or parentheses.
    let rest = &text[text.rfind(')')? + 2..];
    let mut fields = rest.split(' ');
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    Some((state, ppid))
}

/// Whether `pid` has exited: no such process, or one that has exited and waits only to be reaped
/// by whoever inherited it.
pub fn gone(pid: i32) -> bool {
    !matches!(stat(pid), Some((state, _)) if state != 'Z' && state != 'X')
}

/// Wait, with the suite's ceiling, until `pid` is [`gone`].
pub fn wait_gone(what: &str, pid: i32) {
    crate::wait::until(&format!("{what} to be gone"), || gone(pid));
}
