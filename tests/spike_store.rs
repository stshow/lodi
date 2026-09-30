//! M-Spike S-3: verified artifacts, safe extraction, the atomic store and the NAR tree hash.
//!
//! Offline and synthetic. Downloads come from an in-memory fetcher; hostile archives are
//! written byte by byte (`tests/support`). The NAR oracle is Nix: `tests/fixtures/spike/nar/
//! expected.json` holds the SHA-256 of `nix-store --dump` for every fixture tree (recorded by
//! `scripts/spike-nar.sh --record`), and when `nix-store` is on `PATH` the test also compares
//! the serialized bytes with Nix's own, byte for byte. `LODI_NAR_REQUIRE_NIX=1` (set by
//! `scripts/spike-nar.sh`) makes a missing `nix-store` a failure instead of a skipped
//! comparison.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier, Mutex};

use lodi::archive::{self, Limits};
use lodi::fetch::{FetchError, Fetcher, Sink};
use lodi::nar;
use lodi::store::{self, Point, Report, Store};
use lodi::util::sha256_hex;
use support::*;

// ---------------------------------------------------------------------------------------------
// NAR

const NAR_EXPECTED: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/spike/nar/expected.json"
);

fn write(path: &Path, body: &[u8], mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn link(path: &Path, target: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target, path).unwrap();
}

