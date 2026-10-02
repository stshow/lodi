//! The six-step safety gate of the host scope, in order (M-0.6 T-1).
//!
//! **No test in this file, and no test anywhere in this repository, runs host code against `/`.**
//! The package's own must-not list forbids it outright, so the refusal that `/` would produce is
//! proven where it is decided — in the arming check and in the hint it builds — and never by
//! pointing the product at this machine. `python3 -B scripts/check-host-safety.py` is a gate
//! step that fails when a committed script or test acquires a way around that.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use lodi::hostscope::safety::{self, Gate, Operation};

use hostroot::{Root, ids};

#[track_caller]
fn refuses_both_ways(root: &Root, code: &str, status: u8) {
    for (what, result) in [
        ("plan", lodi::hostscope::plan(&root.options())),
        ("apply", lodi::hostscope::apply(&root.options())),
    ] {
        let error = result
            .err()
            .unwrap_or_else(|| panic!("{what} should refuse"));
        assert_eq!(error.codes(), vec![code], "{what}: {error}");
        assert_eq!(error.exit_status(), status, "{what}: {error}");
        assert!(
            error.diagnostics[0]
                .notes
                .iter()
                .any(|note| note.starts_with("hint: ")),
            "{what} refused without a hint: {error}"
        );
    }
}

