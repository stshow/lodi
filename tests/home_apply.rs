//! What 1.x's `lodi home apply` did, as the home part of `lodi switch --home` (LD-518; M-0.5 T-3, acceptance rows (a), (b), (c), (d), (e), (f), (g), (h), (i);
//! design calls D5, D6, D7, D13).
//!
//! Everything here runs the **built binary** in a throwaway set of roots through
//! `support::home_env`, which clears the environment and hands the child exactly the seven
//! variables the home scope may see — and panics if the `HOME` it would hand over were the
//! ambient one. No test of this repository ever runs the home scope against a real home
//! (design call D15, `LD-111`).
//!
//! The dangerous property is proved the only way that cannot be argued with: for the drift
//! refusal the **whole scratch root** is listed with every file's mode and sha256 before and
//! after, and the two listings are compared byte for byte. Every test also checks that the write
//! ledger names nothing outside its own root and that the fixed decoy tree beside the roots is
//! untouched.
//!
//! Offline and deterministic: no network, no Podman, and no name derived from a clock or a pid.

mod support;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::time::SystemTime;

use lodi::home::fsops::{self, RelPath};
use support::{HomeEnv, home_env, home_gate, home_part, nothing};

/// What `lodi switch --home` prints on standard error when somebody else holds a lock it waits for.
const WAITING: &str = "waiting for";

fn config_root(env: &HomeEnv) -> PathBuf {
    env.config().join("lodi")
}

fn write_manifest(env: &HomeEnv, text: &str) {
    let dir = config_root(env);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("home.toml"), text).unwrap();
}

fn run(env: &HomeEnv, args: &[&str]) -> Output {
    env.lodi(args).output().expect("the lodi binary runs")
}

fn apply(env: &HomeEnv) -> Output {
    run(env, &["home", "apply"])
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).expect("stderr is UTF-8")
}

fn ledger_under(env: &HomeEnv) -> Vec<(String, PathBuf)> {
    home_gate()
        .ledger_lines()
        .into_iter()
        .filter(|(_, path)| path.starts_with(env.root()))
        .collect()
}