/// Deterministic pseudo-random bytes (xorshift), so large fixtures need no committed blobs.
fn noise(len: usize, mut seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.extend_from_slice(&seed.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// The NAR fixture inventory: name -> a function that builds the tree at a path.
/// Covers regular, executable, empty and padded files, directories (empty, nested, sort
/// order), and symlinks (relative, to a directory, dangling, absolute, upward), plus a hard
/// link and mode bits Nix ignores.
type Builder = fn(&Path);

fn nar_fixtures() -> Vec<(&'static str, Builder)> {
    vec![
        ("file-empty", |p| write(p, b"", 0o644)),
        ("file-regular", |p| write(p, b"hello\n", 0o644)),
        ("file-executable", |p| {
            write(p, b"#!/bin/sh\necho hi\n", 0o755)
        }),
        ("file-8-bytes", |p| write(p, b"12345678", 0o644)),
        ("file-1mib-plus-3", |p| {
            write(p, &noise((1 << 20) + 3, 7), 0o644)
        }),
        ("symlink-top", |p| link(p, "some/target")),
        ("dir-empty", |p| fs::create_dir_all(p).unwrap()),
        ("dir-nested-empty", |p| {
            fs::create_dir_all(p.join("a/b/c")).unwrap()
        }),
        ("dir-mixed", |p| {
            fs::create_dir_all(p).unwrap();
            write(&p.join("a.txt"), b"hello\n", 0o644);
            write(&p.join("eight"), b"12345678", 0o644);
            write(&p.join("nine"), b"123456789", 0o644);
            write(&p.join("exec.sh"), b"#!/bin/sh\nexit 0\n", 0o755);
            write(&p.join("owner-exec-only"), b"x", 0o700);
            write(&p.join("group-exec-only"), b"x", 0o654);
            write(&p.join("read-only"), b"ro", 0o444);
            for name in ["B", "_", "a-b", "a.b", "Z", "\u{e9}", "with space"] {
                write(&p.join(name), name.as_bytes(), 0o644);
            }
            write(&p.join("sub/deeper/file"), b"deep", 0o644);
            fs::create_dir_all(p.join("empty-sub")).unwrap();
            link(&p.join("link-rel"), "a.txt");
            link(&p.join("link-dir"), "sub");
            link(&p.join("link-dangling"), "missing/x");
            link(&p.join("link-abs"), "/nonexistent/abs");
            link(&p.join("link-up"), "../outside");
            fs::hard_link(p.join("a.txt"), p.join("hard")).unwrap();
            write(&p.join("big"), &noise(200_003, 11), 0o755);
        }),
    ]
}

fn build_nar_fixtures(root: &Path) -> Vec<(String, PathBuf)> {
    nar_fixtures()
        .into_iter()
        .map(|(name, build)| {
            let path = root.join(name);
            build(&path);
            (name.to_string(), path)
        })
        .collect()
}

fn nix_dump(path: &Path) -> Option<Vec<u8>> {
    let out = Command::new("nix-store")
        .arg("--dump")
        .arg(path)
        .output()
        .ok()?;
    assert!(
        out.status.success(),
        "nix-store --dump {} failed: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    Some(out.stdout)
}

/// `LODI_NAR_RECORD=<file>`: write Nix's hashes of the fixtures (from `nix-store --dump`, never
/// from Lodi) as the committed oracle. Used only by `scripts/spike-nar.sh --record`.
fn record_nix_hashes(fixtures: &[(String, PathBuf)], out: &Path) {
    let version = Command::new("nix-store").arg("--version").output().unwrap();
    let mut recorded = serde_json::Map::new();
    for (name, path) in fixtures {
        let dump = nix_dump(path).expect("recording needs nix-store");
        recorded.insert(
            name.clone(),
            serde_json::json!({ "nixSha256": sha256_hex(&dump), "narBytes": dump.len() }),
        );
    }
    let doc = serde_json::json!({
        "source": "nix-store --dump <fixture> | sha256sum, fixtures built by tests/spike_store.rs",
        "nixVersion": String::from_utf8_lossy(&version.stdout).trim(),
        "fixtures": recorded,
    });
    fs::write(out, lodi::lock::canonical_json(&doc)).unwrap();
}

/// `LODI_NAR_EXTRA=<dir>[:<dir>…]` (set by `scripts/spike-nar.sh`): real trees, hashed by Lodi
/// and dumped by Nix, compared and timed. Only base names are printed.
fn compare_extra_trees(extra: &std::ffi::OsStr) {
    let mut compared = 0;
    for dir in std::env::split_paths(&extra) {
        let started = std::time::Instant::now();
        let mut ours = Vec::new();
        nar::dump(&dir, &mut ours).unwrap();
        let hash = nar::hash(&dir).unwrap();
        let lodi_us = started.elapsed().as_micros();
        let started = std::time::Instant::now();
        let theirs = nix_dump(&dir).expect("LODI_NAR_EXTRA needs nix-store");
        let nix_us = started.elapsed().as_micros();
        let name = dir.file_name().unwrap().to_string_lossy();
        assert!(
            ours == theirs,
            "{name}: NAR bytes differ from nix-store --dump"
        );
        assert_eq!(hash, format!("sha256:{}", sha256_hex(&theirs)));
        println!(
            "nar-extra {name} nar_bytes={} lodi_dump_and_hash_us={lodi_us} nix_dump_us={nix_us} match=yes",
            ours.len()
        );
        compared += 1;
    }
    assert!(compared > 0, "LODI_NAR_EXTRA named no directory");
}

#[test]
fn nar_fixtures_agree_with_nix() {
    let root = scratch("nar");
    let fixtures = build_nar_fixtures(&root);
    if let Some(out) = std::env::var_os("LODI_NAR_RECORD") {
        record_nix_hashes(&fixtures, Path::new(&out));
    }
    let expected: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(&fs::read_to_string(NAR_EXPECTED).unwrap()).unwrap();
    let recorded = &expected["fixtures"];
    let names: Vec<&str> = fixtures.iter().map(|(n, _)| n.as_str()).collect();
    let recorded_names: Vec<&String> = recorded.as_object().unwrap().keys().collect();
    assert_eq!(
        names
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        recorded_names.iter().map(|s| s.as_str()).collect(),
        "every fixture has a recorded Nix hash and vice versa"
    );
    let require_nix = std::env::var_os("LODI_NAR_REQUIRE_NIX").is_some_and(|v| v == "1");
    let mut live = 0;
    for (name, path) in &fixtures {
        let mut ours = Vec::new();
        nar::dump(path, &mut ours).unwrap();
        let hash = nar::hash(path).unwrap();
        assert_eq!(hash, format!("sha256:{}", sha256_hex(&ours)), "{name}");
        assert_eq!(
            hash,
            format!("sha256:{}", recorded[name]["nixSha256"].as_str().unwrap()),
            "{name}: our NAR hash differs from the one Nix recorded"
        );
        assert_eq!(
            ours.len() as u64,
            recorded[name]["narBytes"].as_u64().unwrap(),
            "{name}"
        );
        match nix_dump(path) {
            Some(theirs) => {
                assert!(
                    ours == theirs,
                    "{name}: NAR bytes differ from nix-store --dump"
                );
                live += 1;
            }
            None => assert!(
                !require_nix,
                "LODI_NAR_REQUIRE_NIX=1 but nix-store is missing"
            ),
        }
        let started = std::time::Instant::now();
        nar::hash(path).unwrap();
        println!(
            "nar-fixture {name} bytes={} hash_us={}",
            ours.len(),
            started.elapsed().as_micros()
        );
    }
    println!("nar-live-comparisons {live} of {}", fixtures.len());
    if require_nix {
        assert_eq!(live, fixtures.len());
    }
    if let Some(extra) = std::env::var_os("LODI_NAR_EXTRA") {
        compare_extra_trees(&extra);
    }
}

#[test]
fn nar_refuses_special_files_and_changes_with_the_owner_execute_bit_only() {
    let root = scratch("nar-special");
    let fifo = root.join("fifo");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: mkfifo with a valid path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
    assert!(matches!(
        nar::hash(&root),
        Err(nar::NarError::Unsupported(_))
    ));
    fs::remove_file(&fifo).unwrap();

    let f = root.join("f");
    write(&f, b"x", 0o644);
    let plain = nar::hash(&root).unwrap();
    for (mode, same) in [(0o600, true), (0o664, true), (0o654, true), (0o744, false)] {
        fs::set_permissions(&f, fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(nar::hash(&root).unwrap() == plain, same, "mode {mode:o}");
    }
}

// ---------------------------------------------------------------------------------------------
// Verified downloads and the store

/// An in-memory fetcher: URL -> body, with a request log.
#[derive(Default)]
struct MapFetcher {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    log: Mutex<Vec<String>>,
    /// Abort the process after writing half of this URL's body (interruption tests).
    abort_mid: Option<String>,
    /// Write half of this URL's body and then other bytes, restart the sink, and write the body
    /// whole: what a fetcher that tried again after a failed attempt does (LD-386).
    restart_mid: Option<String>,
}

impl MapFetcher {
    fn with(tools: &[&Tool]) -> MapFetcher {
        MapFetcher {
            files: Mutex::new(
                tools
                    .iter()
                    .map(|t| (t.url.clone(), t.bytes.clone()))
                    .collect(),
            ),
            ..MapFetcher::default()
        }
    }

    fn count(&self, url: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|u| *u == url)
            .count()
    }
}

impl Fetcher for MapFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        self.log.lock().unwrap().push(url.to_string());
        self.files
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .ok_or_else(|| FetchError::NotFound(url.to_string()))
    }

    fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn download(&self, url: &str, out: &mut dyn Sink, limit: u64) -> Result<u64, FetchError> {
        let body = self.get(url)?;
        if self.restart_mid.as_deref() == Some(url) {
            out.write_all(&body[..body.len() / 2]).unwrap();
            out.write_all(b"the rest of a failed attempt").unwrap();
            out.restart().unwrap();
        }
        if self.abort_mid.as_deref() == Some(url) {
            out.write_all(&body[..body.len() / 2]).unwrap();
            out.flush().unwrap();
            std::process::abort();
        }
        if body.len() as u64 > limit {
            return Err(FetchError::Transport(url.into(), "too large".into()));
        }
        out.write_all(&body).unwrap();
        Ok(body.len() as u64)
    }
}

fn open_store(name: &str) -> (PathBuf, Store) {
    let home = scratch(name);
    let store = Store::open(&home).unwrap();
    (home, store)
}

fn assert_nothing_published(home: &Path) {
    let entries: Vec<String> = listing(&home.join("store"))
        .into_iter()
        .filter(|p| {
            !p.starts_with(".locks") && !p.starts_with("tmp") && p != ".lock" && p != ".meta"
        })
        .collect();
    assert!(entries.is_empty(), "published: {entries:?}");
    assert!(
        listing(&home.join("store/tmp")).is_empty(),
        "staging residue left"
    );
}

#[test]
fn a_verified_artifact_is_extracted_sealed_and_published_with_separate_identity_and_tree_hash() {
    let (home, store) = open_store("store-publish");
    let py = Tool::python("3.12.14");
    let node = Tool::node("22", "22.23.2");
    let fetcher = MapFetcher::with(&[&py, &node]);
    for tool in [&py, &node] {
        let mut report = Report::default();
        let (name, meta) = store
            .realize_tool(&fetcher, &tool.entry(), &mut report, None)
            .unwrap();
        assert_eq!(
            name,
            format!(
                "art-{}-{}-{}",
                &tool.sha256_hex()[..32],
                tool.label,
                tool.version
            )
        );
        assert_eq!(report.created, std::slice::from_ref(&name));
        assert_eq!(report.downloaded, tool.bytes.len() as u64);
        assert_eq!(meta.identity, format!("sha256:{}", tool.sha256_hex()));
        assert_ne!(
            meta.identity, meta.tree_hash,
            "identity and content hash differ"
        );
        let entry = store.entry_path(&name);
        assert_eq!(meta.tree_hash, nar::hash(&entry).unwrap());
        assert!(meta.complete && meta.kind == "art" && meta.references.is_empty());
        for bin in &tool.bin {
            assert!(fs::metadata(entry.join(bin)).unwrap().is_file());
        }
        // Sealed: read-only directories and files, normalized mtimes.
        let dir_mode = fs::metadata(entry.join("bin"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o555);
        let exe = fs::metadata(entry.join(&tool.bin[0])).unwrap();
        assert_eq!(exe.permissions().mode() & 0o777, 0o555);
        assert_eq!(
            exe.modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            1
        );
        assert!(fs::write(entry.join("bin/new"), b"x").is_err());
        // The verified download is cached under its full hash.
        assert!(store.download_path(&tool.sha256_hex()).is_file());
    }
    // Warm: complete entries are used as they are, without any request.
    let before = fetcher.requests().len();
    let mut report = Report::default();
    store
        .realize_tool(&fetcher, &py.entry(), &mut report, None)
        .unwrap();
    assert_eq!(fetcher.requests().len(), before);
    assert_eq!(report, Report::default());
    assert!(home.join("store/tmp").read_dir().unwrap().next().is_none());
}

#[test]
fn a_hash_mismatch_publishes_and_caches_nothing() {
    let (home, store) = open_store("store-mismatch");
    let py = Tool::python("3.12.14");
    let fetcher = MapFetcher::with(&[&py]);
    // The server now serves other bytes than the lock pins.
    let mut tampered = py.bytes.clone();
    let last = tampered.len() - 10;
    tampered[last] ^= 0xff;
    fetcher
        .files
        .lock()
        .unwrap()
        .insert(py.url.clone(), tampered);
    let err = store
        .realize_tool(&fetcher, &py.entry(), &mut Report::default(), None)
        .unwrap_err();
    assert_eq!(err.code, "E_HASH_MISMATCH");
    assert_eq!(lodi::diag::exit_status(err.code), 5);
    assert!(err.to_string().contains("nothing was stored"));
    assert_nothing_published(&home);
    assert!(listing(&home.join("cache/dl")).is_empty(), "nothing cached");
    // A wrong size alone is refused too.
    let mut entry = py.entry();
    entry.artifacts[0].size = Some(py.bytes.len() as u64 + 1);
    fetcher
        .files
        .lock()
        .unwrap()
        .insert(py.url.clone(), py.bytes.clone());
    let err = store
        .realize_tool(&fetcher, &entry, &mut Report::default(), None)
        .unwrap_err();
    assert!(
        err.code == "E_HASH_MISMATCH" || err.code == "E_FETCH",
        "{err}"
    );
    assert_nothing_published(&home);
}

/// A download tried again after a failed attempt (LD-386) is verified and cached as the last
/// attempt's bytes alone: nothing the failed attempt wrote reaches the digest, the length or the
/// cached file.
#[test]
fn a_download_tried_again_keeps_only_its_last_attempt() {
    let (home, store) = open_store("store-restarted-download");
    let py = Tool::python("3.12.14");
    let fetcher = MapFetcher {
        restart_mid: Some(py.url.clone()),
        ..MapFetcher::with(&[&py])
    };
    let mut report = Report::default();
    let (name, meta) = store
        .realize_tool(&fetcher, &py.entry(), &mut report, None)
        .unwrap();
    assert_eq!(
        fs::read(store.download_path(&py.sha256_hex())).unwrap(),
        py.bytes
    );
    assert_eq!(report.downloaded, py.bytes.len() as u64);
    assert_eq!(meta.tree_hash, nar::hash(&store.entry_path(&name)).unwrap());
    assert!(
        listing(&home.join("cache/dl"))
            .iter()
            .all(|f| !f.ends_with(".part")),
        "no partial download is left"
    );
}

#[test]
fn a_corrupt_cached_download_is_discarded_and_fetched_again() {
    let (_home, store) = open_store("store-corrupt-cache");
    let py = Tool::python("3.12.14");
    let fetcher = MapFetcher::with(&[&py]);
    let cached = store.download_path(&py.sha256_hex());
    fs::write(&cached, b"corrupt bytes under the right name").unwrap();
    let mut report = Report::default();
    let (name, meta) = store
        .realize_tool(&fetcher, &py.entry(), &mut report, None)
        .unwrap();
    assert_eq!(report.cache_discarded, [py.sha256_hex()]);
    assert_eq!(fetcher.count(&py.url), 1);
    assert_eq!(fs::read(&cached).unwrap(), py.bytes);
    assert_eq!(meta.tree_hash, nar::hash(&store.entry_path(&name)).unwrap());

    // A correct cached download is used without a request (an entry that must be rebuilt).
    let (_home2, store2) = open_store("store-good-cache");
    fs::write(store2.download_path(&py.sha256_hex()), &py.bytes).unwrap();
    let fetcher2 = MapFetcher::with(&[&py]);
    store2
        .realize_tool(&fetcher2, &py.entry(), &mut Report::default(), None)
        .unwrap();
    assert!(fetcher2.requests().is_empty());
}

#[test]
fn an_incomplete_entry_is_never_used_and_is_replaced() {
    let (_home, store) = open_store("store-incomplete");
    let py = Tool::python("3.12.14");
    let fetcher = MapFetcher::with(&[&py]);
    let name = store::art_name(&py.entry()).unwrap();
    // A directory without a sidecar (a crash after the rename), holding wrong content.
    fs::create_dir_all(store.entry_path(&name).join("bin")).unwrap();
    fs::write(store.entry_path(&name).join("bin/python3"), b"stale").unwrap();
    assert!(store.complete(&name).is_none());
    let mut report = Report::default();
    let (_, meta) = store
        .realize_tool(&fetcher, &py.entry(), &mut report, None)
        .unwrap();
    assert_eq!(report.incomplete_replaced, std::slice::from_ref(&name));
    assert_ne!(
        fs::read(store.entry_path(&name).join("bin/python3.12")).ok(),
        None
    );
    assert_eq!(meta.tree_hash, nar::hash(&store.entry_path(&name)).unwrap());
    // A sidecar that is not complete is the same case.
    let (_home2, store2) = open_store("store-incomplete-meta");
    store2
        .realize_tool(&fetcher, &py.entry(), &mut Report::default(), None)
        .unwrap();
    let meta_path = store2.home().join(format!("store/.meta/{name}.json"));
    let text = fs::read_to_string(&meta_path)
        .unwrap()
        .replace("\"complete\": true", "\"complete\": false");
    fs::write(&meta_path, text).unwrap();
    assert!(store2.complete(&name).is_none());
    let mut report = Report::default();
    store2
        .realize_tool(&fetcher, &py.entry(), &mut report, None)
        .unwrap();
    assert_eq!(report.incomplete_replaced, std::slice::from_ref(&name));
    assert!(store2.complete(&name).is_some());
}

#[test]
fn a_failure_at_any_point_leaves_no_entry_and_no_residue() {
    let py = Tool::python("3.12.14");
    for point in [Point::Downloaded, Point::Staged, Point::Renamed] {
        let (home, store) = open_store("store-fail-point");
        let fetcher = MapFetcher::with(&[&py]);
        let hook = move |p: Point, _: &str| {
            if p == point {
                Err(io::Error::other("injected"))
            } else {
                Ok(())
            }
        };
        let err = store
            .realize_tool(&fetcher, &py.entry(), &mut Report::default(), Some(&hook))
            .unwrap_err();
        assert_eq!(err.code, "E_STORE_IO", "{point:?}");
        assert_nothing_published(&home);
        // The next realization succeeds from the cached, verified download.
        store
            .realize_tool(&fetcher, &py.entry(), &mut Report::default(), None)
            .unwrap();
        assert_eq!(fetcher.count(&py.url), 1, "{point:?}");
    }
}

const CRASH_TEST: &str = "an_abrupt_stop_at_any_point_publishes_nothing_partial";

/// Child mode of the next test: realize with an abort at `LODI_TEST_CRASH_AT`.
fn crash_child(at: &str, home: &Path) -> ! {
    let py = Tool::python("3.12.14");
    let store = Store::open(home).unwrap();
    let mut fetcher = MapFetcher::with(&[&py]);
    if at == "download" {
        fetcher.abort_mid = Some(py.url.clone());
    }
    let point = match at {
        "downloaded" => Some(Point::Downloaded),
        "staged" => Some(Point::Staged),
        "renamed" => Some(Point::Renamed),
        _ => None,
    };
    let hook = move |p: Point, _: &str| {
        if Some(p) == point {
            std::process::abort();
        }
        Ok(())
    };
    let _ = store.realize_tool(&fetcher, &py.entry(), &mut Report::default(), Some(&hook));
    panic!("the realization was expected to stop at {at}");
}

#[test]
fn an_abrupt_stop_at_any_point_publishes_nothing_partial() {
    if let (Some(at), Some(home)) = (
        std::env::var_os("LODI_TEST_CRASH_AT"),
        std::env::var_os("LODI_TEST_CRASH_HOME"),
    ) {
        crash_child(&at.to_string_lossy(), Path::new(&home));
    }
    let py = Tool::python("3.12.14");
    let reference = {
        let (_h, s) = open_store("store-crash-reference");
        let (name, _) = s
            .realize_tool(
                &MapFetcher::with(&[&py]),
                &py.entry(),
                &mut Report::default(),
                None,
            )
            .unwrap();
        nar::hash(&s.entry_path(&name)).unwrap()
    };
    for at in ["download", "downloaded", "staged", "renamed"] {
        let home = scratch("store-crash");
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CRASH_TEST, "--nocapture", "--test-threads=1"])
            .env("LODI_TEST_CRASH_AT", at)
            .env("LODI_TEST_CRASH_HOME", &home)
            .output()
            .unwrap()
            .status;
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            Some(libc::SIGABRT),
            "{at}: child did not abort"
        );
        let store = Store::open(&home).unwrap();
        let name = store::art_name(&py.entry()).unwrap();
        assert!(
            store.complete(&name).is_none(),
            "{at}: a complete entry exists"
        );
        assert!(
            !home.join(format!("store/.meta/{name}.json")).exists(),
            "{at}: a sidecar exists"
        );
        assert!(
            listing(&home.join("cache/dl"))
                .iter()
                .all(|f| f.ends_with(".part") || *f == py.sha256_hex()),
            "{at}: an unverified download was published to the cache"
        );
        // Recovery: staging and partial downloads of the dead process are pruned and the entry
        // is rebuilt.
        store.prune_staging();
        assert!(listing(&home.join("store/tmp")).is_empty(), "{at}");
        assert!(
            listing(&home.join("cache/dl"))
                .iter()
                .all(|f| !f.ends_with(".part")),
            "{at}"
        );
        let fetcher = MapFetcher::with(&[&py]);
        let mut report = Report::default();
        let (_, meta) = store
            .realize_tool(&fetcher, &py.entry(), &mut report, None)
            .unwrap();
        assert_eq!(meta.tree_hash, reference, "{at}");
        assert_eq!(
            report.incomplete_replaced.len(),
            usize::from(at == "renamed"),
            "{at}"
        );
        let expect_requests = usize::from(at == "download");
        assert_eq!(fetcher.requests().len(), expect_requests, "{at}");
    }
}

