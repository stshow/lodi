//! The store's layout marker and its migration (M-1.0 T-2, acceptance rows (a)…(f); design calls
//! D7, D8, D18).
//!
//! Every store here lives under `CARGO_TARGET_TMPDIR`, allocated by `support::home_env`, which
//! clears the environment and hands the child exactly the variables Lodi may see — and panics if
//! the `HOME` it would hand over were the ambient one. No test of this repository opens the real
//! user store, and nothing here migrates anything but a store it made itself
//! (design call D15, `LD-111`).
//!
//! The layout-0 store is not fabricated: `tests/fixtures/layout/store-0.3.0` is a tree `lodi`
//! 0.3.0 actually built, restored from its recorded census because git stores neither an empty
//! directory nor a directory's mode (that fixture's `README.md` records how it was made and what
//! it does not cover).
//!
//! Offline and deterministic: no network, no container runtime, and the two-process contention is
//! forced by taking the exact lock before either binary starts (the `LD-187` idiom) — never by a
//! sleep and never by hoping two processes collide.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};

use lodi::layout::{self, MARKER, Outcome};
use lodi::store::Store;
use support::{HomeEnv, home_env, home_gate};

/// The line the store lock's waiter prints on standard error.
const WAITING: &str = "waiting for the store lock";

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/layout/store-0.3.0"
);

/// The entry `lodi` 0.3.0 realized into the fixture store.
const ENTRY: &str = "env-7672c4f3884f47f3522700d325defa3e";

// --------------------------------------------------------- the recorded layout-0 store ---

struct Row {
    dir: bool,
    mode: u32,
    path: String,
}

/// `CENSUS.tsv`: `<type>\t<mode>\t<path>`, one row per directory and file of the recorded store.
fn recorded() -> Vec<Row> {
    let text = fs::read_to_string(Path::new(FIXTURE).join("CENSUS.tsv")).expect("the census");
    let rows: Vec<Row> = text
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(|line| {
            let mut fields = line.split('\t');
            let kind = fields.next().expect("a type");
            let mode = fields.next().expect("a mode");
            let path = fields.next().expect("a path");
            assert!(fields.next().is_none(), "a census row has three fields");
            Row {
                dir: match kind {
                    "d" => true,
                    "f" => false,
                    other => panic!("the census records only `d` and `f`, not {other:?}"),
                },
                mode: u32::from_str_radix(mode, 8).expect("an octal mode"),
                path: path.to_string(),
            }
        })
        .collect();
    assert!(!rows.is_empty(), "the census is empty");
    rows
}

