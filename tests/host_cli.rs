//! Reachable M-0.6 T-3 command surface. Every real host invocation uses a scratch `--root`,
//! except the deliberate unarmed `/` refusal required by the package contract.

#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::process::{Command, Output};

use hostroot::{Root, ids};

fn run(root: &Root, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(args)
        .arg("--root")
        .arg(&root.dir)
        .output()
        .expect("lodi runs")
}

#[test]
fn plan_and_apply_are_reachable_and_plan_is_read_only() {
    let root = Root::new("cli-reachable");
    let (uid, gid) = ids(&root);
    root.debian_with(&format!(
        "[files.\"/etc/cli.conf\"]\ncontent = \"ok\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
    ));
    let before = std::fs::read_dir(&root.dir).unwrap().count();
    // check-host-safety: refusal — `run` appends this scratch root as `--root`.
    let plan = run(&root, &["host", "plan"]);
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    assert!(String::from_utf8_lossy(&plan.stdout).contains("+ file /etc/cli.conf"));
    assert_eq!(std::fs::read_dir(&root.dir).unwrap().count(), before);
    assert!(!root.exists("var/lib/lodi"));

    // check-host-safety: refusal — `run` appends this scratch root as `--root`.
    let apply = run(&root, &["host", "apply"]);
    assert!(
        apply.status.success(),
        "{}",
        String::from_utf8_lossy(&apply.stderr)
    );
    assert_eq!(root.read("etc/cli.conf"), "ok\n");
    // check-host-safety: refusal — `run` appends this scratch root as `--root`.
    let plan = run(&root, &["host", "plan"]);
    assert!(plan.status.success());
    assert!(String::from_utf8_lossy(&plan.stdout).contains("nothing to do"));
}

