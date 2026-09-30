//! `lodi gc` (M-0.3 T-3, acceptance rows A12–A16).
//!
//! Everything here is **offline and deterministic**: the stores are fabricated by hand, the
//! "live" session is this very test process (whose pid is alive by construction) and the dead
//! one is a pid that has been reaped, ages are set with `utimensat` rather than waited for, and
//! the Podman half is the real classification function driven over listings written here.
//! Nothing is downloaded, no container runtime is started and no object outside
//! `CARGO_TARGET_TMPDIR` is read or written.
//!
//! The one place a real program is started is `lodi gc` itself, through the built binary, to
//! pin the exit statuses and the "must not run podman" rule.

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

use lodi::gc::{self, Class, Options};

/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

// ------------------------------------------------------------------- fabricating a store ---

fn scratch(tag: &str) -> PathBuf {
    support::scratch(&format!("gc-{tag}"))
}

/// Remove a fabricated store: entries are sealed read-only, so modes are opened first.
fn remove_tree(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
        for entry in fs::read_dir(path)? {
            let child = entry?.path();
            if fs::symlink_metadata(&child)?.file_type().is_dir() {
                remove_tree(&child)?;
            } else {
                fs::remove_file(&child)?;
            }
        }
        return fs::remove_dir(path);
    }
    match fs::symlink_metadata(path) {
        Ok(_) => fs::remove_file(path),
        Err(_) => Ok(()),
    }
}

/// A store laid out exactly as `src/store.rs` documents it, filled by hand.
struct Fake {
    home: PathBuf,
}

impl Fake {
    fn new(tag: &str) -> Fake {
        let home = scratch(tag);
        // `store/.lock` is created here because taking it is the only thing a `--dry-run`
        // ever writes: `flock` needs the file to exist. With it already there, "the dry run
        // changed nothing" is a statement about the whole store, lock file included.
        for dir in [
            "store/.locks",
            "store/.meta",
            "store/tmp",
            "cache/dl",
            "cache/shell",
            "gcroots/sessions",
            "logs",
        ] {
            fs::create_dir_all(home.join(dir)).unwrap();
        }
        fs::write(home.join("store/.lock"), b"").unwrap();
        Fake { home }
    }

    /// A complete entry of `bytes` bytes with the references `refs`.
    fn entry(&self, name: &str, bytes: usize, refs: &[&str]) -> &Fake {
        let dir = self.home.join("store").join(name);
        fs::create_dir_all(dir.join("tree/bin")).unwrap();
        fs::write(dir.join("tree/bin/payload"), vec![b'x'; bytes]).unwrap();
        self.sidecar(name, refs, true);
        self
    }