/// Step 2. The marker is checked before anything else below the root is opened, and the proof
/// is that the manifest beside it cannot be read at all: a refusal that came from the manifest
/// could not be produced here.
#[test]
fn an_unarmed_root_is_refused_before_the_manifest_is_opened() {
    let root = Root::new("safety-unarmed");
    root.debian()
        .write("etc/lodi/host.toml", "[host]\nversion = \"1\"\n");
    std::fs::set_permissions(
        root.path("etc/lodi/host.toml"),
        std::fs::Permissions::from_mode(0o000),
    )
    .expect("an unreadable manifest");
    assert!(
        safety::current_euid() == 0 || std::fs::read(root.path("etc/lodi/host.toml")).is_err(),
        "the manifest must really be unreadable for this proof to mean anything"
    );

    refuses_both_ways(&root, "E_HOST_NOT_ARMED", 9);

    let error = lodi::hostscope::plan(&root.options()).unwrap_err();
    assert!(
        error.to_string().contains(lodi::marker::MARKER),
        "the refusal names the marker: {error}"
    );
    assert!(
        !error.to_string().contains("sudo lodi"), // check-host-safety: refusal
        "a --root refusal never tells anyone to use sudo: {error}"
    );
    assert!(
        !root.exists("var/lib/lodi"),
        "an unarmed root is refused before any state directory is made"
    );
    assert!(!root.exists("etc/lodi/host.lock"));

    // Restore the mode so that the scratch root can be removed.
    std::fs::set_permissions(
        root.path("etc/lodi/host.toml"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("a removable manifest");
}

#[test]
fn a_marker_that_is_not_a_regular_file_is_not_an_armed_root() {
    let root = Root::new("safety-symlink");
    root.debian().write("etc/lodi/host.toml", "");
    std::fs::create_dir_all(root.path("etc/lodi")).expect("the directory");
    std::fs::write(root.path("etc/lodi/elsewhere"), "").expect("the target");
    std::os::unix::fs::symlink("elsewhere", root.path(lodi::marker::MARKER)).expect("the symlink");
    refuses_both_ways(&root, "E_HOST_NOT_ARMED", 9);

    std::fs::remove_file(root.path(lodi::marker::MARKER)).expect("the symlink goes");
    std::fs::create_dir(root.path(lodi::marker::MARKER)).expect("a directory marker");
    refuses_both_ways(&root, "E_HOST_NOT_ARMED", 9);
}

/// Step 3. A distribution this build does not manage stops the command; it never guesses.
#[test]
fn a_distribution_this_build_does_not_manage_is_refused() {
    let root = Root::new("safety-unmanaged");
    root.may_manage()
        .write("etc/os-release", "ID=nixos\nVERSION_ID=\"25.11\"\n")
        .write("etc/lodi/host.toml", "");
    refuses_both_ways(&root, "E_UNSUPPORTED", 3);
    let error = lodi::hostscope::plan(&root.options()).unwrap_err();
    assert!(
        error.to_string().contains("debian, ubuntu, arch"),
        "{error}"
    );
}

/// Step 3, the manifest's own assertion. `[host] distro` disagreeing with the root is a
/// mismatch, so that a Debian manifest cannot be applied to an Arch machine by accident.
#[test]
fn a_declared_distro_that_disagrees_with_the_root_is_a_mismatch() {
    let root = Root::new("safety-mismatch");
    root.debian_with("[host]\ndistro = \"ubuntu\"\n");
    refuses_both_ways(&root, "E_HOST_MISMATCH", 3);
}

/// Step 5. The package manager is found by bare name through `PATH`; a missing one stops the
/// command with the code `lodi develop` already uses for a runtime that is not installed.
#[test]
fn a_root_whose_package_manager_is_not_on_path_stops_at_no_runtime() {
    let path = std::env::var_os("PATH").unwrap_or_default();
    if safety::which("pacman", &path).is_some() {
        // This machine has pacman; the case it proves cannot be produced here honestly.
        return;
    }
    let root = Root::new("safety-noruntime");
    root.may_manage().arch().write("etc/lodi/host.toml", "");
    refuses_both_ways(&root, "E_NO_RUNTIME", 7);
    let error = lodi::hostscope::plan(&root.options()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("lodi installs no package manager"),
        "{error}"
    );
}

/// Step 6. One apply at a time, and the second never waits.
#[test]
fn a_second_apply_while_one_holds_the_lock_is_refused() {
    let root = Root::new("safety-busy");
    let (uid, gid) = ids(&root);
    root.debian_with(&format!(
        "[files.\"/etc/x\"]\ncontent = \"x\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
    ));
    let mut held = Gate::open(&root.options(), Operation::Apply).expect("the first apply's gate");
    held.lock_for_apply().expect("the first apply's lock");
    let error = lodi::hostscope::apply(&root.options()).expect_err("the second apply");
    assert_eq!(error.codes(), vec!["E_SYSTEM_BUSY"]);
    assert_eq!(error.exit_status(), 9);
    drop(held);
    // A plan takes no lock, so it is never refused for this reason.
    lodi::hostscope::plan(&root.options()).expect("a plan takes no lock");
}

/// Step 4, under a `--root`: the invoking user's own privilege bounds the actions, and an
/// `owner` this process cannot set stops the apply **before** anything is written.
#[test]
fn an_owner_this_process_cannot_set_is_refused_before_any_mutation() {
    if safety::current_euid() == 0 {
        // Running the suite as root would make this case unprovable, and a real `chown` to
        // root only ever happens inside a disposable guest.
        return;
    }
    let root = Root::new("safety-needroot");
    root.debian_with("[files.\"/etc/x.conf\"]\ncontent = \"a\\n\"\nowner = \"root\"\n");
    // The plan is fine: `root` resolves, because the root carries its own passwd file.
    root.write("etc/passwd", "root:x:0:0:root:/root:/bin/sh\n")
        .write("etc/group", "root:x:0:\n");
    let error = lodi::hostscope::apply(&root.options()).expect_err("an unprivileged apply");
    assert_eq!(error.codes(), vec!["E_NEED_ROOT"]);
    assert_eq!(error.exit_status(), 9);
    assert!(
        !error.to_string().contains("sudo lodi host apply"), // check-host-safety: refusal
        "under a --root, lodi never tells anyone to run it as root: {error}"
    );
    assert!(!root.exists("etc/x.conf"), "nothing was written");
    assert!(
        !root.path("var/lib/lodi/host/journal").exists(),
        "no journal is written for an apply that cannot start"
    );
}

/// An owner the root's own `/etc/passwd` does not name is a type error, not a silent fallback.
#[test]
fn an_owner_the_root_does_not_know_is_refused() {
    let root = Root::new("safety-unknownowner");
    root.debian_with("[files.\"/etc/x.conf\"]\ncontent = \"a\\n\"\nowner = \"nobodyatall\"\n");
    let error = lodi::hostscope::plan(&root.options()).expect_err("an unknown owner");
    assert_eq!(error.codes(), vec!["E_APPLY"]);
}

/// The manifest is never opened before the gate has passed, and a root that passes the gate
/// reads its identity from the root itself, not from this machine.
#[test]
fn the_context_comes_from_the_root_and_never_from_this_machine() {
    let root = Root::new("safety-context");
    let (uid, gid) = ids(&root);
    root.debian_with(&format!(
        "[files.\"/etc/id.conf\"]\ncontent = \"${{host.distro}} ${{host.release}} \
         ${{host.codename}}\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
    ));
    lodi::hostscope::apply(&root.options()).expect("the apply");
    assert_eq!(root.read("etc/id.conf"), "debian 12 bookworm\n");
}

// ------------------------------------------------------------ T-3: the safe command is reachable

/// The safe command is reachable: `switch` names its journal flag, and no help claims a
/// rollback the host part does not have.
#[test]
fn help_names_switch_with_its_journal_flag_and_no_rollback() {
    let mut text = String::new();
    for subject in [&["--help"][..], &["help", "switch"]] {
        let help = Command::new(env!("CARGO_BIN_EXE_lodi"))
            .args(subject)
            .output()
            .expect("the lodi binary runs");
        assert!(help.status.success());
        text.push_str(&String::from_utf8(help.stdout).expect("utf-8"));
    }
    assert!(text.contains("lodi switch [PATH | URL]"), "{text}");
    assert!(text.contains("--resolved ID"), "{text}");
    assert!(!text.contains("lodi host"), "{text}");
    for claim in ["rollback", "generation", "undo"] {
        assert!(!text.to_lowercase().contains(claim), "{text}");
    }
}

/// The host state directory is trusted only if nobody else can write it: an existing state
/// root, host child or journal directory that is group- or world-writable is refused by plan
/// and apply before the apply lock is taken or any journal in it is read — the unreadable
/// journal planted there is never parsed — and nothing is written.
#[test]
fn a_writable_host_state_directory_is_refused_before_the_lock_or_a_journal_is_read() {
    for (index, (dir, mode)) in [
        ("var/lib/lodi", 0o777),
        ("var/lib/lodi/host", 0o775),
        ("var/lib/lodi/host/journal", 0o757),
    ]
    .into_iter()
    .enumerate()
    {
        let root = Root::new(&format!("safety-state-dir-{index}"));
        let (uid, gid) = ids(&root);
        root.debian_with(&format!(
            "[files.\"/etc/state-test.conf\"]\ncontent = \"x\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
        ));
        root.write(
            "var/lib/lodi/host/journal/20260101T000000Z-0000.json",
            "not a journal\n",
        );
        root.chmod(dir, mode);
        refuses_both_ways(&root, "E_PATH_ESCAPE", 3);
        let error = lodi::hostscope::apply(&root.options()).unwrap_err();
        assert!(
            error.to_string().contains("is not a trusted directory"),
            "{dir}: {error}"
        );
        assert!(
            !root.exists("var/lib/lodi/host/.lock"),
            "{dir}: the lock was not taken"
        );
        assert!(
            !root.exists("etc/state-test.conf"),
            "{dir}: nothing was written"
        );
    }
}

/// The host state path is refused, not skipped, where it cannot be inspected: a state directory
/// or journal directory of mode 0000 under a `--root` could hide a pending journal, so plan and
/// apply both refuse it before the lock is taken, and apply leaves its mode as found. Root reads
/// through any mode, so the case does not exist for it and the test says so and stops.
#[test]
fn a_state_directory_that_cannot_be_inspected_is_refused_not_taken_as_empty() {
    // SAFETY: geteuid cannot fail and takes no arguments.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root is not stopped by a mode of 0000");
        return;
    }
    for (index, dir) in ["var/lib/lodi/host", "var/lib/lodi/host/journal"]
        .into_iter()
        .enumerate()
    {
        let root = Root::new(&format!("safety-state-dir-000-{index}"));
        let (uid, gid) = ids(&root);
        root.debian_with(&format!(
            "[files.\"/etc/state-test.conf\"]\ncontent = \"x\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
        ));
        root.write(
            "var/lib/lodi/host/journal/20260101T000000Z-0000.json",
            "a pending journal the scan must not miss\n",
        );
        for rel in [
            "var/lib/lodi",
            "var/lib/lodi/host",
            "var/lib/lodi/host/journal",
        ] {
            root.chmod(rel, 0o700);
        }
        root.chmod(dir, 0o000);
        // The walk of the state path itself refuses a component it cannot look into; the
        // listing below it refuses a journal directory it cannot read.
        let walk =
            lodi::hostscope::files::check_state_path(&root.dir, "/var/lib/lodi/host/journal");
        if dir.ends_with("journal") {
            walk.expect("the journal directory itself can be inspected");
        } else {
            assert_eq!(walk.expect_err(dir).code, "E_PATH_ESCAPE");
        }
        refuses_both_ways(&root, "E_PATH_ESCAPE", 3);
        let error = lodi::hostscope::apply(&root.options()).unwrap_err();
        assert!(
            error.to_string().contains("could not be inspected"),
            "{dir}: {error}"
        );
        let mode = std::fs::symlink_metadata(root.path(dir))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o000, "{dir}: apply left the mode as found");
        root.chmod(dir, 0o700);
        assert!(
            !root.exists("var/lib/lodi/host/.lock"),
            "{dir}: the lock was not taken"
        );
        assert!(
            !root.exists("etc/state-test.conf"),
            "{dir}: nothing was written"
        );
    }
}