#[test]
fn concurrent_realizations_of_one_key_download_once_and_publish_once() {
    let (home, store) = open_store("store-concurrent");
    let py = Tool::python("3.12.14");
    let fetcher = Arc::new(MapFetcher::with(&[&py]));
    let threads = 8;
    let barrier = Arc::new(Barrier::new(threads));
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let (store, fetcher, barrier, entry) = (
                store.clone(),
                Arc::clone(&fetcher),
                Arc::clone(&barrier),
                py.entry(),
            );
            std::thread::spawn(move || {
                barrier.wait();
                let mut report = Report::default();
                let (name, meta) = store
                    .realize_tool(fetcher.as_ref(), &entry, &mut report, None)
                    .unwrap();
                (name, meta.tree_hash, report.created.len())
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(fetcher.count(&py.url), 1, "one download");
    assert_eq!(
        results.iter().map(|r| r.2).sum::<usize>(),
        1,
        "one publication"
    );
    assert!(
        results
            .iter()
            .all(|r| r.0 == results[0].0 && r.1 == results[0].1)
    );
    assert!(listing(&home.join("store/tmp")).is_empty());
}

// ---------------------------------------------------------------------------------------------
// Hostile and malformed archives

fn extract_into(
    name: &str,
    bytes: &[u8],
    format: &str,
    strip: u32,
) -> (PathBuf, Result<archive::Stats, lodi::diag::Diagnostic>) {
    let root = scratch(name);
    let file = root.join("archive");
    fs::write(&file, bytes).unwrap();
    let dest = root.join("out");
    let result = archive::extract(&file, format, strip, "", &dest, &Limits::default());
    (root, result)
}

#[test]
fn hostile_archives_are_refused_and_write_nothing_outside() {
    let cases: Vec<(&str, Vec<Member>)> = vec![
        ("parent traversal", vec![file("../evil", "x", 0o644)]),
        ("nested traversal", vec![file("a/../../evil", "x", 0o644)]),
        (
            "absolute path",
            vec![file("/tmp/lodi-evil-absolute", "x", 0o644)],
        ),
        ("absolute symlink", vec![symlink("l", "/etc")]),
        (
            "escaping symlink",
            vec![dir("d/"), symlink("d/l", "../../x")],
        ),
        (
            "write through a symlink",
            vec![dir("sub/"), symlink("l", "sub"), file("l/x", "x", 0o644)],
        ),
        (
            "write through an escaping symlink",
            vec![symlink("l", ".."), file("l/evil", "x", 0o644)],
        ),
        (
            "physical escape through a link to .",
            vec![symlink("here", "."), symlink("up", "here/..")],
        ),
        (
            "physical escape defined later",
            vec![symlink("up", "z/.."), symlink("z", ".")],
        ),
        ("hard link outside", vec![hardlink("h", "../x")]),
        ("hard link to nothing", vec![hardlink("h", "missing")]),
        (
            "hard link through a symlink",
            vec![
                dir("s/"),
                file("s/f", "x", 0o644),
                symlink("l", "s"),
                hardlink("h", "l/f"),
            ],
        ),
        (
            "duplicate file",
            vec![file("a", "1", 0o644), file("a", "2", 0o644)],
        ),
        (
            "file replaces a symlink",
            vec![symlink("a", "b"), file("a", "2", 0o644)],
        ),
        (
            "directory replaces a file",
            vec![file("a", "1", 0o644), dir("a/")],
        ),
        (
            "fifo",
            vec![Member {
                path: b"p".to_vec(),
                kind: Kind::Fifo,
            }],
        ),
        (
            "device",
            vec![Member {
                path: b"dev".to_vec(),
                kind: Kind::CharDevice,
            }],
        ),
    ];
    for (what, members) in cases {
        let (root, result) = extract_into("hostile", &gzip(&tar(&members)), "tar.gz", 0);
        let err = result.expect_err(what);
        assert_eq!(err.code, "E_ARCHIVE_UNSAFE", "{what}: {err}");
        assert_eq!(lodi::diag::exit_status(err.code), 6);
        // Nothing outside the destination: the scratch root holds only the archive and out/.
        let outside: Vec<String> = listing(&root)
            .into_iter()
            .filter(|p| p != "archive" && p != "out" && !p.starts_with("out/"))
            .collect();
        assert!(outside.is_empty(), "{what}: {outside:?}");
        assert!(!Path::new("/tmp/lodi-evil-absolute").exists());
    }
}

#[test]
fn archives_are_bounded_and_malformed_ones_are_format_errors() {
    let root = scratch("bounds");
    let many: Vec<Member> = (0..20)
        .map(|i| file(&format!("f{i}"), "x", 0o644))
        .collect();
    let small = Limits {
        max_entries: 10,
        max_bytes: 1000,
        max_path_bytes: 64,
    };
    let path = root.join("a.tar.gz");
    fs::write(&path, gzip(&tar(&many))).unwrap();
    let err = archive::extract(&path, "tar.gz", 0, "", &root.join("o1"), &small).unwrap_err();
    assert_eq!(err.code, "E_ARCHIVE_UNSAFE");
    assert!(err.message.contains("more than 10 entries"));

    let big = vec![file("big", &"x".repeat(5000), 0o644)];
    fs::write(&path, gzip(&tar(&big))).unwrap();
    let err = archive::extract(&path, "tar.gz", 0, "", &root.join("o2"), &small).unwrap_err();
    assert_eq!(err.code, "E_ARCHIVE_UNSAFE", "{err}");

    // A compression bomb: 64 MiB of zeros gzip to about 64 KiB; the stream bound stops it
    // (streamed for gzip; for xz while the stream is decompressed into its temporary file).
    let zeros = tar(&[file("zeros", &"\0".repeat(64 << 20), 0o644)]);
    let packed = gzip(&zeros);
    assert!(packed.len() < 1 << 20);
    let bgz = root.join("bomb.tar.gz");
    fs::write(&bgz, &packed).unwrap();
    let err = archive::extract(&bgz, "tar.gz", 0, "", &root.join("o3"), &small).unwrap_err();
    assert!(
        err.code == "E_ARCHIVE_FORMAT" || err.code == "E_ARCHIVE_UNSAFE",
        "{err}"
    );
    let bxz = root.join("bomb.tar.xz");
    fs::write(
        &bxz,
        xz(&tar(&[file("zeros", &"\0".repeat(1 << 20), 0o644)])),
    )
    .unwrap();
    let err = archive::extract(&bxz, "tar.xz", 0, "", &root.join("o3x"), &small).unwrap_err();
    assert!(err.message.contains("size limit"), "{err}");
    assert!(
        !root.join("o3x.tar").exists(),
        "the temporary tar stream was removed"
    );

    let long = vec![file(&format!("{}/f", "d".repeat(80)), "x", 0o644)];
    fs::write(&path, gzip(&tar(&long))).unwrap();
    let err = archive::extract(&path, "tar.gz", 0, "", &root.join("o4"), &small).unwrap_err();
    assert!(err.message.contains("longer than 64 bytes"), "{err}");

    // Nothing left after strip_components: empty.
    fs::write(&path, gzip(&tar(&[file("top", "x", 0o644)]))).unwrap();
    let err =
        archive::extract(&path, "tar.gz", 1, "", &root.join("o5"), &Limits::default()).unwrap_err();
    assert_eq!(err.code, "E_ARCHIVE_EMPTY");

    for (bytes, format) in [
        (b"not gzip at all".to_vec(), "tar.gz"),
        (b"not xz at all".to_vec(), "tar.xz"),
        (
            gzip(b"not a tar stream, just some text that is long enough"),
            "tar.gz",
        ),
    ] {
        fs::write(&path, bytes).unwrap();
        let dest = root.join(format!("o-{}", sha256_hex(format.as_bytes())));
        let _ = fs::remove_dir_all(&dest);
        let err = archive::extract(&path, format, 0, "", &dest, &Limits::default()).unwrap_err();
        assert!(
            err.code == "E_ARCHIVE_FORMAT" || err.code == "E_ARCHIVE_EMPTY",
            "{format}: {err}"
        );
    }
    let err =
        archive::extract(&path, "zip", 0, "", &root.join("o6"), &Limits::default()).unwrap_err();
    assert_eq!(err.code, "E_ARCHIVE_FORMAT");
}

#[test]
fn safe_archives_keep_in_tree_links_and_lose_special_mode_bits() {
    let members = vec![
        dir("top/"),
        file("top/bin/tool", "#!/bin/sh\n", 0o4775),
        file("top/share/data", "d", 0o666),
        symlink("top/bin/alias", "tool"),
        symlink("top/lib", "share"),
        symlink("top/share/dangling", "missing/inside"),
        hardlink("top/bin/hard", "top/bin/tool"),
        file(
            &format!("top/{}/long-name-file", "n".repeat(120)),
            "long",
            0o644,
        ),
        dir("top/sub/extra/"),
        dir("top/sub/extra/"),
    ];
    let (root, result) = extract_into("safe", &xz(&tar(&members)), "tar.xz", 1);
    let stats = result.unwrap();
    assert_eq!((stats.symlinks, stats.hardlinks), (3, 1));
    let out = root.join("out");
    let tool = fs::metadata(out.join("bin/tool")).unwrap();
    assert_eq!(
        tool.permissions().mode() & 0o7777,
        0o755,
        "setuid and group write dropped"
    );
    assert_eq!(
        fs::metadata(out.join("share/data"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o644
    );
    assert_eq!(
        fs::read_link(out.join("bin/alias")).unwrap(),
        Path::new("tool")
    );
    assert_eq!(
        fs::read_to_string(out.join(format!("{}/long-name-file", "n".repeat(120)))).unwrap(),
        "long"
    );
    assert!(!root.join("out.tar").exists());

    // subdir selects a directory inside the archive after stripping.
    let file_path = root.join("sub.tar.gz");
    fs::write(
        &file_path,
        gzip(&tar(&[
            file("top/keep/bin/x", "x", 0o755),
            file("top/drop/y", "y", 0o644),
        ])),
    )
    .unwrap();
    archive::extract(
        &file_path,
        "tar.gz",
        1,
        "keep",
        &root.join("sub"),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(listing(&root.join("sub")), ["bin", "bin/x"]);
}

#[test]
fn a_missing_bin_is_refused_before_publication() {
    let (home, store) = open_store("store-bin-missing");
    let mut py = Tool::python("3.12.14");
    py.bin.push("bin/python3-config".into());
    let fetcher = MapFetcher::with(&[&py]);
    let err = store
        .realize_tool(&fetcher, &py.entry(), &mut Report::default(), None)
        .unwrap_err();
    assert_eq!(err.code, "E_BIN_MISSING");
    assert_nothing_published(&home);
}

#[test]
fn a_hostile_artifact_is_never_published() {
    let (home, store) = open_store("store-hostile");
    let mut evil = Tool::python("3.12.14");
    evil.bytes = gzip(&tar(&[dir("python/"), symlink("python/bin", "/usr/bin")]));
    let fetcher = MapFetcher::with(&[&evil]);
    let err = store
        .realize_tool(&fetcher, &evil.entry(), &mut Report::default(), None)
        .unwrap_err();
    assert_eq!(err.code, "E_ARCHIVE_UNSAFE");
    assert_nothing_published(&home);
    // The verified download stays cached (its bytes match the lock); it is the content that is
    // refused, and it is refused again on every attempt.
    assert!(store.download_path(&evil.sha256_hex()).is_file());
}

const UMASK_TEST: &str = "a_staging_directory_is_private_before_anything_is_written_into_it";

/// Child mode of the next test, run under `umask 000`: open a fresh store and realize one tool,
/// asserting from inside the build that the staging directory is already there at 0700 and that
/// `store/tmp` is 0700 too, before a byte is written below them.
fn umask_child(home: &Path) -> ! {
    let store = Store::open(home).unwrap();
    let mode = |p: &Path| fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&home.join("store/tmp")), 0o700, "store/tmp");
    let seen = Mutex::new(None);
    store
        .publish(
            store::Entry {
                name: "env-umask".into(),
                kind: "env",
                identity: "umask".into(),
                references: Vec::new(),
            },
            &mut Report::default(),
            &|_, _| Ok(()),
            |staging, _, _| {
                *seen.lock().unwrap() = fs::symlink_metadata(staging).ok().map(|_| mode(staging));
                fs::write(staging.join("env.json"), b"{}\n").unwrap();
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        Some(0o700),
        "the staging directory was not there at 0700 when the build began"
    );
    std::process::exit(0);
}

/// A store staging directory, and `store/tmp` that holds it, are created at an explicit 0700 —
/// not whatever the umask allows — before anything is written into them (LD-361).
#[test]
fn a_staging_directory_is_private_before_anything_is_written_into_it() {
    if let Some(home) = std::env::var_os("LODI_TEST_UMASK_HOME") {
        umask_child(Path::new(&home));
    }
    let home = scratch("store-umask");
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg("umask 000 && exec \"$0\" \"$@\"")
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", UMASK_TEST, "--nocapture", "--test-threads=1"])
        .env("LODI_TEST_UMASK_HOME", &home)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the child under umask 000 failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