/// (h) Every ledger line of this environment is inside its root, and the decoy tree beside the
/// roots is exactly as it was. Called at the end of every test here.
fn contained(env: &HomeEnv, decoy_before: &str) {
    for (op, path) in ledger_under(env) {
        assert!(
            path.starts_with(env.root()),
            "the ledger records {op} {} outside {}",
            path.display(),
            env.root().display()
        );
    }
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

/// Every path below `root`, relative, with its mode and sha256 (or `dir`, or a link's target),
/// sorted: an exact listing **and** the content.
fn tree(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, at: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(entries) = fs::read_dir(at) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap().display().to_string();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.file_type().is_symlink() {
                out.insert(
                    rel,
                    format!("link {}", fs::read_link(&path).unwrap().display()),
                );
            } else if meta.is_dir() {
                out.insert(rel, "dir".into());
                walk(root, &path, out);
            } else {
                use std::os::unix::fs::PermissionsExt;
                out.insert(
                    rel,
                    format!(
                        "{:04o} {}",
                        meta.permissions().mode() & 0o7777,
                        lodi::util::sha256_hex(&fs::read(&path).unwrap())
                    ),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// A tree listing without the switch's own records, which a refused switch writes too: its run
/// log, and `lodi.lock` in the config folder (as a flake's lock is written before it builds).
fn without_records(mut tree: BTreeMap<String, String>) -> BTreeMap<String, String> {
    tree.retain(|path, _| {
        path != "home/.local/state"
            && !path.starts_with("home/.local/state/")
            && path != "home/.config/lodi/lodi.lock"
    });
    tree
}

fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

fn mtime_of(path: &Path) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

/// Every path below `root`, relative, with its modification time — a directory as `dir`, a file
/// as its mtime in nanoseconds and its size. This is what "a second apply changes no file's
/// mtime" is measured over since `validator-repair-1`: the **whole** scratch root, the state
/// file and the lock included, rather than three chosen paths (`LD-186`).
fn mtimes(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, at: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(entries) = fs::read_dir(at) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap().display().to_string();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                out.insert(rel, "dir".into());
                walk(root, &path, out);
            } else {
                let at = meta
                    .modified()
                    .unwrap()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap();
                out.insert(rel, format!("{}ns {} bytes", at.as_nanos(), meta.len()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn state_of(env: &HomeEnv) -> serde_json::Value {
    let text = fs::read_to_string(state_path(env)).expect("the state file is written");
    // The state is canonical JSON: sorted keys, two-space indent, one trailing newline. A
    // round trip through the same writer the lock uses proves it byte for byte.
    let value: serde_json::Value = serde_json::from_str(&text).expect("the state file is JSON");
    assert_eq!(
        lodi::lock::canonical_json(&value),
        text,
        "the state file is not canonical JSON"
    );
    value
}

fn state_path(env: &HomeEnv) -> PathBuf {
    env.share().join("lodi/home-scope/state.json")
}

fn backups(env: &HomeEnv) -> Vec<String> {
    let dir = env.share().join("lodi/home-scope/backups");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name != "index.json")
        .collect();
    names.sort();
    names
}

/// The manifest the `several` fixture holds, inline, so a test can take one entry out of it
/// without editing a committed file.
const SEVERAL: &str = r#"[home]
version = "1"

[home.file.".config/git/ignore"]
text = ".lodi/\n"

[home.file.".gitconfig"]
text = "[core]\n\tautocrlf = input\n"

[home.file.".inputrc"]
text = "set editing-mode vi\n"
mode = "0600"
"#;

// ------------------------------------------------------------- (a) apply and re-apply ---

/// (a) An apply of a manifest with several files creates them with the declared modes and
/// contents; a second apply changes no file's mtime, prints `nothing to do`, and writes **only**
/// the state file — which the ledger proves, not the output text.
#[test]
fn several_files_are_created_and_a_second_apply_changes_nothing() {
    let env = home_env("apply-several");
    let decoy = env.decoy_listing();
    write_manifest(&env, SEVERAL);

    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(out.stdout.is_empty(), "{}", stdout(&out));
    assert_eq!(
        home_part(&out),
        "\
create    .config/git/ignore        0644
create    .gitconfig                0644
create    .inputrc                  0600
3 files: 3 create
"
    );

    let ignore = env.home().join(".config/git/ignore");
    let gitconfig = env.home().join(".gitconfig");
    let inputrc = env.home().join(".inputrc");
    assert_eq!(fs::read_to_string(&ignore).unwrap(), ".lodi/\n");
    assert_eq!(
        fs::read_to_string(&gitconfig).unwrap(),
        "[core]\n\tautocrlf = input\n"
    );
    assert_eq!(
        fs::read_to_string(&inputrc).unwrap(),
        "set editing-mode vi\n"
    );
    assert_eq!(mode_of(&ignore), 0o644);
    assert_eq!(mode_of(&inputrc), 0o600);
    // (D7) The parent directory an apply had to create is 0755 and is recorded.
    assert_eq!(mode_of(&env.home().join(".config/git")), 0o755);
    let state = state_of(&env);
    assert_eq!(state["version"], 5);
    // 2.0 records the config folder as the absolute path it resolved (1.x wrote `~/.config/lodi`).
    assert_eq!(
        state["source"],
        config_root(&env).display().to_string().as_str()
    );
    assert_eq!(state["directories"], serde_json::json!([".config/git"]));
    assert_eq!(state["files"][".inputrc"]["mode"], "0600");
    assert_eq!(state["files"][".inputrc"]["origin"]["kind"], "content");
    assert_eq!(state["files"][".inputrc"]["onRemove"], "restore");

    // The mtime of *everything* under the scratch root, not of three chosen files: the state
    // file, the lock, the backups and every directory are all in here (`LD-186`).
    let before = mtimes(env.root());
    for expected in [
        "home/.inputrc",
        "home/.gitconfig",
        "home/.config/git/ignore",
        "data/lodi/home-scope/state.json",
        "data/lodi/home-scope/.lock",
    ] {
        assert!(
            before.contains_key(expected),
            "the mtime comparison does not cover {expected}: {before:?}"
        );
    }
    assert_eq!(
        before["data/lodi/home-scope/state.json"],
        format!(
            "{}ns {} bytes",
            mtime_of(&state_path(&env))
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            fs::metadata(state_path(&env)).unwrap().len()
        ),
        "the state file is not the one the comparison measured"
    );
    let ledger_before = ledger_under(&env).len();

    let again = apply(&env);
    assert!(nothing(&again), "{}", stderr(&again));
    assert_eq!(
        mtimes(env.root()),
        before,
        "a second apply rewrote something under the scratch root; an empty plan over a current \
         state writes no file at all, state.json included"
    );

    // And the proof that does not depend on a clock at all: the write ledger gained **no** line
    // for the second run — not a write, not a mkdir, not a lock.
    let second: Vec<(String, PathBuf)> = ledger_under(&env).split_off(ledger_before);
    assert!(
        second.is_empty(),
        "a second apply recorded a mutation: {second:?}"
    );
    contained(&env, &decoy);
}

// --------------------------------------------------------------------- (b) the backup ---

/// (b) A pre-existing unmanaged file is copied into the backup store **before** it is replaced,
/// exactly once, and a second apply — even one that changes the declared bytes — makes no second
/// backup, so `restore` keeps pointing at the user's original (design call D5).
#[test]
fn an_unmanaged_file_is_backed_up_once_and_never_again() {
    let env = home_env("apply-backup");
    let decoy = env.decoy_listing();
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n\
         [home.file.\".inputrc\"]\ntext = \"first\\n\"\nmode = \"0600\"\n",
    );
    let inputrc = env.home().join(".inputrc");
    fs::write(&inputrc, "the user's own\n").unwrap();

    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        home_part(&out),
        "create    .inputrc                  0600    (backup: first unmanaged copy kept)\n\
         1 file: 1 create\n"
    );
    assert_eq!(fs::read_to_string(&inputrc).unwrap(), "first\n");
    let kept = backups(&env);
    assert_eq!(kept.len(), 1, "{kept:?}");
    let name = kept[0].clone();
    let stored = env.share().join("lodi/home-scope/backups").join(&name);
    assert_eq!(fs::read_to_string(&stored).unwrap(), "the user's own\n");
    // The name is derived from the path and the content and from nothing else (D13).
    assert!(
        name.starts_with(&lodi::util::sha256_hex(b".inputrc")),
        "{name}"
    );

    // A later apply of different bytes replaces Lodi's own file and keeps no second copy.
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n\
         [home.file.\".inputrc\"]\ntext = \"second\\n\"\nmode = \"0600\"\n",
    );
    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!stderr(&out).contains("backup:"), "{}", stderr(&out));
    assert_eq!(fs::read_to_string(&inputrc).unwrap(), "second\n");
    assert_eq!(
        backups(&env),
        vec![name.clone()],
        "a second backup was made"
    );
    assert_eq!(fs::read_to_string(&stored).unwrap(), "the user's own\n");
    contained(&env, &decoy);
}

// ------------------------------------------------------------ (c) the entry disappears ---

/// (c) Removing an entry does what the `on_remove` recorded **with the file** said at the apply
/// that wrote it (design call D5): `restore` puts the original bytes and mode back, `delete`
/// removes the file, `keep` leaves it and only drops the state row.
#[test]
fn a_removed_entry_is_restored_deleted_or_kept_as_it_was_recorded() {
    let env = home_env("apply-on-remove");
    let decoy = env.decoy_listing();
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n\
         [home.file.\"restored\"]\ntext = \"lodi\\n\"\non_remove = \"restore\"\n\n\
         [home.file.\"deleted\"]\ntext = \"lodi\\n\"\non_remove = \"delete\"\n\n\
         [home.file.\"kept\"]\ntext = \"lodi\\n\"\non_remove = \"keep\"\n",
    );
    // The `restore` path had a file of the user's own, at a mode of their own.
    let restored = env.home().join("restored");
    fs::write(&restored, "the original\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&restored, fs::Permissions::from_mode(0o640)).unwrap();

    assert_eq!(apply(&env).status.code(), Some(0));
    assert_eq!(fs::read_to_string(&restored).unwrap(), "lodi\n");
    assert_eq!(mode_of(&restored), 0o644);

    // Every entry disappears at once.
    write_manifest(&env, "[home]\nversion = \"1\"\n");
    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = home_part(&out);
    assert!(
        text.contains("restore   restored                  0640    (entry removed"),
        "{text}"
    );
    assert!(
        text.contains("remove    deleted                   0644    (entry removed"),
        "{text}"
    );
    assert!(
        text.contains("keep      kept                      0644    (entry removed"),
        "{text}"
    );

    assert_eq!(
        fs::read_to_string(&restored).unwrap(),
        "the original\n",
        "restore did not put the user's bytes back"
    );
    assert_eq!(
        mode_of(&restored),
        0o640,
        "restore did not put the mode back"
    );
    assert!(!env.home().join("deleted").exists(), "delete left the file");
    assert_eq!(
        fs::read_to_string(env.home().join("kept")).unwrap(),
        "lodi\n",
        "keep removed the file"
    );
    // Every state row is gone, and no directory was ever removed.
    let state = state_of(&env);
    assert_eq!(state["files"], serde_json::json!({}));

    // An entry that disappears with `restore` and no backup is a removal that says so.
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[home.file.\"never-had-one\"]\ntext = \"x\\n\"\n",
    );
    assert_eq!(apply(&env).status.code(), Some(0));
    write_manifest(&env, "[home]\nversion = \"1\"\n");
    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        home_part(&out)
            .contains("remove    never-had-one             0644    (entry removed: no backup"),
        "{}",
        stderr(&out)
    );
    assert!(!env.home().join("never-had-one").exists());
    contained(&env, &decoy);
}

// ---------------------------------------------------------------------- (d) the drift ---

/// (d) A hand-edited managed file makes `apply` exit 8 with `E_DRIFT` having written **nothing**:
/// the whole scratch root is byte-identical before and after (design call D6).
#[test]
fn a_hand_edited_file_stops_the_whole_apply_and_nothing_is_written() {
    let env = home_env("apply-drift");
    let decoy = env.decoy_listing();
    write_manifest(&env, SEVERAL);
    assert_eq!(apply(&env).status.code(), Some(0));

    // The user edits one of the three by hand, and adds a fourth entry that would be created.
    fs::write(env.home().join(".gitconfig"), "[user]\n\tname = me\n").unwrap();
    write_manifest(
        &env,
        &format!("{SEVERAL}\n[home.file.\".profile-of-mine\"]\ntext = \"export X=1\\n\"\n"),
    );

    let before = without_records(tree(env.root()));
    let ledger_before = ledger_under(&env).len();
    let out = apply(&env);
    let text = stderr(&out);
    assert_eq!(out.status.code(), Some(8), "{text}");
    assert!(
        text.contains("lodi: warning W_DRIFT: .gitconfig has changed"),
        "{text}"
    );
    assert!(
        text.contains("recorded ") && text.contains("now "),
        "{text}"
    );
    assert!(text.contains("lodi: error E_DRIFT"), "{text}");
    assert!(text.contains("nothing was applied"), "{text}");
    assert!(
        text.contains("hint: lodi switch --home --overwrite-drift takes them back"),
        "{text}"
    );

    assert_eq!(
        without_records(tree(env.root())),
        before,
        "the refused apply changed the tree"
    );
    assert_eq!(
        ledger_under(&env).len(),
        ledger_before,
        "the refused apply appended to the write ledger"
    );
    assert!(
        !env.home().join(".profile-of-mine").exists(),
        "the refused apply created the undrifted entry anyway"
    );
    contained(&env, &decoy);
}

/// (e) `--overwrite-drift` takes the path back and keeps the drifted bytes in the backup store,
/// beside — never instead of — the original copy of that path.
#[test]
fn overwrite_drift_takes_the_path_back_and_keeps_the_edited_bytes() {
    let env = home_env("apply-overwrite-drift");
    let decoy = env.decoy_listing();
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[home.file.\".inputrc\"]\ntext = \"lodi's own\\n\"\n",
    );
    fs::write(env.home().join(".inputrc"), "the original\n").unwrap();
    assert_eq!(apply(&env).status.code(), Some(0));
    let original = backups(&env);
    assert_eq!(original.len(), 1, "{original:?}");

    fs::write(env.home().join(".inputrc"), "edited by hand\n").unwrap();
    let out = run(&env, &["home", "apply", "--overwrite-drift"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        fs::read_to_string(env.home().join(".inputrc")).unwrap(),
        "lodi's own\n"
    );

    let kept = backups(&env);
    assert_eq!(kept.len(), 2, "{kept:?}");
    let drift: Vec<&String> = kept.iter().filter(|n| n.contains(".drift-")).collect();
    assert_eq!(drift.len(), 1, "{kept:?}");
    let bytes =
        fs::read_to_string(env.share().join("lodi/home-scope/backups").join(drift[0])).unwrap();
    assert_eq!(bytes, "edited by hand\n");
    // The original is exactly as it was: a take-back never replaces it (design call D5).
    let stored = env
        .share()
        .join("lodi/home-scope/backups")
        .join(&original[0]);
    assert_eq!(fs::read_to_string(&stored).unwrap(), "the original\n");
    contained(&env, &decoy);
}

// --------------------------------------------------------------------- (f) state=absent ---

/// (f) `state = "absent"` removes a managed file and backs up an unmanaged one first.
#[test]
fn an_absent_entry_removes_a_managed_file_and_backs_up_an_unmanaged_one() {
    let env = home_env("apply-absent");
    let decoy = env.decoy_listing();
    // An unmanaged file the user had: declared absent, it is copied away and removed.
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[home.file.\".netrc\"]\nstate = \"absent\"\nbackup = true\n",
    );
    fs::write(env.home().join(".netrc"), "machine example\n").unwrap();
    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        home_part(&out).contains("remove    .netrc                    0644    (backup: first"),
        "{}",
        stderr(&out)
    );
    assert!(
        !env.home().join(".netrc").exists(),
        "the file is still there"
    );
    assert_eq!(backups(&env).len(), 1);

    // A file Lodi wrote, then declared absent, is simply removed and stops being managed.
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[home.file.\"managed\"]\ntext = \"x\\n\"\n",
    );
    assert_eq!(apply(&env).status.code(), Some(0));
    assert!(env.home().join("managed").exists());
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[home.file.\"managed\"]\nstate = \"absent\"\n",
    );
    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        home_part(&out),
        "remove    managed                   0644\n1 file: 1 remove\n"
    );
    // A file lodi wrote holds lodi's own bytes, so removing it keeps no copy: the backup store
    // is for the user's originals only (design call D5).
    assert!(
        !stderr(&out).contains("backup:"),
        "a managed file was copied into the backup store: {}",
        stderr(&out)
    );
    assert!(!env.home().join("managed").exists());
    assert_eq!(state_of(&env)["files"], serde_json::json!({}));
    // A second apply has nothing left to do and keeps no second copy.
    let again = apply(&env);
    assert!(nothing(&again), "{}", stderr(&again));
    assert_eq!(backups(&env).len(), 1, "{:?}", backups(&env));
    contained(&env, &decoy);
}

// ------------------------------------------------------------------- (g) the symlink ---

/// (g) A path whose parent is a symbolic link is `E_PATH_ESCAPE` and nothing is written — not
/// the entry itself, not the entries beside it, and not the state file.
#[test]
fn a_path_whose_parent_is_a_symlink_is_refused_and_nothing_is_written() {
    let env = home_env("apply-escape");
    let decoy = env.decoy_listing();
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n\
         [home.file.\".config/git/ignore\"]\ntext = \".lodi/\\n\"\n\n\
         [home.file.\"zzz-innocent\"]\ntext = \"x\\n\"\n",
    );
    let elsewhere = env.root().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("ignore"), "not lodi's\n").unwrap();
    std::os::unix::fs::symlink(&elsewhere, env.home().join(".config/git")).unwrap();

    let before = without_records(tree(env.root()));
    let out = apply(&env);
    let text = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{text}");
    assert!(text.contains("lodi: error E_PATH_ESCAPE"), "{text}");
    assert!(text.contains("symbolic link"), "{text}");
    assert_eq!(
        without_records(tree(env.root())),
        before,
        "the refused apply changed the tree; a refusal creates nothing, not even its own lock"
    );
    assert_eq!(
        fs::read_to_string(elsewhere.join("ignore")).unwrap(),
        "not lodi's\n",
        "the file the link points at was written through"
    );
    assert!(
        !state_path(&env).exists(),
        "a refused apply wrote a state file"
    );
    contained(&env, &decoy);
}

// ------------------------------------------------------------------ (i) two at once ---

/// (i) Two `lodi switch --home` processes contend, the first for `<data>/home-scope/.lock` and the
/// second for the config's lock that the first holds meanwhile (LD-524), **deterministically**
/// (`LD-187`): the test takes the exact lock the binary takes — through
/// `fsops::lock`, the data root plus `home-scope/.lock` — before either process starts, so both
/// are provably blocked on it rather than merely fast enough to miss each other. While the
/// barrier is held both processes are still running, both have reported the contention on
/// standard error and no state file exists. When it is released both exit 0 and the state is
/// canonical JSON naming every declared file.
#[test]
fn two_applies_started_together_serialize_on_the_lock() {
    let env = home_env("apply-concurrent");
    let state_path = env.share().join("lodi/home-scope/state.json");
    let decoy = env.decoy_listing();
    write_manifest(&env, SEVERAL);

    // The barrier: `<data>/home-scope/` and the lock itself, taken exclusively by this test
    // through the one entry point the binary uses.
    let data = lodi::roots::Roots::from_vars(
        Some(std::ffi::OsString::from(env.home()).as_os_str()),
        None,
        None,
        Some(std::ffi::OsString::from(env.share().join("lodi")).as_os_str()),
    )
    .expect("the scratch roots")
    .data_root();
    let scope = RelPath::new(lodi::home::state::HOME_SCOPE_DIR).unwrap();
    let lock = RelPath::new(lodi::home::state::LOCK_FILE).unwrap();
    fsops::mkdir_p(&data, &scope).expect("the scope directory");
    let barrier = fsops::lock(&data, &lock, true).expect("the home lock, held by the test");

    // Both processes, with their standard error in a file each so that it can be read *while*
    // they are blocked.
    let logs = env.root().join("contention");
    fs::create_dir_all(&logs).unwrap();
    let mut children: Vec<(PathBuf, Child)> = ["first", "second"]
        .iter()
        .map(|name| {
            let log = logs.join(format!("{name}.err"));
            let child = env
                .lodi(&["home", "apply"])
                .stderr(Stdio::from(fs::File::create(&log).unwrap()))
                .stdout(Stdio::piped())
                .spawn()
                .unwrap_or_else(|e| panic!("the {name} apply starts: {e}"));
            (log, child)
        })
        .collect();

    // Both are blocked: each has printed the contention line, and neither has exited. The wait
    // is bounded by the suite's shared ceiling (`tests/support/wait.rs`) rather than by a minute
    // of its own, because two real child processes here compete with every other lane on the
    // machine. Nothing about the proof moves: a process that exited while the barrier is held
    // still fails at once, inside the condition.
    wait::until("the two applies to report contention on the lock", || {
        let waiting = children.iter().all(|(log, _)| {
            fs::read_to_string(log)
                .unwrap_or_default()
                .contains(WAITING)
        });
        for (log, child) in &mut children {
            assert!(
                child.try_wait().unwrap().is_none(),
                "an apply exited while the test held the lock: {}",
                fs::read_to_string(log).unwrap_or_default()
            );
        }
        waiting
    });
    // Blocked means blocked: nothing has been applied, and there is no state file yet.
    assert!(!state_path.exists(), "a blocked apply wrote the state file");
    assert!(
        !env.home().join(".inputrc").exists(),
        "a blocked apply wrote a managed file"
    );

    // Release the barrier; the two now serialize on it.
    drop(barrier);
    let mut outputs = Vec::new();
    for (log, child) in children {
        let out = child.wait_with_output().unwrap();
        let text = fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(out.status.code(), Some(0), "{text}");
        assert!(
            text.contains(WAITING),
            "the contention line is gone from the run's own standard error: {text}"
        );
        outputs.push(text);
    }
    // One of them did the work and the other found nothing to do: they serialized, they did not
    // both apply the same plan.
    assert!(
        outputs
            .iter()
            .any(|text| text.contains("nothing to switch")),
        "neither apply saw the other's work: {outputs:?}"
    );

    let text = fs::read_to_string(&state_path).expect("the state file");
    let state: serde_json::Value = serde_json::from_str(&text).expect("the state is JSON");
    let files = state["files"].as_object().expect("files is an object");
    assert_eq!(files.len(), 3, "{files:?}");
    for path in [".config/git/ignore", ".gitconfig", ".inputrc"] {
        assert!(files.contains_key(path), "{path} is not in the state");
        assert!(env.home().join(path).is_file(), "{path} was not written");
    }
    assert_eq!(
        fs::read_to_string(env.home().join(".inputrc")).unwrap(),
        "set editing-mode vi\n"
    );
    assert!(
        env.share().join("lodi/home-scope/.lock").exists(),
        "the lock the two processes serialized on is under the data root"
    );
    contained(&env, &decoy);
}

// ----------------------------------------------------------------- what apply must not ---

/// A manifest that names no `home.toml` at all is `E_NO_MANIFEST` at exit 3, and the apply
/// creates nothing — not the configuration directory, not the data directory.
#[test]
fn an_apply_without_a_manifest_creates_nothing_of_its_own() {
    let env = home_env("apply-no-manifest");
    let decoy = env.decoy_listing();
    let out = apply(&env);
    let text = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{text}");
    assert!(text.contains("lodi: error E_NO_MANIFEST"), "{text}");
    assert!(
        !config_root(&env).exists(),
        "the configuration root was created"
    );
    assert!(!state_path(&env).exists(), "a state file was written");
    contained(&env, &decoy);
}

/// No shell configuration file is ever touched, and no directory is ever removed: an apply that
/// creates a directory for an entry, then loses that entry, leaves the directory behind
/// (design call D7) and never goes near an rc file.
#[test]
fn no_directory_is_removed_and_no_rc_file_is_touched() {
    let env = home_env("apply-directories");
    let decoy = env.decoy_listing();
    for rc in [
        ".bashrc",
        ".zshrc",
        ".profile",
        ".bash_profile",
        "config.fish",
    ] {
        fs::write(env.home().join(rc), format!("# {rc}, the user's own\n")).unwrap();
    }
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n\
         [home.file.\"a/b/c/file\"]\ntext = \"x\\n\"\non_remove = \"delete\"\n",
    );
    assert_eq!(apply(&env).status.code(), Some(0));
    assert!(env.home().join("a/b/c/file").exists());
    assert_eq!(
        state_of(&env)["directories"],
        serde_json::json!(["a", "a/b", "a/b/c"])
    );

    write_manifest(&env, "[home]\nversion = \"1\"\n");
    assert_eq!(apply(&env).status.code(), Some(0));
    assert!(!env.home().join("a/b/c/file").exists(), "the file stayed");
    assert!(
        env.home().join("a/b/c").is_dir(),
        "a directory lodi created was removed"
    );

    for rc in [
        ".bashrc",
        ".zshrc",
        ".profile",
        ".bash_profile",
        "config.fish",
    ] {
        assert_eq!(
            fs::read_to_string(env.home().join(rc)).unwrap(),
            format!("# {rc}, the user's own\n"),
            "{rc} was touched"
        );
    }
    let named: Vec<PathBuf> = ledger_under(&env)
        .into_iter()
        .map(|(_, path)| path)
        .filter(|path| {
            path.file_name().is_some_and(|n| {
                [
                    ".bashrc",
                    ".zshrc",
                    ".profile",
                    ".bash_profile",
                    "config.fish",
                ]
                .contains(&&*n.to_string_lossy())
            })
        })
        .collect();
    assert!(
        named.is_empty(),
        "the ledger names a shell rc file: {named:?}"
    );
    contained(&env, &decoy);
}

/// A mode that differs from the declaration is simply set back: it destroys nothing, so it is
/// not drift and needs no confirmation (design call D6).
#[test]
fn a_changed_mode_is_reset_without_being_drift() {
    let env = home_env("apply-mode");
    let decoy = env.decoy_listing();
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[home.file.\".inputrc\"]\ntext = \"x\\n\"\nmode = \"0600\"\n",
    );
    assert_eq!(apply(&env).status.code(), Some(0));
    let inputrc = env.home().join(".inputrc");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&inputrc, fs::Permissions::from_mode(0o666)).unwrap();

    let out = apply(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!stderr(&out).contains("W_DRIFT"), "{}", stderr(&out));
    assert_eq!(mode_of(&inputrc), 0o600, "the mode was not set back");
    assert_eq!(fs::read_to_string(&inputrc).unwrap(), "x\n");
    contained(&env, &decoy);
}

// ------------------------------------------------------- the scope's own modes (LD-361) ---

/// The home scope's own files hold digests of the user's private originals, and its backups hold
/// the originals themselves: under `umask 000` the scope directory and the backup store are 0700
/// and the state, the index and every backup 0600. The modes are explicit, never the umask's.
#[test]
fn the_scope_directories_and_files_have_explicit_modes_under_umask_000() {
    let env = home_env("apply-umask");
    let decoy = env.decoy_listing();
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[home.file.\".inputrc\"]\ntext = \"x\\n\"\n",
    );
    fs::write(env.home().join(".inputrc"), "the user's own\n").unwrap();

    let mut command = env.command_under_umask("000");
    let out = env
        .on_gate(&mut command, &["home", "apply"])
        .stdin(Stdio::null())
        .output()
        .expect("the lodi binary runs under sh");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let scope = env.share().join("lodi/home-scope");
    assert_eq!(mode_of(&scope), 0o700, "home-scope/");
    assert_eq!(
        mode_of(&scope.join("backups")),
        0o700,
        "home-scope/backups/"
    );
    assert_eq!(mode_of(&state_path(&env)), 0o600, "state.json");
    assert_eq!(
        mode_of(&scope.join("backups/index.json")),
        0o600,
        "index.json"
    );
    let kept = backups(&env);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(mode_of(&scope.join("backups").join(&kept[0])), 0o600);
    // The declared file still gets exactly its declared mode.
    assert_eq!(mode_of(&env.home().join(".inputrc")), 0o644);
    contained(&env, &decoy);
}
