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

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use lodi::hostscope::source;

use fakehost::{Case, Machine, Pkg, err, nothing, out, story};
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

// ---------------------------------------------------------------------- D2: selection ---

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

/// D3, refusals on the real binary: each breaks one rule of the config's trust walk, is
/// `E_PATH_ESCAPE` at exit 3 naming the path, and the same root plans once the rule holds again.
/// The config is the scratch `~/.config/lodi`, its file source `files/etc/w3-src.conf` (#699).
#[test]
fn d3_every_broken_rule_is_path_escape_naming_the_path() {
    type Break = fn(&Case);
    type Mend = fn(&Case);
    const ABOVE: &str = "home/.config";
    const CONFIG: &str = "home/.config/lodi";
    const HOST: &str = "home/.config/lodi/host.toml";
    const FILES: &str = "home/.config/lodi/files";
    const SRC: &str = "home/.config/lodi/files/etc/w3-src.conf";
    let cases: &[(&str, &str, Break, Mend)] = &[
        (
            "config folder g+w",
            CONFIG,
            |c| {
                c.root.chmod(CONFIG, 0o775);
            },
            |c| {
                c.root.chmod(CONFIG, 0o755);
            },
        ),
        (
            "the folder above it o+w",
            ABOVE,
            |c| {
                c.root.chmod(ABOVE, 0o757);
            },
            |c| {
                c.root.chmod(ABOVE, 0o755);
            },
        ),
        (
            "the folder above it sticky and world-writable",
            ABOVE,
            |c| {
                c.root.chmod(ABOVE, 0o1777);
            },
            |c| {
                c.root.chmod(ABOVE, 0o755);
            },
        ),
        (
            "host.toml g+w",
            HOST,
            |c| {
                c.root.chmod(HOST, 0o664);
            },
            |c| {
                c.root.chmod(HOST, 0o644);
            },
        ),
        (
            "a source o+w",
            SRC,
            |c| {
                c.root.chmod(SRC, 0o646);
            },
            |c| {
                c.root.chmod(SRC, 0o644);
            },
        ),
        (
            "files/ g+w",
            FILES,
            |c| {
                c.root.chmod(FILES, 0o775);
            },
            |c| {
                c.root.chmod(FILES, 0o755);
            },
        ),
        (
            "the config folder a symbolic link",
            CONFIG,
            |c| {
                fs::rename(c.root.path(CONFIG), c.root.path(&format!("{ABOVE}/real"))).unwrap();
                std::os::unix::fs::symlink("real", c.root.path(CONFIG)).unwrap();
            },
            |c| {
                fs::remove_file(c.root.path(CONFIG)).unwrap();
                fs::rename(c.root.path(&format!("{ABOVE}/real")), c.root.path(CONFIG)).unwrap();
            },
        ),
        (
            "host.toml a symbolic link",
            HOST,
            |c| {
                fs::rename(
                    c.root.path(HOST),
                    c.root.path(&format!("{CONFIG}/real.toml")),
                )
                .unwrap();
                std::os::unix::fs::symlink("real.toml", c.root.path(HOST)).unwrap();
            },
            |c| {
                fs::remove_file(c.root.path(HOST)).unwrap();
                fs::rename(
                    c.root.path(&format!("{CONFIG}/real.toml")),
                    c.root.path(HOST),
                )
                .unwrap();
            },
        ),
        (
            "a source a symbolic link",
            SRC,
            |c| {
                let at = c.root.path(SRC);
                fs::rename(&at, c.root.path(&format!("{FILES}/etc/real.conf"))).unwrap();
                std::os::unix::fs::symlink("real.conf", &at).unwrap();
            },
            |c| {
                let at = c.root.path(SRC);
                fs::remove_file(&at).unwrap();
                fs::rename(c.root.path(&format!("{FILES}/etc/real.conf")), &at).unwrap();
            },
        ),
        (
            "a source a FIFO",
            SRC,
            |c| {
                let at = c.root.path(SRC);
                fs::rename(&at, c.root.path(&format!("{FILES}/etc/real.conf"))).unwrap();
                let status = std::process::Command::new("mkfifo")
                    .arg(&at)
                    .status()
                    .unwrap();
                assert!(status.success());
            },
            |c| {
                let at = c.root.path(SRC);
                fs::remove_file(&at).unwrap();
                fs::rename(c.root.path(&format!("{FILES}/etc/real.conf")), &at).unwrap();
            },
        ),
    ];
    let case = hosts_case("d3-refusals");
    let (uid, gid) = ids(&case.root);
    case.set_manifest(&format!(
        "[host]\ndistro = \"debian\"\n\n[files.\"/etc/w3-src.conf\"]\n\
         source = \"files/etc/w3-src.conf\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
    ));
    case.write_beside("files/etc/w3-src.conf", "from a source\n");
    let control = case.plan();
    assert_eq!(code(&control), Some(0), "the control: {}", story(&control));
    assert!(
        err(&control).contains("/etc/w3-src.conf"),
        "{}",
        story(&control)
    );

    for (what, path, broken, mended) in cases {
        broken(&case);
        let plan = case.plan();
        let said = err(&plan);
        assert_eq!(code(&plan), Some(3), "{what}: {}", story(&plan));
        assert!(said.contains("E_PATH_ESCAPE"), "{what}: {said}");
        assert!(
            said.contains(&case.root.path(path).display().to_string()),
            "{what}: the refusal does not name {path}: {said}"
        );
        let apply = case.apply(&[]);
        assert_eq!(code(&apply), Some(3), "{what}: {}", story(&apply));
        assert!(
            !case.root.exists("etc/w3-src.conf"),
            "{what}: the apply wrote"
        );
        mended(&case);
        let again = case.plan();
        assert_eq!(code(&again), Some(0), "{what}, mended: {}", story(&again));
    }
}