#[test]
fn usage_errors_are_exit_two() {
    // A bare `host` is the scope's help since LD-360, not a usage error.
    for args in [
        vec!["host", "status"],
        vec!["host", "plan", "a", "b", "--root", "/not-opened"],
        vec!["host", "plan", "--host", "box", "--root", "/not-opened"],
        vec!["host", "apply", "--resolved", "--root", "/not-opened"],
        vec!["host", "plan", "--overwrite-drift", "--root", "/not-opened"],
        vec!["host", "plan", "--json", "--root", "/not-opened"],
        vec!["host", "arm", "--force", "--root", "/not-opened"],
        vec!["host", "arm", "extra", "--root", "/not-opened"],
        vec!["host", "disarm", "--root", "/not-opened"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_lodi"))
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn unarmed_scratch_and_system_roots_are_refused_without_writes() {
    let root = Root::new("cli-unarmed");
    root.debian().write("etc/lodi/host.toml", "");
    // check-host-safety: refusal — `run` appends this scratch root as `--root`.
    let output = run(&root, &["host", "plan"]);
    assert_eq!(output.status.code(), Some(9));
    assert!(String::from_utf8_lossy(&output.stderr).contains("E_HOST_NOT_ARMED"));
    assert!(!root.exists("var/lib/lodi"));

    // check-host-safety: refusal — this invocation proves the arming gate refuses `/` itself.
    let output = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["host", "plan", "--root", "/"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(9));
    assert!(String::from_utf8_lossy(&output.stderr).contains("E_HOST_NOT_ARMED"));
}

#[test]
fn warnings_go_to_standard_error() {
    let root = Root::new("cli-warning");
    let (uid, gid) = ids(&root);
    root.write("etc/x", "old\n").debian_with(&format!(
        "[files.\"/etc/x\"]\ncontent = \"new\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
    ));
    // check-host-safety: refusal — `run` appends this scratch root as `--root`.
    let output = run(&root, &["host", "apply"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("W_REPLACED_UNMANAGED"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("W_REPLACED_UNMANAGED"));
}

// ------------------------------------------------- `lodi host arm` (LD-320), under a --root

/// `lodi host arm --root R` as this unprivileged user, with the umask given. Every call here
/// names its scratch root; `/` is never armed or asked to be (`AGENTS.md` §8).
fn arm(root: &Root, umask: libc::mode_t) -> Output {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new(env!("CARGO_BIN_EXE_lodi"));
    command.args(["host", "arm", "--root"]).arg(&root.dir);
    // SAFETY: `umask` is async-signal-safe and touches only the child's own mask.
    unsafe {
        command.pre_exec(move || {
            libc::umask(umask);
            Ok(())
        });
    }
    command.output().expect("lodi runs")
}

fn marker_stat(root: &Root) -> (bool, u64, u32, u32, i64, i64) {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(root.path("etc/lodi/host-allowed")).expect("a marker");
    (
        meta.file_type().is_file(),
        meta.len(),
        meta.mode() & 0o7777,
        meta.uid(),
        meta.mtime(),
        meta.mtime_nsec(),
    )
}

#[test]
fn arm_creates_an_empty_0644_marker_that_the_gate_then_accepts() {
    let root = Root::new("cli-arm");
    let (uid, gid) = ids(&root);
    // Unarmed, with no `etc/lodi` at all: `arm` creates the directory as well.
    root.debian();
    assert!(!root.exists("etc/lodi"));
    // check-host-safety: refusal — `run` appends this scratch root as `--root`.
    assert_eq!(run(&root, &["host", "plan"]).status.code(), Some(9));

    // A umask that would strip every bit but the owner's: the marker is 0644 regardless.
    let output = arm(&root, 0o077);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let (regular, len, mode, owner, _, _) = marker_stat(&root);
    assert!(regular && len == 0, "the marker is an empty regular file");
    assert_eq!(mode, 0o644);
    assert_eq!(owner, uid, "owned by the invoking user");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.lines().count(), 1, "one sentence: {stdout}");
    assert!(
        stdout.contains("plan, apply and import may now act on it"),
        "{stdout}"
    );
    assert!(
        stdout.contains("delete ") && stdout.contains("to disarm it"),
        "{stdout}"
    );
    assert!(stdout.contains("etc/lodi/host-allowed"), "{stdout}");
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The marker it wrote is exactly the one the gate accepts.
    root.write(
        "etc/lodi/host.toml",
        &format!(
            "[files.\"/etc/armed.conf\"]\ncontent = \"ok\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
        ),
    );
    // check-host-safety: refusal — `run` appends this scratch root as `--root`.
    let plan = run(&root, &["host", "plan"]);
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    assert!(String::from_utf8_lossy(&plan.stdout).contains("+ file /etc/armed.conf"));
}

#[test]
fn a_second_arm_says_already_armed_and_leaves_the_marker_untouched() {
    let root = Root::new("cli-arm-twice");
    assert!(arm(&root, 0o022).status.success());
    let before = marker_stat(&root);
    // A minimum, not a wait: two mtimes kept apart, so a rewrite could not keep the same one.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let again = arm(&root, 0o022);
    assert_eq!(
        again.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    let stdout = String::from_utf8_lossy(&again.stdout);
    assert!(stdout.contains("is already armed"), "{stdout}");
    assert!(stdout.contains("to disarm it"), "{stdout}");
    assert_eq!(
        marker_stat(&root),
        before,
        "mode, owner and mtime are unchanged"
    );

    // A marker a human wrote, 0600 and not empty, is valid for the gate and left as it is.
    root.write("etc/lodi/host-allowed", "armed by hand\n")
        .chmod("etc/lodi/host-allowed", 0o600);
    let before = marker_stat(&root);
    let again = arm(&root, 0o022);
    assert_eq!(again.status.code(), Some(0));
    assert_eq!(marker_stat(&root), before);
    assert_eq!(root.read("etc/lodi/host-allowed"), "armed by hand\n");
}

#[test]
fn arm_refuses_an_invalid_marker_and_leaves_it_as_found() {
    let root = Root::new("cli-arm-invalid");

    // A group- or world-writable marker.
    root.write("etc/lodi/host-allowed", "")
        .chmod("etc/lodi/host-allowed", 0o666);
    let output = arm(&root, 0o022);
    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("E_EXISTS") && stderr.contains("0666"),
        "{stderr}"
    );
    assert_eq!(marker_stat(&root).2, 0o666, "the mode was not repaired");

    // Something that is not a regular file.
    std::fs::remove_file(root.path("etc/lodi/host-allowed")).unwrap();
    std::fs::create_dir(root.path("etc/lodi/host-allowed")).unwrap();
    let output = arm(&root, 0o022);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("E_EXISTS"));
    assert!(root.path("etc/lodi/host-allowed").is_dir(), "left as found");
    std::fs::remove_dir(root.path("etc/lodi/host-allowed")).unwrap();

    // A symbolic link planted at the marker: never followed, never replaced.
    root.write("elsewhere", "");
    std::os::unix::fs::symlink("../../elsewhere", root.path("etc/lodi/host-allowed")).unwrap();
    let output = arm(&root, 0o022);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("E_PATH_ESCAPE"));
    let link = std::fs::symlink_metadata(root.path("etc/lodi/host-allowed")).unwrap();
    assert!(link.file_type().is_symlink(), "the link is left as found");
    assert_eq!(root.read("elsewhere"), "", "its target was not written");
}

#[test]
fn arm_refuses_a_symlink_on_the_way_to_the_marker() {
    for (planted, target) in [("etc/lodi", "../outside"), ("etc", "outside")] {
        let root = Root::new("cli-arm-parent");
        std::fs::create_dir_all(root.path("outside")).unwrap();
        std::fs::create_dir_all(root.path(planted).parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, root.path(planted)).unwrap();
        let output = arm(&root, 0o022);
        assert_eq!(output.status.code(), Some(3), "{planted}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("E_PATH_ESCAPE"),
            "{planted}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            std::fs::symlink_metadata(root.path(planted))
                .unwrap()
                .file_type()
                .is_symlink(),
            "{planted} is left as found"
        );
        assert_eq!(
            std::fs::read_dir(root.path("outside")).unwrap().count(),
            0,
            "{planted}: nothing was written through the link"
        );
    }
}

#[test]
fn arm_refuses_a_root_that_is_not_a_directory() {
    let root = Root::new("cli-arm-missing");
    let missing = root.dir.join("no-such-root");
    let output = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["host", "arm", "--root"])
        .arg(&missing)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(6));
    assert!(String::from_utf8_lossy(&output.stderr).contains("E_STORE_IO"));
    assert!(!missing.exists(), "a missing root is not created");
}

#[test]
fn the_unarmed_refusal_names_the_arm_command_for_that_root() {
    let root = Root::new("cli-arm-hint");
    root.debian().write("etc/lodi/host.toml", "");
    // check-host-safety: refusal — `run` appends this scratch root as `--root`.
    let output = run(&root, &["host", "plan"]);
    assert_eq!(output.status.code(), Some(9));
    let stderr = String::from_utf8_lossy(&output.stderr);
    let root_shown = std::fs::canonicalize(&root.dir).unwrap();
    assert!(
        stderr.contains(&format!("lodi host arm --root {}", root_shown.display())),
        "{stderr}"
    );
    assert!(stderr.contains("an empty file"), "{stderr}");
    // Following the hint arms the root, and the same plan then passes the gate.
    assert!(arm(&root, 0o022).status.success());
    // check-host-safety: refusal — `run` appends this scratch root as `--root`.
    let output = run(&root, &["host", "plan"]);
    assert_ne!(
        output.status.code(),
        Some(9),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
