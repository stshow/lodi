//! M-1.0 T-4: the spike's fault cases (`tests/spike_faults.rs`), transposed to the home scope.
//!
//! Every case here runs the **built binary** against a throwaway set of roots through
//! `support::home_env`, which clears the environment and hands the child exactly the variables
//! the home scope may see — and panics if the `HOME` it would hand over were the ambient one. No
//! test of this repository ever runs the home scope against a real home (design call D15,
//! `LD-111`). Everything is offline: no network, no guest, nothing outside
//! `CARGO_TARGET_TMPDIR`.
//!
//! **The synchronization rule (design call D10).** The one case that needs a process stopped in
//! the middle of its work does not wait on a clock for it. It replaces the backup the apply must
//! read with a **named pipe**: opening the write end of that pipe blocks until the apply has
//! opened the read end, so the test is released at exactly the moment the apply is inside that
//! read — one declared file already written, the next one not — and kills it there.
//! [`the_home_fault_cases_never_wait_on_a_clock`] scans this file and
//! `tests/concurrency.rs` and fails if the name of the wall-clock wait appears in either.
//!
//! Every case ends the way the task requires: with a following apply that completes from
//! whatever the failure left behind.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use support::{HomeEnv, home_env, home_gate};

// ------------------------------------------------------------------------------- the harness ---

fn config_dir(env: &HomeEnv) -> PathBuf {
    env.config().join("lodi")
}

fn write_manifest(env: &HomeEnv, text: &str) {
    let dir = config_dir(env);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("home.toml"), text).unwrap();
}

fn run(env: &HomeEnv, args: &[&str]) -> Output {
    env.command()
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("the lodi binary runs")
}

