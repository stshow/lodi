//! LD-379 (W3): a host is a directory you own.
//!
//! `plan`, `apply` and `import` take a positional `SOURCE`: a host directory holding `host.toml`
//! and `files/`, or a directory of hosts one of which is chosen by the root's hostname or by
//! `--host`. Applying it is safe by rule: every directory from the root down to it and every
//! file the apply reads there is not a symbolic link, belongs to a trusted owner and is neither
//! group- nor world-writable; a network filesystem is refused; and what the apply acts on is
//! read once. The lock and the journal stay machine-local and root-owned.
//!
//! Every case is the real binary or the real library against a scratch `--root` with the fake
//! machine of `tests/support/fakehost.rs` behind it. Nothing reaches `/` and no real package
//! manager runs (`AGENTS.md` §8): every host command here carries `--root`, and every `SOURCE`
//! lies inside that root, which under `--root` is where the trust walk begins.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::cell::RefCell;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use lodi::diag::Diagnostic;
use lodi::hostscope::privilege::Privilege;
use lodi::hostscope::{Options, source};

use fakehost::{Case, Machine, Pkg, err, out, story};
use hostroot::{Hold, ids};

/// The child half of the read-once case: one real apply, in its own process.
#[test]
fn child_apply() {
    if !hostroot::is_child() {
        return;
    }
    hostroot::child_apply();
}

/// A manifest declaring one file with inline content, owned by the invoking user.
fn manifest_with(root: &hostroot::Root, path: &str, content: &str) -> String {
    let (uid, gid) = ids(root);
    format!(
        "[host]\ndistro = \"debian\"\n\n[files.\"{path}\"]\ncontent = \"{content}\\n\"\n\
         owner = \"{uid}\"\ngroup = \"{gid}\"\n"
    )
}

/// A Debian case whose root is called `box`, with a directory of hosts at `<root>/hosts`
/// holding `box` and `other`, each declaring a file of its own.
fn hosts_case(name: &str) -> Case {
    let case = Case::new(name, Machine::debian());
    let root = &case.root;
    root.write("etc/hostname", "box\n");
    let box_manifest = manifest_with(root, "/etc/w3-box.conf", "from box");
    let other_manifest = manifest_with(root, "/etc/w3-other.conf", "from other");
    root.write("hosts/box/host.toml", &box_manifest);
    root.write("hosts/other/host.toml", &other_manifest);
    case
}

fn hosts(case: &Case) -> String {
    case.root.path("hosts").display().to_string()
}

fn code(output: &std::process::Output) -> Option<i32> {
    output.status.code()
}

struct DropRecorder {
    uid: u32,
    gid: u32,
    root: std::path::PathBuf,
    events: std::cell::RefCell<Vec<String>>,
}

impl lodi::hostscope::privilege::Privilege for DropRecorder {
    fn euid(&self) -> u32 {
        0
    }

    fn sudo(&self) -> Option<(u32, u32)> {
        Some((self.uid, self.gid))
    }

    fn become_user(&self, uid: u32, gid: u32) -> Result<(), lodi::diag::Diagnostic> {
        let lock = self.root.join("etc/lodi/host.lock").exists();
        let note = self.root.join("people/sample/note").exists();
        self.events
            .borrow_mut()
            .push(format!("drop {uid}:{gid} lock={lock} home-write={note}"));
        Ok(())
    }
}

#[test]
fn h3_drop_occurs_after_host_lock_and_before_home_write() {
    if hostroot::is_child() {
        let root = std::path::PathBuf::from(std::env::var_os("LODI_TEST_ROOT").unwrap());
        let hosts = root.join("hosts");
        let uid = std::env::var("LODI_TEST_UID").unwrap().parse().unwrap();
        let gid = std::env::var("LODI_TEST_GID").unwrap().parse().unwrap();
        let recorder = DropRecorder {
            uid,
            gid,
            root: root.clone(),
            events: std::cell::RefCell::new(Vec::new()),
        };
        let options = lodi::hostscope::Options {
            root: Some(root),
            source: Some(hosts),
            ..Default::default()
        };
        lodi::hostscope::apply_with_privilege(&options, None, &recorder).unwrap();
        for event in recorder.events.borrow().iter() {
            println!("EVENT {event}");
        }
        return;
    }
    let case = Case::new("h3-drop", Machine::debian());
    let (uid, gid) = ids(&case.root);
    case.root.write("etc/hostname", "box\n");
    case.root.write(
        "etc/passwd",
        &format!("sample:x:{uid}:{gid}::/people/sample:/bin/sh\n"),
    );
    case.root.write(
        "hosts/box/host.toml",
        &manifest_with(&case.root, "/etc/drop-host", "host first"),
    );
    let home = ["hosts", "box", "home", "sample", "home.toml"].join("/");
    case.root.write(
        &home,
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"made\"\n",
    );
    std::fs::create_dir_all(case.root.path("people/sample")).unwrap();
    let root = case.root.dir.display().to_string();
    let path = case.fake_path();
    let uid = uid.to_string();
    let gid = gid.to_string();
    let output = hostroot::spawn(
        "h3_drop_occurs_after_host_lock_and_before_home_write",
        &[
            ("LODI_TEST_ROOT", &root),
            ("LODI_TEST_UID", &uid),
            ("LODI_TEST_GID", &gid),
            ("PATH", &path),
        ],
    )
    .wait_with_output()
    .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", story(&output));
    assert!(
        out(&output).contains(&format!(
            "EVENT drop {uid}:{gid} lock=true home-write=false"
        )),
        "{}",
        story(&output)
    );
    assert_eq!(case.root.read("people/sample/note"), "made");
}