/// Restore the recorded 0.3.0 store into `home`, exactly as 0.3.0 left it.
///
/// The census is the authority: every `d` row is created, every `f` row is copied out of `tree/`,
/// and the recorded modes are applied from the deepest path upwards — reverse lexicographic order
/// puts a child before its parent — so the sealed entry (`0555`) and its plan (`0444`) come back
/// read-only. The census and `tree/` must agree in **both** directions, so neither a file added
/// to the fixture without a row nor a row without a file passes unnoticed.
fn restore(home: &Path) {
    let rows = recorded();
    let tree = Path::new(FIXTURE).join("tree");
    fs::create_dir_all(home).expect("the store root");
    for row in rows.iter().filter(|r| r.dir) {
        fs::create_dir_all(home.join(&row.path)).expect("a recorded directory");
    }
    for row in rows.iter().filter(|r| !r.dir) {
        fs::copy(tree.join(&row.path), home.join(&row.path)).expect("a recorded file");
    }

    let from_census: BTreeSet<&str> = rows
        .iter()
        .filter(|r| !r.dir)
        .map(|r| r.path.as_str())
        .collect();
    let mut in_tree = BTreeSet::new();
    let mut stack = vec![tree.clone()];
    while let Some(at) = stack.pop() {
        for entry in fs::read_dir(&at).expect("the fixture tree").flatten() {
            let path = entry.path();
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                stack.push(path);
            } else {
                in_tree.insert(
                    path.strip_prefix(&tree)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    let in_tree: BTreeSet<&str> = in_tree.iter().map(String::as_str).collect();
    assert_eq!(
        from_census, in_tree,
        "the fixture census and the fixture tree disagree"
    );

    let mut ordered: Vec<&Row> = rows.iter().collect();
    ordered.sort_by(|a, b| b.path.cmp(&a.path));
    for row in ordered {
        fs::set_permissions(home.join(&row.path), fs::Permissions::from_mode(row.mode))
            .expect("a recorded mode");
    }
    assert!(
        !home.join(MARKER).exists(),
        "the recorded 0.3.0 store carries a layout marker"
    );
}

/// The project the fixture store was realized from, written where the binary will be run.
fn restore_project(env: &HomeEnv) {
    for name in ["lodi.toml", "lodi.lock"] {
        fs::copy(
            Path::new(FIXTURE).join("project").join(name),
            env.root().join(name),
        )
        .expect("a recorded project file");
    }
}

/// `$LODI_HOME` of a [`HomeEnv`]: the same path `env.command()` hands the child.
fn store_home(env: &HomeEnv) -> PathBuf {
    env.data().join("lodi")
}

/// [`HomeEnv::command`] plus `LODI_TRUST=1`, which authorizes the fixture project's one task for
/// exactly one invocation and records nothing (ADR-014). The store, not the trust gate, is what
/// these tests are about; a command that is refused for want of trust never reaches the store at
/// all, which is why the refusals below are checked with this set.
fn lodi(env: &HomeEnv) -> std::process::Command {
    let mut command = env.command();
    command.env("LODI_TRUST", "1").stdin(Stdio::null());
    command
}

// ------------------------------------------------------------------------ the census ---

/// Everything at or below `root`: path → (shape, mtime). The shape is the type, mode, size and
/// content digest; the mtime is separate so that a comparison can be made either way. `"."` is
/// the root itself, so that a write *into* it cannot hide behind its own directory entry.
fn census(root: &Path) -> BTreeMap<String, (String, String)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(at) = stack.pop() {
        let meta = fs::symlink_metadata(&at).expect("a store path");
        let rel = if at == root {
            ".".to_string()
        } else {
            at.strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        let kind = meta.file_type();
        let shape = if kind.is_dir() {
            stack.extend(
                fs::read_dir(&at)
                    .expect("a store directory")
                    .flatten()
                    .map(|e| e.path()),
            );
            format!("dir mode={:04o}", meta.mode() & 0o7777)
        } else if kind.is_symlink() {
            format!("link -> {}", fs::read_link(&at).unwrap().to_string_lossy())
        } else {
            format!(
                "file mode={:04o} size={} sha256={}",
                meta.mode() & 0o7777,
                meta.len(),
                lodi::util::sha256_hex(&fs::read(&at).expect("a store file"))
            )
        };
        out.insert(
            rel,
            (shape, format!("{}.{:09}", meta.mtime(), meta.mtime_nsec())),
        );
    }
    out
}

fn shapes(census: &BTreeMap<String, (String, String)>) -> BTreeMap<&str, &str> {
    census
        .iter()
        .map(|(path, (shape, _))| (path.as_str(), shape.as_str()))
        .collect()
}

fn marker_of(home: &Path) -> serde_json::Value {
    let bytes = fs::read(home.join(MARKER)).expect("the layout marker");
    serde_json::from_slice(&bytes).expect("the layout marker is JSON")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// ------------------------------------------------------- (a) migrated on the first write ---

/// (a) A store `lodi` 0.3.0 built is migrated to layout 1 by the first command that writes into
/// it, and the environment it already holds still enters: `lodi run hello` runs the task inside
/// the realized entry and prints what the manifest's `[env]` put there. The entry is **reused**,
/// not realized again — its plan and its sidecar are byte for byte the ones 0.3.0 wrote — and the
/// marker the migration leaves is 0644, canonical, and names this build.
#[test]
fn a_zero_three_zero_store_is_migrated_on_the_first_write_and_still_enters() {
    let env = home_env("layout-migrate");
    let decoy = env.decoy_listing();
    let home = store_home(&env);
    restore(&home);
    restore_project(&env);
    let before = census(&home);

    let out = lodi(&env)
        .args(["run", "hello"])
        .output()
        .expect("lodi run starts");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("hello"),
        "the task did not run: {:?}",
        stdout(&out)
    );

    // Entered, not merely opened: the environment pairs of the plan 0.3.0 realized are in force
    // inside the process the migrated store hands over to.
    let out = lodi(&env)
        .args(["develop", "--", "/bin/sh", "-c", "echo LAYOUT=$LAYOUT"])
        .output()
        .expect("lodi develop starts");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("LAYOUT=1"),
        "the recorded environment did not enter: {:?}",
        stdout(&out)
    );

    // The marker: layout 1, this build, 0644.
    let marker = marker_of(&home);
    assert_eq!(marker["layout"], serde_json::json!(layout::CURRENT));
    assert_eq!(
        marker["lodiVersion"],
        serde_json::json!(lodi::schema::LODI_VERSION)
    );
    assert!(
        marker["written"].as_str().is_some_and(|w| w.ends_with('Z')),
        "the marker records no UTC time: {marker}"
    );
    assert_eq!(
        fs::metadata(home.join(MARKER)).unwrap().mode() & 0o7777,
        0o644
    );
    assert_eq!(
        fs::read_to_string(home.join(MARKER)).unwrap(),
        lodi::lock::canonical_json(
            &serde_json::from_value::<lodi::layout::Marker>(marker).unwrap()
        ),
        "the marker on disk is not what the canonical writer produces"
    );

    // Nothing 0.3.0 wrote was moved, renamed, rewritten or removed: the entry is the same entry,
    // and its plan and sidecar are the recorded bytes.
    let after = census(&home);
    for path in [
        format!("store/{ENTRY}/env.json"),
        format!("store/.meta/{ENTRY}.json"),
    ] {
        assert_eq!(
            before.get(&path),
            after.get(&path),
            "{path} changed under the migration"
        );
        assert_eq!(
            fs::read(home.join(&path)).unwrap(),
            fs::read(Path::new(FIXTURE).join("tree").join(&path)).unwrap(),
            "{path} is no longer the bytes 0.3.0 wrote"
        );
    }
    let entries: Vec<&str> = after
        .keys()
        .filter(|p| p.starts_with("store/env-"))
        .map(String::as_str)
        .collect();
    assert_eq!(
        entries
            .iter()
            .filter(|p| p.matches('/').count() == 1)
            .count(),
        1,
        "the environment was realized again instead of entered: {entries:?}"
    );
    assert!(
        after.contains_key(&format!("store/{ENTRY}")),
        "the recorded entry is gone: {entries:?}"
    );
    for path in before.keys() {
        assert!(after.contains_key(path), "{path} was removed");
    }
    assert_eq!(env.decoy_listing(), decoy, "the decoy tree was touched");
}

// --------------------------------------------------------------- (b) idempotent ---

/// (b) The migration is idempotent. The first one adds `.layout.json` and **only** that: the
/// shape of every other path under `$LODI_HOME` — type, mode, size and sha256 — is unchanged. The
/// second writes nothing at all: a full census including every mtime to the nanosecond, and the
/// store root's own mtime with it, is identical before and after, and the migration reports that
/// the store was already current rather than doing the work twice.
#[test]
fn a_second_migration_writes_nothing_and_changes_no_mtime() {
    let env = home_env("layout-idempotent");
    let home = store_home(&env);
    restore(&home);
    let before = census(&home);

    let store = Store::open(&home).expect("the recorded store opens");
    assert_eq!(
        layout::migrate(&store).expect("the first migration"),
        Outcome::Migrated {
            from: layout::UNMARKED
        }
    );
    let once = census(&home);

    // The only shape that changed is the marker's, which was not there before.
    let (added, kept): (Vec<&str>, Vec<&str>) = shapes(&once)
        .into_keys()
        .partition(|path| !before.contains_key(*path));
    assert_eq!(
        added,
        vec![MARKER],
        "the migration added more than the marker"
    );
    for path in kept {
        assert_eq!(
            shapes(&before).get(path),
            shapes(&once).get(path),
            "{path} was changed by a migration that only adds"
        );
    }
    assert_eq!(
        before.len() + 1,
        once.len(),
        "the migration removed or added a path"
    );

    // The second migration is not a write of any kind.
    assert_eq!(
        layout::migrate(&store).expect("the second migration"),
        Outcome::Current
    );
    assert_eq!(
        once,
        census(&home),
        "a second migration changed a file or an mtime"
    );
    // And neither is opening the store for writing again, which is how a command reaches it.
    Store::open_for_write(&home).expect("the migrated store opens for writing");
    assert_eq!(
        once,
        census(&home),
        "opening the migrated store for writing changed a file or an mtime"
    );
}

// ------------------------------------------------------ (c) two processes, one store ---

/// (c) Two processes migrating the same store serialize on `store/.lock` **deterministically**
/// (`LD-187`): the test takes the exact exclusive lock the migration takes — through
/// `lodi::store::Store`, the store root plus `store/.lock` — before either binary starts, so both
/// are provably blocked on it rather than merely fast enough to miss each other. Each is required
/// to report the contention on standard error and to still be running while the lock is held; the
/// wait for that report is a blocking read of the child's own standard error, so nothing here
/// sleeps on a wall clock. Only then is the lock released, and then both exit 0 and the store is
/// at layout 1 exactly once.
#[test]
fn two_writers_started_together_serialize_on_the_store_lock() {
    let env = home_env("layout-concurrent");
    let decoy = env.decoy_listing();
    let home = store_home(&env);

    // The barrier: the store's directories and the exclusive `store/.lock`, taken by this test
    // through the one entry point the migration uses. The store is empty, so the collector that
    // runs behind the migration has nothing to sweep.
    let store = Store::open(&home).expect("the scratch store");
    let barrier = store
        .exclusive_lock()
        .expect("the store lock, held by the test");
    assert!(
        !home.join(MARKER).exists(),
        "an unmigrated store has a marker"
    );

    let mut children: Vec<(&str, Child, BufReader<std::process::ChildStderr>)> =
        ["first", "second"]
            .iter()
            .map(|name| {
                let mut child = env
                    .command()
                    .arg("gc")
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap_or_else(|e| panic!("the {name} collector starts: {e}"));
                let err = BufReader::new(child.stderr.take().expect("the child's standard error"));
                (*name, child, err)
            })
            .collect();

    // Both are blocked. `read_line` returns when the child has written its first line — or at
    // once, empty, if it exited without writing one, which fails below rather than hanging.
    for (name, child, err) in &mut children {
        let mut line = String::new();
        err.read_line(&mut line).expect("the child's first line");
        assert!(
            line.contains(WAITING),
            "the {name} collector's first line is not the contention line: {line:?}"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "the {name} collector exited while the test held the store lock"
        );
    }
    // Blocked means blocked: neither has migrated anything.
    assert!(
        !home.join(MARKER).exists(),
        "a blocked writer wrote the layout marker"
    );

    // Release it; the two now serialize on it.
    drop(barrier);
    for (name, mut child, mut err) in children {
        let mut rest = String::new();
        err.read_to_string(&mut rest)
            .expect("the rest of the child's standard error");
        let status = child.wait().expect("the child exits");
        assert_eq!(status.code(), Some(0), "the {name} collector: {rest}");
    }

    assert_eq!(
        marker_of(&home)["layout"],
        serde_json::json!(layout::CURRENT)
    );
    assert_eq!(
        layout::read(&home).expect("the marker reads"),
        layout::CURRENT
    );
    assert_eq!(env.decoy_listing(), decoy, "the decoy tree was touched");
}

// ---------------------------------------------- (d) a layout this build does not know ---

/// (d) A marker from the future is `E_STORE_VERSION` at exit 6, and the refusal names the layout
/// found, the layout this build knows and the Lodi that wrote the marker. **Nothing** under the
/// store is opened or written: not an entry, not a lock, not even the directories `Store::open`
/// creates for every other command — the layout is read before the store is opened at all. A
/// marker that records no layout number is the same code, saying plainly that it records none; one
/// that is not JSON is `E_STORE_IO`, at the same status.
#[test]
fn a_store_from_the_future_is_refused_and_nothing_below_it_is_touched() {
    let env = home_env("layout-future");
    let decoy = env.decoy_listing();
    let home = store_home(&env);

    for (body, code, wanted) in [
        (
            r#"{"layout":9,"lodiVersion":"9.9.9","written":"2026-09-21T00:00:00Z"}"#,
            "E_STORE_VERSION",
            vec!["has layout version 9", "9.9.9"],
        ),
        (
            r#"{"layout":"one","lodiVersion":"9.9.9","written":"2026-09-21T00:00:00Z"}"#,
            "E_STORE_VERSION",
            vec!["records no numeric `layout`", "9.9.9"],
        ),
        (
            r#"{"layout":9}"#,
            "E_STORE_VERSION",
            vec![
                "has layout version 9",
                "does not record which lodi wrote it",
            ],
        ),
        (
            "this is not a document\n",
            "E_STORE_IO",
            vec!["the store layout marker is not JSON"],
        ),
    ] {
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join(MARKER), body).unwrap();
        let untouched = census(&home);

        for args in [&["gc"][..], &["run", "hello"]] {
            restore_project(&env);
            let out = lodi(&env).args(args).output().expect("lodi starts");
            let text = stderr(&out);
            assert_eq!(
                out.status.code(),
                Some(i32::from(lodi::diag::exit_status(code))),
                "{args:?} on {body}: {text}"
            );
            assert_eq!(
                lodi::diag::exit_status(code),
                6,
                "the status of {code} moved"
            );
            assert!(
                text.contains(&format!("lodi: error {code}:")),
                "{args:?} on {body}: {text}"
            );
            for want in &wanted {
                assert!(text.contains(want), "{args:?} on {body}: {text}");
            }
            assert_eq!(
                untouched,
                census(&home),
                "{args:?} opened or wrote something below a store it refused"
            );
            assert!(
                !home.join("store").exists(),
                "{args:?} created the store directories of a store it refused"
            );
        }
    }
    assert_eq!(env.decoy_listing(), decoy, "the decoy tree was touched");
}

// --------------------------------------------------- (e) a read-only command never migrates ---

/// (e) A read-only command creates no marker and migrates nothing. `lodi gc --dry-run` opens the
/// recorded 0.3.0 store, predicts against the layout it finds, and leaves the tree byte for byte
/// and mtime for mtime as it was — including the absence of `.layout.json`. The write ledger
/// (`LODI_FS_LEDGER`, which every home-scope write appends to) is unchanged across the run, and
/// the full census is the proof for the store itself.
#[test]
fn a_read_only_command_creates_no_marker_and_migrates_nothing() {
    let env = home_env("layout-read-only");
    let decoy = env.decoy_listing();
    let home = store_home(&env);
    restore(&home);
    restore_project(&env);
    let before = census(&home);
    let ledger = home_gate().ledger_lines();

    let out = lodi(&env)
        .args(["gc", "--dry-run"])
        .output()
        .expect("lodi gc --dry-run starts");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    assert!(
        !home.join(MARKER).exists(),
        "a read-only command created the layout marker"
    );
    assert_eq!(
        layout::read(&home).expect("an unmarked store reads"),
        layout::UNMARKED,
        "the store is no longer layout 0"
    );
    assert_eq!(
        before,
        census(&home),
        "a read-only command changed a file or an mtime under the store"
    );
    assert_eq!(
        ledger,
        home_gate().ledger_lines(),
        "a read-only command appended to the write ledger"
    );
    assert_eq!(env.decoy_listing(), decoy, "the decoy tree was touched");
}

// ------------------------------------------------- a store that is not there is not created ---

/// A store that does not exist yet is not a store to migrate. `lodi gc` reports an empty plan and
/// creates nothing at all — no store root, no directories and no marker — so the marker cannot
/// appear anywhere except inside a store that already exists.
#[test]
fn a_store_that_does_not_exist_is_not_migrated_into_being() {
    let env = home_env("layout-absent");
    let decoy = env.decoy_listing();
    let home = store_home(&env);
    assert!(
        !home.exists(),
        "the scratch store exists before any command"
    );

    let out = lodi(&env).arg("gc").output().expect("lodi gc starts");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        !home.exists(),
        "a collection created a store that was not there: {:?}",
        fs::read_dir(&home).map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>())
    );
    assert_eq!(env.decoy_listing(), decoy, "the decoy tree was touched");
}
