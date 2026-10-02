//! The config's one `lodi.lock` (M-Flake fv-1, LD-416; DONE D4; M150 V6; #692): canonical
//! bytes, and the refusals between it and a project's `lodi.lock`. The rows that ran the
//! top-level `lodi plan` or `lodi apply` went with those commands in 2.0 (#699, LD-500), and the
//! migration of 1.4's per-folder `pins.lock` and `home.lock` went with every 1.x reader (LD-516).
//!
//! Every host command carries a scratch `--root`, with the fake machine behind it, the config at
//! the scratch `HOME`'s `~/.config/lodi`, and the recorded dated archives on loopback; nothing
//! reaches the network or `/`.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
#[path = "support/pinverbs.rs"]
mod pinverbs;
mod support;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::fs;
use std::process::Command;

use fakehost::{err, story};
use pinverbs::*;
use serde_json::Value;

/// The child half of every held apply of this binary (`tests/support/hostroot.rs`).
#[test]
fn child_apply() {
    if !hostroot::is_child() {
        return;
    }
    hostroot::child_apply();
}

fn json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("JSON")
}

/// A machine `box` with `tzdata` newer than the date it is pinned to, its config's `host.toml`
/// managing `bc` and `tzdata`, and `tzdata` pinned to 2025-03-01 by `lodi pin`.
fn pinned(name: &str) -> Verbs {
    let v = Verbs::debian(name, &[("bc", BC), ("tzdata", TZ_RECENT)]);
    v.case
        .set_manifest(&host("debian", None, &["bc", "tzdata"], &[]));
    ok(&v.run("pin", &["tzdata", "--to", "2025-03-01"]));
    v
}

fn config_lock(v: &Verbs) -> Vec<u8> {
    fs::read(v.case.beside("lodi.lock")).expect("the config's lodi.lock")
}

/// V6: the config's lock is canonical JSON of its own format, and equal inputs give equal bytes
/// on two machines: nothing of the machine, the path or the user who wrote it.
#[test]
fn equal_inputs_give_identical_canonical_bytes() {
    let one = pinned("root-lock-bytes-one");
    let two = pinned("root-lock-bytes-two");
    let (a, b) = (config_lock(&one), config_lock(&two));
    assert_eq!(a, b);
    let parsed = json(&a);
    let mut canonical = serde_json::to_string_pretty(&parsed).unwrap();
    canonical.push('\n');
    assert_eq!(String::from_utf8(a.clone()).unwrap(), canonical);
    assert_eq!(parsed["format"], "lodi-config-lock");
    assert_eq!(parsed["version"], 1);
    assert_eq!(
        parsed["hosts"]["host.toml"]["pins"]["tzdata"]["version"], TZ_OLDER,
        "{parsed}"
    );
    let text = String::from_utf8(a).unwrap();
    for machine in [one.root(), two.root()] {
        assert!(
            !text.contains(&machine),
            "a path of the machine is in the lock"
        );
    }
    let (uid, _) = hostroot::ids(&one.case.root);
    assert!(!text.contains(&format!("\"{uid}\"")));
}

/// V6: a project's `develop` refuses the config's lock by its name, and never replaces it.
#[test]
fn a_project_refuses_the_config_lock_by_name() {
    let v = pinned("root-lock-in-project");
    let root = config_lock(&v);
    let project = v.case.base().join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("lodi.toml"), "[project]\nversion = \"1\"\n").unwrap();
    fs::write(project.join("lodi.lock"), &root).unwrap();
    // `develop` reads the project lock first and never replaces one it cannot read.
    let scratch_home = v.case.base().join("project-home");
    fs::create_dir_all(&scratch_home).unwrap();
    let checked = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["develop", "--trust", "--", "true"])
        .current_dir(&project)
        .env_clear()
        .env("PATH", "")
        .env("HOME", &scratch_home)
        .env("XDG_CONFIG_HOME", scratch_home.join("config"))
        .env("LODI_HOME", scratch_home.join("lodi"))
        .output()
        .unwrap();
    assert_ne!(checked.status.code(), Some(0));
    assert_eq!(fs::read(project.join("lodi.lock")).unwrap(), root);
    // Named as the config's lock, with what to run: never as a stale project lock.
    let text = err(&checked);
    for said in [
        "lodi: error E_CONFIG: lodi.lock is a config's lock (lodi-config-lock",
        "not a project lock",
        "lodi develop",
    ] {
        assert!(text.contains(said), "{said}: {}", story(&checked));
    }
    assert!(!text.contains("E_LOCK_STALE"), "{}", story(&checked));
}

/// V6: the config's lock refuses a project's lock by naming both kinds, and a config lock with
/// an unknown field is refused.
#[test]
fn the_config_lock_refuses_a_project_lock_and_an_unknown_field() {
    let v = pinned("root-lock-formats");
    let root = config_lock(&v);
    let project_lock = serde_json::json!({"version": 1, "format": "lodi-spike-lock/1"});
    v.case.write_beside(
        "lodi.lock",
        serde_json::to_string_pretty(&project_lock).unwrap(),
    );
    let refused_project = v.run("plan", &[]);
    refused(&refused_project, "E_CONFIG");
    assert!(
        err(&refused_project).contains("is not a lodi-config-lock version 1 lock")
            && err(&refused_project).contains("a project's"),
        "{}",
        story(&refused_project)
    );

    let mut unknown = json(&root);
    unknown["extra"] = serde_json::json!(true);
    v.case
        .write_beside("lodi.lock", serde_json::to_string_pretty(&unknown).unwrap());
    refused(&v.run("plan", &[]), "E_CONFIG");
}
