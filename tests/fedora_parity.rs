//! fx-1 (LD-482): everything lodi did before 1.7 on Debian, Ubuntu and Arch, also on Fedora 44.
//!
//! The older checks, grouped by area: exact mode, drift and the reconcile; and a user-owned host
//! directory, a file owner and a metapackage. The home and recipe rows ran the top-level
//! `lodi apply`, and went with it in 2.0 (#699, LD-500).
//! Every case is the real binary against a scratch `--root` with the fake machine of
//! `tests/support/fakehost.rs` behind it, whose dnf5 and rpm answer as a Fedora 44 guest printed,
//! and, where the check compares, the same steps on a scratch Debian root. Upstreams are served
//! on loopback from the recorded fixtures through `LODI_FETCH_REWRITE`; nothing reaches the
//! network or `/` (`AGENTS.md` §8), and nothing runs as root.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
mod support;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::{Command, Output};

use fakehost::{Case, Machine, Pkg, err, nothing, out, story};
use hostroot::ids;

/// The child half of every held apply of this binary (`tests/support/hostroot.rs`).
#[test]
fn child_apply() {
    if !hostroot::is_child() {
        return;
    }
    hostroot::child_apply();
}

/// A package at a Fedora 44 build.
fn fc44(name: &str, version: &str) -> Pkg {
    let mut pkg = Pkg::new(name);
    pkg.version = format!("0:{version}.fc44");
    pkg
}

/// The login of the test's own user, as `lodi home apply SOURCE` selects `home/<login>/`.
fn login() -> String {
    let id = Command::new("id").arg("-un").output().unwrap();
    String::from_utf8(id.stdout).unwrap().trim().to_string()
}

/// A case called `box` whose passwd names the test's own user by its login, with its home at
/// `/people/sample`, so a home's apply drops to who the test already is.
fn machine_box(name: &str, machine: Machine) -> Case {
    let case = Case::new(name, machine);
    let (uid, gid) = ids(&case.root);
    case.root.write(
        "etc/passwd",
        &format!(
            "root:x:0:0::/root:/bin/sh\n{}:x:{uid}:{gid}::/people/sample:/bin/sh\n",
            login()
        ),
    );
    case.root
        .write("etc/group", &format!("root:x:0:\n{}:x:{gid}:\n", login()));
    case.root.write("etc/hostname", "box\n");
    fs::create_dir_all(case.root.path("people/sample")).unwrap();
    case
}

fn ok(output: &Output) {
    assert_eq!(output.status.code(), Some(0), "{}", story(output));
}

// ------------------------------------------------------ exact mode, drift and the reconcile ---

/// The names of `[packages.fedora]`'s `add` list, in the order the file has them.
fn declared(manifest: &str) -> Vec<String> {
    manifest
        .split("\n[packages.fedora]\n")
        .nth(1)
        .unwrap_or_else(|| panic!("no [packages.fedora]:\n{manifest}"))
        .lines()
        .skip_while(|line| *line != "add = [")
        .skip(1)
        .take_while(|line| *line != "]")
        .map(|line| line.trim().trim_end_matches(',').trim_matches('"'))
        .filter(|name| !name.is_empty() && !name.starts_with('#'))
        .map(ToString::to_string)
        .collect()
}

fn remove_by_hand(case: &Case, name: &str) {
    case.edit(|m| {
        m["installed"].as_object_mut().unwrap().remove(name);
    });
}

