//! The one ceiling every wait in this suite uses.
//!
//! A test that drives real child processes, a rootless Podman container or a lock two processes
//! contend for has to wait for something to happen. Waiting for the *event* is what these
//! helpers do; the clock is only the backstop that turns a genuinely hung child into a failure
//! instead of a suite that never returns. That backstop therefore has to be generous: on
//! 2026-09-21 this machine ran six lanes' gates at once, the load average sat near a hundred on
//! thirty cores, and ceilings of thirty and sixty seconds failed with no product defect behind
//! them.
//!
//! So every such wait takes its ceiling from here, and `LODI_TEST_WAIT_SECONDS` raises or lowers
//! it for a loaded or an impatient machine. Nothing here relaxes an assertion: the condition a
//! caller waits for is unchanged, and a child that never reaches it still fails the test.
#![allow(dead_code)]

use std::time::{Duration, Instant};

/// The environment variable that overrides [`ceiling`], in whole seconds.
pub const WAIT_SECONDS_ENV: &str = "LODI_TEST_WAIT_SECONDS";

/// The default ceiling, in seconds. Generous on purpose: it is a backstop against a hang, not a
/// performance budget, and no test is expected to come near it.
pub const DEFAULT_WAIT_SECONDS: u64 = 300;

/// How long any of these waits may take before it calls the thing it waits for hung.
pub fn ceiling() -> Duration {
    ceiling_from(std::env::var(WAIT_SECONDS_ENV).ok().as_deref())
}

/// [`ceiling`] over a value that is handed in rather than read, so that the rule it applies can
/// be checked without touching the environment of a running test process.
pub fn ceiling_from(raw: Option<&str>) -> Duration {
    let seconds = match raw {
        Some(text) => text.trim().parse::<u64>().unwrap_or_else(|_| {
            panic!("{WAIT_SECONDS_ENV} must be a whole number of seconds, not {text:?}")
        }),
        None => DEFAULT_WAIT_SECONDS,
    };
    assert!(seconds > 0, "{WAIT_SECONDS_ENV} must be greater than zero");
    Duration::from_secs(seconds)
}

/// The message a timed-out wait fails with, so that every caller says the same thing.
pub fn timed_out(what: &str, waited: Duration) -> String {
    format!(
        "timed out after {waited:?} waiting for {what}; if this machine is loaded, raise \
         {WAIT_SECONDS_ENV} (default {DEFAULT_WAIT_SECONDS}s)"
    )
}

/// Wait until `ready` answers true, or fail once the shared ceiling has passed.
///
/// The poll backs off from 5 ms to 100 ms so that a wait which is satisfied at once stays cheap
/// and a long one does not spin against a machine that is already busy.
pub fn until(what: &str, ready: impl FnMut() -> bool) {
    until_within(what, ceiling(), ready);
}

/// [`until`] with an explicit ceiling, for the one caller that has already computed it.
pub fn until_within(what: &str, ceiling: Duration, mut ready: impl FnMut() -> bool) {
    let start = Instant::now();
    let mut step = Duration::from_millis(5);
    while !ready() {
        assert!(start.elapsed() < ceiling, "{}", timed_out(what, ceiling));
        std::thread::sleep(step);
        step = (step * 2).min(Duration::from_millis(100));
    }
}