    /// A directory with no complete sidecar: the residue of an interrupted realization.
    fn incomplete(&self, name: &str, bytes: usize, sidecar: bool) -> &Fake {
        let dir = self.home.join("store").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("half"), vec![b'y'; bytes]).unwrap();
        if sidecar {
            self.sidecar(name, &[], false);
        }
        self
    }

    fn sidecar(&self, name: &str, refs: &[&str], complete: bool) {
        let identity = format!("sha256:{}", digest_of(name));
        let meta = serde_json::json!({
            "name": name,
            "type": if name.starts_with("env-") { "env" } else { "art" },
            "identity": identity,
            "treeHash": format!("sha256:{}", digest_of(&format!("tree-{name}"))),
            "created": "2026-09-20T00:00:00Z",
            "references": refs,
            "complete": complete,
            "lodiVersion": "0.2.0",
        });
        fs::write(
            self.home.join("store/.meta").join(format!("{name}.json")),
            serde_json::to_vec(&meta).unwrap(),
        )
        .unwrap();
    }

    /// A cached download named by its digest, aged `days` days.
    fn download(&self, digest: &str, bytes: usize, days: u64) -> &Fake {
        let path = self.home.join("cache/dl").join(digest);
        fs::write(&path, vec![b'z'; bytes]).unwrap();
        age(&path, days * 24 * 60 * 60);
        self
    }

    /// The download an entry's sidecar names, so it is referenced and survives.
    fn download_of(&self, name: &str, bytes: usize, days: u64) -> &Fake {
        self.download(&digest_of(name), bytes, days)
    }

    fn staging(&self, name: &str, bytes: usize, seconds: u64) -> &Fake {
        let dir = self.home.join("store/tmp").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("work"), vec![b'w'; bytes]).unwrap();
        age(&dir, seconds);
        self
    }

    /// A live-session root naming `entries`, owned by `pid`.
    fn session(&self, pid: i32, entries: &[&str], image: Option<&str>) -> &Fake {
        let mut record = serde_json::json!({
            "pid": pid,
            "activation": "sha256:0",
            "entries": entries,
        });
        if let Some(image) = image {
            record["image"] = serde_json::json!({ "name": image, "tag": "t", "id": "i" });
        }
        fs::write(
            self.home.join("gcroots/sessions").join(pid.to_string()),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        self
    }

    /// The persistent home root uses the same `entries` shape as a session root, without a pid.
    fn home_root(&self, entries: &[&str]) -> &Fake {
        fs::write(
            self.home.join("gcroots/home"),
            serde_json::to_vec(&serde_json::json!({ "entries": entries })).unwrap(),
        )
        .unwrap();
        self
    }

    fn write(&self, relative: &str, contents: &str) -> &Fake {
        let path = self.home.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
        self
    }

    fn exists(&self, relative: &str) -> bool {
        fs::symlink_metadata(self.home.join(relative)).is_ok()
    }

    fn listing(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![self.home.clone()];
        while let Some(at) = stack.pop() {
            let Ok(entries) = fs::read_dir(&at) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let relative = path.strip_prefix(&self.home).unwrap().to_owned();
                out.push(relative.to_string_lossy().into_owned());
                if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_dir()) {
                    stack.push(path);
                }
            }
        }
        out.sort();
        out
    }

    /// `du -sb $LODI_HOME`, the measurement acceptance row A14 compares against.
    fn du(&self) -> u64 {
        let out = Command::new("du")
            .args(["-sb".as_ref(), self.home.as_os_str()])
            .output()
            .expect("du runs");
        assert!(out.status.success(), "du failed");
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .expect("du -sb prints a byte count")
    }

    fn gc(&self, options: &Options) -> gc::Plan {
        gc::collect(&self.home, options).expect("gc succeeds")
    }

    fn lines(&self, options: &Options) -> Vec<String> {
        gc::render(&self.gc(options), options)
    }
}

fn digest_of(text: &str) -> String {
    // A stable 64-hex name for a fabricated artifact; the collector only ever compares it.
    let mut out = String::new();
    let bytes = text.as_bytes();
    for i in 0..32 {
        out.push_str(&format!("{:02x}", bytes[i % bytes.len()] ^ (i as u8)));
    }
    out
}

/// Set `path`'s mtime `seconds` into the past, so ages are exact and nothing is waited for.
fn age(path: &Path, seconds: u64) {
    let when = SystemTime::now() - Duration::from_secs(seconds);
    let stamp = when
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("after the epoch");
    let file = fs::File::options()
        .write(true)
        .open(path)
        .or_else(|_| fs::File::open(path))
        .expect("the path opens");
    file.set_times(fs::FileTimes::new().set_accessed(when).set_modified(when))
        .unwrap_or_else(|e| panic!("setting the mtime of {}: {e} ({stamp:?})", path.display()));
}

fn dry() -> Options {
    Options {
        dry_run: true,
        ..Options::default()
    }
}

fn me() -> i32 {
    std::process::id() as i32
}

/// A pid that is certainly not alive: a child that has already been reaped.
fn dead_pid() -> i32 {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 0")
        .spawn()
        .unwrap();
    child.wait().unwrap();
    child.id() as i32
}

// --------------------------------------------------------------- A12: a live session wins ---