/// H3: a combined plan drops to the user, once, before it plans the home.
#[test]
fn h3_plan_drops_before_the_home_part() {
    if hostroot::is_child() {
        let root = std::path::PathBuf::from(std::env::var_os("LODI_TEST_ROOT").unwrap());
        let uid = std::env::var("LODI_TEST_UID").unwrap().parse().unwrap();
        let gid = std::env::var("LODI_TEST_GID").unwrap().parse().unwrap();
        let recorder = DropRecorder {
            uid,
            gid,
            root: root.clone(),
            events: std::cell::RefCell::new(Vec::new()),
        };
        let options = lodi::hostscope::Options {
            root: Some(root.clone()),
            source: Some(root.join("hosts")),
            ..Default::default()
        };
        let plan = lodi::hostscope::plan_with_privilege(&options, &recorder).unwrap();
        println!("PLAN {}", plan.contains("home sample"));
        for event in recorder.events.borrow().iter() {
            println!("EVENT {event}");
        }
        return;
    }
    let case = Case::new("h3-plan-drop", Machine::debian());
    let (uid, gid) = ids(&case.root);
    case.root.write("etc/hostname", "box\n");
    case.root.write(
        "etc/passwd",
        &format!("sample:x:{uid}:{gid}::/people/sample:/bin/sh\n"),
    );
    case.root.write(
        "hosts/box/host.toml",
        "[host]\nversion = \"1\"\ndistro = \"debian\"\n",
    );
    case.root.write(
        &["hosts", "box", "home", "sample", "home.toml"].join("/"),
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"made\"\n",
    );
    std::fs::create_dir_all(case.root.path("people/sample")).unwrap();
    let root = case.root.dir.display().to_string();
    let path = case.fake_path();
    let (uid, gid) = (uid.to_string(), gid.to_string());
    let output = hostroot::spawn(
        "h3_plan_drops_before_the_home_part",
        &[
            ("LODI_TEST_ROOT", &root),
            ("LODI_TEST_UID", &uid),
            ("LODI_TEST_GID", &gid),
            ("PATH", &path),
        ],
    )
    .wait_with_output()
    .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", story(&output));
    let text = out(&output);
    assert!(text.contains("PLAN true"), "{}", story(&output));
    let drops = text
        .lines()
        .filter(|line| line.starts_with("EVENT drop"))
        .count();
    assert_eq!(drops, 1, "{}", story(&output));
    assert!(
        text.contains(&format!("EVENT drop {uid}:{gid} lock=false")),
        "{}",
        story(&output)
    );
}

// ------------------------------------------------------------ D1: the positional SOURCE ---

/// D1. `plan SOURCE` and `apply SOURCE` read the host the directory of hosts holds for this
/// root's hostname; a bare plan and apply still read the root's own `etc/lodi`.
#[test]
fn d1_plan_and_apply_read_the_host_a_positional_source_selects() {
    let case = hosts_case("d1-source");
    let plan = case.plan_with(&[&hosts(&case)]);
    assert_eq!(code(&plan), Some(0), "{}", story(&plan));
    assert!(out(&plan).contains("/etc/w3-box.conf"), "{}", story(&plan));
    assert!(
        !out(&plan).contains("/etc/w3-other.conf"),
        "{}",
        story(&plan)
    );

    let apply = case.apply(&[&hosts(&case)]);
    assert_eq!(code(&apply), Some(0), "{}", story(&apply));
    assert_eq!(case.root.read("etc/w3-box.conf"), "from box\n");
    assert!(!case.root.exists("etc/w3-other.conf"));
}

#[test]
fn d1_a_bare_plan_and_apply_still_read_etc_lodi() {
    let case = hosts_case("d1-bare");
    let bare = manifest_with(&case.root, "/etc/w3-bare.conf", "from etc lodi");
    case.set_manifest(&bare);
    let plan = case.plan();
    assert_eq!(code(&plan), Some(0), "{}", story(&plan));
    assert!(out(&plan).contains("/etc/w3-bare.conf"), "{}", story(&plan));
    assert!(!out(&plan).contains("/etc/w3-box.conf"), "{}", story(&plan));
    let apply = case.apply(&[]);
    assert_eq!(code(&apply), Some(0), "{}", story(&apply));
    assert_eq!(case.root.read("etc/w3-bare.conf"), "from etc lodi\n");
}

// ---------------------------------------------------------------------- D2: selection ---

/// D2. A `SOURCE` holding `host.toml` is the host itself, and `--host` beside it is a usage
/// error.
#[test]
fn d2_a_source_holding_host_toml_is_the_host_and_host_beside_it_is_a_usage_error() {
    let case = hosts_case("d2-itself");
    let other = case.root.path("hosts/other").display().to_string();
    let plan = case.plan_with(&[&other]);
    assert_eq!(code(&plan), Some(0), "{}", story(&plan));
    assert!(
        out(&plan).contains("/etc/w3-other.conf"),
        "{}",
        story(&plan)
    );

    let both = case.plan_with(&[&other, "--host", "box"]);
    assert_eq!(code(&both), Some(2), "{}", story(&both));
    assert!(err(&both).contains("--host"), "{}", story(&both));
}

/// D2. `--host NAME` overrides the hostname.
#[test]
fn d2_host_overrides_the_hostname() {
    let case = hosts_case("d2-override");
    let plan = case.plan_with(&[&hosts(&case), "--host", "other"]);
    assert_eq!(code(&plan), Some(0), "{}", story(&plan));
    assert!(
        out(&plan).contains("/etc/w3-other.conf"),
        "{}",
        story(&plan)
    );
    assert!(!out(&plan).contains("/etc/w3-box.conf"), "{}", story(&plan));
}

/// D2. A host directory that is not there is `E_NO_MANIFEST`, and the hint lists the hosts
/// that are.
#[test]
fn d2_a_missing_host_is_no_manifest_and_names_the_hosts_found() {
    let case = hosts_case("d2-missing");
    case.root.write("etc/hostname", "elsewhere\n");
    let plan = case.plan_with(&[&hosts(&case)]);
    assert_eq!(code(&plan), Some(3), "{}", story(&plan));
    let said = err(&plan);
    assert!(said.contains("E_NO_MANIFEST"), "{said}");
    assert!(said.contains("elsewhere"), "{said}");
    assert!(said.contains("box") && said.contains("other"), "{said}");

    // A root with no hostname at all says so, and names the flag that chooses one.
    fs::remove_file(case.root.path("etc/hostname")).unwrap();
    let plan = case.plan_with(&[&hosts(&case)]);
    assert_eq!(code(&plan), Some(3), "{}", story(&plan));
    assert!(err(&plan).contains("E_NO_MANIFEST"), "{}", story(&plan));
    assert!(err(&plan).contains("--host"), "{}", story(&plan));
}

