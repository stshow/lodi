//! M-1.0 T-7: the host scope's fault cases, each one failing closed.
//!
//! The spike proved the project scope's failure modes (`tests/spike_faults.rs`) and M-1.0 T-4
//! proved the home scope's (`tests/faults_home.rs`). This file is the third scope, so that
//! "every documented failure mode has a test that fails closed" is true for all three.
//!
//! Every case here is a **real** host apply — the library entry point `src/main.rs`
//! calls, in this process or in a child process of this test binary — against a scratch root
//! this test creates, arms and destroys. There is no test-only branch, environment variable or
//! hidden flag anywhere in the product's host paths: what a test owns is the scratch root and
//! the package-manager shims on `PATH`, which is the whole seam (M-0.6 design call D3, LD-115).
//! **No host command in this repository is ever run against `/`**, in any mode, the refusal
//! included (`AGENTS.md` §8, LD-45, and the owner's rule after LD-252/LD-260):
//! `python3 -B scripts/check-host-safety.py` is the gate step that keeps it so.
//!
//! Each case asserts three things, which is what acceptance (a) asks for:
//!
//! 1. the **documented code** — a code with a `docs/ERRORS.md` row and a committed specimen
//!    (M-1.0 T-3);
//! 2. its **documented exit status**, read from `lodi::diag::exit_status` rather than written
//!    down twice;
//! 3. a **machine state a following apply completes from**: the cause is cleared the way the
//!    diagnostic's own hint says an operator would clear it, and the next apply finishes.
//!
//! Offline throughout: no network, no guest, no package manager. The guest half of this package
//! — a real apply killed by a signal on a fresh guest per gated distribution — is
//! `docs/milestones/m-0.6/probes/hostvm.py --case kill-restart`.

#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Once;

use lodi::diag::exit_status;
use lodi::hostscope::safety::{Gate, Operation};
use lodi::hostscope::{HostError, apply};

use hostroot::{Hold, Root, backups, ids, journals, wait_until};

/// The child half of the two cases that have to interrupt a real apply from outside it. It does
/// nothing at all in the parent's own run.
#[test]
fn child_apply() {
    if !hostroot::is_child() {
        return;
    }
    hostroot::child_apply();
}

// ------------------------------------------------------------------------------ the harness ---

/// One process-wide setup, and the only place this test binary's `PATH` is written.
///
/// `hostroot::shims()` owns one write of its own (the apt programs an armed Debian root needs).
/// This adds a second directory, for the one case below that needs a package manager it can
/// take **away** again — which is why it cannot be the shared directory, and why `pacman` is
/// deliberately not in it. Both writes happen inside this `Once`, and every test in this file
/// enters through [`root`], so no other thread is reading `PATH` while either happens.
fn setup() -> PathBuf {
    static SETUP: Once = Once::new();
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let dir = DIR
        .get_or_init(|| support::scratch("host-faults-bin"))
        .clone();
    SETUP.call_once(|| {
        hostroot::shims();
        let existing = std::env::var("PATH").unwrap_or_default();
        // SAFETY: every `PATH` write in this test binary happens inside this `Once`, and every
        // test enters through `root()`, which calls it before it touches anything else.
        unsafe { std::env::set_var("PATH", format!("{}:{existing}", dir.display())) };
    });
    dir
}

/// A fresh scratch root. Every test in this file starts here, so that [`setup`] has returned
/// before anything reads `PATH`.
fn root(name: &str) -> Root {
    setup();
    Root::new(name)
}

/// Assert the code, and the exit status the registry gives that code. The status is never
/// written down twice: `docs/ERRORS.md` and `lodi::diag::CODES` are held to each other by
/// `tests/diagnostics.rs`, and this reads the same table the product exits with.
#[track_caller]
fn refused(error: &HostError, code: &'static str, status: u8) {
    assert_eq!(error.codes(), vec![code], "{error}");
    assert_eq!(error.exit_status(), status, "{error}");
    assert_eq!(
        exit_status(code),
        status,
        "the registry disagrees with {code}"
    );
}

