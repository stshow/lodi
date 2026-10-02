//! Reachable M-0.6 T-3 command surface, as 2.0 runs it: a dry run changes nothing, a switch
//! writes, and its warnings go to standard error. Every case is the real binary against a scratch
//! `--root` with a fake machine behind it (`tests/support/fakehost.rs`, LD-514, LD-522).

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use fakehost::{Case, Machine, err, nothing, out, story};
use hostroot::ids;

/// A Debian machine whose `host.toml` declares `/etc/NAME` with `content`, owned by the person
/// running the test.
fn case(name: &str, file: &str, content: &str) -> Case {
    let case = Case::new(name, Machine::debian());
    let (uid, gid) = ids(&case.root);
    case.set_manifest(&format!(
        "[host]\nversion = \"1\"\n\n[files.\"/etc/{file}\"]\ncontent = \"{content}\\n\"\n\
         owner = \"{uid}\"\ngroup = \"{gid}\"\n"
    ));
    case
}

#[test]
fn a_dry_run_switch_writes_nothing_and_a_switch_applies() {
    let case = case("cli-reachable", "cli.conf", "ok");
    let before = std::fs::read_dir(&case.root.dir).unwrap().count();
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(
        err(&plan).contains("+ file /etc/cli.conf"),
        "{}",
        story(&plan)
    );
    assert_eq!(std::fs::read_dir(&case.root.dir).unwrap().count(), before);
    assert!(!case.root.exists("etc/cli.conf"));
    assert!(!case.root.exists("var/lib/lodi"));

    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert_eq!(case.root.read("etc/cli.conf"), "ok\n");
    let plan = case.plan();
    assert!(nothing(&plan), "{}", story(&plan));
}

#[test]
fn warnings_go_to_standard_error() {
    let case = case("cli-warning", "x", "new");
    case.root.write("etc/x", "old\n");
    let output = case.apply(&[]);
    assert!(output.status.success(), "{}", story(&output));
    assert!(
        err(&output).contains("W_REPLACED_UNMANAGED"),
        "{}",
        story(&output)
    );
    assert!(
        !out(&output).contains("W_REPLACED_UNMANAGED"),
        "{}",
        story(&output)
    );
}