/// D2. A hostname, or a `--host`, that is not one safe path component is refused.
#[test]
fn d2_a_name_that_is_not_one_safe_path_component_is_refused() {
    let case = hosts_case("d2-names");
    for name in ["..", ".", "/", "a/b", "../box", ""] {
        let plan = case.plan_with(&[&hosts(&case), "--host", name]);
        assert!(
            matches!(code(&plan), Some(2 | 3)),
            "--host {name:?}: {}",
            story(&plan)
        );
        assert!(
            !out(&plan).contains("/etc/w3-"),
            "--host {name:?} selected a host: {}",
            story(&plan)
        );
        if !name.is_empty() {
            assert!(
                err(&plan).contains("E_PATH_ESCAPE"),
                "--host {name:?}: {}",
                story(&plan)
            );
        }
    }
    for name in ["..", "../box", "a/b", "/"] {
        case.root.write("etc/hostname", &format!("{name}\n"));
        let plan = case.plan_with(&[&hosts(&case)]);
        assert_eq!(code(&plan), Some(3), "hostname {name:?}: {}", story(&plan));
        assert!(
            err(&plan).contains("E_PATH_ESCAPE"),
            "hostname {name:?}: {}",
            story(&plan)
        );
    }
}

/// D2. There is no `#` selector: `hosts#box` is a path like any other, and it is not there.
#[test]
fn d2_there_is_no_hash_selector() {
    let case = hosts_case("d2-hash");
    let plan = case.plan_with(&[&format!("{}#box", hosts(&case))]);
    assert_ne!(code(&plan), Some(0), "{}", story(&plan));
    assert!(!out(&plan).contains("/etc/w3-box.conf"), "{}", story(&plan));
}

// -------------------------------------------------------------------------- D3: trust ---

/// D3, the decision table: whom a host directory may belong to.
#[test]
fn d3_the_trusted_owners_are_decided_by_one_table() {
    let rows: &[(u32, Option<&str>, &[u32])] = &[
        // An unprivileged user: that user (and root, who owns `/` and can write anything).
        (1000, None, &[0, 1000]),
        (1000, Some("1001"), &[0, 1000]),
        // Under sudo with SUDO_UID not 0: root and that user.
        (0, Some("1000"), &[0, 1000]),
        // Otherwise root only.
        (0, Some("0"), &[0]),
        (0, None, &[0]),
        (0, Some(""), &[0]),
        (0, Some("x1000"), &[0]),
        (0, Some("-1"), &[0]),
    ];
    for (euid, sudo, expected) in rows {
        assert_eq!(
            source::trusted_owners(*euid, sudo.map(OsStr::new)),
            expected.to_vec(),
            "euid {euid}, SUDO_UID {sudo:?}"
        );
    }
}

/// D3, the decision table: which owner and mode pass.
#[test]
fn d3_an_entry_passes_only_with_a_trusted_owner_and_no_group_or_world_write() {
    let owners = [0, 1000];
    let rows: &[(u32, u32, bool)] = &[
        (1000, 0o755, true),
        (0, 0o755, true),
        (1000, 0o700, true),
        (1000, 0o644, true),
        (1000, 0o775, false),
        (1000, 0o757, false),
        (1000, 0o1777, false),
        (0, 0o1777, false),
        (1000, 0o664, false),
        (1001, 0o755, false),
    ];
    for (owner, mode, passes) in rows {
        let why = source::untrusted(*owner, *mode, &owners);
        assert_eq!(
            why.is_none(),
            *passes,
            "owner {owner}, mode {mode:04o}: {why:?}"
        );
    }
}

/// D3, the decision table: NFS and SMB/CIFS are refused by their `statfs` type.
#[test]
fn d3_a_network_filesystem_is_refused_by_its_statfs_type() {
    for (magic, refused) in [
        (0x6969_i64, true),   // NFS
        (0x517B, true),       // SMB
        (0xFF53_4D42, true),  // CIFS
        (0xFE53_4D42, true),  // SMB2
        (0x0102_1994, false), // tmpfs
        (0xEF53, false),      // ext2/3/4
        (0x5846_5342, false), // xfs
        (0x9123_683E, false), // btrfs
    ] {
        assert_eq!(
            source::refused_filesystem(magic).is_some(),
            refused,
            "statfs type {magic:#x}"
        );
    }
}