/// A manifest declaring one file with content, owned by whoever runs the suite.
fn one_file(uid: u32, gid: u32) -> String {
    format!(
        "[files.\"/etc/a.conf\"]\ncontent = \"alpha\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
    )
}

/// The same, with a second declaration, so that a following apply has work to do.
fn two_files(uid: u32, gid: u32) -> String {
    format!(
        "{}\n[files.\"/etc/b.conf\"]\ncontent = \"beta\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n",
        one_file(uid, gid)
    )
}

/// The journal directory of a root.
fn journal_dir(root: &Root) -> PathBuf {
    root.path("var/lib/lodi/host/journal")
}

// ---------------------------------------------------------- a journal that cannot be created ---

/// The journal is the apply's promise that it wrote down what it was about to do. A journal it
/// cannot create is therefore not a warning: the apply stops, with the boundary's own code, and
/// the declared file is still not there.
#[test]
fn a_journal_directory_that_cannot_be_created_stops_before_the_first_mutation() {
    let root = root("faults-journal-dir");
    let (uid, gid) = ids(&root);
    root.debian_with(&one_file(uid, gid));
    // Something else occupies the name the journal directory needs. The host scope creates a
    // directory one checked component at a time and follows nothing, so this is a refusal and
    // never a silent write somewhere else.
    root.write("var/lib/lodi/host/journal", "not a directory\n");

    let error = apply(&root.options()).expect_err("the apply cannot journal");
    refused(&error, "E_PATH_ESCAPE", 3);
    assert!(
        !root.exists("etc/a.conf"),
        "the apply mutated the machine although it could not write down what it was doing"
    );
    assert!(
        !root.exists("etc/lodi/host.lock"),
        "a failed apply left a record of what it never applied"
    );

    // The operator moves the object aside, exactly as the diagnostic's hint says.
    fs::remove_file(journal_dir(&root)).expect("the obstacle goes");
    apply(&root.options()).expect("the following apply completes");
    assert_eq!(root.read("etc/a.conf"), "alpha\n");
    assert_eq!(journals(&root).len(), 1, "one journal, and it committed");
}

// ------------------------------------------------------------------- a journal it cannot read ---

/// Run one real apply so that the root holds a real journal, and return its header line.
fn real_journal_header(root: &Root) -> String {
    apply(&root.options()).expect("the first apply");
    let files = journals(root);
    assert_eq!(files.len(), 1, "one journal: {files:?}");
    let text = fs::read_to_string(&files[0]).expect("the journal reads");
    fs::remove_file(&files[0]).expect("the real journal goes");
    text.lines().next().expect("a header line").to_string()
}

/// Plant `bytes` as the only journal in the root, ask for a second file, and require that the
/// apply refuses with `E_LOCK_VERSION` and applies nothing. Then remove it and require that the
/// following apply completes.
#[track_caller]
fn a_journal_this_build_cannot_read(name: &str, make: impl Fn(&str) -> String) {
    let root = root(name);
    let (uid, gid) = ids(&root);
    root.debian_with(&one_file(uid, gid));
    let header = real_journal_header(&root);

    let planted = journal_dir(&root).join("20260921T000000Z-planted.json");
    fs::write(&planted, make(&header)).expect("the planted journal");
    root.write("etc/lodi/host.toml", &two_files(uid, gid));

    let error = apply(&root.options()).expect_err("a journal this build cannot read");
    refused(&error, "E_LOCK_VERSION", 4);
    assert!(
        !root.exists("etc/b.conf"),
        "the apply carried on although it could not read the journal it had to classify"
    );
    assert_eq!(root.read("etc/a.conf"), "alpha\n", "and it changed nothing");

    fs::remove_file(&planted).expect("the unreadable journal goes");
    apply(&root.options()).expect("the following apply completes");
    assert_eq!(root.read("etc/b.conf"), "beta\n");
}