// ------------------------------------------- LD-357: a fixed place, and files judged like the marker

/// A manifest with one file action, so that "nothing was changed" has something to look at.
fn with_one_file(root: &Root) -> String {
    let (uid, gid) = ids(root);
    format!(
        "[host]\nversion = \"1\"\n\n[files.\"/etc/a.conf\"]\ncontent = \"alpha\\n\"\n\
         owner = \"{uid}\"\ngroup = \"{gid}\"\n"
    )
}

#[track_caller]
fn nothing_was_changed(root: &Root) {
    assert!(!root.exists("etc/a.conf"), "a file action ran");
    assert!(!root.exists("var/lib/lodi"), "host state was created");
}

/// Rule (LD-357): on the running system's `/`, the environment cannot choose the program root
/// runs. This test binary's `PATH` starts with a directory of its own `apt-get` and
/// `dpkg-query`; resolving for a scratch root finds them — the seam is real — and resolving for
/// `/` never does, whatever `PATH` says. No host command is run against `/`: this is the
/// resolution itself, as the gate calls it.
#[test]
fn on_the_system_root_the_environment_cannot_choose_the_package_manager() {
    let shims = hostroot::shims();
    let euid = safety::current_euid();
    let path = std::env::var_os("PATH").unwrap_or_default();
    assert!(
        std::env::split_paths(&path).next() == Some(shims.clone()),
        "the shim directory leads PATH, so the environment is steering if anything can"
    );
    assert_eq!(
        safety::resolve_program("apt-get", false, euid),
        Ok(Some(shims.join("apt-get"))),
        "under a --root the caller's PATH is the seam"
    );
    assert_eq!(
        safety::search_path(true, Some(path.as_os_str())),
        std::ffi::OsString::from(safety::FIXED_PATH)
    );
    for name in ["apt-get", "dpkg-query", "pacman"] {
        // Absent or refused on this machine is fine; found anywhere but the fixed path is not.
        if let Ok(Some(found)) = safety::resolve_program(name, true, euid) {
            assert!(
                !found.starts_with(&shims),
                "{name} resolved to {}",
                found.display()
            );
            let dir = found.parent().expect("a directory").to_path_buf();
            assert!(
                std::env::split_paths(safety::FIXED_PATH).any(|d| d == dir),
                "{name} resolved outside the fixed path: {}",
                found.display()
            );
        }
    }
    match safety::find_package_manager(safety::Distro::Debian, true, euid, Operation::Plan) {
        Ok(pm) => {
            for (name, found) in &pm.programs {
                assert!(
                    !found.starts_with(&shims),
                    "{name} resolved to {}",
                    found.display()
                );
            }
        }
        Err(error) => {
            assert_eq!(error.code, "E_NO_RUNTIME");
            assert!(
                error.message.contains(safety::FIXED_PATH),
                "{}",
                error.message
            );
        }
    }
}