/// D3, refusals on the real binary: each breaks one rule, is `E_PATH_ESCAPE` at exit 3 naming
/// the path and the rule, and the same root plans once the rule holds again.
#[test]
fn d3_every_broken_rule_is_path_escape_naming_the_path() {
    type Break = fn(&Case);
    type Mend = fn(&Case);
    let cases: &[(&str, &str, Break, Mend)] = &[
        (
            "host directory g+w",
            "hosts/box",
            |c| {
                c.root.chmod("hosts/box", 0o775);
            },
            |c| {
                c.root.chmod("hosts/box", 0o755);
            },
        ),
        (
            "hosts directory o+w",
            "hosts",
            |c| {
                c.root.chmod("hosts", 0o757);
            },
            |c| {
                c.root.chmod("hosts", 0o755);
            },
        ),
        (
            "hosts directory sticky and world-writable",
            "hosts",
            |c| {
                c.root.chmod("hosts", 0o1777);
            },
            |c| {
                c.root.chmod("hosts", 0o755);
            },
        ),
        (
            "host.toml g+w",
            "hosts/box/host.toml",
            |c| {
                c.root.chmod("hosts/box/host.toml", 0o664);
            },
            |c| {
                c.root.chmod("hosts/box/host.toml", 0o644);
            },
        ),
        (
            "a source o+w",
            "hosts/box/files/etc/w3-src.conf",
            |c| {
                c.root.chmod("hosts/box/files/etc/w3-src.conf", 0o646);
            },
            |c| {
                c.root.chmod("hosts/box/files/etc/w3-src.conf", 0o644);
            },
        ),
        (
            "files/ g+w",
            "hosts/box/files",
            |c| {
                c.root.chmod("hosts/box/files", 0o775);
            },
            |c| {
                c.root.chmod("hosts/box/files", 0o755);
            },
        ),
        (
            "the host directory a symbolic link",
            "hosts/box",
            |c| {
                fs::rename(c.root.path("hosts/box"), c.root.path("hosts/real-box")).unwrap();
                std::os::unix::fs::symlink("real-box", c.root.path("hosts/box")).unwrap();
            },
            |c| {
                fs::remove_file(c.root.path("hosts/box")).unwrap();
                fs::rename(c.root.path("hosts/real-box"), c.root.path("hosts/box")).unwrap();
            },
        ),
        (
            "host.toml a symbolic link",
            "hosts/box/host.toml",
            |c| {
                fs::rename(
                    c.root.path("hosts/box/host.toml"),
                    c.root.path("hosts/box/real.toml"),
                )
                .unwrap();
                std::os::unix::fs::symlink("real.toml", c.root.path("hosts/box/host.toml"))
                    .unwrap();
            },
            |c| {
                fs::remove_file(c.root.path("hosts/box/host.toml")).unwrap();
                fs::rename(
                    c.root.path("hosts/box/real.toml"),
                    c.root.path("hosts/box/host.toml"),
                )
                .unwrap();
            },
        ),
        (
            "a source a symbolic link",
            "hosts/box/files/etc/w3-src.conf",
            |c| {
                let at = c.root.path("hosts/box/files/etc/w3-src.conf");
                fs::rename(&at, c.root.path("hosts/box/files/etc/real.conf")).unwrap();
                std::os::unix::fs::symlink("real.conf", &at).unwrap();
            },
            |c| {
                let at = c.root.path("hosts/box/files/etc/w3-src.conf");
                fs::remove_file(&at).unwrap();
                fs::rename(c.root.path("hosts/box/files/etc/real.conf"), &at).unwrap();
            },
        ),
        (
            "a source a FIFO",
            "hosts/box/files/etc/w3-src.conf",
            |c| {
                let at = c.root.path("hosts/box/files/etc/w3-src.conf");
                fs::rename(&at, c.root.path("hosts/box/files/etc/real.conf")).unwrap();
                let status = std::process::Command::new("mkfifo")
                    .arg(&at)
                    .status()
                    .unwrap();
                assert!(status.success());
            },
            |c| {
                let at = c.root.path("hosts/box/files/etc/w3-src.conf");
                fs::remove_file(&at).unwrap();
                fs::rename(c.root.path("hosts/box/files/etc/real.conf"), &at).unwrap();
            },
        ),
    ];
    let case = hosts_case("d3-refusals");
    let (uid, gid) = ids(&case.root);
    case.root.write(
        "hosts/box/host.toml",
        &format!(
            "[host]\ndistro = \"debian\"\n\n[files.\"/etc/w3-src.conf\"]\n\
             source = \"files/etc/w3-src.conf\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
        ),
    );
    case.root
        .write("hosts/box/files/etc/w3-src.conf", "from a source\n")
        .chmod("hosts/box/files/etc/w3-src.conf", 0o644);
    let control = case.plan_with(&[&hosts(&case)]);
    assert_eq!(code(&control), Some(0), "the control: {}", story(&control));
    assert!(
        out(&control).contains("/etc/w3-src.conf"),
        "{}",
        story(&control)
    );

    for (what, path, broken, mended) in cases {
        broken(&case);
        let plan = case.plan_with(&[&hosts(&case)]);
        let said = err(&plan);
        assert_eq!(code(&plan), Some(3), "{what}: {}", story(&plan));
        assert!(said.contains("E_PATH_ESCAPE"), "{what}: {said}");
        assert!(
            said.contains(&case.root.path(path).display().to_string()),
            "{what}: the refusal does not name {path}: {said}"
        );
        let apply = case.apply(&[&hosts(&case)]);
        assert_eq!(code(&apply), Some(3), "{what}: {}", story(&apply));
        assert!(
            !case.root.exists("etc/w3-src.conf"),
            "{what}: the apply wrote"
        );
        mended(&case);
        let again = case.plan_with(&[&hosts(&case)]);
        assert_eq!(code(&again), Some(0), "{what}, mended: {}", story(&again));
    }
}

/// D3. Under `--root` the trust walk begins at the root, so a `SOURCE` outside it is refused;
/// and arming is unchanged: an unarmed root is `E_HOST_NOT_ARMED` whatever `SOURCE` says.
#[test]
fn d3_a_source_outside_the_root_is_refused_and_arming_is_unchanged() {
    let case = hosts_case("d3-outside");
    let outside = PathBuf::from(format!("{}-outside-hosts", case.root.dir.display()));
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(outside.join("box")).unwrap();
    fs::write(
        outside.join("box/host.toml"),
        manifest_with(&case.root, "/etc/w3-outside.conf", "outside"),
    )
    .unwrap();
    let plan = case.plan_with(&[&outside.display().to_string()]);
    let _ = fs::remove_dir_all(&outside);
    assert_eq!(code(&plan), Some(3), "{}", story(&plan));
    assert!(err(&plan).contains("E_PATH_ESCAPE"), "{}", story(&plan));

    fs::remove_file(case.root.path("etc/lodi/host-allowed")).unwrap();
    let plan = case.plan_with(&[&hosts(&case)]);
    assert_eq!(code(&plan), Some(9), "{}", story(&plan));
    assert!(err(&plan).contains("E_HOST_NOT_ARMED"), "{}", story(&plan));
}

/// The README's fresh machine (LD-380 F2): an import into a directory of hosts puts no marker
/// in it, and a `host-allowed` copied into that directory arms nothing: an unarmed root is still
/// `E_HOST_NOT_ARMED` for plan and apply, and nothing is written.
#[test]
fn a_marker_copied_with_the_host_directory_does_not_arm() {
    let case = Case::new("marker-copy", Machine::debian().with(Pkg::new("jq")));
    case.root.write("etc/hostname", "box\n");
    fs::create_dir_all(case.root.path("hosts")).unwrap();
    let import = case.verb("import", &[&hosts(&case)]);
    assert_eq!(code(&import), Some(0), "{}", story(&import));
    assert!(
        !case.root.exists("hosts/host-allowed"),
        "{}",
        story(&import)
    );
    assert!(
        !case.root.exists("hosts/box/host-allowed"),
        "{}",
        story(&import)
    );

    let box_manifest = manifest_with(&case.root, "/etc/w3-copied.conf", "copied");
    case.root.write("hosts/box/host.toml", &box_manifest);
    let marker = fs::read(case.root.path("etc/lodi/host-allowed")).unwrap();
    fs::write(case.root.path("hosts/host-allowed"), &marker).unwrap();
    fs::write(case.root.path("hosts/box/host-allowed"), &marker).unwrap();
    fs::remove_file(case.root.path("etc/lodi/host-allowed")).unwrap();
    let plan = case.plan_with(&[&hosts(&case)]);
    assert_eq!(code(&plan), Some(9), "{}", story(&plan));
    assert!(err(&plan).contains("E_HOST_NOT_ARMED"), "{}", story(&plan));
    let apply = case.apply(&[&hosts(&case)]);
    assert_eq!(code(&apply), Some(9), "{}", story(&apply));
    assert!(
        err(&apply).contains("E_HOST_NOT_ARMED"),
        "{}",
        story(&apply)
    );
    assert!(!case.root.exists("etc/w3-copied.conf"), "{}", story(&apply));
}

