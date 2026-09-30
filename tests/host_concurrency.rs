//! M-1.0 T-7: the host scope's own concurrency, driven as real processes.
//!
//! Two questions, and both are answered without waiting on a clock for the answer:
//!
//! 1. **One apply at a time, and the second never waits.** The host scope's apply lock is
//!    the file `var/lib/lodi/host/.lock` below the selected root, taken exclusively and tried
//!    once (`src/hostscope/safety.rs`
//!    step 6). A second apply against a held lock is `E_SYSTEM_BUSY` at exit 9. The proof that it
//!    does not wait is structural rather than timed: **this test takes the exact lock the product
//!    takes, through the product's own entry point, before either apply process starts**, and
//!    both processes have reported and exited while the test still holds it. A lock that was
//!    waited on could not have been reported yet.
//! 2. **A collection leaves the host scope alone.** `lodi gc` reclaims the per-user store and
//!    removes nothing outside `$LODI_HOME` without `--images`; the host scope's files are outside
//!    it by construction. A real collection runs, with something real to collect, while a real
//!    host apply is held inside its transaction, and a census of the scratch root — every
//!    path, its type, mode, owner, size, mtime to the nanosecond and content digest — is byte for
//!    byte the same before and after.
//!
//! Every root is a scratch directory this test creates, arms and destroys, and every `$LODI_HOME`
//! is a scratch one under `CARGO_TARGET_TMPDIR`. **No host command runs against `/`** in any mode
//! (`AGENTS.md` §8, LD-45). Offline, no guest, no package manager, no network.

#[path = "support/hostroot.rs"]
mod hostroot;
#[path = "support/mod.rs"]
mod support;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use lodi::hostscope::safety::{Gate, Operation};
use lodi::hostscope::{apply, plan};
use lodi::util::sha256_hex;

use hostroot::{Hold, Root, ids, journals, spawn};
use support::home_env;

/// The child half of the case that has to hold a real apply still from outside it.
#[test]
fn child_apply() {
    if !hostroot::is_child() {
        return;
    }
    hostroot::child_apply();
}

/// A manifest with a gate action a [`Hold`] can stop at and a second file after it, so that a
/// real apply can be held still inside its transaction with work still to do.
fn gated_manifest(uid: u32, gid: u32) -> String {
    format!(
        "[files.\"/etc/a-gate.conf\"]\ncontent = \"alpha\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n\n\
         [files.\"/etc/b-after.conf\"]\ncontent = \"beta\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
    )
}

// ----------------------------------------------------------------------------- the census ---

/// One line per path below `root`: its type, mode, owner, size, mtime to the nanosecond and the
/// digest of its content (or of a symbolic link's target). Anything a write could change is in
/// it, so an unchanged census is an unchanged tree.
fn census(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, at: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(entries) = fs::read_dir(at) else {
            return;
        };
        let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            let rel = path
                .strip_prefix(root)
                .expect("below the root")
                .display()
                .to_string();
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(error) => {
                    out.insert(rel, format!("unreadable {error}"));
                    continue;
                }
            };
            let kind = if meta.is_dir() {
                "dir"
            } else if meta.file_type().is_symlink() {
                "symlink"
            } else if meta.is_file() {
                "file"
            } else {
                "other"
            };
            let digest = if meta.is_file() {
                fs::read(&path).map_or_else(|e| e.to_string(), |b| sha256_hex(&b))
            } else if meta.file_type().is_symlink() {
                fs::read_link(&path).map_or_else(
                    |e| e.to_string(),
                    |t| sha256_hex(t.display().to_string().as_bytes()),
                )
            } else {
                String::from("-")
            };
            out.insert(
                rel,
                format!(
                    "{kind} mode={:04o} uid={} gid={} size={} mtime={}.{:09} sha256={digest}",
                    meta.permissions().mode() & 0o7777,
                    meta.uid(),
                    meta.gid(),
                    meta.size(),
                    meta.mtime(),
                    meta.mtime_nsec(),
                ),
            );
            if meta.is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// The exact difference between two censuses, for a failure that has to name what moved.