/// A journal whose header line was cut in half — the shape a power loss leaves. The tail of a
/// journal is allowed to be truncated, because that is what one JSON object per line is for;
/// the **header** is not, because nothing after it can be read without it.
#[test]
fn a_truncated_journal_is_refused_and_nothing_is_applied() {
    a_journal_this_build_cannot_read("faults-journal-truncated", |header| {
        format!("{}\n", &header[..header.len() / 2])
    });
}

/// A file in the journal directory that is JSON, and is not a journal. It records no journal
/// version, so it is refused as a version rather than as a missing field (M-1.0 T-1, D3, D18).
#[test]
fn a_journal_that_is_not_the_json_lodi_wrote_is_refused_and_nothing_is_applied() {
    a_journal_this_build_cannot_read("faults-journal-foreign", |_| {
        "{\"record\":\"header\",\"note\":\"written by something else\"}\n".to_string()
    });
}

/// A real header whose schema version this build does not read: the registry decides the
/// refusal, and the message names the Lodi that wrote it.
#[test]
fn a_journal_schema_version_this_build_does_not_read_is_refused() {
    a_journal_this_build_cannot_read("faults-journal-version", |header| {
        let mut raw: serde_json::Value = serde_json::from_str(header).expect("a real header");
        raw["version"] = serde_json::json!(99);
        format!("{raw}\n")
    });
}

// ---------------------------------------------------------------------- a lock it cannot read ---

/// The host lock is the record of what the last apply did, and drift and `auto_remove` are both
/// comparisons against it. A lock this build cannot read is therefore refused: an apply that
/// treated it as absent would plan from nothing, call every managed file unmanaged, and back
/// up bytes it wrote itself.
#[test]
fn a_host_lock_this_build_cannot_read_is_refused_rather_than_planned_from_nothing() {
    let root = root("faults-lock-version");
    let (uid, gid) = ids(&root);
    root.debian_with(&one_file(uid, gid));
    apply(&root.options()).expect("the first apply");
    let good = root.read("etc/lodi/host.lock");
    assert!(good.contains("/etc/a.conf"), "the lock records the file");

    let mut raw: serde_json::Value = serde_json::from_str(&good).expect("a real lock");
    raw["version"] = serde_json::json!(99);
    root.write("etc/lodi/host.lock", &format!("{raw}\n"));
    root.write("etc/lodi/host.toml", &two_files(uid, gid));

    let error = apply(&root.options()).expect_err("a lock this build cannot read");
    refused(&error, "E_LOCK_VERSION", 4);
    assert!(!root.exists("etc/b.conf"), "the apply planned anyway");
    assert!(
        hostroot::backups(&root).is_empty(),
        "the apply treated its own managed file as unmanaged and kept a backup of it"
    );
    assert_eq!(journals(&root).len(), 1, "no second journal was opened");

    // The hint says to use the lodi that wrote it or move it aside; moving it aside is what an
    // operator with one lodi can do, and the following apply completes from there.
    root.write("etc/lodi/host.lock", &good);
    apply(&root.options()).expect("the following apply completes");
    assert_eq!(root.read("etc/b.conf"), "beta\n");
}

/// A package the lock records is a name `auto_remove` hands to the package manager, so each key
/// is held to the package-name grammar a manifest name is held to. A key that is not one is a
/// lock this build does not read, refused before anything is planned, run or journalled.
#[test]
fn a_host_lock_package_key_that_is_not_a_package_name_is_refused_before_anything_runs() {
    let root = root("faults-lock-key");
    let (uid, gid) = ids(&root);
    root.debian_with(&one_file(uid, gid));
    apply(&root.options()).expect("the first apply");
    let good = root.read("etc/lodi/host.lock");

    let mut raw: serde_json::Value = serde_json::from_str(&good).expect("a real lock");
    raw["packages"]["-oAPT::Get::Assume-Yes=1"] =
        serde_json::json!({"version": "1", "mark": "manual", "held": false});
    root.write("etc/lodi/host.lock", &format!("{raw}\n"));
    root.write("etc/lodi/host.toml", &two_files(uid, gid));

    let error = apply(&root.options()).expect_err("a lock key that is not a package name");
    refused(&error, "E_LOCK_VERSION", 4);
    assert!(
        error.to_string().contains("which is not a package name"),
        "{error}"
    );
    assert!(!root.exists("etc/b.conf"), "the apply planned anyway");
    assert_eq!(journals(&root).len(), 1, "no second journal was opened");

    root.write("etc/lodi/host.lock", &good);
    apply(&root.options()).expect("the following apply completes");
    assert_eq!(root.read("etc/b.conf"), "beta\n");
}