/// One apply of `<root>/hosts/box`, held at its first open of `box`; while it is held, `hosts` is
/// swapped for a link to `evil` when `swap` is set.
fn held_source_apply(name: &str, swap: bool) -> (hostroot::Root, std::process::Output) {
    let root = hostroot::Root::new(name);
    root.arm().debian();
    root.write(
        "hosts/box/host.toml",
        &manifest_with(&root, "/etc/w3-box.conf", "from box"),
    );
    root.write(
        "evil/box/host.toml",
        &manifest_with(&root, "/etc/w3-evil.conf", "from evil"),
    );
    root.chmod("evil", 0o777);
    let source = root.path("hosts/box").display().to_string();
    let output = {
        let hold = Hold::at_open_of(&root, "box");
        let child = hold.spawn_with(&root, &[(hostroot::SOURCE_VAR, &source)]);
        hold.wait_held();
        if swap {
            fs::rename(root.path("hosts"), root.path("hosts.real")).unwrap();
            std::os::unix::fs::symlink(root.path("evil"), root.path("hosts")).unwrap();
        }
        hold.release();
        child.wait_with_output().expect("the apply finishes")
    };
    (root, output)
}

/// D3 (validator-repair-1). The trust walk is not a check of names followed by opens of them: a
/// directory on the way to the host, judged trusted, then swapped for a symbolic link to a
/// directory anyone can write before the apply reads the host, is refused with `E_PATH_ESCAPE`
/// and nothing is done. The apply is held at its first open of the host directory `box`, after
/// the first walk has judged `hosts`; the test then moves `hosts` aside, puts a link to `evil` —
/// mode 0777, holding a `box` of its own — where it was, and lets the apply go on.
#[test]
fn d3_an_intermediate_directory_swapped_for_a_link_is_path_escape_and_no_action() {
    // The control: the same held apply, nothing swapped, applies the host.
    let (root, output) = held_source_apply("d3-swap-control", false);
    assert_eq!(
        output.status.code(),
        Some(0),
        "the control: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(root.read("etc/w3-box.conf"), "from box\n");

    let (root, output) = held_source_apply("d3-swap", true);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(3),
        "the swapped intermediate directory was followed:\n{}{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("E_PATH_ESCAPE") && stderr.contains("it is a symbolic link"),
        "{stderr}"
    );
    for written in ["etc/w3-evil.conf", "etc/w3-box.conf", "etc/lodi/host.lock"] {
        assert!(!root.exists(written), "{written} was written: {stderr}");
    }
    assert!(hostroot::journals(&root).is_empty(), "a journal was begun");
}

// ---------------------------------------------------------------------- D4: read once ---

/// D4. What the apply writes is what it planned: the source is read once, before the first plan,
/// and a source swapped between that plan and the write changes nothing written.
#[test]
fn d4_a_source_swapped_between_plan_and_write_still_writes_the_planned_bytes() {
    let root = hostroot::Root::new("d4-read-once");
    let (uid, gid) = ids(&root);
    root.write("etc/lodi/files/etc/b.conf", "planned\n");
    // `/etc/a-gate.conf` sorts first, so it is `f1`: the apply is held at its write, after both
    // plans and before `f2` writes `/etc/b.conf` from its source.
    root.debian_with(&format!(
        "[files.\"/etc/a-gate.conf\"]\ncontent = \"gate\\n\"\nowner = \"{uid}\"\n\
         group = \"{gid}\"\n\n[files.\"/etc/b.conf\"]\nsource = \"files/etc/b.conf\"\n\
         owner = \"{uid}\"\ngroup = \"{gid}\"\n"
    ));
    let hold = Hold::at_write_of(&root, "a-gate.conf");
    let child = hold.spawn(&root);
    hold.wait_held();
    root.write("etc/lodi/files/etc/b.conf", "swapped\n");
    hold.release();
    let output = child.wait_with_output().expect("the apply finishes");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        root.read("etc/b.conf"),
        "planned\n",
        "the write re-read its source"
    );
    let lock = root.read("etc/lodi/host.lock");
    assert!(
        lock.contains(&lodi::util::sha256_tagged(b"planned\n")),
        "the record is not of the planned bytes: {lock}"
    );
}

/// D4. A source over the size cap is refused before anything is planned.
#[test]
fn d4_a_source_over_the_cap_is_refused() {
    let root = hostroot::Root::new("d4-cap");
    let (uid, gid) = ids(&root);
    root.write("etc/lodi/files/etc/big.conf", "");
    let big = fs::OpenOptions::new()
        .write(true)
        .open(root.path("etc/lodi/files/etc/big.conf"))
        .unwrap();
    big.set_len(lodi::hostscope::MAX_SOURCE_BYTES as u64 + 1)
        .unwrap();
    root.debian_with(&format!(
        "[files.\"/etc/big.conf\"]\nsource = \"files/etc/big.conf\"\nowner = \"{uid}\"\n\
         group = \"{gid}\"\n"
    ));
    let error = lodi::hostscope::plan(&root.options()).expect_err("over the cap");
    assert_eq!(error.codes(), vec!["E_SYNTAX"], "{error}");
    assert!(error.to_string().contains("/etc/big.conf"), "{error}");
}

// -------------------------------------------------------------- D5: the lock's source ---

/// D5. The lock records the source of the last apply; a bare apply whose lock names another
/// source stops with `E_DECLINED` naming it; the lock and the journal stay at their
/// machine-local paths and never enter the host directory.
#[test]
fn d5_a_bare_apply_after_a_source_apply_is_declined_and_names_it() {
    let case = hosts_case("d5-fence");
    case.set_manifest(&manifest_with(&case.root, "/etc/w3-old.conf", "old"));
    let apply = case.apply(&[&hosts(&case)]);
    assert_eq!(code(&apply), Some(0), "{}", story(&apply));
    assert_eq!(
        case.lock()["source"],
        "/hosts/box",
        "{}",
        case.root.read("etc/lodi/host.lock")
    );
    assert!(
        case.root.exists("var/lib/lodi/host/journal"),
        "the journal is machine-local"
    );
    for leaked in ["host.lock", "journal", ".lock"] {
        assert!(
            !case.root.exists(&format!("hosts/box/{leaked}")),
            "{leaked} is in the host directory"
        );
    }

    let bare = case.apply(&[]);
    assert_eq!(code(&bare), Some(11), "{}", story(&bare));
    assert!(err(&bare).contains("E_DECLINED"), "{}", story(&bare));
    assert!(err(&bare).contains("/hosts/box"), "{}", story(&bare));
    assert!(
        !case.root.exists("etc/w3-old.conf"),
        "the old manifest was applied"
    );

    let again = case.apply(&[&hosts(&case)]);
    assert_eq!(code(&again), Some(0), "{}", story(&again));
    assert!(out(&again).contains("nothing to do"), "{}", story(&again));
}