fn differences(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Vec<String> {
    let mut out = Vec::new();
    for (path, was) in before {
        match after.get(path) {
            None => out.push(format!("{path}: removed ({was})")),
            Some(now) if now != was => out.push(format!("{path}: {was} -> {now}")),
            Some(_) => {}
        }
    }
    for path in after.keys() {
        if !before.contains_key(path) {
            out.push(format!("{path}: added ({})", after[path]));
        }
    }
    out
}

// ------------------------------------------------ one apply at a time, and no second waits ---

/// Start one real apply against `root`, with nothing to hold it still: it is expected to be
/// refused and to exit on its own.
fn start_apply(root: &Root) -> Child {
    spawn(
        "child_apply",
        &[("LODI_TEST_ROOT", &root.dir.display().to_string())],
    )
}

/// Acceptance (b). The apply lock is held by **this test**, through the product's own
/// `Gate::lock_for_apply`, before either apply process exists. Both are refused with
/// `E_SYSTEM_BUSY` at exit 9 and both have exited before the lock is released, which is what
/// "does not wait" means when it is proved rather than timed.
#[test]
fn a_second_apply_against_a_held_root_lock_is_refused_and_never_waits() {
    let root = Root::new("concurrency-busy");
    let (uid, gid) = ids(&root);
    root.debian_with(&format!(
        "[files.\"/etc/a.conf\"]\ncontent = \"alpha\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
    ));

    // The lock, taken the way an apply takes it, before either process starts.
    let mut held = Gate::open(&root.options(), Operation::Apply).expect("the gate");
    held.lock_for_apply().expect("the lock");
    assert!(
        root.exists("var/lib/lodi/host/.lock"),
        "the apply lock is the file the product names"
    );

    let first = start_apply(&root);
    let second = start_apply(&root);
    for (which, child) in [("the first", first), ("the second", second)] {
        let output = child.wait_with_output().expect("it exits");
        let said = String::from_utf8_lossy(&output.stderr).into_owned();
        assert_eq!(
            output.status.code(),
            Some(9),
            "{which} apply did not exit 9: {said}"
        );
        assert!(
            said.contains("E_SYSTEM_BUSY"),
            "{which} apply did not say E_SYSTEM_BUSY: {said}"
        );
        assert!(
            // check-host-safety: refusal — what follows is a product message, not a command.
            said.contains("another lodi host apply holds"),
            "{which} apply did not name the lock: {said}"
        );
    }
    // Both reported while the lock was still held. Nothing was applied, and a plan — which takes
    // no lock at all — is never refused for this reason.
    assert!(!root.exists("etc/a.conf"), "a refused apply wrote a file");
    assert!(
        journals(&root).is_empty(),
        "a refused apply opened a journal"
    );
    plan(&root.options()).expect("a plan takes no lock");

    drop(held);
    apply(&root.options()).expect("the following apply completes");
    assert_eq!(root.read("etc/a.conf"), "alpha\n");
}

// --------------------------------------- a collection during an apply, and the host scope ---

/// Acceptance (c). A real `lodi gc`, with something real to collect, runs to completion while a
/// real host apply is held inside its transaction — journal written, apply lock held,
/// first action open. Nothing under the scratch root moves, by a census that would notice a
/// mode, an owner, a byte or a nanosecond.
///
/// A second apply is not asked for here: the lock's own answer is
/// [`a_second_apply_against_a_held_root_lock_is_refused_and_never_waits`], which holds the lock
/// itself and needs no apply to be in flight at all.
#[test]
fn a_collection_during_a_host_apply_changes_nothing_the_host_scope_owns() {
    let root = Root::new("concurrency-gc");
    let (uid, gid) = ids(&root);
    root.debian_with(&gated_manifest(uid, gid));

    // Held at the first action's write: the journal is written, the apply lock held and the
    // first action begun.
    let hold = Hold::at_write_of(&root, "a-gate.conf");
    let child = hold.spawn(&root);
    hold.wait_held();
    assert!(
        root.exists("var/lib/lodi/host/.lock"),
        "the apply lock is held"
    );
    assert!(!journals(&root).is_empty(), "the journal is written");

    // A scratch `$LODI_HOME` with one cached download nothing names: real work for a real
    // collection, so that this is not a gc that found nothing to do.
    let env = home_env("host-concurrency-gc");
    let lodi_home = env.data().join("lodi");
    let junk = sha256_hex(b"a download no entry names");
    fs::create_dir_all(lodi_home.join("cache/dl")).expect("the download cache");
    fs::write(lodi_home.join("cache/dl").join(&junk), b"bytes").expect("the cached download");

    let before = census(&root.dir);
    let collection = env
        .command()
        .args(["gc", "-v", "--keep-days", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("lodi gc runs");
    let after = census(&root.dir);

    let out = String::from_utf8_lossy(&collection.stdout).into_owned();
    let err = String::from_utf8_lossy(&collection.stderr).into_owned();
    assert_eq!(collection.status.code(), Some(0), "gc failed: {out}{err}");
    assert!(
        !lodi_home.join("cache/dl").join(&junk).exists(),
        "the collection had nothing to collect, so it proves nothing: {out}{err}"
    );
    assert!(
        differences(&before, &after).is_empty(),
        "`lodi gc` changed the host scope's root:\n{}",
        differences(&before, &after).join("\n")
    );

    // And the apply the collection ran beside finishes normally.
    hold.release();
    let output = child.wait_with_output().expect("the apply finishes");
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(0), "{said}");
    assert_eq!(root.read("etc/a-gate.conf"), "alpha\n");
    assert_eq!(root.read("etc/b-after.conf"), "beta\n");
    assert!(
        root.read("etc/lodi/host.lock")
            .contains("/etc/b-after.conf")
    );
}

// ------------------------------------------------------------------- the host root and gc ---

/// The reason the case above can hold: `lodi gc` removes nothing outside `$LODI_HOME` without
/// `--images`, and the host scope's state lives outside it. This asserts the premise directly,
/// so that a later change which moved host state into `$LODI_HOME` would fail here and not only
/// in a timing-sensitive case.
#[test]
fn the_host_scopes_state_is_outside_the_store_lodi_gc_reclaims() {
    let root = Root::new("concurrency-outside");
    let (uid, gid) = ids(&root);
    root.debian_with(&format!(
        "[files.\"/etc/a.conf\"]\ncontent = \"alpha\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
    ));
    apply(&root.options()).expect("the apply");

    let env = home_env("host-concurrency-outside");
    let lodi_home = env.data().join("lodi");
    for owned in [
        root.path("var/lib/lodi/host"),
        root.path("etc/lodi/host.lock"),
        root.path("etc/a.conf"),
    ] {
        assert!(owned.exists(), "{} is there", owned.display());
        assert!(
            !owned.starts_with(&lodi_home),
            "{} is inside the store lodi gc reclaims",
            owned.display()
        );
    }

    let collection = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["gc", "-v"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", env.home())
        .env("XDG_CONFIG_HOME", env.config())
        .env("XDG_DATA_HOME", env.data())
        .env("LODI_HOME", &lodi_home)
        .current_dir(env.root())
        .output()
        .expect("lodi gc runs");
    assert_eq!(collection.status.code(), Some(0));
    let said = String::from_utf8_lossy(&collection.stdout).into_owned();
    assert!(
        !said.contains(&root.dir.display().to_string()),
        "the collection named the host scope's root: {said}"
    );
    assert_eq!(root.read("etc/a.conf"), "alpha\n");
}
