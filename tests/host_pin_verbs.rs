//! M-Pin verbs (m140-verbs, pv-1): the parts of `host versions`, `host pin` and `host unpin`
//! that are this lane's own and need no resolver — the one-key, byte-preserving
//! rewrite of `host.toml` (V3–V7), the name-keyed versions cache (V2), the versions table, the
//! advisory lock on the host directory and the ordered, atomic commit of `pins.lock` then
//! `host.toml` (V1).
//!
//! Every case runs the real library against scratch directories below `CARGO_TARGET_TMPDIR`, a
//! fixed clock and an injected fetch; nothing reaches the network, `/` or a package manager. The
//! flake loop runs the real binary on two scratch roots (`tests/support/pinverbs.rs`).

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
#[path = "support/pinverbs.rs"]
mod pinverbs;
mod support;
#[path = "support/wait.rs"]
mod wait;

use std::cell::Cell;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use fakehost::{Machine, out, story};
use lodi::diag::Diagnostic;
use lodi::hostscope::pin::verbs::cache::{Arrival, Source, VersionsCache};
use lodi::hostscope::pin::verbs::commit::{self, LockChange};
use lodi::hostscope::pin::verbs::dirlock::HostDirLock;
use lodi::hostscope::pin::verbs::{rewrite, table};
use pinverbs::{BC, OLDER, RECENT, TZ_OLDER, TZ_RECENT, Verbs, census, copy_tree, ok, pkg};

const HOST: &str = "\
# my laptop
[host]
distro = \"debian\"
snapshot = \"2026-09-18T14:00:00Z\"   # recorded by the import