/// A root writing reconcile of SOURCE must advance the machine baseline without changing the
/// lock's source fence. A bare apply must still refuse the stale in-place manifest.
#[test]
fn d5_a_root_reconcile_of_source_keeps_the_bare_apply_fence() {
    let case = Case::new(
        "d5-reconcile-fence",
        Machine::debian()
            .with(Pkg::new("jq"))
            .offering(Pkg::new("zip")),
    );
    case.root.write("etc/hostname", "box\n");
    fs::create_dir_all(case.root.path("hosts")).unwrap();
    case.set_manifest(&manifest_with(&case.root, "/etc/w3-old.conf", "old"));
    let (uid, gid) = ids(&case.root);
    let source = case.root.path("hosts");
    let first = import_pretending(&case, &source, Some((uid, gid)));
    assert_eq!(code(&first), Some(0), "{}", story(&first));
    let apply = case.apply(&[&hosts(&case)]);
    assert_eq!(code(&apply), Some(0), "{}", story(&apply));
    assert_eq!(case.lock()["source"], "/hosts/box");
    assert!(
        !case.lock()["baseline"]["explicit"]
            .to_string()
            .contains("zip")
    );

    case.install_by_hand(Pkg::new("zip"));
    let again = import_pretending(&case, &source, Some((uid, gid)));
    assert_eq!(code(&again), Some(0), "{}", story(&again));
    assert!(case.root.read("hosts/box/host.toml").contains("\"zip\""));
    let lock = case.lock();
    assert_eq!(lock["source"], "/hosts/box", "{lock}");
    assert!(
        lock["baseline"]["explicit"].to_string().contains("zip"),
        "{lock}"
    );
    let bare = case.apply(&[]);
    assert_eq!(code(&bare), Some(11), "{}", story(&bare));
    assert!(err(&bare).contains("E_DECLINED"), "{}", story(&bare));
    assert!(err(&bare).contains("/hosts/box"), "{}", story(&bare));
    assert!(!case.root.exists("etc/w3-old.conf"));
}

/// D8, the documented migration: a machine managed in `etc/lodi` moves to a host directory by
/// copying `host.toml` and `files/` there — never `host-allowed` — and applying it, which has
/// nothing to do and records the new source; a bare apply then refuses.
#[test]
fn d8_the_migration_from_etc_lodi_has_nothing_to_do() {
    let machine = Machine::debian().with(Pkg::new("jq").conffile("/etc/jq.conf"));
    let case = Case::new("d8-migration", machine);
    let (uid, gid) = ids(&case.root);
    case.root
        .write(
            "etc/passwd",
            &format!(
                "root:x:0:0:root:/root:/bin/sh\nlodi-d8:x:{uid}:{gid}::/nonexistent:/bin/sh\n"
            ),
        )
        .write("etc/group", &format!("root:x:0:\nlodi-d8:x:{gid}:\n"))
        .write("etc/jq.conf", "mine\n")
        .chmod("etc/jq.conf", 0o644)
        .write("etc/hostname", "box\n");
    let import = case.import();
    assert_eq!(code(&import), Some(0), "{}", story(&import));
    // The import copies no file (si-1): the file is declared as an earlier import wrote it.
    assert!(!case.root.exists("etc/lodi/files"), "{}", story(&import));
    case.set_manifest(&format!(
        "{}\n[files.\"/etc/jq.conf\"]\nsource = \"files/etc/jq.conf\"\nmode = \"0644\"\n\
         owner = \"lodi-d8\"\ngroup = \"lodi-d8\"\n",
        case.manifest()
    ));
    case.root.write("etc/lodi/files/etc/jq.conf", "mine\n");
    let apply = case.apply(&[]);
    assert_eq!(code(&apply), Some(0), "{}", story(&apply));

    let status = std::process::Command::new("cp")
        .arg("-r")
        .arg(case.root.path("etc/lodi/host.toml"))
        .arg(case.root.path("etc/lodi/files"))
        .arg({
            fs::create_dir_all(case.root.path("hosts/box")).unwrap();
            case.root.path("hosts/box")
        })
        .status()
        .unwrap();
    assert!(status.success());
    let moved = case.apply(&[&hosts(&case)]);
    assert_eq!(code(&moved), Some(0), "{}", story(&moved));
    assert!(out(&moved).contains("nothing to do"), "{}", story(&moved));
    assert_eq!(case.lock()["source"], "/hosts/box");
    let again = case.apply(&[&hosts(&case)]);
    assert_eq!(out(&again).trim(), "nothing to do", "{}", story(&again));
    let bare = case.apply(&[]);
    assert_eq!(code(&bare), Some(11), "{}", story(&bare));
}

// ------------------------------------------------------- D6: import into a directory ---

/// D6. An unprivileged import into a directory of hosts writes the host as the invoking user,
/// says it wrote no record, and the next apply writes it.
#[test]
fn d6_an_unprivileged_import_into_a_source_writes_no_record_and_the_next_apply_does() {
    let case = Case::new("d6-unprivileged", Machine::debian().with(Pkg::new("jq")));
    case.root.write("etc/hostname", "box\n");
    fs::create_dir_all(case.root.path("hosts")).unwrap();
    let import = case.verb("import", &[&hosts(&case)]);
    assert_eq!(code(&import), Some(0), "{}", story(&import));
    assert!(
        case.root.exists("hosts/box/host.toml"),
        "{}",
        story(&import)
    );
    assert!(case.root.read("hosts/box/host.toml").contains("\"jq\""));
    assert!(
        out(&import).contains("wrote no record"),
        "{}",
        story(&import)
    );
    assert!(!case.has_lock(), "an unprivileged import wrote the record");
    assert!(
        !case.root.exists("etc/lodi/host.toml"),
        "it landed in the root's etc/lodi"
    );

    let apply = case.apply(&[&hosts(&case)]);
    assert_eq!(code(&apply), Some(0), "{}", story(&apply));
    assert!(case.has_lock(), "{}", story(&apply));
    assert_eq!(case.lock()["source"], "/hosts/box");
    assert!(
        case.lock()["baseline"]["explicit"]
            .to_string()
            .contains("jq")
    );
    let again = case.apply(&[&hosts(&case)]);
    assert!(out(&again).contains("nothing to do"), "{}", story(&again));
}

