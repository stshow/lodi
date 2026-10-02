//! The pin verbs as the operator runs them (LD-397, charter §5 V1–V8; 2.0 #696): `pin NAME`
//! (the versions listing), `pin NAME|--all [--to …]` and `unpin NAME|--all` on the config's
//! `lodi.lock`, the plan's behind-latest line and the re-import rule.
//!
//! Every case is the real binary against a scratch `--root` with the fake machine of
//! `tests/support/fakehost.rs` behind it, the config at the scratch `HOME`'s `~/.config/lodi`, and
//! the recorded dated archives of `tests/fixtures/host/pin/` served on loopback through
//! `LODI_FETCH_REWRITE`. Nothing reaches `/`: every host command carries `--root`.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
#[path = "support/pinverbs.rs"]
mod pinverbs;
mod support;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;

use fakehost::{Machine, err, out, story};
use pinverbs::*;

/// The child half of every held apply of this binary (`tests/support/hostroot.rs`).
#[test]
fn child_apply() {
    if !hostroot::is_child() {
        return;
    }
    hostroot::child_apply();
}

// ------------------------------------------------------------------ V1 verb safety ---

/// V1. A pin of a user-owned host needs no root and takes no apply lock: it holds an advisory
/// lock on the host directory, so a second verb on it is refused by name and writes nothing; and
/// it never touches the machine — no package-manager change, no path under `etc/` or `var/`.
#[test]
fn v1_a_pin_holds_the_host_directory_lock_and_never_touches_the_machine() {
    let v = Verbs::debian("verbs-lock", &[("bc", BC), ("tzdata", TZ_RECENT)]);
    let text = host("debian", None, &["bc", "tzdata"], &[]);
    v.set_host(&text);
    // The verbs hold the config folder's advisory lock (LD-499).
    let held = fs::File::open(v.dir()).unwrap();
    // SAFETY: flock on a descriptor this test owns.
    let taken = unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(taken, 0);
    let busy = v.run("pin", &["--all", "--to", RECENT]);
    refused(&busy, "E_SYSTEM_BUSY");
    assert_eq!(v.host_toml(), text);
    assert!(v.lock().is_none());
    drop(held);
    // A sibling test's fork can hold a copy of the descriptor until its exec: wait it out.
    for _ in 0..100 {
        let probe = fs::File::open(v.dir()).unwrap();
        // SAFETY: flock on a descriptor this test owns; closing it releases the lock.
        if unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let machine = (
        census(&v.case.root.path("etc")),
        census(&v.case.root.path("var")),
        v.case.machine(),
    );
    let pinned = v.run("pin", &["--all", "--to", RECENT]);
    ok(&pinned);
    assert!(v.host_toml().contains(&format!("snapshot = \"{RECENT}\"")));
    assert_eq!(
        (
            census(&v.case.root.path("etc")),
            census(&v.case.root.path("var")),
            v.case.machine(),
        ),
        machine,
        "the machine changed"
    );
    let changing: Vec<String> = v
        .case
        .log()
        .into_iter()
        .filter(|line| {
            line.starts_with("apt-mark")
                || line.starts_with("dpkg ")
                || (line.starts_with("apt-get") && !line.contains(" -s "))
        })
        .collect();
    assert!(changing.is_empty(), "{changing:?}");
}

// -------------------------------------------------------------- V3 pin --all ---

/// V3. `pin --all --to DAY` sets `[host] snapshot` to that instant, records the digest of every
/// dated index file in `pins.lock`, and leaves every per-entry pin as it was.
#[test]
fn v3_pin_all_to_a_day_sets_the_snapshot_records_its_indexes_and_keeps_entry_pins() {
    let v = Verbs::debian("verbs-pin-all", &[("bc", BC), ("tzdata", TZ_OLDER)]);
    let text = host(
        "debian",
        None,
        &["bc", "tzdata"],
        &[("tzdata", "2025-03-01")],
    );
    v.set_host(&text);
    let pinned = v.run("pin", &["--all", "--to", "2026-09-01"]);
    ok(&pinned);
    assert_eq!(
        changed_lines(&text, &v.host_toml()),
        (vec![], vec![format!("snapshot = \"{RECENT}\"")])
    );
    let lock = v.lock().expect("pins.lock is written");
    assert_eq!(lock["snapshot"]["requested"], RECENT);
    assert_eq!(lock["snapshot"]["instant"], RECENT);
    let indexes = lock["snapshot"]["indexes"].as_object().expect("indexes");
    assert_eq!(
        indexes.keys().map(String::as_str).collect::<Vec<_>>(),
        vec![
            "debian-security/dists/bookworm-security/Release",
            "debian/dists/bookworm-updates/Release",
            "debian/dists/bookworm/Release",
        ]
    );
    assert!(
        indexes.values().all(|d| d
            .as_str()
            .is_some_and(|d| d.len() == 71 && d.starts_with("sha256:"))),
        "{indexes:?}"
    );
    assert_eq!(lock["pins"]["tzdata"]["requested"], "2025-03-01");
    assert_eq!(lock["pins"]["tzdata"]["version"], TZ_OLDER);
}

/// V3. With no `--to` the snapshot is the last UTC day that has ended (LD-444).
#[test]
fn v3_pin_all_without_to_takes_the_last_ended_day() {
    let v = Verbs::debian("verbs-pin-all-now", &[("bc", BC)]);
    v.set_host(&host("debian", None, &["bc"], &[]));
    let instants = v.alias_now();
    let pinned = v.run("pin", &["--all"]);
    ok(&pinned);
    let text = v.host_toml();
    let written = text
        .lines()
        .find_map(|line| line.strip_prefix("snapshot = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("a snapshot line");
    assert!(
        instants.iter().any(|i| i == written),
        "{written} {instants:?}"
    );
    let lock = v.lock().expect("lodi.lock");
    assert_eq!(lock["snapshot"]["instant"], written);
    assert_eq!(lock["snapshot"]["indexes"].as_object().unwrap().len(), 3);
}

/// Idempotence. Pinning the file twice leaves both files byte-identical, untouched, and says so.
#[test]
fn v3_pinning_the_file_twice_is_byte_identical_and_says_so() {
    let v = Verbs::debian("verbs-pin-all-twice", &[("bc", BC)]);
    v.set_host(&host("debian", None, &["bc"], &[]));
    ok(&v.run("pin", &["--all", "--to", RECENT]));
    let before = census(&v.dir());
    let again = v.run("pin", &["--all", "--to", RECENT]);
    ok(&again);
    assert!(out(&again).contains("nothing written"), "{}", story(&again));
    assert_eq!(census(&v.dir()), before);
}

// ------------------------------------------------------------ V4 unpin --all ---

/// V4. `unpin --all` removes `[host] snapshot`, every pin table and their `lodi.lock` entries;
/// the next switch releases every hold a pin set and changes no version.
#[test]
fn v4_unpin_all_floats_the_file_and_the_next_apply_releases_every_hold_keeping_versions() {
    let v = Verbs::debian("verbs-unpin-all", &[("bc", BC), ("tzdata", TZ_RECENT)]);
    v.case.set_manifest(
        "[host]\nversion = \"1\"\ndistro = \"debian\"\npackages = \"managed\"\n\n\
         [packages]\ncommon = [\"bc\", \"tzdata\"]\n\n[packages.pin]\ntzdata = \"2025-03-01\"\n\n\
         [packages.debian.pin]\nbc = \"2026-09-01\"\n",
    );
    ok(&v.run("pin", &["--all", "--to", RECENT]));
    assert!(v.lock().is_some_and(|lock| lock.get("snapshot").is_some()));
    ok(&v.run("apply", &[]));
    let machine = v.case.machine();
    let version = |m: &serde_json::Value, n: &str| m["installed"][n]["version"].clone();
    let held = |m: &serde_json::Value, n: &str| m["installed"][n]["held"] == true;
    assert_eq!(version(&machine, "tzdata"), TZ_OLDER);
    assert!(
        held(&machine, "tzdata") && held(&machine, "bc"),
        "{machine}"
    );

    let unpinned = v.run("unpin", &["--all"]);
    ok(&unpinned);
    let text = v.case.manifest();
    assert!(
        !text.contains("snapshot") && !text.contains(".pin]") && text.contains("common = ["),
        "{text}"
    );
    let lock = v.lock().expect("the host's lodi.lock section");
    assert!(
        lock.get("snapshot").is_none() && lock["pins"].as_object().unwrap().is_empty(),
        "{lock}"
    );
    ok(&v.run("apply", &[]));
    let after = v.case.machine();
    assert!(!held(&after, "tzdata") && !held(&after, "bc"), "{after}");
    assert_eq!(version(&after, "tzdata"), TZ_OLDER);
    assert_eq!(version(&after, "bc"), BC);
}

// ------------------------------------------------------------ V5 pin NAME ---

/// V5. `pin NAME --to DAY` writes one key and one `pins.lock` entry.
#[test]
fn v5_pin_one_name_to_a_day_writes_one_key_and_one_entry() {
    let v = Verbs::debian("verbs-pin-one", &[("bc", BC), ("tzdata", TZ_RECENT)]);
    let text = host("debian", None, &["bc", "tzdata"], &[]);
    v.set_host(&text);
    ok(&v.run("pin", &["tzdata", "--to", "2025-03-01"]));
    assert_eq!(
        changed_lines(&text, &v.host_toml()),
        (
            vec![],
            vec![
                String::new(),
                "[packages.pin]".into(),
                "tzdata = \"2025-03-01\"".into()
            ]
        )
    );
    let lock = v.lock().expect("lodi.lock");
    assert_eq!(
        lock["pins"].as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["tzdata"]
    );
    assert_eq!(lock["pins"]["tzdata"]["policy"], "date");
    assert_eq!(lock["pins"]["tzdata"]["version"], TZ_OLDER);
    assert!(lock.get("snapshot").is_none(), "{lock}");
}

/// V5. An Ubuntu name is pinned to a version, which the per-package interface resolves.
#[test]
fn v5_pin_to_a_version_on_ubuntu_writes_that_version() {
    let v = Verbs::ubuntu("verbs-pin-ubuntu", &[("tzdata", "2024b-0ubuntu0.24.04.1")]);
    let text = host("ubuntu", None, &["tzdata"], &[]);
    v.set_host(&text);
    ok(&v.run("pin", &["tzdata", "--to", "2024b-0ubuntu0.24.04.1"]));
    assert_eq!(
        changed_lines(&text, &v.host_toml()).1,
        vec![
            String::new(),
            "[packages.pin]".to_string(),
            "tzdata = \"2024b-0ubuntu0.24.04.1\"".to_string()
        ]
    );
    let lock = v.lock().expect("lodi.lock");
    assert_eq!(lock["pins"]["tzdata"]["policy"], "version");
    assert_eq!(lock["pins"]["tzdata"]["snapshot"], "2025-02-05T19:00:00Z");
}

/// V5. It resolves before it writes: a request the archive refuses writes nothing.
#[test]
fn v5_a_request_that_does_not_resolve_writes_nothing() {
    let v = Verbs::debian("verbs-pin-fails", &[("bc", BC), ("tzdata", TZ_RECENT)]);
    let text = host("debian", None, &["bc", "tzdata"], &[("bc", "2026-09-01")]);
    v.set_host(&text);
    let before = census(&v.dir());
    let too_old = v.run("pin", &["tzdata", "--to", "2001-01-01"]);
    refused(&too_old, "E_SNAPSHOT_TOO_OLD");
    let a_version = v.run("pin", &["tzdata", "--to", TZ_RECENT]);
    refused(&a_version, "E_UNSUPPORTED");
    assert!(
        err(&a_version).contains("YYYY-MM-DD"),
        "{}",
        story(&a_version)
    );
    let undeclared = v.run("pin", &["curl", "--to", "2026-09-01"]);
    refused(&undeclared, "E_UNKNOWN_PACKAGE");
    assert_eq!(census(&v.dir()), before);
}

/// Idempotence. Pinning a hand-edited pin to its own value records it in `pins.lock` and leaves
/// `host.toml` byte-identical; pinning it again writes nothing and says so.
#[test]
fn v5_pinning_a_hand_edited_pin_to_its_own_value_records_it_and_keeps_host_toml() {
    let v = Verbs::debian("verbs-pin-own", &[("bc", BC), ("tzdata", TZ_OLDER)]);
    let text = host(
        "debian",
        None,
        &["bc", "tzdata"],
        &[("tzdata", "2025-03-01")],
    );
    v.set_host(&text);
    let toml = v.dir().join("host.toml");
    let meta = (
        fs::read(&toml).unwrap(),
        fs::metadata(&toml).unwrap().mtime_nsec(),
    );
    let recorded = v.run("pin", &["tzdata", "--to", "2025-03-01"]);
    ok(&recorded);
    assert_eq!(
        (
            fs::read(&toml).unwrap(),
            fs::metadata(&toml).unwrap().mtime_nsec()
        ),
        meta
    );
    assert_eq!(
        v.lock().expect("lodi.lock")["pins"]["tzdata"]["version"],
        TZ_OLDER
    );
    let before = census(&v.dir());
    let again = v.run("pin", &["tzdata", "--to", "2025-03-01"]);
    ok(&again);
    assert!(out(&again).contains("nothing written"), "{}", story(&again));
    assert_eq!(census(&v.dir()), before);
}

// ---------------------------------------------------------- V6 unpin NAME ---

/// V6. `unpin NAME` removes one key and one entry; a name with no pin is exit 0 with a message
/// and changes nothing.
#[test]
fn v6_unpin_one_name_removes_one_key_and_one_entry_and_no_pin_is_exit_zero() {
    let v = Verbs::debian("verbs-unpin-one", &[("bc", BC), ("tzdata", TZ_OLDER)]);
    v.set_host(&host(
        "debian",
        None,
        &["bc", "tzdata"],
        &[("bc", "2026-09-01"), ("tzdata", "2025-03-01")],
    ));
    ok(&v.run("pin", &["tzdata", "--to", "2025-03-01"]));
    let text = v.host_toml();
    let lock = v.lock().expect("lodi.lock");
    assert!(lock["pins"].get("bc").is_some(), "{lock}");
    ok(&v.run("unpin", &["bc"]));
    assert_eq!(
        changed_lines(&text, &v.host_toml()),
        (vec!["bc = \"2026-09-01\"".to_string()], vec![])
    );
    let after = v.lock().expect("lodi.lock");
    assert_eq!(
        after["pins"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["tzdata"]
    );
    assert_eq!(after["pins"]["tzdata"], lock["pins"]["tzdata"]);
    let before = census(&v.dir());
    let none = v.run("unpin", &["bc"]);
    ok(&none);
    assert!(out(&none).contains("bc has no pin"), "{}", story(&none));
    assert_eq!(census(&v.dir()), before);
}

// -------------------------------------------------------------- V7 moving ---

/// V7. Moving one pin, then the file, changes exactly those bytes.
#[test]
fn v7_moving_a_pin_or_the_snapshot_changes_exactly_that_line_and_that_entry() {
    let v = Verbs::debian("verbs-move", &[("bc", BC), ("tzdata", TZ_OLDER)]);
    v.set_host(&host("debian", Some(OLDER), &["bc", "tzdata"], &[]));
    ok(&v.run("pin", &["tzdata", "--to", "2025-03-01"]));
    let (text, lock) = (v.host_toml(), v.lock().expect("lodi.lock"));
    ok(&v.run("pin", &["tzdata", "--to", "2026-09-01"]));
    assert_eq!(
        changed_lines(&text, &v.host_toml()),
        (
            vec!["tzdata = \"2025-03-01\"".to_string()],
            vec!["tzdata = \"2026-09-01\"".to_string()]
        )
    );
    let moved = v.lock().expect("lodi.lock");
    assert_eq!(moved["pins"]["tzdata"]["version"], TZ_RECENT);
    assert_eq!(moved["snapshot"], lock["snapshot"]);

    let text = v.host_toml();
    ok(&v.run("pin", &["--all", "--to", RECENT]));
    assert_eq!(
        changed_lines(&text, &v.host_toml()),
        (
            vec![format!("snapshot = \"{OLDER}\"")],
            vec![format!("snapshot = \"{RECENT}\"")]
        )
    );
    let file = v.lock().expect("lodi.lock");
    assert_eq!(file["pins"], moved["pins"]);
    assert_eq!(file["snapshot"]["instant"], RECENT);
}

// ------------------------------------------------------------ versions NAME ---

/// `versions NAME` on Ubuntu: every version the release's own pockets published, newest first,
/// with the day it arrived and whether it is the installed, the pinned or the latest one, then
/// one line to copy.
#[test]
fn versions_on_ubuntu_lists_every_version_newest_first_and_one_line_to_copy() {
    let v = Verbs::ubuntu(
        "verbs-versions-ubuntu",
        &[("tzdata", "2024b-0ubuntu0.24.04.1")],
    );
    v.set_host(&host(
        "ubuntu",
        None,
        &["tzdata"],
        &[("tzdata", "2024b-0ubuntu0.24.04.1")],
    ));
    let listed = v.run("versions", &["tzdata"]);
    ok(&listed);
    let text = out(&listed);
    let mut lines = text.lines();
    let header = lines.next().unwrap();
    assert!(
        header.starts_with("version") && header.contains("arrived") && header.ends_with("state"),
        "{text}"
    );
    let rows: Vec<&str> = lines.by_ref().take_while(|l| !l.is_empty()).collect();
    let versions: Vec<&str> = rows
        .iter()
        .map(|r| r.split_whitespace().next().unwrap())
        .collect();
    assert_eq!(
        versions,
        vec![
            "2026c-0ubuntu0.24.04.1",
            "2026b-0ubuntu0.24.04.1",
            "2026a-0ubuntu0.24.04.1",
            "2025b-0ubuntu0.24.04.1",
            "2025b-0ubuntu0.24.04",
            "2025a-0ubuntu0.24.04",
            "2024b-0ubuntu0.24.04.1",
            "2024b-0ubuntu0.24.04",
            "2024a-3ubuntu1.1",
            "2024a-2ubuntu1",
            "2024a-1ubuntu1",
            "2023d-1ubuntu2",
            "2023d-1ubuntu1",
            "2023c-9ubuntu1",
        ]
    );
    assert!(
        rows[0].contains("2026-07-29") && rows[0].ends_with("latest"),
        "{text}"
    );
    assert!(
        rows[6].contains("2025-02-05") && rows[6].ends_with("installed, pinned"),
        "{text}"
    );
    let copy = lines.collect::<Vec<_>>();
    assert_eq!(
        copy,
        vec!["lodi pin tzdata --to 2026c-0ubuntu0.24.04.1".to_string()]
    );
}

/// `versions NAME` on Debian, which has no per-package interface (P1): the version the dated
/// archive of this hour serves (the latest), the pinned one with its day, and the installed one;
/// the line to copy names the instant.
#[test]
fn versions_on_debian_lists_what_the_dated_archive_serves_and_the_pinned_and_installed() {
    let v = Verbs::debian("verbs-versions-debian", &[("bc", BC), ("tzdata", TZ_OLDER)]);
    v.set_host(&host(
        "debian",
        None,
        &["bc", "tzdata"],
        &[("tzdata", "2025-03-01")],
    ));
    ok(&v.run("pin", &["tzdata", "--to", "2025-03-01"]));
    let instants = v.alias_now();
    let listed = v.run("versions", &["tzdata"]);
    ok(&listed);
    let text = out(&listed);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("version"), "{text}");
    assert!(
        lines[1].starts_with(TZ_RECENT) && lines[1].ends_with("latest"),
        "{text}"
    );
    assert!(
        lines[2].starts_with(TZ_OLDER)
            && lines[2].contains("2025-03-01")
            && lines[2].ends_with("installed, pinned"),
        "{text}"
    );
    assert_eq!(lines[3], "");
    assert!(
        instants
            .iter()
            .any(|at| lines[4] == format!("lodi pin tzdata --to {at}")),
        "{text}"
    );
    assert_eq!(lines.len(), 5, "{text}");
}

/// V2. The versions cache: one file per name; a young file answers with zero requests, and a
/// corrupt or stale one is refetched.
#[test]
fn v2_versions_answers_from_a_young_cache_and_refetches_a_corrupt_or_stale_one() {
    let v = Verbs::ubuntu("verbs-cache", &[("tzdata", "2024b-0ubuntu0.24.04.1")]);
    v.set_host(&host("ubuntu", None, &["tzdata"], &[]));
    let first = v.run("versions", &["tzdata"]);
    ok(&first);
    assert!(!v.server.requests().is_empty());
    let cache = v
        .case
        .root
        .path("lodi-home/cache/versions/ubuntu/tzdata.json");
    assert!(cache.is_file());

    v.server.clear();
    let second = v.run("versions", &["tzdata"]);
    ok(&second);
    assert_eq!(out(&second), out(&first));
    assert!(v.server.requests().is_empty(), "{:?}", v.server.requests());

    fs::write(&cache, "{ not a cache").unwrap();
    let third = v.run("versions", &["tzdata"]);
    ok(&third);
    assert!(!v.server.requests().is_empty());
    let mut file: serde_json::Value = serde_json::from_slice(&fs::read(&cache).unwrap()).unwrap();
    assert_eq!(file["name"], "tzdata");

    file["fetched"] = serde_json::json!(file["fetched"].as_i64().unwrap() - 2 * 86_400);
    fs::write(&cache, serde_json::to_vec(&file).unwrap()).unwrap();
    v.server.clear();
    let fourth = v.run("versions", &["tzdata"]);
    ok(&fourth);
    assert!(!v.server.requests().is_empty(), "a stale file was used");
}

/// V2. A cold cache with no network is `E_REPO_UNREACHABLE` (exit 4), and nothing is cached.
#[test]
fn v2_a_cold_cache_with_no_network_is_e_repo_unreachable() {
    let v = Verbs::ubuntu("verbs-offline", &[("tzdata", "2024b-0ubuntu0.24.04.1")]);
    v.set_host(&host("ubuntu", None, &["tzdata"], &[]));
    let closed = support::ClosedPort::bind();
    let offline = v.case.verb_env(
        "versions",
        &["tzdata"],
        &[
            ("LODI_FETCH_REWRITE", closed.rewrite().as_str()),
            // One attempt: a refused connection is retried with a backoff otherwise (LD-386).
            ("LODI_FETCH_ATTEMPTS", "1"),
        ],
    );
    refused(&offline, "E_REPO_UNREACHABLE");
    assert_eq!(code(&offline), Some(4));
    assert!(
        !v.case
            .root
            .exists("lodi-home/cache/versions/ubuntu/tzdata.json")
    );
}

// ------------------------------------------------------------------ the plan ---

/// Plan prints one behind-latest line for a pinned name the live archive has moved past, and
/// none for a pinned name it has not.
#[test]
fn plan_prints_one_behind_latest_line_for_a_pinned_name_the_archive_moved_past() {
    let v = Verbs::on(
        "verbs-behind",
        with(Machine::debian(), &[("bc", BC), ("tzdata", TZ_OLDER)])
            .offering(pkg("tzdata", "2026c-0+deb12u1")),
        false,
    );
    v.case.set_manifest(&host(
        "debian",
        None,
        &["bc", "tzdata"],
        &[("bc", "2026-09-01"), ("tzdata", "2025-03-01")],
    ));
    let plan = v.run("plan", &[]);
    ok(&plan);
    let behind: Vec<String> = err(&plan)
        .lines()
        .filter(|l| l.contains("behind latest"))
        .map(str::to_string)
        .collect();
    assert_eq!(behind.len(), 1, "{}", story(&plan));
    assert!(
        behind[0].contains("tzdata") && behind[0].contains("2026c-0+deb12u1"),
        "{behind:?}"
    );
}

// ------------------------------------------------------------ the re-import ---

/// Re-import never moves a pin: the snapshot, every pin and their `lodi.lock` section are kept
/// while what changed on the machine is merged in.
#[test]
fn a_re_import_keeps_the_snapshot_pins_and_lock() {
    let v = Verbs::debian("verbs-reimport", &[("bc", BC), ("tzdata", TZ_RECENT)]);
    ok(&v.run("import", &[]));
    ok(&v.run("pin", &["--all", "--to", RECENT]));
    ok(&v.run("pin", &["tzdata", "--to", RECENT]));
    let lock = v.lock().expect("the host's lodi.lock section");
    v.case.install_by_hand(pkg("tree", "2.1.0-1"));
    let again = v.run("import", &[]);
    ok(&again);
    let text = v.case.manifest();
    assert!(text.contains("\"tree\""), "{}", story(&again));
    assert!(text.contains(&format!("snapshot = \"{RECENT}\"")), "{text}");
    assert!(text.contains(&format!("tzdata = \"{RECENT}\"")), "{text}");
    assert_eq!(v.lock().expect("the host's lodi.lock section"), lock);
}