fn apply(env: &HomeEnv) -> Output {
    run(env, &["home", "apply"])
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn data(env: &HomeEnv) -> PathBuf {
    env.data().join("lodi")
}

fn state_path(env: &HomeEnv) -> PathBuf {
    data(env).join("home-scope/state.json")
}

fn backup_dir(env: &HomeEnv) -> PathBuf {
    data(env).join("home-scope/backups")
}

/// The one `original` backup of `path`, as `index.json` names it.
fn backup_of(env: &HomeEnv, path: &str) -> String {
    let index: serde_json::Value =
        serde_json::from_slice(&fs::read(backup_dir(env).join("index.json")).unwrap()).unwrap();
    index["backups"]
        .as_object()
        .expect("the index names its backups")
        .iter()
        .find(|(_, record)| record["path"] == path && record["kind"] == "original")
        .unwrap_or_else(|| panic!("no original backup of {path}: {index}"))
        .0
        .clone()
}

/// The state file's `files` table, path to recorded sha256.
fn state_files(env: &HomeEnv) -> BTreeMap<String, String> {
    let text = fs::read_to_string(state_path(env)).expect("the state file is there");
    let value: serde_json::Value = serde_json::from_str(&text).expect("the state file is JSON");
    assert_eq!(
        lodi::lock::canonical_json(&value),
        text,
        "the state file is not canonical JSON"
    );
    value["files"]
        .as_object()
        .expect("files is an object")
        .iter()
        .map(|(path, record)| {
            (
                path.clone(),
                record["sha256"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// Every path below `root`, relative, with its mode and content hash (or `dir`, or a link's
/// target), sorted: an exact listing **and** the content, so neither a new file nor an edited one
/// can pass unnoticed.
fn tree(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, at: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(entries) = fs::read_dir(at) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let rel = path.strip_prefix(root).unwrap().display().to_string();
            let mode = meta.permissions().mode() & 0o7777;
            if meta.file_type().is_symlink() {
                out.insert(
                    rel,
                    format!("link {}", fs::read_link(&path).unwrap().display()),
                );
            } else if meta.is_dir() {
                out.insert(rel, format!("dir {mode:04o}"));
                walk(root, &path, out);
            } else if meta.is_file() {
                out.insert(
                    rel,
                    format!(
                        "{mode:04o} {}",
                        lodi::util::sha256_hex(&fs::read(&path).unwrap())
                    ),
                );
            } else {
                out.insert(rel, format!("special {mode:04o}"));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// The containment check every home-scope test of this repository ends with: the write ledger
/// names nothing outside the gate root, and the fixed decoy tree beside the roots is untouched.
fn contained(env: &HomeEnv, decoy_before: &str) {
    let outside: Vec<PathBuf> = home_gate()
        .ledger_lines()
        .into_iter()
        .map(|(_, path)| path)
        .filter(|path| !path.starts_with(home_gate().root()))
        .collect();
    assert!(
        outside.is_empty(),
        "the ledger left the gate root: {outside:?}"
    );
    assert_eq!(env.decoy_listing(), decoy_before, "the decoy tree changed");
}

/// A named pipe at `path`, made by the system's own `mkfifo`.
fn mkfifo(path: &Path) {
    let status = Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("mkfifo runs");
    assert!(status.success(), "mkfifo {}: {status}", path.display());
}

const ORIGINAL: &[u8] = b"# the user's own zshrc\n";

/// The starting point of the cases that need a managed file with a backup behind it: `.zshrc`
/// exists as the user's own file, the manifest takes it over, and one apply has succeeded.
fn taken_over(env: &HomeEnv) {
    fs::write(env.home().join(".zshrc"), ORIGINAL).unwrap();
    write_manifest(
        env,
        "[home]\nversion = \"1\"\n\n[files.\".zshrc\"]\ncontent = \"# lodi\\n\"\n\
         on_remove = \"restore\"\n",
    );
    let out = apply(env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        fs::read_to_string(env.home().join(".zshrc")).unwrap(),
        "# lodi\n"
    );
}

/// The manifest of the second apply: `.aliases` is new and sorts first, `.zshrc` is gone and
/// must be restored from the backup — so the apply writes one file and then reads the backup.
const AFTER: &str =
    "[home]\nversion = \"1\"\n\n[files.\".aliases\"]\ncontent = \"alias l=ls\\n\"\n";

// ------------------------------------------------------------ (a) an apply killed in the middle ---

/// An apply killed **between two files** writes no state, and the next apply re-plans from what
/// is on disk and completes.
///
/// The kill point is exact and owes nothing to a clock: the backup `.zshrc` must be restored from
/// is replaced by a named pipe, and this test's `open` of its write end returns at precisely the
/// moment the apply blocks in the matching read — after `.aliases` has been written and before
/// `.zshrc` has been touched.
#[test]
fn an_apply_killed_between_two_files_writes_no_state_and_the_next_one_completes() {
    let env = home_env("faults-home-killed");
    let decoy = env.decoy_listing();
    taken_over(&env);
    let name = backup_of(&env, ".zshrc");
    let backup = backup_dir(&env).join(&name);
    let kept = fs::read(&backup).unwrap();
    assert_eq!(kept, ORIGINAL, "the backup is not the user's own bytes");

    write_manifest(&env, AFTER);
    let state_before = fs::read(state_path(&env)).unwrap();
    fs::remove_file(&backup).unwrap();
    mkfifo(&backup);

    let mut child = env
        .command()
        .args(["home", "apply"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the apply starts");
    // Blocks until the apply has opened the read end of the pipe: it is inside the restore.
    let writer = fs::OpenOptions::new()
        .write(true)
        .open(&backup)
        .expect("the write end of the pipe");
    child.kill().expect("the apply is killed");
    let out = child.wait_with_output().unwrap();
    drop(writer);
    assert_eq!(
        out.status.code(),
        None,
        "the apply was not killed in the middle: {}",
        stderr(&out)
    );

    // One file written, the state untouched, the managed file not yet restored.
    assert_eq!(
        fs::read_to_string(env.home().join(".aliases")).unwrap(),
        "alias l=ls\n",
        "the first file was not written before the kill"
    );
    assert_eq!(
        fs::read(state_path(&env)).unwrap(),
        state_before,
        "a killed apply wrote the state file"
    );
    assert_eq!(
        fs::read_to_string(env.home().join(".zshrc")).unwrap(),
        "# lodi\n",
        "the restore ran although the apply was killed inside it"
    );

    // The next apply re-plans from observed state and completes.
    fs::remove_file(&backup).unwrap();
    fs::write(&backup, &kept).unwrap();
    fs::set_permissions(&backup, fs::Permissions::from_mode(0o600)).unwrap();
    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(fs::read(env.home().join(".zshrc")).unwrap(), ORIGINAL);
    assert_eq!(
        fs::read_to_string(env.home().join(".aliases")).unwrap(),
        "alias l=ls\n"
    );
    // The state the recovery wrote describes the home as it now is: `.zshrc` is no longer
    // managed, because it was restored. `.aliases` is **not** recorded either — the killed apply
    // wrote those exact bytes, so the re-plan sees an unmanaged file that already holds what the
    // manifest declares and calls it unchanged, exactly as 1.0 did (*The `.aliases` observation*
    // of `docs/milestones/m-1.0/records/t-4.md`). M-Home adopts such a path for the 1.1 tables
    // only: a `[files]` entry keeps 1.0's behaviour byte for byte (`docs/design/HOME_PROGRAMS.md`
    // §10 I10, `tests/home_compat.rs`).
    let files = state_files(&env);
    assert!(
        files.is_empty(),
        "the recovered state is not what is on disk: {files:?}"
    );
    // Proof that the re-plan really did settle: a second apply has nothing left to do.
    let out = run(&env, &["home", "plan"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        !text.contains("create") && !text.contains("replace"),
        "the home has not settled: {text}"
    );
    contained(&env, &decoy);
}

// --------------------------------------------------------- (b) a declared file that cannot be written ---

/// A declared path whose nearest existing ancestor this user cannot write is `E_STORE_PERM` at
/// exit 9 **before the first write**, and the roots are byte-identical afterwards.
#[test]
fn a_declared_file_that_cannot_be_written_stops_the_apply_before_anything_is_written() {
    let env = home_env("faults-home-unwritable");
    let decoy = env.decoy_listing();
    let locked = env.home().join("locked");
    fs::create_dir_all(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[files.\".aliases\"]\ncontent = \"alias l=ls\\n\"\n\n\
         [files.\"locked/z.conf\"]\ncontent = \"nope\\n\"\n",
    );

    let before = tree(env.root());
    let out = apply(&env);
    assert_eq!(out.status.code(), Some(9), "{}", stderr(&out));
    assert!(stderr(&out).contains("E_STORE_PERM"), "{}", stderr(&out));
    assert_eq!(
        tree(env.root()),
        before,
        "a refused apply changed something under the roots"
    );

    // Recovery: the permission is the user's to fix, and the apply then completes.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(fs::read_to_string(locked.join("z.conf")).unwrap(), "nope\n");
    assert_eq!(
        state_files(&env).keys().collect::<Vec<_>>(),
        vec![".aliases", "locked/z.conf"]
    );
    contained(&env, &decoy);
}

/// A step that fails **in the middle** of an apply stops with that step's own code and exit
/// status, and the state file that is written last records exactly what did happen: the file
/// that was written, and nothing about the step that failed.
#[test]
fn a_step_that_fails_stops_with_its_own_code_and_a_truthful_state() {
    let env = home_env("faults-home-step");
    let decoy = env.decoy_listing();
    taken_over(&env);
    let name = backup_of(&env, ".zshrc");
    let backup = backup_dir(&env).join(&name);
    let kept = fs::read(&backup).unwrap();

    // The backup the restore must read is replaced by a directory: the read fails with the
    // home scope's own I/O code, after `.aliases` has already been written.
    write_manifest(&env, AFTER);
    fs::remove_file(&backup).unwrap();
    fs::create_dir(&backup).unwrap();

    let out = apply(&env);
    assert_eq!(out.status.code(), Some(6), "{}", stderr(&out));
    assert!(stderr(&out).contains("E_STORE_IO"), "{}", stderr(&out));
    assert_eq!(
        fs::read_to_string(env.home().join(".aliases")).unwrap(),
        "alias l=ls\n"
    );
    assert_eq!(
        fs::read_to_string(env.home().join(".zshrc")).unwrap(),
        "# lodi\n",
        "the file whose step failed was changed"
    );
    // Nothing partial: every path the state names holds exactly the bytes it records.
    for (path, sha) in state_files(&env) {
        let on_disk = lodi::util::sha256_hex(&fs::read(env.home().join(&path)).unwrap());
        assert_eq!(on_disk, sha, "the state's record of {path} is not on disk");
    }

    // Recovery: put the backup back, and the next apply finishes the work.
    fs::remove_dir(&backup).unwrap();
    fs::write(&backup, &kept).unwrap();
    fs::set_permissions(&backup, fs::Permissions::from_mode(0o600)).unwrap();
    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(fs::read(env.home().join(".zshrc")).unwrap(), ORIGINAL);
    assert_eq!(
        state_files(&env).keys().collect::<Vec<_>>(),
        vec![".aliases"]
    );
    contained(&env, &decoy);
}

// ------------------------------------------------------- (c) a backup store that cannot be written ---

/// A backup store that cannot be written stops the apply **before the first file is replaced**:
/// the user's own file still holds the user's own bytes, and the state names nothing.
#[test]
fn a_backup_store_that_cannot_be_written_stops_before_the_first_replacement() {
    let env = home_env("faults-home-backups");
    let decoy = env.decoy_listing();
    fs::write(env.home().join(".zshrc"), ORIGINAL).unwrap();
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[files.\".zshrc\"]\ncontent = \"# lodi\\n\"\n",
    );
    let backups = backup_dir(&env);
    fs::create_dir_all(&backups).unwrap();
    fs::set_permissions(&backups, fs::Permissions::from_mode(0o500)).unwrap();

    let out = apply(&env);
    assert_eq!(out.status.code(), Some(6), "{}", stderr(&out));
    assert!(stderr(&out).contains("E_STORE_IO"), "{}", stderr(&out));
    assert_eq!(
        fs::read(env.home().join(".zshrc")).unwrap(),
        ORIGINAL,
        "the file was replaced although its copy could not be kept"
    );
    assert!(
        state_files(&env).is_empty(),
        "the state names a file that was never written: {:?}",
        state_files(&env)
    );
    assert!(
        fs::read_dir(&backups).unwrap().next().is_none(),
        "a half-made backup was left behind"
    );

    // Recovery: the permission is the user's to fix, and the apply then keeps the copy and
    // takes the path over.
    fs::set_permissions(&backups, fs::Permissions::from_mode(0o755)).unwrap();
    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("W_REPLACED_UNMANAGED"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        fs::read_to_string(env.home().join(".zshrc")).unwrap(),
        "# lodi\n"
    );
    assert_eq!(
        fs::read(backups.join(backup_of(&env, ".zshrc"))).unwrap(),
        ORIGINAL
    );
    contained(&env, &decoy);
}

// --------------------------------------------------------------- (d) a state file lodi did not write ---

/// A `state.json` that is truncated, that is not the JSON Lodi wrote, or that carries a schema
/// version outside this build's read-set is refused with the artifact's own code and exit status,
/// **nothing is written**, and the message says which Lodi wrote the file — or says plainly that
/// it records none (M-1.0 T-1, design calls D3, D18).
#[test]
fn a_state_file_lodi_did_not_write_is_refused_and_nothing_is_written() {
    let env = home_env("faults-home-state");
    let decoy = env.decoy_listing();
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[files.\".aliases\"]\ncontent = \"alias l=ls\\n\"\n",
    );
    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let good = fs::read(state_path(&env)).unwrap();

    let cases: [(&str, Vec<u8>, &str); 4] = [
        ("truncated", good[..good.len() / 2].to_vec(), "lodi 0.3.0"),
        ("not json", b"not json at all\n".to_vec(), "lodi 0.3.0"),
        (
            "a newer schema",
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 99, "lodiVersion": "lodi 9.9.9", "files": {}, "directories": []
            }))
            .unwrap(),
            "lodi 9.9.9",
        ),
        (
            "a newer schema with no writer",
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 99, "files": {}, "directories": []
            }))
            .unwrap(),
            "does not record which lodi",
        ),
    ];
    for (what, bytes, names) in cases {
        fs::write(state_path(&env), &bytes).unwrap();
        let before = tree(env.home());
        let out = apply(&env);
        let text = stderr(&out);
        assert_eq!(out.status.code(), Some(6), "{what}: {text}");
        assert!(text.contains("E_STORE_IO"), "{what}: {text}");
        if what.starts_with("a newer schema") {
            assert!(
                text.contains(names),
                "{what}: the refusal does not say who wrote the file: {text}"
            );
        }
        assert_eq!(
            fs::read(state_path(&env)).unwrap(),
            bytes,
            "{what}: rewritten"
        );
        assert_eq!(tree(env.home()), before, "{what}: the home root changed");
    }

    // Recovery: the state Lodi wrote is readable again, and the apply completes.
    fs::write(state_path(&env), &good).unwrap();
    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        state_files(&env).keys().collect::<Vec<_>>(),
        vec![".aliases"]
    );
    contained(&env, &decoy);
}

// -------------------------------------- (d2) a digest or backup name that is not one (LD-361) ---

/// A digest or a backup name in `state.json` or `index.json` that is not the lowercase hex Lodi
/// writes — a multibyte value among them — is the same invalid-state refusal as any other state
/// Lodi did not write (`E_STORE_IO`: exit 6 from `status` and `apply`, and `plan`'s own exit 3),
/// never a panic, and nothing is written.
#[test]
fn a_state_or_index_value_that_is_not_a_digest_is_refused_and_never_panics() {
    let env = home_env("faults-home-digest");
    let decoy = env.decoy_listing();
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[files.\".aliases\"]\ncontent = \"alias l=ls\\n\"\n",
    );
    fs::write(env.home().join(".aliases"), "the user's own\n").unwrap();
    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let index_path = backup_dir(&env).join("index.json");
    let good_state = fs::read(state_path(&env)).unwrap();
    let good_index = fs::read(&index_path).unwrap();
    let kept = backup_of(&env, ".aliases");

    // Three-byte characters, so no fixed byte prefix of the value is a character boundary.
    let bad = "€".repeat(22);
    type Edit<'a> = Box<dyn Fn(&mut serde_json::Value, &mut serde_json::Value) + 'a>;
    let cases: Vec<(&str, Edit<'_>)> = vec![
        (
            "the state's digest",
            Box::new(|s, _| s["files"][".aliases"]["sha256"] = bad.clone().into()),
        ),
        (
            "the state's origin digest",
            Box::new(|s, _| s["files"][".aliases"]["origin"]["sha256"] = bad.clone().into()),
        ),
        (
            "the state's backup name",
            Box::new(|s, _| s["files"][".aliases"]["backup"] = bad.clone().into()),
        ),
        (
            "the index's digest",
            Box::new(|_, i| i["backups"][kept.as_str()]["sha256"] = bad.clone().into()),
        ),
        (
            "the index's backup name, of a path the state no longer names",
            Box::new(|_, i| {
                let mut record = i["backups"][kept.as_str()].clone();
                record["path"] = ".gone".into();
                i["backups"]
                    .as_object_mut()
                    .unwrap()
                    .insert(bad.clone(), record);
            }),
        ),
    ];
    for (what, edit) in &cases {
        let mut state: serde_json::Value = serde_json::from_slice(&good_state).unwrap();
        let mut index: serde_json::Value = serde_json::from_slice(&good_index).unwrap();
        edit(&mut state, &mut index);
        let state_bytes = lodi::lock::canonical_json(&state).into_bytes();
        let index_bytes = lodi::lock::canonical_json(&index).into_bytes();
        fs::write(state_path(&env), &state_bytes).unwrap();
        fs::write(&index_path, &index_bytes).unwrap();
        for verb in ["status", "plan", "apply"] {
            let before = tree(env.root());
            let out = run(&env, &["home", verb]);
            let text = stderr(&out);
            assert!(!text.contains("panicked"), "{what}, {verb}: {text}");
            // `plan` reports every refusal at the manifest-error status, as it always has.
            let status = if verb == "plan" { 3 } else { 6 };
            assert_eq!(out.status.code(), Some(status), "{what}, {verb}: {text}");
            assert!(text.contains("E_STORE_IO"), "{what}, {verb}: {text}");
            assert!(
                text.contains("is not the JSON lodi wrote"),
                "{what}, {verb}: {text}"
            );
            assert_eq!(
                tree(env.root()),
                before,
                "{what}, {verb}: something was written"
            );
        }
    }

    // Recovery: the records Lodi wrote are readable again, and the apply completes.
    fs::write(state_path(&env), &good_state).unwrap();
    fs::write(&index_path, &good_index).unwrap();
    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    contained(&env, &decoy);
}

/// A symbolic link where a backup is about to be kept is refused as a path that leaves the root,
/// before anything is recorded: the backup store's "already kept" check asks `lstat`, never the
/// file a link points at, so the index never names a backup that was not written.
#[test]
fn a_link_where_a_backup_would_go_is_refused_and_no_backup_is_recorded() {
    let env = home_env("faults-home-backup-link");
    let decoy = env.decoy_listing();
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[files.\".aliases\"]\ncontent = \"alias l=ls\\n\"\n",
    );
    let original = b"the user's own\n";
    fs::write(env.home().join(".aliases"), original).unwrap();
    let name =
        lodi::home::backup::name_for(".aliases", original, lodi::home::backup::Kind::Original);
    fs::create_dir_all(backup_dir(&env)).unwrap();
    let planted = backup_dir(&env).join(&name);
    std::os::unix::fs::symlink(env.decoy().join("keep.txt"), &planted).unwrap();

    let before = tree(env.home());
    let out = apply(&env);
    let text = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{text}");
    assert!(text.contains("E_PATH_ESCAPE"), "{text}");
    assert_eq!(tree(env.home()), before, "the home root changed");
    assert!(
        !backup_dir(&env).join("index.json").exists(),
        "an index was written"
    );
    // The state records what finished before a failure (step 5 of an apply), and nothing did.
    if state_path(&env).exists() {
        assert!(state_files(&env).is_empty(), "the state records a write");
    }
    assert!(
        fs::symlink_metadata(&planted)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link was replaced rather than refused"
    );

    // Recovery: with the link gone, the apply keeps the real original and records it.
    fs::remove_file(&planted).unwrap();
    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(backup_of(&env, ".aliases"), name);
    assert_eq!(fs::read(&planted).unwrap(), original);
    contained(&env, &decoy);
}

// ------------------------------------------------- (e) a target that changed type between plan and apply ---

/// What a case does to a managed target between `lodi home plan` and `lodi home apply`: it is
/// handed the target and the unrelated file a symbolic link may point at.
type Swap = fn(&Path, &Path);

/// A managed target replaced by a directory, by a symbolic link or by any other object that is
/// not a regular file between `lodi home plan` and `lodi home apply` is refused — never followed,
/// never removed — and the apply writes nothing.
#[test]
fn a_target_that_became_a_directory_or_a_link_is_refused_rather_than_followed() {
    let env = home_env("faults-home-target");
    let decoy = env.decoy_listing();
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[files.\".inputrc\"]\ncontent = \"set editing-mode vi\\n\"\n",
    );
    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let target = env.home().join(".inputrc");
    let elsewhere = env.home().join("elsewhere");
    fs::write(&elsewhere, b"not lodi's\n").unwrap();

    let swaps: [(&str, Swap); 3] = [
        ("a directory", |target, _| {
            fs::remove_file(target).unwrap();
            fs::create_dir(target).unwrap();
        }),
        ("a symbolic link", |target, elsewhere| {
            fs::remove_file(target).unwrap();
            std::os::unix::fs::symlink(elsewhere, target).unwrap();
        }),
        ("a named pipe", |target, _| {
            fs::remove_file(target).unwrap();
            mkfifo(target);
        }),
    ];
    for (what, swap) in swaps {
        // The plan agrees there is work to do while the target is still a regular file.
        let out = run(&env, &["home", "plan"]);
        assert_eq!(out.status.code(), Some(0), "{what}: {}", stderr(&out));

        swap(&target, &elsewhere);
        let before = tree(env.root());
        let out = apply(&env);
        let text = stderr(&out);
        assert_eq!(out.status.code(), Some(3), "{what}: {text}");
        assert!(text.contains("E_PATH_ESCAPE"), "{what}: {text}");
        assert_eq!(tree(env.root()), before, "{what}: something was written");
        assert_eq!(
            fs::read_to_string(&elsewhere).unwrap(),
            "not lodi's\n",
            "{what}: the link was followed"
        );

        // Recovery: the user moves what is in the way, and the apply completes.
        if fs::symlink_metadata(&target).unwrap().is_dir() {
            fs::remove_dir(&target).unwrap();
        } else {
            fs::remove_file(&target).unwrap();
        }
        let out = apply(&env);
        assert_eq!(out.status.code(), Some(0), "{what}: {}", stderr(&out));
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "set editing-mode vi\n",
            "{what}"
        );
    }
    contained(&env, &decoy);
}

// ----------------------------------------------------------------------- the rule, enforced ---

/// Design call D10, enforced rather than stated: neither this file nor `tests/concurrency.rs`
/// may wait on a wall clock. The name of that call is assembled here so that the scan cannot
/// match its own assertion.
#[test]
fn the_home_fault_cases_never_wait_on_a_clock() {
    let banned = concat!("sle", "ep");
    for name in ["tests/faults_home.rs", "tests/concurrency.rs"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        let hits: Vec<usize> = text
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains(banned))
            .map(|(i, _)| i + 1)
            .collect();
        assert!(
            hits.is_empty(),
            "{name} waits on a wall clock at line(s) {hits:?}; design call D10 forbids it"
        );
    }
}