/// Rule (LD-357): a package-manager program that someone other than its owner could write is
/// never run, and the search does not go on past it. The real binary previews a switch of a
/// scratch root with a `PATH` whose first directory's `apt-get` is group- or world-writable,
/// ahead of the fake machine's own.
#[test]
fn a_package_manager_writable_by_others_is_refused() {
    for (mode, name) in [(0o775, "group"), (0o757, "world")] {
        let case = fakehost::Case::new(&format!("safety-pm-{name}"), fakehost::Machine::debian());
        case.set_manifest(&with_one_file(&case.root));
        let bin = case.root.path(".bin");
        std::fs::create_dir_all(&bin).expect("a bin directory");
        {
            let _writing = fakehost::writing();
            for program in ["apt-get", "dpkg-query"] {
                let path = bin.join(program);
                std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("a program");
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("its mode");
            }
            std::fs::set_permissions(bin.join("apt-get"), std::fs::Permissions::from_mode(mode))
                .expect("a writable program");
        }
        let path = format!("{}:{}", bin.display(), case.elevating_path());
        let output = case.verb_env("plan", &[], &[("PATH", &path)]);
        let stderr = fakehost::err(&output);
        assert_eq!(output.status.code(), Some(7), "{name}: {stderr}");
        assert!(stderr.contains("E_NO_RUNTIME"), "{name}: {stderr}");
        assert!(
            stderr.contains("group- or world-writable"),
            "{name}: {stderr}"
        );
        nothing_was_changed(&case.root);
    }
}