/// A12: with a live session naming them, neither the entries nor the downloads they name are
/// touched, and `--dry-run` predicts exactly that.
#[test]
fn a_live_session_keeps_its_entries_and_its_downloads() {
    let fake = Fake::new("live");
    fake.entry("env-live", 100, &["art-live"])
        .entry("art-live", 200, &[])
        .entry("art-cold", 300, &[])
        .download_of("art-live", 400, 99)
        .download_of("art-cold", 500, 99)
        .session(me(), &["env-live"], None);

    let predicted = fake.lines(&dry());
    // The dry run changed nothing at all.
    let before = fake.listing();
    assert!(fake.exists("store/art-live") && fake.exists("store/art-cold"));

    let plan = fake.gc(&Options::default());
    assert!(
        fake.exists("store/env-live") && fake.exists("store/art-live"),
        "a live session's entries were collected"
    );
    assert!(
        fake.exists(&format!("cache/dl/{}", digest_of("art-live"))),
        "a live session's download was evicted"
    );
    // The unrooted entry, and only it, went; its download went with it (older than 7 days).
    assert!(!fake.exists("store/art-cold"));
    assert!(!fake.exists(&format!("cache/dl/{}", digest_of("art-cold"))));
    assert_eq!(plan.count(Class::Entry), 1);
    assert_eq!(plan.count(Class::Download), 1);

    // A13's comparison, on this store: the prediction is the collection, verb for verb.
    let applied = gc::render(&plan, &Options::default());
    let mapped: Vec<String> = predicted.iter().map(|l| gc::as_applied(l)).collect();
    assert_eq!(mapped, applied);
    assert_ne!(before, fake.listing(), "the apply removed nothing at all");
}

/// M-0.5 A15: the persistent home root is a source of marks exactly like a live root. A missing
/// name warns without failing, and once the root file is removed every entry is collectable.
#[test]
fn a_home_root_keeps_its_entries_until_the_root_is_removed() {
    let fake = Fake::new("home-root");
    fake.entry("env-home", 100, &["art-home"])
        .entry("art-home", 200, &[])
        .entry("art-cold", 300, &[])
        .home_root(&["env-home", "art-missing"]);

    let first = fake.gc(&Options::default());
    assert!(fake.exists("store/env-home"));
    assert!(fake.exists("store/art-home"));
    assert!(!fake.exists("store/art-cold"));
    assert!(
        fake.exists("gcroots/home"),
        "gc removed the persistent root"
    );
    assert!(
        first
            .warnings
            .iter()
            .any(|line| line.starts_with("W_MISSING_REF") && line.contains("art-missing")),
        "{:?}",
        first.warnings
    );

    fs::remove_file(fake.home.join("gcroots/home")).unwrap();
    let second = fake.gc(&Options::default());
    assert_eq!(second.count(Class::Entry), 2);
    assert!(!fake.exists("store/env-home"));
    assert!(!fake.exists("store/art-home"));
}

// ------------------------------------ A13: the dry run predicts and changes nothing at all ---

/// A13: `--dry-run` leaves the store byte-identical, and the lines it prints are the lines the
/// following `lodi gc` prints, compared line by line after the verb is put in the indicative.
#[test]
fn the_dry_run_predicts_exactly_what_the_apply_removes_and_changes_nothing() {
    let fake = Fake::new("dry");
    fake.entry("env-a", 100, &["art-a"])
        .entry("art-a", 200, &[])
        .entry("art-b", 300, &[])
        .incomplete("art-half", 50, false)
        .download_of("art-a", 400, 30)
        .download_of("art-b", 500, 30)
        .download("f".repeat(64).as_str(), 600, 30)
        .staging("999999-0-art-a", 700, 48 * 60 * 60)
        .session(dead_pid(), &["env-a"], None);

    let before = fake.listing();
    let sizes: Vec<u64> = before
        .iter()
        .map(|p| {
            fs::symlink_metadata(fake.home.join(p))
                .map(|m| m.len())
                .unwrap_or(0)
        })
        .collect();

    let verbose = Options {
        verbose: true,
        ..dry()
    };
    let predicted = fake.lines(&verbose);
    assert_eq!(fake.listing(), before, "--dry-run removed something");
    let after_sizes: Vec<u64> = before
        .iter()
        .map(|p| {
            fs::symlink_metadata(fake.home.join(p))
                .map(|m| m.len())
                .unwrap_or(0)
        })
        .collect();
    assert_eq!(after_sizes, sizes, "--dry-run rewrote something");

    let applied = gc::render(
        &fake.gc(&Options {
            verbose: true,
            ..Options::default()
        }),
        &Options {
            verbose: true,
            ..Options::default()
        },
    );
    let mapped: Vec<String> = predicted.iter().map(|l| gc::as_applied(l)).collect();
    assert_eq!(
        mapped, applied,
        "the prediction and the collection disagree line by line"
    );
    // And it really did predict something: every class above is represented.
    assert!(
        predicted
            .iter()
            .any(|l| l.contains("would remove entry art-b"))
    );
    assert!(
        predicted
            .iter()
            .any(|l| l.contains("would remove session root"))
    );
    assert!(
        predicted
            .iter()
            .any(|l| l.contains("would remove staging directory"))
    );
    assert!(
        predicted
            .iter()
            .any(|l| l.contains("would remove download"))
    );
    assert!(predicted.iter().any(|l| l.starts_with("would free ")));
}