/// D3. Under `--root` the trust walk begins at the root, so a typed config outside it is
/// refused.
#[test]
fn d3_a_config_outside_the_root_is_refused() {
    let case = hosts_case("d3-outside");
    let outside = PathBuf::from(format!("{}-outside-config", case.root.dir.display()));
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(&outside).unwrap();
    fs::write(
        outside.join("host.toml"),
        manifest_with(&case.root, "/etc/w3-outside.conf", "outside"),
    )
    .unwrap();
    let plan = case.plan_with(&[&outside.display().to_string()]);
    let _ = fs::remove_dir_all(&outside);
    assert_eq!(code(&plan), Some(3), "{}", story(&plan));
    assert!(err(&plan).contains("E_PATH_ESCAPE"), "{}", story(&plan));
    assert!(!case.root.exists("etc/w3-outside.conf"));
}

/// One apply of `<root>/hosts/box`, held at its first open of `box`; while it is held, `hosts` is
/// swapped for a link to `evil` when `swap` is set.
fn held_source_apply(name: &str, swap: bool) -> (hostroot::Root, std::process::Output) {
    let root = hostroot::Root::new(name);
    root.may_manage().debian();
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
/// a machine whose apt index has never been fetched — a fresh cloud image — is imported, the
/// owner adds a package that index does not know yet, and the switch refreshes the index as its
/// own first action and installs the package; a second switch has nothing to do. The imported
/// host says `packages = "exact"`, whose removal decision asks apt to simulate the change: the
/// plan must not ask apt about a name it carries past the refresh, which apt cannot locate
/// until the refresh has run.
#[test]
fn w3_an_imported_host_installs_a_package_its_never_fetched_index_did_not_know() {
    let machine = Machine::debian()
        .with(Pkg::new("jq"))
        .offering_once_fetched(Pkg::new("sl"));
    let case = Case::new("w3-unfetched", machine);
    case.root.write("etc/hostname", "box\n");
    fs::remove_file(case.root.path("var/lib/apt/lists/fake_Packages")).unwrap();
    let import = case.import();
    assert_eq!(code(&import), Some(0), "{}", story(&import));
    let text = case.manifest();
    assert!(text.contains("packages = \"exact\""), "{text}");
    case.set_manifest(&text.replacen("common = [\n", "common = [\n  \"sl\",\n", 1));

    let plan = case.plan();
    assert_eq!(code(&plan), Some(0), "{}", story(&plan));
    assert!(err(&plan).contains("+ package sl"), "{}", story(&plan));

    let apply = case.apply(&[]);
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

    let again = case.apply(&[]);
    assert!(nothing(&again), "{}", story(&again));
}