/// Rule (LD-357): the host manifest is read only as a regular file, never through a symbolic
/// link, and the refusal comes before anything is read from the machine or changed on it.
#[test]
fn a_symlinked_manifest_is_refused_before_any_action() {
    let root = Root::new("safety-manifest-link");
    let manifest = with_one_file(&root);
    root.may_manage()
        .debian()
        .write("srv/elsewhere.toml", &manifest);
    std::os::unix::fs::symlink(
        root.path("srv/elsewhere.toml"),
        root.path("etc/lodi/host.toml"),
    )
    .expect("a symlinked manifest");
    refuses_both_ways(&root, "E_PATH_ESCAPE", 3);
    let error = lodi::hostscope::plan(&root.options()).unwrap_err();
    assert!(error.to_string().contains("symbolic link"), "{error}");
    nothing_was_changed(&root);
}

/// Rule (LD-357): `etc/lodi` — and the root and `etc` above it — is trusted only if nobody but
/// its owner can write it, and a symbolic link there is not followed. Each is refused before the
/// manifest is read or anything is changed.
#[test]
fn an_etc_lodi_writable_by_others_or_linked_is_refused_before_any_action() {
    for (dir, mode) in [
        ("etc/lodi", 0o775),
        ("etc/lodi", 0o757),
        ("etc/lodi", 0o1777),
        ("etc", 0o775),
    ] {
        let root = Root::new(&format!("safety-etc-lodi-{mode:o}"));
        root.debian_with(&with_one_file(&root)).chmod(dir, mode);
        refuses_both_ways(&root, "E_PATH_ESCAPE", 3);
        let error = lodi::hostscope::plan(&root.options()).unwrap_err();
        assert!(
            error.to_string().contains("not a trusted directory"),
            "{dir} {mode:o}: {error}"
        );
        nothing_was_changed(&root);
        root.chmod(dir, 0o755);
    }
    let root = Root::new("safety-etc-lodi-link");
    let manifest = with_one_file(&root);
    root.debian()
        .write("srv/lodi/may-manage", "")
        .write("srv/lodi/host.toml", &manifest);
    std::os::unix::fs::symlink(root.path("srv/lodi"), root.path("etc/lodi")).expect("a link");
    refuses_both_ways(&root, "E_PATH_ESCAPE", 3);
    nothing_was_changed(&root);
}

/// Rule (LD-357): the manifest and the host lock themselves are judged by the marker's owner and
/// mode rule: writable by group or others is refused before any action.
#[test]
fn a_manifest_or_lock_writable_by_others_is_refused_before_any_action() {
    for (file, mode) in [
        ("etc/lodi/host.toml", 0o664),
        ("etc/lodi/host.toml", 0o646),
        ("etc/lodi/host.lock", 0o664),
    ] {
        let root = Root::new(&format!("safety-config-{mode:o}"));
        root.debian_with(&with_one_file(&root));
        if file.ends_with(".lock") {
            root.write(file, "{}\n");
        }
        root.chmod(file, mode);
        refuses_both_ways(&root, "E_PATH_ESCAPE", 3);
        let error = lodi::hostscope::plan(&root.options()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&root.path(file).display().to_string()),
            "{file}: {error}"
        );
        nothing_was_changed(&root);
    }
}

/// Rule: a `--root` that does not exist is reported as missing (`E_STORE_IO`, exit 6) by every
/// 2.0 command that takes one, before anything is read: never as a root that names no hostname,
/// and the root is not created.
#[test]
fn a_root_that_does_not_exist_is_missing_not_unarmed() {
    let root = Root::new("safety-missing-root");
    let missing = root.path("not-there");
    let home = root.path("scratch-home");
    std::fs::create_dir_all(&home).expect("a scratch home");
    let commands: [&[&str]; 6] = [
        &["switch", "--host", "--dry-run"],
        &["switch"],
        &["import", "--yes"],
        &["update"],
        &["pin", "--all"],
        &["unpin", "--all"],
    ];
    for command in commands {
        let output = Command::new(env!("CARGO_BIN_EXE_lodi"))
            .args(command)
            .arg("--root")
            .arg(&missing)
            .env_clear()
            .env("HOME", &home)
            .env("PATH", root.path(".no-bin"))
            .env("LODI_HOST_REQUIRE_ROOT", "1")
            .output()
            .expect("lodi runs");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(6), "{command:?}: {stderr}");
        assert!(
            stderr.contains("lodi: error E_STORE_IO"),
            "{command:?}: {stderr}"
        );
        assert!(stderr.contains("does not exist"), "{command:?}: {stderr}");
        assert!(!stderr.contains("hostname"), "{command:?}: {stderr}");
        assert!(!missing.exists(), "{command:?} created the root");
    }
}

