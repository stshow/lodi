//! The suite's one wait ceiling, and its negative control (M-Arch-Base R-3).
//!
//! `tests/support/wait.rs` is where every wait in this repository that guards a real concurrency
//! or container step takes its backstop from. Raising that backstop is only safe if it is still
//! a backstop, so this file proves the two halves that matter: the override is read the way the
//! helper says it is, and a child that really is hung still fails the test rather than being
//! waited out for ever.
//!
//! Offline, no container runtime, no store, nothing outside `CARGO_TARGET_TMPDIR`.

mod support;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Long-lived children that end with their test, pass or panic (LD-372).
#[path = "support/fixture.rs"]
mod fixture;
#[path = "support/wait.rs"]
mod wait;

use fixture::Fixture;

#[test]
fn the_ceiling_is_generous_by_default_and_the_environment_overrides_it() {
    assert_eq!(
        wait::ceiling_from(None),
        Duration::from_secs(wait::DEFAULT_WAIT_SECONDS),
        "the default ceiling is the documented one"
    );
    const _: () = assert!(
        wait::DEFAULT_WAIT_SECONDS >= 300,
        "the default is a backstop against a hang, not a performance budget"
    );
    assert_eq!(wait::ceiling_from(Some("7")), Duration::from_secs(7));
    assert_eq!(
        wait::ceiling_from(Some(" 7 ")),
        Duration::from_secs(7),
        "a value a shell pasted with spaces still reads"
    );
    // The live reader agrees with the rule, whatever this run's environment says.
    assert_eq!(
        wait::ceiling(),
        wait::ceiling_from(std::env::var(wait::WAIT_SECONDS_ENV).ok().as_deref())
    );
}

#[test]
fn a_nonsense_or_zero_override_is_refused_rather_than_silently_ignored() {
    for bad in ["", "soon", "-1", "1.5"] {
        let refused = std::panic::catch_unwind(|| wait::ceiling_from(Some(bad)));
        assert!(refused.is_err(), "{bad:?} was accepted as a ceiling");
    }
    assert!(std::panic::catch_unwind(|| wait::ceiling_from(Some("0"))).is_err());
}

/// The negative control: a real child process that never reaches the condition. The wait must
/// end in a failure that names what it waited for, not run to the generous default.
#[test]
fn a_genuinely_hung_child_still_fails_the_wait() {
    let marker = support::scratch("wait-hung").join("marker");
    let child = Fixture::spawn(
        "the hung child",
        Command::new("/bin/sh")
            .args(["-c", "sleep 600"])
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );

    let ceiling = Duration::from_secs(1);
    let started = Instant::now();
    let outcome = std::panic::catch_unwind(|| {
        wait::until_within("a marker the child never writes", ceiling, || {
            marker.exists()
        })
    });
    let waited = started.elapsed();

    drop(child);

    let panic = outcome.expect_err("the wait returned although the child never got there");
    let message = panic
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_else(|| String::from("<not a string>"));
    assert!(
        message.contains("a marker the child never writes"),
        "the failure names what was waited for: {message}"
    );
    assert!(
        message.contains(wait::WAIT_SECONDS_ENV),
        "the failure says how to raise the ceiling: {message}"
    );
    assert!(
        waited >= ceiling,
        "the wait gave up before its ceiling: {waited:?}"
    );
    assert!(
        !marker.exists(),
        "the negative control wrote the marker it claims never appears"
    );
}