// ---------------------------------------------- A14: the measured bytes, roots and ages ---

/// A14: the bytes the run reports equal the measured `du -sb $LODI_HOME` difference. The
/// tolerance is one file-system block (4096 bytes): `du` counts the directory inodes the run
/// removes, and taking `store/.lock` creates that file when it is absent.
#[test]
fn the_freed_bytes_are_the_measured_difference_of_du_sb() {
    // `du -sb` sums the apparent size of every file and symlink and adds nothing for a
    // directory (**measured** here with GNU coreutils on ext4), which is exactly what the
    // collector counts. The tolerance covers the file-system accounting a store this small
    // could still differ by; the record states the difference this run actually measured.
    const TOLERANCE: i64 = 4096;
    let fake = Fake::new("bytes");
    fake.entry("env-keep", 1_000, &["art-keep"])
        .entry("art-keep", 2_000, &[])
        .entry("art-gone", 500_000, &[])
        .entry("env-gone", 250_000, &[])
        .download_of("art-gone", 300_000, 30)
        .download_of("art-keep", 10_000, 30)
        .staging("999999-1-x", 40_000, 48 * 60 * 60)
        .session(me(), &["env-keep"], None);

    let before = fake.du();
    let plan = fake.gc(&Options::default());
    let after = fake.du();

    let measured = (before - after) as i64;
    let reported = plan.freed() as i64;
    assert!(
        (measured - reported).abs() <= TOLERANCE,
        "gc reported {reported} freed bytes, du -sb measured {measured} \
         (difference {}, tolerance {TOLERANCE})",
        measured - reported
    );
    assert!(
        reported > 1_000_000,
        "nothing substantial was freed: {reported}"
    );
}

/// A14: a dead session root is pruned, and `-v` names it; a live one is not.
#[test]
fn a_dead_session_root_is_pruned_and_named_with_v() {
    let fake = Fake::new("roots");
    let dead = dead_pid();
    fake.entry("art-x", 10, &[])
        .session(dead, &["art-x"], None)
        .session(me(), &["art-x"], None);

    let lines = fake.lines(&Options {
        verbose: true,
        ..Options::default()
    });
    assert!(
        lines
            .iter()
            .any(|l| l == &format!("removed session root {dead} (its process is gone)")),
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l == "removed 1 session root"));
    assert!(!fake.exists(&format!("gcroots/sessions/{dead}")));
    assert!(fake.exists(&format!("gcroots/sessions/{}", me())));
    // The live root still marked the entry, so nothing of the store went.
    assert!(fake.exists("store/art-x"));
}

/// A14: an entry whose sidecar is absent, or present and not `complete`, is collected — it is
/// the residue of an interrupted realization and is never used.
#[test]
fn an_entry_whose_sidecar_is_incomplete_is_collected() {
    let fake = Fake::new("incomplete");
    fake.incomplete("art-no-sidecar", 10, false)
        .incomplete("art-not-complete", 10, true)
        .entry("art-whole", 10, &[])
        .session(me(), &["art-whole", "art-no-sidecar"], None);

    let lines = fake.lines(&Options {
        verbose: true,
        ..Options::default()
    });
    assert!(!fake.exists("store/art-not-complete"));
    assert!(!fake.exists("store/.meta/art-not-complete.json"));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("art-not-complete") && l.contains("incomplete")),
        "{lines:?}"
    );
    // A live session that names an incomplete directory keeps it: roots decide, and a root
    // naming it is a fact about this machine the collector does not overrule.
    assert!(fake.exists("store/art-no-sidecar"));
    assert!(fake.exists("store/art-whole"));
}