// ------------------------------------------------- a target that changed between plan and apply ---

/// Hold a real apply still inside its **first** file action and let `plant` change the machine
/// under it, then let it go and return what it said.
///
/// The child is a real apply in its own process. What holds it still is a [`Hold`] at the open
/// of the temporary file the first action's write goes through (LD-379); the product has no idea
/// it is under a test.
fn apply_interrupted_by(name: &str, plant: impl FnOnce(&Root)) -> (i32, String) {
    let root = root(name);
    let (uid, gid) = ids(&root);
    root.write("etc/decoy.conf", "untouched\n");
    // `/etc/a-gate.conf` sorts before `/etc/b-target.conf`, so the gate is action `f1` and the
    // target is `f2`: the apply is held at `f1` while the machine changes under `f2`.
    root.debian_with(&format!(
        "[files.\"/etc/a-gate.conf\"]\ncontent = \"alpha\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n\n\
         [files.\"/etc/b-target.conf\"]\ncontent = \"beta\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
    ));

    let hold = Hold::at_write_of(&root, "a-gate.conf");
    let child = hold.spawn(&root);
    hold.wait_held();
    assert!(!journals(&root).is_empty(), "the journal is written first");

    plant(&root);

    hold.release();
    let output = child.wait_with_output().expect("the apply finishes");
    let status = output
        .status
        .code()
        .expect("it exited rather than signalled");
    let said = String::from_utf8_lossy(&output.stderr).into_owned();

    assert_eq!(status, 8, "{said}");
    assert!(
        said.contains("E_APPLY") && said.contains("/etc/b-target.conf"),
        "the refusal does not name the action and its path: {said}"
    );
    assert!(
        said.contains("not a regular file") || said.contains("symbolic link"),
        "the refusal does not say what it found: {said}"
    );
    assert_eq!(
        root.read("etc/decoy.conf"),
        "untouched\n",
        "the apply followed what it found and wrote through it"
    );
    assert_eq!(root.read("etc/a-gate.conf"), "alpha\n", "f1 did happen");

    // The operator moves the object aside; the following apply completes from there.
    fs::remove_dir_all(root.path("etc/b-target.conf"))
        .or_else(|_| fs::remove_file(root.path("etc/b-target.conf")))
        .expect("the obstacle goes");
    root.write(
        "etc/lodi/host.toml",
        &format!(
            "[files.\"/etc/b-target.conf\"]\ncontent = \"beta\\n\"\n\
             owner = \"{uid}\"\ngroup = \"{gid}\"\n"
        ),
    );
    apply(&root.options()).expect("the following apply completes");
    assert_eq!(root.read("etc/b-target.conf"), "beta\n");
    (status, said)
}

/// A managed path that became a symbolic link after the plan was made. Lodi never follows the
/// final component of a host destination, so the bytes go nowhere: the action is refused, the
/// transaction stops, and what the link pointed at is untouched.
#[test]
fn a_target_that_became_a_symlink_between_plan_and_apply_is_refused_not_followed() {
    apply_interrupted_by("faults-target-symlink", |root| {
        std::os::unix::fs::symlink("decoy.conf", root.path("etc/b-target.conf"))
            .expect("the symlink is planted");
    });
}

/// The same for a directory, which no amount of following would make writable anyway: the
/// point is that the refusal is the boundary's, before a temporary file is even made.
#[test]
fn a_target_that_became_a_directory_between_plan_and_apply_is_refused() {
    apply_interrupted_by("faults-target-directory", |root| {
        fs::create_dir(root.path("etc/b-target.conf")).expect("the directory is planted");
    });
}