#[test]
fn exact_drift_and_reconcile_on_fedora() {
    let machine = Machine::fedora()
        .with(fc44("tree", "2.2.1-4").depends(&["glibc"]))
        .with(fc44("jq", "1.8.1-1").depends(&["glibc"]))
        .with(fc44("bat", "0.25.0-2").depends(&["glibc"]))
        .offering(fc44("htop", "3.4.1-3").depends(&["glibc"]))
        .offering(fc44("zip", "3.0-44").depends(&["glibc"]));
    let case = Case::new("fx-exact", machine);
    ok(&case.import());
    let manifest = case.manifest();
    assert!(manifest.contains("packages = \"exact\""), "{manifest}");
    assert_eq!(
        declared(&manifest),
        ["bash", "bat", "dnf5", "jq", "kernel", "tree"]
    );

    // A hand install is drift: named, refused at 11 with the machine unchanged, then removed
    // when the user says so; a second apply has nothing to do.
    case.install_by_hand(fc44("htop", "3.4.1-3").depends(&["glibc"]));
    let plan = case.plan();
    ok(&plan);
    assert!(
        err(&plan).contains("W_DRIFT: package htop was installed by hand"),
        "{}",
        story(&plan)
    );
    let before = case.machine();
    let refused = case.apply(&[]);
    assert_eq!(refused.status.code(), Some(11), "{}", story(&refused));
    assert!(
        err(&refused).contains("E_DECLINED") && err(&refused).contains("htop"),
        "{}",
        story(&refused)
    );
    assert_eq!(case.machine(), before, "the refusal moved the machine");
    ok(&case.apply(&["--overwrite-drift"]));
    assert!(!case.installed().contains("htop"));
    assert!(nothing(&case.apply(&[])));

    // Outside lodi: zip installed, tree removed. By hand: a note, jq after the kernel, and
    // bat's line deleted. The re-import adopts zip, drops tree and keeps every edit.
    case.install_by_hand(fc44("zip", "3.0-44").depends(&["glibc"]));
    remove_by_hand(&case, "tree");
    let text = case
        .manifest()
        .replacen(
            "  \"jq\",\n  \"kernel\",\n",
            "  \"kernel\",\n  \"jq\",\n",
            1,
        )
        .replacen("add = [\n", "# my own note\nadd = [\n", 1);
    case.set_manifest(&text);
    case.delete_line("bat");
    let again = case.import();
    ok(&again);
    assert!(
        out(&again).contains("1 added, 1 removed") && out(&again).contains("0 conflict"),
        "{}",
        story(&again)
    );
    let manifest = case.manifest();
    assert_eq!(
        declared(&manifest),
        ["bash", "dnf5", "kernel", "jq", "zip"],
        "{manifest}"
    );
    assert!(manifest.contains("# my own note\nadd = ["), "{manifest}");

    // The deleted line's package is removed, and a later import does not add it back.
    let apply = case.apply(&[]);
    ok(&apply);
    assert!(!case.installed().contains("bat"), "{}", story(&apply));
    assert!(case.installed().contains("zip") && case.installed().contains("jq"));
    let noop = case.import();
    ok(&noop);
    assert_eq!(case.manifest(), manifest, "{}", story(&noop));
    assert!(!case.installed().contains("bat"));
    assert!(nothing(&case.plan()));
}

// ----------------------------------------- a user-owned host directory, owner, metapackage ---

#[test]
fn host_directory_owner_and_metapackage_on_fedora() {
    let machine = Machine::fedora()
        .with(fc44("server-meta", "44-1").depends(&["gitlike", "lvmlike"]))
        .with(fc44("gitlike", "2.51.0-1").dep().depends(&["glibc"]))
        .with(fc44("lvmlike", "2.03.32-1").depends(&["glibc"]));
    let case = machine_box("fx-hostdir", machine);
    ok(&case.import());
    let imported = case.manifest();
    assert!(imported.contains("\"server-meta\""), "{imported}");
    assert!(!imported.contains("\"gitlike\""), "{imported}");

    // The host is one of a config's hosts, chosen by the machine's hostname, declaring a file
    // owned by that user; the other host's file is never read.
    let owned = "/etc/fx-owned.conf";
    let user = login();
    let host = format!(
        "{imported}\n[files.\"{owned}\"]\ncontent = \"the user's own\\n\"\nowner = \"{user}\"\n\
         group = \"{user}\"\nmode = \"0640\"\n"
    );
    fs::remove_file(case.beside("host.toml")).unwrap();
    case.write_beside(
        "config.toml",
        "[hosts.box]\nhost = \"box/host.toml\"\n\n[hosts.other]\nhost = \"other/host.toml\"\n",
    );
    case.write_beside("box/host.toml", &host);
    case.write_beside("other/host.toml", "not a manifest [\n");
    let (uid, gid) = ids(&case.root);
    assert_eq!(fs::metadata(case.beside("box")).unwrap().uid(), uid);

    let before = case.installed();
    let apply = case.apply(&[]);
    ok(&apply);
    let meta = fs::metadata(case.root.path("etc/fx-owned.conf")).unwrap();
    assert_eq!((meta.uid(), meta.gid()), (uid, gid), "{}", story(&apply));
    assert_eq!(meta.permissions().mode() & 0o7777, 0o640);
    assert_eq!(case.root.read("etc/fx-owned.conf"), "the user's own\n");

    // A hand removal takes the metapackage and a member; the next apply puts both back and
    // removes nothing, and a second apply has nothing to do.
    remove_by_hand(&case, "server-meta");
    remove_by_hand(&case, "gitlike");
    let plan = case.plan();
    ok(&plan);
    assert!(
        err(&plan).contains("+ package server-meta") && !err(&plan).contains("W_DRIFT"),
        "{}",
        story(&plan)
    );
    ok(&case.apply(&[]));
    assert_eq!(case.installed(), before);
    assert!(case.is_explicit("server-meta") && case.is_explicit("lvmlike"));
    let again = case.apply(&[]);
    assert!(nothing(&again), "{}", story(&again));
}