/// `verb` ([`fakehost::command_line`]) on `case`'s scratch root, on a terminal, with `bin`
/// after the stub elevators as the whole `PATH`: an import asks an elevator first, and its root
/// run finds the same `PATH`.
fn on_path(case: &fakehost::Case, verb: &str, bin: &Path) -> std::process::Output {
    let path = format!(
        "{}:{}",
        case.base().join("elevators").display(),
        bin.display()
    );
    case.verb_env(verb, &[], &[("PATH", path.as_str())])
}

/// Rule: when a program the package backend needs is not on `PATH`, the hint names the 2.0
/// command that was run, and no 1.x verb (LD-523). The real binary runs `switch --host
/// --dry-run` and `import --yes` on a scratch Debian root whose `PATH` holds only `apt-get` and
/// `dpkg-query`, which the gate requires, so the refusal comes from the backend at the moment it
/// reaches for one of the others.
#[test]
fn a_missing_package_program_names_the_command_that_was_run() {
    for (verb, named) in [("plan", "lodi switch"), ("import", "lodi import")] {
        let case =
            fakehost::Case::new(&format!("safety-norun-{verb}"), fakehost::Machine::debian());
        if verb == "plan" {
            case.set_manifest("[host]\nversion = \"1\"\n\n[packages]\ncommon = [\"bc\"]\n");
        }
        let bin = case.root.path(".bin");
        std::fs::create_dir_all(&bin).expect("a bin directory");
        for program in ["apt-get", "dpkg-query"] {
            let path = bin.join(program);
            std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("a program");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("its mode");
        }
        let output = on_path(&case, verb, &bin);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(7), "{verb}: {stderr}");
        assert!(
            stderr.contains("lodi: error E_NO_RUNTIME"),
            "{verb}: {stderr}"
        );
        assert!(stderr.contains("is not on PATH"), "{verb}: {stderr}");
        assert!(
            stderr.contains(&format!("run `{named}`")),
            "{verb}: {stderr}"
        );
        assert!(!stderr.contains("lodi host"), "{verb}: {stderr}");
        assert!(
            !case.root.exists("var/lib/lodi"),
            "{verb} created host state"
        );
        assert_eq!(
            case.elevator_calls().len(),
            usize::from(verb == "import"),
            "{verb}"
        );
    }
}

/// Rule: on an Arch root whose `PATH` has no `pacman`, the gate's step 5 refuses before any
/// backend exists, and its hint names the 2.0 command that was run as the backend's own would,
/// and no 1.x verb (LD-523). The real binary runs `switch --host --dry-run`, `import --yes` and
/// `switch --host` on a scratch Arch root with a one-file config and an empty `PATH`; each stops
/// at `E_NO_RUNTIME` with its own command, and nothing is changed.
#[test]
fn a_missing_pacman_names_the_command_that_was_run() {
    let commands = [
        ("plan", "lodi switch"),
        ("import", "lodi import"),
        ("apply", "lodi switch"),
    ];
    for (verb, named) in commands {
        let case = fakehost::Case::new(
            &format!("safety-nopacman-{verb}"),
            fakehost::Machine::arch(),
        );
        if verb != "import" {
            case.set_manifest(&with_one_file(&case.root));
        }
        let bin = case.root.path(".bin");
        std::fs::create_dir_all(&bin).expect("an empty bin directory");
        let output = on_path(&case, verb, &bin);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(7), "{verb}: {stderr}");
        assert!(
            stderr.contains("lodi: error E_NO_RUNTIME"),
            "{verb}: {stderr}"
        );
        assert!(
            stderr.contains("`pacman` is not on PATH"),
            "{verb}: {stderr}"
        );
        assert!(
            stderr.contains(&format!("run `{named}` on a arch machine")),
            "{verb}: {stderr}"
        );
        assert!(!stderr.contains("lodi host"), "{verb}: {stderr}");
        nothing_was_changed(&case.root);
        assert_eq!(
            case.elevator_calls().len(),
            usize::from(verb == "import"),
            "{verb}"
        );
    }
}