/// A14: `--keep-days` decides a download nothing references, and nothing else does.
#[test]
fn an_unreferenced_download_is_kept_until_keep_days_and_then_evicted() {
    let fake = Fake::new("keep-days");
    fake.download("a".repeat(64).as_str(), 100, 3)
        .download("b".repeat(64).as_str(), 100, 9)
        .entry("art-ref", 10, &[])
        .download_of("art-ref", 100, 9000)
        .session(me(), &["art-ref"], None);

    let plan = fake.gc(&Options::default());
    assert_eq!(plan.count(Class::Download), 1, "{:?}", plan.removals);
    assert!(fake.exists(&format!("cache/dl/{}", "a".repeat(64))));
    assert!(!fake.exists(&format!("cache/dl/{}", "b".repeat(64))));
    // The referenced one survives however old it is: references outrank age.
    assert!(fake.exists(&format!("cache/dl/{}", digest_of("art-ref"))));

    // A wider window keeps the older one too; a zero window evicts both.
    let fake = Fake::new("keep-days-2");
    fake.download("a".repeat(64).as_str(), 100, 3)
        .download("b".repeat(64).as_str(), 100, 9);
    assert_eq!(
        fake.gc(&Options {
            keep_days: 30,
            ..Options::default()
        })
        .count(Class::Download),
        0
    );
    assert_eq!(
        fake.gc(&Options {
            keep_days: 0,
            ..Options::default()
        })
        .count(Class::Download),
        2
    );
}

/// `store/tmp` is swept by age (24 h, `spec/03` §7 step 5) and never while its creator runs:
/// Podman's own temporary image copies live there under the live process's pid (LD-25).
#[test]
fn staging_older_than_a_day_goes_unless_its_process_is_still_running() {
    let fake = Fake::new("staging");
    fake.staging("999999-0-old", 10, 48 * 60 * 60)
        .staging("999999-1-young", 10, 60)
        .staging(&format!("{}-podman", me()), 10, 48 * 60 * 60);

    let plan = fake.gc(&Options::default());
    assert_eq!(plan.count(Class::Staging), 1, "{:?}", plan.removals);
    assert!(!fake.exists("store/tmp/999999-0-old"));
    assert!(fake.exists("store/tmp/999999-1-young"));
    assert!(fake.exists(&format!("store/tmp/{}-podman", me())));
}

/// A reference no entry satisfies is the warning `W_MISSING_REF`, and a warning never changes
/// what the run does or what it exits with.
#[test]
fn a_missing_reference_is_a_warning_and_nothing_more() {
    let fake = Fake::new("missing-ref");
    fake.entry("env-a", 10, &["art-absent"])
        .session(me(), &["env-a"], None);
    let plan = fake.gc(&Options::default());
    assert!(
        plan.warnings.iter().any(|w| w.starts_with("W_MISSING_REF")),
        "{:?}",
        plan.warnings
    );
    assert_eq!(plan.removals, Vec::new());
    assert!(fake.exists("store/env-a"));
}

/// A live session root this build cannot read makes the whole run conservative: it sweeps no
/// entry at all rather than guess what that session is using.
#[test]
fn an_unreadable_live_root_stops_the_sweep_instead_of_guessing() {
    let fake = Fake::new("unreadable-root");
    fake.entry("art-x", 10, &[])
        .download("c".repeat(64).as_str(), 10, 30);
    fs::write(
        fake.home.join("gcroots/sessions").join(me().to_string()),
        b"{ not json",
    )
    .unwrap();
    let plan = fake.gc(&Options::default());
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.starts_with("W_ROOT_UNREADABLE")),
        "{:?}",
        plan.warnings
    );
    assert_eq!(plan.count(Class::Entry), 0);
    assert!(fake.exists("store/art-x"));
}

// -------------------------------------------------- A15: nothing outside $LODI_HOME goes ---

/// A15, the "must not" of this package: a symlink inside the store that points out of it is
/// unlinked, and what it pointed at is left exactly as it was.
#[test]
fn a_symlink_out_of_the_store_is_unlinked_and_never_followed() {
    let outside = scratch("outside");
    fs::write(outside.join("precious"), b"not lodi's").unwrap();
    let fake = Fake::new("symlink");
    fake.entry("art-link", 10, &[]);
    std::os::unix::fs::symlink(&outside, fake.home.join("store/art-link/escape")).unwrap();
    std::os::unix::fs::symlink(
        outside.join("precious"),
        fake.home.join("store/art-link/file"),
    )
    .unwrap();

    fake.gc(&Options::default());
    assert!(!fake.exists("store/art-link"), "the entry survived");
    assert!(
        outside.join("precious").is_file(),
        "gc followed a symlink out of the store"
    );
    assert!(outside.is_dir());
    remove_tree(&outside).unwrap();
}