/// D6. A second import into the same `SOURCE` reconciles the host directory as the first one
/// wrote it (LD-378 in a host directory): a package installed outside Lodi is adopted, the
/// person's edit is kept, and unprivileged it again writes no record.
#[test]
fn d6_a_second_import_into_a_source_reconciles_the_host_directory() {
    let case = Case::new(
        "d6-reconcile",
        Machine::debian()
            .with(Pkg::new("jq"))
            .offering(Pkg::new("zip")),
    );
    case.root.write("etc/hostname", "box\n");
    fs::create_dir_all(case.root.path("hosts")).unwrap();
    let first = case.verb("import", &[&hosts(&case)]);
    assert_eq!(code(&first), Some(0), "{}", story(&first));
    let apply = case.apply(&[&hosts(&case)]);
    assert_eq!(code(&apply), Some(0), "{}", story(&apply));
    let text = case.root.read("hosts/box/host.toml").replacen(
        "common = [\n",
        "# kept by hand\ncommon = [\n",
        1,
    );
    case.root.write("hosts/box/host.toml", &text);
    case.install_by_hand(Pkg::new("zip"));

    let again = case.verb("import", &[&hosts(&case)]);
    assert_eq!(code(&again), Some(0), "{}", story(&again));
    let manifest = case.root.read("hosts/box/host.toml");
    assert!(
        manifest.contains("\"zip\""),
        "{}\n{manifest}",
        story(&again)
    );
    assert!(manifest.contains("# kept by hand"), "{manifest}");
    assert!(out(&again).contains("wrote no record"), "{}", story(&again));
    assert!(
        !case.root.exists("etc/lodi/host.toml"),
        "it landed in the root's etc/lodi"
    );
    let plan = case.plan_with(&[&hosts(&case)]);
    assert_eq!(code(&plan), Some(0), "{}", story(&plan));
    assert!(
        !out(&plan).contains("zip"),
        "the plan still acts on zip: {}",
        story(&plan)
    );
}

/// A privilege this test decides: it says it is root under sudo, and records when it is asked
/// to become someone else and what was on disk at that instant.
struct Pretend {
    euid: u32,
    sudo: Option<(u32, u32)>,
    root: PathBuf,
    events: RefCell<Vec<String>>,
}

impl Privilege for Pretend {
    fn euid(&self) -> u32 {
        self.euid
    }
    fn sudo(&self) -> Option<(u32, u32)> {
        self.sudo
    }
    fn become_user(&self, uid: u32, gid: u32) -> Result<(), Diagnostic> {
        let lock = self.root.join("etc/lodi/host.lock").exists();
        let manifest = self.root.join("hosts/box/host.toml").exists();
        let files = self.root.join("hosts/box").exists();
        self.events.borrow_mut().push(format!(
            "become {uid}:{gid} lock={lock} host-dir={files} manifest={manifest}"
        ));
        Ok(())
    }
}

/// The child half of the pretended-sudo imports: one real import through the library, in its
/// own process so that the fake machine's `PATH` is its own, with [`Pretend`] as its privilege.
/// It prints each event on a line of its own, then the report.
#[test]
fn child_import_pretending() {
    let Some(pretend) = std::env::var_os("LODI_TEST_PRETEND") else {
        return;
    };
    if !hostroot::is_child() {
        return;
    }
    let root = PathBuf::from(std::env::var_os("LODI_TEST_ROOT").expect("a root"));
    let sudo = pretend
        .to_string_lossy()
        .split_once(':')
        .map(|(uid, gid)| (uid.parse().unwrap(), gid.parse().unwrap()));
    let pretend = Pretend {
        euid: 0,
        sudo,
        root: root.clone(),
        events: RefCell::new(Vec::new()),
    };
    let options = Options {
        root: Some(root),
        source: Some(PathBuf::from(
            std::env::var_os("LODI_TEST_SOURCE").expect("a source"),
        )),
        ..Options::default()
    };
    let result = lodi::hostscope::run_import_with(&options, &pretend);
    for event in pretend.events.borrow().iter() {
        println!("EVENT {event}");
    }
    match result {
        Ok(report) => {
            print!("{report}");
            std::process::exit(0);
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(i32::from(error.exit_status()));
        }
    }
}

/// Run [`child_import_pretending`] over `case` into `source`, pretending to be root with
/// `sudo` as `SUDO_UID:SUDO_GID` (or none).
fn import_pretending(case: &Case, source: &Path, sudo: Option<(u32, u32)>) -> std::process::Output {
    let root = case.root.dir.display().to_string();
    let source = source.display().to_string();
    let pretend = sudo.map_or_else(|| "none".to_string(), |(uid, gid)| format!("{uid}:{gid}"));
    let path = case.fake_path();
    hostroot::spawn(
        "child_import_pretending",
        &[
            ("LODI_TEST_ROOT", &root),
            ("LODI_TEST_SOURCE", &source),
            ("LODI_TEST_PRETEND", &pretend),
            ("PATH", &path),
        ],
    )
    .wait_with_output()
    .expect("the import finishes")
}

fn events(output: &std::process::Output) -> Vec<String> {
    out(output)
        .lines()
        // libtest may print its own `test NAME ... ` before the first line on the same line.
        .filter_map(|line| line.split_once("EVENT ").map(|(_, event)| event))
        .map(ToString::to_string)
        .collect()
}

/// D6. An import into `SOURCE` run as root under sudo reads as root, writes the baseline into
/// the lock, and only then becomes the owner of `SOURCE` before it writes anything under it, so
/// that the files are born that user's.
#[test]
fn d6_a_sudo_import_writes_the_lock_then_becomes_the_owner_before_writing_the_host() {
    let case = Case::new("d6-sudo", Machine::debian().with(Pkg::new("jq")));
    case.root.write("etc/hostname", "box\n");
    fs::create_dir_all(case.root.path("hosts")).unwrap();
    let (uid, gid) = ids(&case.root);
    let import = import_pretending(&case, &case.root.path("hosts"), Some((uid, gid)));
    assert_eq!(code(&import), Some(0), "{}", story(&import));
    assert_eq!(
        events(&import),
        [format!(
            "become {uid}:{gid} lock=true host-dir=false manifest=false"
        )],
        "{}",
        story(&import)
    );
    assert!(case.root.exists("hosts/box/host.toml"));
    assert!(
        !out(&import).contains("wrote no record"),
        "{}",
        story(&import)
    );
    assert_eq!(case.lock()["source"], "/hosts/box");
    use std::os::unix::fs::MetadataExt;
    for made in ["hosts/box", "hosts/box/host.toml"] {
        let meta = fs::metadata(case.root.path(made)).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (uid, gid), "{made}");
    }
}