// ------------------------------------------- a kept backup that is not a regular file any more ---

/// A managed path is read only if it is a regular file, and that open never waits: a backup
/// that became a FIFO while a real apply was held before its restore is refused at once, under
/// the apply lock, instead of holding the apply (and the lock) still for a writer that never
/// comes. The wait below is the suite's backstop, so a regression fails here rather than hangs.
#[test]
fn a_kept_backup_that_became_a_fifo_is_refused_without_blocking_the_apply() {
    let root = root("faults-kept-fifo");
    let (uid, gid) = ids(&root);
    // An unmanaged file is replaced, so its original bytes are kept beside it.
    root.write("etc/b-target.conf", "original\n");
    root.debian_with(&format!(
        "[files.\"/etc/b-target.conf\"]\ncontent = \"beta\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
    ));
    apply(&root.options()).expect("the first apply manages the file");
    let kept = backups(&root);
    assert_eq!(kept.len(), 1, "one backup is kept: {kept:?}");
    let kept = kept[0].clone();
    // The first apply left its lock file behind; its creation is the hand-off below.
    fs::remove_file(root.path("var/lib/lodi/host/.lock")).expect("the lock file goes");

    // The declaration leaves the manifest, so the apply restores the kept bytes as `f2`, after
    // the gate `f1` it is held at.
    root.write(
        "etc/lodi/host.toml",
        &format!(
            "[host]\ndistro = \"debian\"\n\n[files.\"/etc/a-gate.conf\"]\ncontent = \"alpha\\n\"\n\
             owner = \"{uid}\"\ngroup = \"{gid}\"\n"
        ),
    );
    // Whatever fails below, the child does not outlive this test.
    struct Reaped(Option<std::process::Child>);
    impl Drop for Reaped {
        fn drop(&mut self) {
            if let Some(child) = self.0.as_mut() {
                hostroot::kill(child);
            }
        }
    }
    let hold = Hold::at_write_of(&root, "a-gate.conf");
    let journals_before = journals(&root).len();
    let mut guard = Reaped(Some(hold.spawn(&root)));
    hold.wait_held();
    assert!(
        journals(&root).len() > journals_before,
        "the journal is written first"
    );

    fs::remove_file(&kept).expect("the kept bytes go");
    let status = std::process::Command::new("mkfifo")
        .arg(&kept)
        .status()
        .expect("mkfifo runs");
    assert!(status.success(), "a FIFO takes the backup's place");

    hold.release();
    wait_until("the apply to finish rather than wait on the FIFO", || {
        let child = guard.0.as_mut().expect("the child");
        child
            .try_wait()
            .expect("the child can be waited on")
            .is_some()
    });
    let child = guard.0.take().expect("the child");
    let output = child.wait_with_output().expect("the apply finished");
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(8), "{said}");
    assert!(
        said.contains("E_APPLY") && said.contains("/etc/b-target.conf"),
        "the refusal does not name the action and its path: {said}"
    );
    assert!(said.contains("not a regular file"), "{said}");
    assert_eq!(
        root.read("etc/b-target.conf"),
        "beta\n",
        "nothing was restored"
    );
    assert_eq!(root.read("etc/a-gate.conf"), "alpha\n", "f1 did happen");

    // The operator puts the kept bytes back, as the hint says; the following apply restores them.
    fs::remove_file(&kept).expect("the FIFO goes");
    fs::write(&kept, "original\n").expect("the kept bytes are back");
    root.write("etc/lodi/host.toml", "[host]\ndistro = \"debian\"\n");
    apply(&root.options()).expect("the following apply completes");
    assert_eq!(root.read("etc/b-target.conf"), "original\n");
}

// ------------------------------------------------------- a package manager that went away ---

/// The recordings of a real Arch guest that `hostvm.py --record-fixtures` produced for M-0.6
/// T-5. This case replays them rather than inventing answers, so the only thing it makes up is
/// the moment the program disappears.
fn pacman_fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host/pacman/arch/install")
}