/// A15: the ownership rule of LD-31/LD-32, applied to what `--images` may take. The listings
/// are the ones `podman images` and `podman ps -a` produce; the function under test is the one
/// the command uses.
#[test]
fn only_exact_lodi_env_tags_are_reclaimable_and_a_container_keeps_its_image() {
    let held = format!("localhost/lodi-env:{}", "1".repeat(32));
    let free = format!("localhost/lodi-env:{}", "2".repeat(32));
    let live = format!("localhost/lodi-env:{}", "3".repeat(32));
    let images = [
        ("localhost/lodi-env:not-a-hash", "id-odd"),
        (
            "localhost/lodi-env:NOTLOWERCASEHEXNOTLOWERCASEHEXAA",
            "id-up",
        ),
        ("localhost/lodi-tmp-base:4711-0", "id-base"),
        (held.as_str(), "id-held"),
        (free.as_str(), "id-free"),
        (live.as_str(), "id-live"),
        ("docker.io/library/debian:bookworm", "id-debian"),
    ]
    .map(|(tag, id)| lodi::container::ImageEntry {
        tag: tag.into(),
        id: id.into(),
    });
    let containers = [lodi::container::ContainerEntry {
        name: "someone-elses-shell".into(),
        image_id: "id-held".into(),
        image: held.clone(),
    }];
    let live_tags: BTreeSet<String> = [live.clone()].into_iter().collect();
    let rows = gc::classify_images(&images, &containers, &live_tags);

    let reclaimable: Vec<&str> = rows
        .iter()
        .filter(|r| r.reclaimable)
        .map(|r| r.tag.as_str())
        .collect();
    assert_eq!(reclaimable, [free.as_str()]);

    let why = |tag: &str| {
        rows.iter()
            .find(|r| r.tag == tag)
            .unwrap_or_else(|| panic!("{tag} is not listed at all"))
            .why
            .clone()
    };
    assert!(why("localhost/lodi-env:not-a-hash").contains("32 lower-case hex"));
    assert!(
        why("localhost/lodi-env:NOTLOWERCASEHEXNOTLOWERCASEHEXAA").contains("32 lower-case hex")
    );
    assert!(why("localhost/lodi-tmp-base:4711-0").contains("build base"));
    assert!(why(&held).contains("someone-elses-shell"));
    assert!(why(&live).contains("live session"));
    // Nothing that is not Lodi's by its own name appears in the block at all.
    assert!(
        !rows
            .iter()
            .any(|r| r.tag == "docker.io/library/debian:bookworm"),
        "an image that is not Lodi's was listed"
    );
}

// --------------------------------------------------------- A16: the exclusive store lock ---

/// A16: a `lodi gc` waits for whoever holds the store lock instead of racing it. The holder
/// here is the shared lock every realization and every entry takes (`src/host.rs`), taken
/// through the same library function; `flock` locks the open file description, so a second
/// exclusive take in this very process blocks just as another process would.
#[test]
fn the_collector_waits_for_the_store_lock_rather_than_racing_it() {
    let fake = Fake::new("lock");
    fake.entry("art-x", 10, &[]);
    let store = lodi::store::Store::open(&fake.home).unwrap();
    let held = store.shared_lock().unwrap();

    // While it is held, the exclusive take does not succeed.
    assert!(
        store.try_exclusive_lock().unwrap().is_none(),
        "the exclusive lock was taken while a shared one was held"
    );

    let home = fake.home.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let plan = gc::collect(&home, &Options::default()).expect("gc succeeds");
        tx.send(plan.count(Class::Entry)).unwrap();
    });
    // The collector is blocked: it produces nothing while the shared lock is held.
    assert!(
        rx.recv_timeout(Duration::from_millis(400)).is_err(),
        "gc swept the store while another process held the store lock"
    );
    assert!(
        fake.exists("store/art-x"),
        "gc removed an entry while blocked"
    );

    drop(held);
    let removed = rx.recv_timeout(wait::ceiling()).unwrap_or_else(|_| {
        panic!(
            "{}",
            wait::timed_out("gc to finish once the lock is free", wait::ceiling())
        )
    });
    worker.join().unwrap();
    assert_eq!(removed, 1);
    assert!(!fake.exists("store/art-x"));
}

// -------------------------------------------------------- the binary: statuses and podman ---

fn lodi(dir: &Path, home: &Path, args: &[&str], path: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lodi"));
    command.args(args).current_dir(dir).env("LODI_HOME", home);
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command.output().expect("the lodi binary runs")
}