/// D6. Root without `SUDO_UID` trusts root only, so a directory of the user's is refused before
/// anything is read, and nobody is become.
#[test]
fn d6_root_without_sudo_trusts_only_root() {
    let case = Case::new("d6-root-only", Machine::debian());
    case.root.write("etc/hostname", "box\n");
    fs::create_dir_all(case.root.path("hosts")).unwrap();
    let import = import_pretending(&case, &case.root.path("hosts"), None);
    assert_eq!(code(&import), Some(3), "{}", story(&import));
    assert!(err(&import).contains("E_PATH_ESCAPE"), "{}", story(&import));
    assert!(events(&import).is_empty(), "{}", story(&import));
    assert!(!case.root.exists("hosts/box"));
    assert!(!case.has_lock());
}

// ------------------------------------------------------------ D7: capture reachability ---

/// D7. The import copies no changed file, whatever the directory above it (si-1): a 0644 file
/// inside a 0750 directory and one in an open directory are both `NOT CAPTURED` lines naming
/// the `[etc."PATH"]` that declares them, and neither one's bytes leaves the machine.
#[test]
fn d7_no_changed_file_is_copied_whatever_its_directory() {
    let machine = Machine::debian().with(
        Pkg::new("app")
            .conffile("/etc/app/private/secret.conf")
            .conffile("/etc/app/open.conf"),
    );
    let case = Case::new("d7-reach", machine);
    let (uid, gid) = ids(&case.root);
    case.root
        .write(
            "etc/passwd",
            &format!(
                "root:x:0:0:root:/root:/bin/sh\nlodi-d7:x:{uid}:{gid}::/nonexistent:/bin/sh\n"
            ),
        )
        .write("etc/group", &format!("root:x:0:\nlodi-d7:x:{gid}:\n"))
        .write("etc/app/private/secret.conf", "reach me not\n")
        .write("etc/app/open.conf", "open\n")
        .chmod("etc/app/private/secret.conf", 0o644)
        .chmod("etc/app/open.conf", 0o644)
        .chmod("etc/app/private", 0o750);
    let import = case.import();
    assert_eq!(code(&import), Some(0), "{}", story(&import));
    let manifest = case.manifest();
    assert!(!case.root.exists("etc/lodi/files"), "{manifest}");
    assert!(!manifest.contains("reach me not"), "{manifest}");
    let (_, block) = manifest
        .split_once("# NOT CAPTURED")
        .unwrap_or_else(|| panic!("no NOT CAPTURED block: {manifest}"));
    for (path, table) in [
        ("/etc/app/open.conf", "[etc.\"app/open.conf\"]"),
        (
            "/etc/app/private/secret.conf",
            "[etc.\"app/private/secret.conf\"]",
        ),
    ] {
        assert!(
            block.contains(&format!("#   {path}\n#     declare its text as {table}\n")),
            "{block}"
        );
        assert!(err(&import).contains(path), "{}", story(&import));
    }
    let _ = fs::set_permissions(
        case.root.path("etc/app/private"),
        fs::Permissions::from_mode(0o755),
    );
}

// ------------------------------------------ W3 on a machine whose index was never fetched ---

/// The W3 guest row as the Debian and Ubuntu guests ran it (LD-379, the supervisor's guest run):
/// a machine whose apt index has never been fetched — a fresh cloud image — is imported into a
/// directory of hosts under sudo, the owner adds a package that index does not know yet, and the
/// apply refreshes the index as its own first action and installs the package; a second apply
/// has nothing to do. The imported host says `packages = "exact"`, whose removal decision asks
/// apt to simulate the change: the plan must not ask apt about a name it carries past the
/// refresh, which apt cannot locate until the refresh has run.
#[test]
fn w3_an_imported_host_installs_a_package_its_never_fetched_index_did_not_know() {
    let machine = Machine::debian()
        .with(Pkg::new("jq"))
        .offering_once_fetched(Pkg::new("sl"));
    let case = Case::new("w3-unfetched", machine);
    case.root.write("etc/hostname", "box\n");
    fs::remove_file(case.root.path("var/lib/apt/lists/fake_Packages")).unwrap();
    fs::create_dir_all(case.root.path("hosts")).unwrap();
    let (uid, gid) = ids(&case.root);
    let import = import_pretending(&case, &case.root.path("hosts"), Some((uid, gid)));
    assert_eq!(code(&import), Some(0), "{}", story(&import));
    let text = case.root.read("hosts/box/host.toml");
    assert!(text.contains("packages = \"exact\""), "{text}");
    case.root.write(
        "hosts/box/host.toml",
        &text.replacen("common = [\n", "common = [\n  \"sl\",\n", 1),
    );

    // The import wrote `[host] snapshot`, which is live since M-Pin (D-A, LD-395): the package
    // the never-fetched index did not know comes from the dated archive at that instant, read
    // through the private source set that the apply refreshes before the transaction.
    let plan = case.plan_with(&[&hosts(&case)]);
    assert_eq!(code(&plan), Some(0), "{}", story(&plan));
    assert!(
        out(&plan).contains("= index (private: the transaction installs from the dated archive at")
            && out(&plan).contains("+ package sl"),
        "{}",
        story(&plan)
    );

    let apply = case.apply(&[&hosts(&case)]);
    assert_eq!(code(&apply), Some(0), "{}", story(&apply));
    assert!(case.installed().contains("sl"), "{}", story(&apply));
    let log = case.log();
    let update = log
        .iter()
        .position(|line| line.starts_with("apt-get update"));
    let install = log.iter().position(|line| {
        line.starts_with("apt-get install") && !line.contains(" -s ") && line.ends_with("-- sl")
    });
    assert!(
        update.is_some() && install.is_some() && update < install,
        "the index was not refreshed before the install: {log:?}"
    );

    let again = case.apply(&[&hosts(&case)]);
    assert_eq!(code(&again), Some(0), "{}", story(&again));
    assert!(
        out(&again).trim_end().ends_with("nothing to do"),
        "{}",
        story(&again)
    );
}