[packages]
common = [\"bc\", \"curl\", \"git\"]   # keep sorted
hold = [\"git\"]

[packages.debian]
add = [\"htop\"]
";

/// The lines of `after` that are not in `before`, and those of `before` not in `after`.
fn line_diff(before: &str, after: &str) -> (Vec<String>, Vec<String>) {
    let b: Vec<&str> = before.lines().collect();
    let a: Vec<&str> = after.lines().collect();
    let added = a
        .iter()
        .filter(|l| !b.contains(l))
        .map(|l| l.to_string())
        .collect();
    let removed = b
        .iter()
        .filter(|l| !a.contains(l))
        .map(|l| l.to_string())
        .collect();
    (added, removed)
}

// ---- V5, V6, V7: one package, one key --------------------------------------------------------

#[test]
fn pin_one_records_one_entry_and_changes_one_line() {
    let edit = rewrite::set_pin(HOST, "bc", Some("1.07.1-3")).unwrap();
    assert!(edit.changed);
    let (added, removed) = line_diff(HOST, &edit.text);
    assert_eq!(removed, Vec::<String>::new(), "{}", edit.text);
    assert_eq!(
        added,
        vec![
            "[packages.pin]".to_string(),
            "bc = \"1.07.1-3\"".to_string()
        ],
        "{}",
        edit.text
    );
    assert!(
        edit.text.starts_with(HOST),
        "every earlier byte kept:\n{}",
        edit.text
    );
    // A second name in the existing table is exactly one more line.
    let second = rewrite::set_pin(&edit.text, "curl", Some("2026-01-04")).unwrap();
    let (added, removed) = line_diff(&edit.text, &second.text);
    assert_eq!(removed, Vec::<String>::new());
    assert_eq!(added, vec!["curl = \"2026-01-04\"".to_string()]);
}

#[test]
fn unpin_one_removes_one_entry_and_one_line() {
    let one = rewrite::set_pin(HOST, "bc", Some("1.07.1-3")).unwrap().text;
    let two = rewrite::set_pin(&one, "curl", Some("2026-01-04"))
        .unwrap()
        .text;
    let back = rewrite::set_pin(&two, "curl", None).unwrap();
    assert!(back.changed);
    assert_eq!(back.text, one, "one key removed, every other byte kept");
    // The table goes with its last key.
    let empty = rewrite::set_pin(&one, "bc", None).unwrap();
    assert_eq!(empty.text, HOST);
    // A name with no pin changes nothing.
    let none = rewrite::set_pin(HOST, "git", None).unwrap();
    assert!(!none.changed);
    assert_eq!(none.text, HOST);
}

#[test]
fn pin_moves_one_entry_to_another_snapshot() {
    let one = rewrite::set_pin(HOST, "bc", Some("2026-01-04"))
        .unwrap()
        .text;
    let two = rewrite::set_pin(&one, "curl", Some("2026-01-04"))
        .unwrap()
        .text;
    let moved = rewrite::set_pin(&two, "bc", Some("2026-03-01")).unwrap();
    assert!(moved.changed);
    let (added, removed) = line_diff(&two, &moved.text);
    assert_eq!(removed, vec!["bc = \"2026-01-04\"".to_string()]);
    assert_eq!(added, vec!["bc = \"2026-03-01\"".to_string()]);
    assert_eq!(
        moved.text.len(),
        two.len(),
        "the same bytes, moved in place"
    );
}

#[test]
fn pin_twice_is_byte_identical() {
    let one = rewrite::set_pin(HOST, "bc", Some("1.07.1-3")).unwrap().text;
    let again = rewrite::set_pin(&one, "bc", Some("1.07.1-3")).unwrap();
    assert!(!again.changed);
    assert_eq!(again.text, one);
    // A hand-edited pin with its own spacing and comment, pinned to its own value.
    let hand = format!("{HOST}\n[packages.pin]\nbc   =   \"1.07.1-3\"   # hand\n");
    let same = rewrite::set_pin(&hand, "bc", Some("1.07.1-3")).unwrap();
    assert!(!same.changed);
    assert_eq!(same.text, hand);
    let snap = rewrite::set_snapshot(HOST, Some("2026-09-18T14:00:00Z")).unwrap();
    assert!(!snap.changed);
    assert_eq!(snap.text, HOST);
}

#[test]
fn a_moved_key_keeps_its_comment() {
    let hand = format!("{HOST}\n[packages.pin]\nbc = \"1.07.1-3\"   # why\n");
    let moved = rewrite::set_pin(&hand, "bc", Some("1.07.1-4")).unwrap();
    assert!(
        moved.text.contains("bc = \"1.07.1-4\"   # why"),
        "{}",
        moved.text
    );
    let snap = rewrite::set_snapshot(HOST, Some("2026-09-20T00:00:00Z")).unwrap();
    assert!(
        snap.text
            .contains("snapshot = \"2026-09-20T00:00:00Z\"   # recorded by the import"),
        "{}",
        snap.text
    );
}

// ---- V3, V4: the whole file ------------------------------------------------------------------

#[test]
fn pin_all_moves_the_file_to_another_snapshot() {
    let pinned = rewrite::set_pin(HOST, "bc", Some("1.07.1-3")).unwrap().text;
    let moved = rewrite::set_snapshot(&pinned, Some("2026-09-24T21:00:00Z")).unwrap();
    assert!(moved.changed);
    let (added, removed) = line_diff(&pinned, &moved.text);
    assert_eq!(
        removed,
        vec!["snapshot = \"2026-09-18T14:00:00Z\"   # recorded by the import".to_string()]
    );
    assert_eq!(
        added,
        vec!["snapshot = \"2026-09-24T21:00:00Z\"   # recorded by the import".to_string()]
    );
    assert!(
        moved.text.contains("bc = \"1.07.1-3\""),
        "per-entry pins left alone"
    );
}

#[test]
fn pin_all_sets_a_snapshot_the_file_lacks() {
    let bare = HOST.replace(
        "snapshot = \"2026-09-18T14:00:00Z\"   # recorded by the import\n",
        "",
    );
    let set = rewrite::set_snapshot(&bare, Some("2026-09-24T21:00:00Z")).unwrap();
    let (added, removed) = line_diff(&bare, &set.text);
    assert_eq!(removed, Vec::<String>::new());
    assert_eq!(
        added,
        vec!["snapshot = \"2026-09-24T21:00:00Z\"".to_string()]
    );
    let host_at = set.text.find("[host]").unwrap();
    let packages_at = set.text.find("[packages]").unwrap();
    let snap_at = set.text.find("snapshot =").unwrap();
    assert!(host_at < snap_at && snap_at < packages_at, "{}", set.text);
}

#[test]
fn pin_all_without_to_takes_the_last_ended_day_everywhere() {
    // 2026-09-24T21:33:52Z: a dated apt index of an instant this recent can still change, so apt
    // takes the last ended day too (LD-444).
    let now = 1_790_285_632;
    assert_eq!(lodi::util::format_utc(now), "2026-09-24T21:33:52Z");
    assert_eq!(
        rewrite::default_snapshot("debian", now).unwrap(),
        "2026-09-23T00:00:00Z"
    );
    assert_eq!(
        rewrite::default_snapshot("ubuntu", now).unwrap(),
        "2026-09-23T00:00:00Z"
    );
    let midnight = lodi::util::parse_utc("2026-09-26T00:07:00Z").unwrap();
    assert_eq!(
        rewrite::default_snapshot("debian", midnight).unwrap(),
        "2026-09-25T00:00:00Z"
    );
    assert_eq!(
        rewrite::default_snapshot("arch", now).unwrap(),
        "2026-09-23T00:00:00Z"
    );
    assert_eq!(
        rewrite::default_snapshot("fedora", now).unwrap_err().code,
        "E_UNSUPPORTED"
    );
}

#[test]
fn unpin_all_floats_every_entry() {
    let text = format!(
        "{HOST}\n[packages.pin]\nbc = \"1.07.1-3\"\n\n[packages.debian.pin]\ncurl = \"2026-01-04\"\n"
    );
    let floated = rewrite::float_all(&text).unwrap();
    assert!(floated.changed);
    let expected = HOST.replace(
        "snapshot = \"2026-09-18T14:00:00Z\"   # recorded by the import\n",
        "",
    );
    assert_eq!(
        floated.text.trim_end(),
        expected.trim_end(),
        "{}",
        floated.text
    );
    assert!(
        floated.text.contains("add = [\"htop\"]"),
        "the distro table's other keys kept"
    );
    let again = rewrite::float_all(&floated.text).unwrap();
    assert!(!again.changed);
    assert_eq!(again.text, floated.text);
}

#[test]
fn a_file_that_does_not_parse_is_refused_and_nothing_is_rewritten() {
    let err = rewrite::set_pin("[host\n", "bc", Some("1")).unwrap_err();
    assert_eq!(err.code, "E_SYNTAX");
    let err = rewrite::set_pin("packages = 3\n", "bc", Some("1")).unwrap_err();
    assert_eq!(err.code, "E_TYPE");
}

// ---- V2: the versions cache ------------------------------------------------------------------

const DAY: i64 = 86_400;
const NOW: i64 = 1_790_285_632;

fn rows() -> Vec<Arrival> {
    vec![
        Arrival {
            version: "8.5.0-2".into(),
            arrived: NOW - 40 * DAY,
        },
        Arrival {
            version: "7.88.1-10+deb12u8".into(),
            arrived: NOW - 400 * DAY,
        },
    ]
}

#[test]
fn a_young_cache_is_used_with_zero_requests() {
    let home = support::scratch("pin-cache-young");
    let cache = VersionsCache::new(&home, "debian");
    let requests = Cell::new(0);
    let first = cache
        .lookup("curl", NOW, || {
            requests.set(requests.get() + 1);
            Ok(rows())
        })
        .unwrap();
    assert_eq!(first.source, Source::Fetched);
    assert_eq!(requests.get(), 1);
    let path = home.join("cache/versions/debian/curl.json");
    assert!(path.is_file(), "one file per name at {}", path.display());
    let later = cache
        .lookup("curl", NOW + DAY - 1, || {
            requests.set(requests.get() + 1);
            Ok(Vec::new())
        })
        .unwrap();
    assert_eq!(later.source, Source::Cached);
    assert_eq!(requests.get(), 1, "younger than a day: zero requests");
    assert_eq!(later.rows, rows());
}

#[test]
fn an_old_cache_is_refetched() {
    let home = support::scratch("pin-cache-old");
    let cache = VersionsCache::new(&home, "debian");
    cache.lookup("curl", NOW, || Ok(rows())).unwrap();
    let requests = Cell::new(0);
    let newer = vec![Arrival {
        version: "8.6.0-1".into(),
        arrived: NOW + DAY,
    }];
    let later = cache
        .lookup("curl", NOW + DAY, || {
            requests.set(requests.get() + 1);
            Ok(newer.clone())
        })
        .unwrap();
    assert_eq!(requests.get(), 1, "a day old: refetched");
    assert_eq!(later.source, Source::Fetched);
    assert_eq!(later.rows, newer);
    let again = cache
        .lookup("curl", NOW + DAY + 1, || panic!("no request"))
        .unwrap();
    assert_eq!(again.rows, newer, "the refetch replaced the file");
}

#[test]
fn a_corrupt_cache_is_refused_and_refetched_never_repaired() {
    let home = support::scratch("pin-cache-corrupt");
    let cache = VersionsCache::new(&home, "debian");
    cache.lookup("curl", NOW, || Ok(rows())).unwrap();
    let path = home.join("cache/versions/debian/curl.json");
    let good = fs::read_to_string(&path).unwrap();
    // A truncated file, and a well-formed file for another name, are both corrupt.
    for bad in [
        &good[..good.len() / 2],
        &good.replace("\"curl\"", "\"wget\""),
    ] {
        fs::write(&path, bad).unwrap();
        let requests = Cell::new(0);
        let got = cache
            .lookup("curl", NOW + 1, || {
                requests.set(requests.get() + 1);
                Ok(rows())
            })
            .unwrap();
        assert_eq!(requests.get(), 1, "refused and refetched: {bad}");
        assert_eq!(got.source, Source::Fetched);
        let refetched = good.replace(&NOW.to_string(), &(NOW + 1).to_string());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            refetched,
            "rewritten from the fetch"
        );
    }
}

#[test]
fn a_cold_cache_with_no_network_is_e_repo_unreachable() {
    let home = support::scratch("pin-cache-cold");
    let cache = VersionsCache::new(&home, "ubuntu");
    let offline = || -> Result<Vec<Arrival>, Diagnostic> {
        Err(Diagnostic::new("E_NETWORK", "connection refused"))
    };
    let err = cache.lookup("curl", NOW, offline).unwrap_err();
    assert_eq!(err.code, "E_REPO_UNREACHABLE");
    assert_eq!(lodi::diag::exit_status(err.code), 4);
    assert!(err.message.contains("curl"), "{err}");
    assert!(
        !home.join("cache/versions/ubuntu/curl.json").exists(),
        "nothing written"
    );
    // A stale file with no network is refused the same way: it is refetched, never served.
    cache.lookup("curl", NOW, || Ok(rows())).unwrap();
    let err = cache.lookup("curl", NOW + 2 * DAY, offline).unwrap_err();
    assert_eq!(err.code, "E_REPO_UNREACHABLE");
}

#[test]
fn a_name_that_is_not_a_package_name_touches_no_path() {
    let home = support::scratch("pin-cache-name");
    let cache = VersionsCache::new(&home, "debian");
    for name in ["../x", "a/b", "", ".hidden"] {
        let err = cache
            .lookup(name, NOW, || panic!("no request"))
            .unwrap_err();
        assert_eq!(err.code, "E_UNKNOWN_PACKAGE", "{name}");
    }
}

// ---- host versions: the table ----------------------------------------------------------------

#[test]
fn versions_prints_newest_first_with_states_and_one_line_to_copy() {
    let mut offered = rows();
    offered.reverse();
    offered.push(Arrival {
        version: "8.6.0-1".into(),
        arrived: NOW - DAY,
    });
    let out = table::render("curl", &offered, Some("7.88.1-10+deb12u8"), Some("8.5.0-2"));
    // check-host-safety: refusal — a line the table prints to copy; nothing runs it.
    let copy = "lodi host pin curl --to 8.6.0-1\n";
    let expected = format!(
        "\
version            arrived     state
8.6.0-1            2026-09-23  latest
8.5.0-2            2026-08-15  pinned
7.88.1-10+deb12u8  2025-08-20  installed

{copy}"
    );
    assert_eq!(out, expected);
}

#[test]
fn a_name_the_distribution_does_not_offer_names_the_nearest() {
    let known: Vec<String> = ["curl", "curlftpfs", "libcurl4", "git"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let err = table::unknown_package("culr", "debian", &known);
    assert_eq!(err.code, "E_UNKNOWN_PACKAGE");
    assert_eq!(lodi::diag::exit_status(err.code), 3);
    let text = err.to_string();
    assert!(text.contains("did you mean `curl`?"), "{text}");
    let none = table::unknown_package("zzzzzzzz", "debian", &known).to_string();
    assert!(!none.contains("did you mean"), "{none}");
}

// ---- V1: the directory lock and the ordered commit -------------------------------------------

#[test]
fn a_second_concurrent_pin_is_refused_by_name() {
    let dir = support::scratch("pin-dirlock");
    let held = HostDirLock::take(&dir).unwrap();
    let err = HostDirLock::take(&dir).unwrap_err();
    assert_eq!(err.code, "E_SYSTEM_BUSY");
    assert!(err.message.contains(&dir.display().to_string()), "{err}");
    drop(held);
    let _again = HostDirLock::take(&dir).unwrap();
    let entries: Vec<_> = fs::read_dir(&dir).unwrap().collect();
    assert!(
        entries.is_empty(),
        "the lock adds no file to the host directory"
    );
}

#[test]
fn pins_lock_is_written_first_then_host_toml_atomically_keeping_the_mode() {
    let dir = support::scratch("pin-commit");
    let host = dir.join("host.toml");
    fs::write(&host, HOST).unwrap();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o640)).unwrap();
    let before = fs::metadata(&host).unwrap();
    let new = rewrite::set_pin(HOST, "bc", Some("1.07.1-3")).unwrap().text;
    let order = std::cell::RefCell::new(Vec::new());
    commit::write(
        &dir,
        LockChange::Write(b"{}\n".to_vec()),
        Some(&new),
        || {
            order.borrow_mut().push((
                dir.join("pins.lock").exists(),
                fs::read_to_string(&host).unwrap() == HOST,
            ));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        *order.borrow(),
        vec![(true, true)],
        "pins.lock first, host.toml after"
    );
    assert_eq!(fs::read_to_string(&host).unwrap(), new);
    let after = fs::metadata(&host).unwrap();
    assert_eq!(after.permissions().mode() & 0o7777, 0o640);
    assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
    let lock = fs::metadata(dir.join("pins.lock")).unwrap();
    assert_eq!(lock.permissions().mode() & 0o7777, 0o644);
    let names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "no temporary file left: {names:?}");
}

#[test]
fn a_crash_between_the_two_writes_leaves_host_toml_unchanged() {
    let dir = support::scratch("pin-crash");
    let host = dir.join("host.toml");
    fs::write(&host, HOST).unwrap();
    let new = rewrite::set_pin(HOST, "bc", Some("1.07.1-3")).unwrap().text;
    let err = commit::write(
        &dir,
        LockChange::Write(b"{\"x\": 1}\n".to_vec()),
        Some(&new),
        || Err(std::io::Error::other("injected crash")),
    )
    .unwrap_err();
    assert!(err.to_string().contains("injected crash"), "{err}");
    assert_eq!(
        fs::read_to_string(&host).unwrap(),
        HOST,
        "host.toml untouched"
    );
    assert_eq!(fs::read(dir.join("pins.lock")).unwrap(), b"{\"x\": 1}\n");
}

#[test]
fn unpin_all_removes_pins_lock_first() {
    let dir = support::scratch("pin-remove");
    let host = dir.join("host.toml");
    fs::write(&host, HOST).unwrap();
    fs::write(dir.join("pins.lock"), "{}\n").unwrap();
    let floated = rewrite::float_all(HOST).unwrap().text;
    let seen = Cell::new(false);
    commit::write(&dir, LockChange::Remove, Some(&floated), || {
        seen.set(!dir.join("pins.lock").exists() && fs::read_to_string(&host).unwrap() == HOST);
        Ok(())
    })
    .unwrap();
    assert!(
        seen.get(),
        "pins.lock removed before host.toml was rewritten"
    );
    assert_eq!(fs::read_to_string(&host).unwrap(), floated);
    // Nothing to change writes nothing and moves no mtime.
    let mtime = fs::metadata(&host).unwrap().mtime_nsec();
    let outcome = commit::write(&dir, LockChange::Remove, None, || Ok(())).unwrap();
    assert!(!outcome.changed);
    assert_eq!(fs::metadata(&host).unwrap().mtime_nsec(), mtime);
}

/// V8. Import into a directory of hosts, pin, copy the directory to another machine with another
/// hostname and apply it there with `--host`: the pinned names install at exactly the recorded
/// versions, a second apply has nothing to do, and the directory is byte-identical afterwards.
#[test]
fn flake_loop_same_versions_on_a_second_root() {
    let first = Verbs::debian("verbs-flake-first", &[("bc", BC), ("tzdata", TZ_RECENT)]);
    fs::remove_dir(first.dir()).unwrap();
    ok(&first.run("import", &[&first.hosts()]));
    ok(&first.run("pin", &["--all", "--to", RECENT, &first.hosts()]));
    ok(&first.run("pin", &["tzdata", &first.hosts()]));
    ok(&first.run("pin", &["bc", &first.hosts()]));
    let text = first.host_toml();
    for name in ["bc", "tzdata"] {
        assert!(text.contains(&format!("{name} = \"{RECENT}\"")), "{text}");
    }

    let second = Verbs::named(
        "verbs-flake-second",
        Machine::debian()
            .offering(pkg("bc", "1.07.1-3+b2"))
            .offering(pkg("tzdata", "2026c-0+deb12u1")),
        false,
        "other",
    );
    copy_tree(&first.dir(), &second.dir());
    let before = census(&second.dir());
    let apply = second.run("apply", &[&second.hosts(), "--host", "box"]);
    ok(&apply);
    let machine = second.case.machine();
    assert_eq!(
        machine["installed"]["bc"]["version"],
        BC,
        "{}",
        story(&apply)
    );
    assert_eq!(machine["installed"]["tzdata"]["version"], TZ_RECENT);
    let again = second.run("apply", &[&second.hosts(), "--host", "box"]);
    ok(&again);
    assert!(
        out(&again).trim_end().ends_with("nothing to do"),
        "{}",
        story(&again)
    );
    assert_eq!(census(&second.dir()), before);
}

/// D5 (LD-395). Pin an older version and apply, pin a newer one and apply, then put the older
/// commit's `host.toml` and root `lodi.lock` back byte for byte — what `git revert` of the newer pin
/// writes — and apply: the declared package is back at exactly the older version.
#[test]
fn a_reverted_pin_reinstalls_the_older_version() {
    let verbs = Verbs::debian("verbs-revert", &[("bc", BC), ("tzdata", TZ_RECENT)]);
    fs::remove_dir(verbs.dir()).unwrap();
    ok(&verbs.run("import", &[&verbs.hosts()]));
    ok(&verbs.run("pin", &["--all", "--to", RECENT, &verbs.hosts()]));
    let tzdata = |verbs: &Verbs| verbs.case.machine()["installed"]["tzdata"]["version"].clone();

    ok(&verbs.run("pin", &["tzdata", "--to", OLDER, &verbs.hosts()]));
    // The pins of a host a SOURCE chose live in the repository's root lock (LD-416).
    let older = (verbs.host_toml(), verbs.root_lock().unwrap());
    let apply = verbs.run("apply", &[&verbs.hosts()]);
    ok(&apply);
    assert_eq!(tzdata(&verbs), TZ_OLDER, "{}", story(&apply));

    ok(&verbs.run("pin", &["tzdata", "--to", RECENT, &verbs.hosts()]));
    let apply = verbs.run("apply", &[&verbs.hosts()]);
    ok(&apply);
    assert_eq!(tzdata(&verbs), TZ_RECENT, "{}", story(&apply));

    verbs.set_host(&older.0);
    fs::write(verbs.case.root.path("hosts/lodi.lock"), &older.1).unwrap();
    let apply = verbs.run("apply", &[&verbs.hosts()]);
    ok(&apply);
    assert_eq!(tzdata(&verbs), TZ_OLDER, "{}", story(&apply));
    let again = verbs.run("apply", &[&verbs.hosts()]);
    ok(&again);
    assert!(
        out(&again).trim_end().ends_with("nothing to do"),
        "{}",
        story(&again)
    );
}