/// Write the test's own `pacman` into [`setup`]'s directory. The fixture paths are baked into
/// the script, so the shim needs no environment of its own and the product's fixed
/// non-interactive environment is the only one it sees.
fn write_pacman(dir: &Path, state: &Path) {
    let fixtures = pacman_fixtures();
    let script = format!(
        "#!/bin/sh\n\
         # A test-owned shim (M-0.6 design call D3): it replays what a real Arch guest printed.\n\
         # The product starts it with a cleared environment and a fixed PATH (LD-357), so the\n\
         # PATH its own tools are found on is baked in when the test writes it.\n\
         PATH='{path}'; export PATH\n\
         state='{state}'\n\
         fixtures='{fixtures}'\n\
         phase=$(cat \"$state/phase\" 2>/dev/null || echo before)\n\
         case \"$1\" in\n\
         \x20 -Q)  cat \"$fixtures/$phase/pacman-Q.txt\" ;;\n\
         \x20 -Qe) cat \"$fixtures/$phase/pacman-Qe.txt\" ;;\n\
         \x20 -Slq) cat \"$fixtures/pacman-Slq.txt\" ;;\n\
         \x20 -Si) cat \"$fixtures/pacman-Si.txt\" ;;\n\
         \x20 -Syu|-S|-Rns|-D) printf 'after\\n' > \"$state/phase\" ;;\n\
         esac\n\
         exit 0\n",
        state = state.display(),
        path = std::env::var("PATH").unwrap_or_default(),
        fixtures = fixtures.display(),
    );
    let path = dir.join("pacman");
    fs::write(&path, script).expect("the shim");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("the shim mode");
}

/// The safety gate resolves every program the distribution needs by bare name through `PATH`,
/// and the journal records what it resolved. A program that is gone by the time the apply needs
/// it is the runtime code — never a half-run transaction — and the apply stops **before** the
/// journal records a started action, because there is no journal at all.
#[test]
fn a_package_manager_that_vanished_after_the_gate_is_the_runtime_code_and_journals_nothing() {
    let bin = setup();
    let root = Root::new("faults-pm-gone");
    let state = root.path(".shim-state");
    fs::create_dir_all(&state).expect("the shim state");
    write_pacman(&bin, &state);
    let (uid, gid) = ids(&root);
    root.may_manage().arch();
    // A synchronised database, so that the plan can report the index age of a real machine.
    root.write("var/lib/pacman/sync/extra.db", "recorded database\n");
    root.write(
        "etc/lodi/host.toml",
        &format!(
            "[host]\nversion = \"1\"\n\n[packages]\ncommon = [\"tree\"]\n\n\
             [files.\"/etc/a.conf\"]\ncontent = \"alpha\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
        ),
    );

    // Step 5 of the gate: the program is there, and this is the moment it is resolved.
    let gate = Gate::open(&root.options(), Operation::Apply).expect("the safety gate passes");
    assert!(
        gate.pm.programs.iter().any(|(name, _)| name == "pacman"),
        "the gate resolved the package manager: {:?}",
        gate.pm.programs
    );
    drop(gate);

    // And this is the moment it goes away.
    fs::remove_file(bin.join("pacman")).expect("the package manager goes");
    let error = apply(&root.options()).expect_err("the apply has no package manager");
    refused(&error, "E_NO_RUNTIME", 7);
    assert!(
        journals(&root).is_empty(),
        "a started action was journalled although nothing could run"
    );
    assert!(!root.exists("etc/a.conf"), "and nothing was applied");

    // Put it back, and the apply completes from exactly that state.
    write_pacman(&bin, &state);
    let report = apply(&root.options()).expect("the following apply completes");
    assert!(report.contains("journal "), "{report}");
    assert_eq!(root.read("etc/a.conf"), "alpha\n");
    assert!(
        root.read("etc/lodi/host.lock").contains("\"tree\""),
        "the lock records what the transaction installed"
    );
}