/// A store that was never created is not an error: exit 0, one line, and nothing is made.
#[test]
fn a_store_that_does_not_exist_is_exit_0_and_nothing_to_collect() {
    let dir = scratch("absent");
    let home = dir.join("never-made");
    let out = lodi(&dir, &home, &["gc"], None);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "nothing to collect"
    );
    assert!(
        !home.exists(),
        "gc created the store it was asked to collect"
    );
    remove_tree(&dir).unwrap();
}

/// A store directory that cannot be written is `E_STORE_PERM` at exit 9, before anything runs.
#[test]
fn a_store_that_cannot_be_written_is_e_store_perm_at_exit_9() {
    let fake = Fake::new("perm");
    fake.entry("art-x", 10, &[]);
    let store = fake.home.join("store");
    fs::set_permissions(&store, fs::Permissions::from_mode(0o500)).unwrap();
    let out = lodi(&fake.home, &fake.home, &["gc"], None);
    fs::set_permissions(&store, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        out.status.code(),
        Some(9),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("E_STORE_PERM"));
    assert!(fake.exists("store/art-x"));
}

/// Usage errors are usage errors: every flag `spec/03` §7 has and this build does not, a
/// `--keep-days` that is not a non-negative integer, and a positional argument (D9, LD-54).
#[test]
fn the_unimplemented_flags_and_a_bad_keep_days_exit_2() {
    let fake = Fake::new("usage");
    for args in [
        &["gc", "--no-wait"][..],
        &["gc", "--max-size", "1G"],
        &["gc", "--keep-outputs"],
        &["gc", "--keep-days"],
        &["gc", "--keep-days", "-1"],
        &["gc", "--keep-days", "1.5"],
        // `gc --help` is the command's help since LD-360, not one of these.
        &["gc", "everything"],
    ] {
        let out = lodi(&fake.home, &fake.home, args, None);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("unsupported"),
            "{args:?}"
        );
    }
}

/// The "must not" of this package: `lodi gc` does not start `podman` at all when the store
/// holds no `img-` record — not even under `--images`. The `podman` on `PATH` here records
/// every invocation, so "it was not run" is a fact on disk rather than an absence of output.
#[test]
fn podman_is_never_run_when_the_store_holds_no_img_record() {
    let fake = Fake::new("no-podman");
    fake.entry("art-x", 10, &[]);
    let bin = fake.home.join("fake-bin");
    fs::create_dir_all(&bin).unwrap();
    let marker = fake.home.join("podman-was-run");
    let spy = bin.join("podman");
    let mut file = fs::File::create(&spy).unwrap();
    writeln!(
        file,
        "#!/bin/sh\necho \"$@\" >> {}\nexit 1",
        marker.display()
    )
    .unwrap();
    drop(file);
    fs::set_permissions(&spy, fs::Permissions::from_mode(0o755)).unwrap();

    for args in [
        &["gc"][..],
        &["gc", "--images"],
        &["gc", "--dry-run", "--images"],
    ] {
        let out = lodi(&fake.home, &fake.home, args, Some(&bin));
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !marker.exists(),
            "{args:?} started podman with no img- record in the store: {}",
            fs::read_to_string(&marker).unwrap_or_default()
        );
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("images:"),
            "{args:?} printed an image block without asking Podman"
        );
    }

    // With an `img-` record present, Podman *is* asked — the rule is about the record, not
    // about the flag. A podman that fails is a failure of the run, never a silent skip.
    fake.write(
        "store/.meta/img-00000000000000000000000000000000.json",
        "{\"name\":\"img-00000000000000000000000000000000\",\"type\":\"img\",\
         \"identity\":\"sha256:0\",\"tag\":\"localhost/lodi-env:00000000000000000000000000000000\",\
         \"imageId\":\"id\",\"closureHash\":\"sha256:0\",\"packages\":1,\
         \"created\":\"2026-09-20T00:00:00Z\",\"complete\":true,\"lodiVersion\":\"0.2.0\"}",
    );
    let out = lodi(&fake.home, &fake.home, &["gc", "--dry-run"], Some(&bin));
    assert!(
        marker.exists(),
        "podman was not asked though an img- record exists"
    );
    assert_ne!(
        out.status.code(),
        Some(0),
        "a podman that fails was treated as a skip"
    );
}
