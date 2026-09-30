//! The repository's one `lodi.lock` (M-Flake fv-1, LD-416; DONE D4; M150 V6, V7): canonical
//! bytes, the refusals between it and a project's `lodi.lock`, and the migration of 1.4's
//! per-folder `pins.lock` and `home.lock` into it by every writer.
//!
//! The 1.4 locks are the ones `tests/fixtures/host/compat-1.4/locks/` holds, recorded from the
//! v1.4.1 release commit by the procedure of `tests/fixtures/host/compat-1.4/README.md`
//! (`record_1_4_folder_locks` below, which does nothing unless recording). Every host command
//! carries a scratch `--root`, with the fake machine behind it and the recorded dated archives on
//! loopback; nothing reaches the network or `/`.

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
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use fakehost::{Machine, err, out, story};
use pinverbs::*;
use serde_json::Value;

/// The child half of every held apply of this binary (`tests/support/hostroot.rs`).
#[test]
fn child_apply() {
    if !hostroot::is_child() {
        return;
    }
    hostroot::child_apply();
}

const TOOL_URL: &str = "https://fixtures.test/hello";
const TOOL_BODY: &[u8] = b"#!/bin/sh\necho hello\n";

fn fixtures() -> PathBuf {
    repo().join("tests/fixtures/host/compat-1.4/locks")
}

fn fixture(name: &str) -> Vec<u8> {
    fs::read(fixtures().join(name)).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

fn json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("JSON")
}

/// The binary a run uses: this build's, or while recording the one `LODI_COMPAT_BINARY` names.
fn binary() -> std::ffi::OsString {
    std::env::var_os("LODI_COMPAT_BINARY")
        .unwrap_or_else(|| std::ffi::OsString::from(env!("CARGO_BIN_EXE_lodi")))
}

fn recording() -> bool {
    std::env::var_os("LODI_RECORD_EXPECTED").is_some()
        && std::env::var_os("LODI_COMPAT_BINARY").is_some()
}

/// A home manifest with one inline tool, served by the case's loopback archive.
fn home_toml() -> String {
    format!(
        "[home]\nversion = \"1\"\n\n[tools.hello]\nversion = \"1.0.0\"\nurl = \"{TOOL_URL}\"\n\
         sha256 = \"{}\"\nformat = \"binary\"\npath = [\"bin\"]\n",
        lodi::util::sha256_hex(TOOL_BODY)
    )
}

/// A top-level or scoped `lodi` command of `v`'s case, the binary chosen by [`binary`]: `args`
/// then `--root <the case's root>`, with the loopback archive and `LODI_HOST_REQUIRE_ROOT=1`.
// check-host-safety: refusal — one place, and it appends this scratch root as --root.
fn lodi(v: &Verbs, args: &[&str]) -> Output {
    let rewrite = v.server.rewrite();
    let _spawning = fakehost::spawning();
    Command::new(binary())
        .args(args)
        .arg("--root")
        .arg(&v.case.root.dir)
        .current_dir(v.case.base())
        .env("PATH", v.case.fake_path())
        .env("HOME", v.case.root.path("home"))
        .env("XDG_CONFIG_HOME", v.case.root.path("home/config"))
        .env("XDG_DATA_HOME", v.case.root.path("home/data"))
        .env("LODI_HOME", v.case.root.path("lodi-home"))
        .env("LODI_HOST_REQUIRE_ROOT", "1")
        .env("LODI_FETCH_REWRITE", rewrite)
        .env_remove("LODI_REPO")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("lodi runs")
}

/// A machine `box` with `tzdata` newer than the pin, the tool served, and the directory of
/// hosts holding 1.4's recorded locks: `box/pins.lock`, and the home lock of `sample` beside
/// the home manifest it was resolved for.
fn with_1_4_locks(name: &str) -> Verbs {
    let v = Verbs::debian(name, &[("bc", BC), ("tzdata", TZ_RECENT)]);
    v.server
        .files
        .lock()
        .unwrap()
        .insert(TOOL_URL.to_string(), TOOL_BODY.to_vec());
    v.set_host(&String::from_utf8(fixture("host.toml")).unwrap());
    let home = v.dir().join("home/sample");
    fs::create_dir_all(&home).unwrap();
    fs::write(v.dir().join("pins.lock"), fixture("pins.lock")).unwrap();
    fs::write(home.join("home.toml"), fixture("home.toml")).unwrap();
    fs::write(home.join("home.lock"), fixture("home.lock")).unwrap();
    fs::create_dir_all(v.case.root.path("people/sample")).unwrap();
    for dir in [v.dir().join("home"), home] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
    }
    v
}

fn root_lock(v: &Verbs) -> Option<Value> {
    v.root_lock().map(|bytes| json(&bytes))
}

/// A 1.4 lock's body as its section of the root lock holds it: without version and format (and a
/// home lock's always-null base, its `packages` under `tools`).
fn as_section(old: &[u8], home: bool) -> Value {
    let mut body = json(old);
    let object = body.as_object_mut().unwrap();
    object.remove("version");
    object.remove("format");
    if home {
        object.remove("base");
        let tools = object.remove("packages").unwrap();
        object.insert("tools".into(), tools);
    }
    body
}

/// Recording (never a validation command): with `LODI_RECORD_EXPECTED=1` and
/// `LODI_COMPAT_BINARY` naming a build of the v1.4.1 release commit, 1.4.1's own `host pin`
/// writes `pins.lock` and its own `home apply` writes `home.lock`, and both are kept as fixtures
/// with the manifests they lock.
#[test]
fn record_1_4_folder_locks() {
    if !recording() {
        return;
    }
    let v = Verbs::debian("record-locks", &[("bc", BC), ("tzdata", TZ_RECENT)]);
    v.set_host(&host("debian", None, &["bc", "tzdata"], &[]));
    let hosts = v.hosts();
    // check-host-safety: refusal — `lodi` appends this case's scratch root as --root.
    ok(&lodi(&v, &["host", "pin", "--all", "--to", OLDER, &hosts]));
    ok(&lodi(
        &v,
        // check-host-safety: refusal — `lodi` appends this case's scratch root as --root.
        &["host", "pin", "tzdata", "--to", "2025-03-01", &hosts],
    ));
    let out_dir = fixtures();
    fs::create_dir_all(&out_dir).unwrap();
    fs::write(out_dir.join("host.toml"), v.host_toml()).unwrap();
    fs::copy(v.dir().join("pins.lock"), out_dir.join("pins.lock")).unwrap();

    v.server
        .files
        .lock()
        .unwrap()
        .insert(TOOL_URL.to_string(), TOOL_BODY.to_vec());
    let config = v.case.root.path("home/config/lodi");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("home.toml"), home_toml()).unwrap();
    let rewrite = v.server.rewrite();
    let _spawning = fakehost::spawning();
    let applied = Command::new(binary())
        .args(["home", "apply"])
        .env("PATH", v.case.fake_path())
        .env("HOME", v.case.root.path("home"))
        .env("XDG_CONFIG_HOME", v.case.root.path("home/config"))
        .env("XDG_DATA_HOME", v.case.root.path("home/data"))
        .env("LODI_HOME", v.case.root.path("lodi-home"))
        .env("LODI_FETCH_REWRITE", rewrite)
        .output()
        .unwrap();
    ok(&applied);
    fs::write(out_dir.join("home.toml"), home_toml()).unwrap();
    fs::copy(config.join("home.lock"), out_dir.join("home.lock")).unwrap();
}

/// V7, the migration: a writer given a repository holding 1.4's `pins.lock` and `home.lock`
/// writes one root `lodi.lock` holding every entry of both, unchanged, removes the old files and
/// says so once per file; an apply from the migrated repository asks for exactly what they
/// pinned; and a second run of the writer changes no byte.
#[test]
fn migrates_1_4_folder_locks_into_the_root_lock() {
    let v = with_1_4_locks("root-lock-migrate");
    let pins = v.dir().join("pins.lock");
    let home_lock = v.dir().join("home/sample/home.lock");
    let pinned = json(&fixture("pins.lock"))["pins"]["tzdata"]["requested"]
        .as_str()
        .unwrap()
        .to_string();
    let written = v.run("pin", &["tzdata", "--to", &pinned, &v.hosts()]);
    ok(&written);
    let lock = root_lock(&v).expect("the root lock is written");
    assert_eq!(
        lock["hosts"]["box"],
        as_section(&fixture("pins.lock"), false)
    );
    assert_eq!(
        lock["homes"]["box/home/sample"],
        as_section(&fixture("home.lock"), true)
    );
    assert!(!pins.exists() && !home_lock.exists());
    for old in [&pins, &home_lock] {
        let lines = out(&written)
            .lines()
            .filter(|line| line.contains(&old.display().to_string()))
            .count();
        assert_eq!(lines, 1, "{}: {}", old.display(), story(&written));
    }

    let bytes = v.root_lock().unwrap();
    let applied = lodi(&v, &["apply", &v.hosts()]);
    ok(&applied);
    let version = json(&fixture("pins.lock"))["pins"]["tzdata"]["version"].clone();
    assert_eq!(
        v.case.machine()["installed"]["tzdata"]["version"],
        version,
        "{}",
        story(&applied)
    );
    assert_eq!(
        v.root_lock().unwrap(),
        bytes,
        "the apply re-resolved: {}",
        story(&applied)
    );

    let text = v.host_toml();
    ok(&v.run("pin", &["tzdata", "--to", &pinned, &v.hosts()]));
    assert_eq!((v.root_lock().unwrap(), v.host_toml()), (bytes, text));
}

/// V7: a root lock with a section for a directory that still holds a 1.4 lock locking something
/// else is `E_LOCK_STALE` naming both files, for the readers and the writers alike.
#[test]
fn a_root_lock_overlapping_an_old_lock_is_refused_naming_both() {
    let v = with_1_4_locks("root-lock-overlap");
    let pinned = json(&fixture("pins.lock"))["pins"]["tzdata"]["requested"]
        .as_str()
        .unwrap()
        .to_string();
    ok(&v.run("pin", &["tzdata", "--to", &pinned, &v.hosts()]));
    fs::write(v.dir().join("pins.lock"), differing_pins()).unwrap();
    let root = v.case.root.path("hosts/lodi.lock").display().to_string();
    let old = v.dir().join("pins.lock").display().to_string();
    for output in [
        v.run("plan", &[&v.hosts(), "--no-home"]),
        v.run("pin", &["tzdata", "--to", &pinned, &v.hosts()]),
    ] {
        refused(&output, "E_LOCK_STALE");
        assert!(
            err(&output).contains(&root) && err(&output).contains(&old),
            "{}",
            story(&output)
        );
    }
}

/// 1.4's recorded `pins.lock` with its one pin's version changed: an old lock that locks
/// something else than the root lock's section does.
fn differing_pins() -> Vec<u8> {
    let mut pins = json(&fixture("pins.lock"));
    pins["pins"]["tzdata"]["version"] = "0.1-1".into();
    serde_json::to_vec_pretty(&pins).unwrap()
}

/// #296: a move into the root lock cut short leaves old locks equal to their sections: a plan
/// reads them and writes nothing, an apply goes on and removes them with the root lock's bytes
/// unchanged, and an old lock that differs from its section still stops naming both.
#[test]
fn an_old_lock_equal_to_its_section_finishes_the_move() {
    let v = with_1_4_locks("root-lock-cut-short");
    let pinned = json(&fixture("pins.lock"))["pins"]["tzdata"]["requested"]
        .as_str()
        .unwrap()
        .to_string();
    ok(&v.run("pin", &["tzdata", "--to", &pinned, &v.hosts()]));
    let bytes = v.root_lock().expect("the root lock is written");
    let pins = v.dir().join("pins.lock");
    let home_lock = v.dir().join("home/sample/home.lock");
    fs::write(&pins, fixture("pins.lock")).unwrap();
    fs::write(&home_lock, fixture("home.lock")).unwrap();

    let before = census(&v.case.root.path("hosts"));
    let planned = lodi(&v, &["plan", &v.hosts()]);
    ok(&planned);
    assert_eq!(census(&v.case.root.path("hosts")), before);
    let applied = lodi(&v, &["apply", &v.hosts()]);
    ok(&applied);
    assert!(!pins.exists() && !home_lock.exists(), "{}", story(&applied));
    assert_eq!(v.root_lock().unwrap(), bytes);

    fs::write(&pins, differing_pins()).unwrap();
    let root = v.case.root.path("hosts/lodi.lock").display().to_string();
    let stopped = lodi(&v, &["apply", &v.hosts()]);
    refused(&stopped, "E_LOCK_STALE");
    let old = pins.display().to_string();
    assert!(
        err(&stopped).contains(&root) && err(&stopped).contains(&old),
        "{}",
        story(&stopped)
    );
    assert!(pins.exists());
}

/// V7: with no root lock the 1.4 locks are read unchanged, and an apply writes no repository
/// lock at all: only a writer moves them.
#[test]
fn an_apply_reads_the_old_locks_and_writes_no_repository_lock() {
    let v = with_1_4_locks("root-lock-apply");
    let before = census(&v.case.root.path("hosts"));
    let applied = lodi(&v, &["apply", &v.hosts()]);
    ok(&applied);
    let version = json(&fixture("pins.lock"))["pins"]["tzdata"]["version"].clone();
    assert_eq!(v.case.machine()["installed"]["tzdata"]["version"], version);
    assert_eq!(
        census(&v.case.root.path("hosts")),
        before,
        "{}",
        story(&applied)
    );
}

/// V7: a home's own tool writer, run as the user with no root, moves every old lock in and
/// then changes its own section of the root lock and nothing else.
#[test]
fn a_home_user_edits_only_its_own_section() {
    let v = with_1_4_locks("root-lock-home");
    let login = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_string();
    let mine = v.dir().join("home").join(&login);
    fs::create_dir_all(&mine).unwrap();
    fs::set_permissions(&mine, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(mine.join("home.toml"), home_toml()).unwrap();
    let person = v.case.base().join("person");
    fs::create_dir_all(&person).unwrap();
    let rewrite = v.server.rewrite();
    let applied = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["home", "apply"])
        .arg(v.hosts())
        .args(["--host", "box"])
        .env("HOME", &person)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("LODI_HOME")
        .env("LODI_FETCH_REWRITE", rewrite)
        .output()
        .unwrap();
    ok(&applied);
    let lock = root_lock(&v).expect("the root lock is written");
    assert_eq!(
        lock["hosts"]["box"],
        as_section(&fixture("pins.lock"), false)
    );
    assert_eq!(
        lock["homes"]["box/home/sample"],
        as_section(&fixture("home.lock"), true)
    );
    let own = &lock["homes"][format!("box/home/{login}")];
    assert!(own["tools"]["hello"].is_object(), "{lock}");
    assert_eq!(lock["homes"].as_object().unwrap().len(), 2, "{lock}");
    assert!(!mine.join("home.lock").exists());
    support::make_writable(v.case.base());
}

/// V7 on Fedora (ff-1): `lodi apply` of a Fedora host folder locks its home's tools in the
/// root lock, which has no host section: a Fedora package is not pinned by this build yet.
#[test]
fn a_fedora_host_s_home_is_locked_in_the_root_lock() {
    let v = Verbs::on("root-lock-fedora", Machine::fedora(), false);
    v.server
        .files
        .lock()
        .unwrap()
        .insert(TOOL_URL.to_string(), TOOL_BODY.to_vec());
    v.set_host("[host]\ndistro = \"fedora\"\n");
    let home = v.dir().join("home/sample");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("home.toml"), home_toml()).unwrap();
    fs::create_dir_all(v.case.root.path("people/sample")).unwrap();
    for dir in [v.dir().join("home"), home] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
    }
    ok(&lodi(&v, &["apply", &v.hosts()]));
    let lock = root_lock(&v).expect("the root lock is written");
    assert!(lock.get("hosts").is_none(), "{lock}");
    assert!(
        lock["homes"]["box/home/sample"]["tools"]["hello"].is_object(),
        "{lock}"
    );
    let bytes = v.root_lock().unwrap();
    ok(&lodi(&v, &["apply", &v.hosts()]));
    assert_eq!(v.root_lock().unwrap(), bytes);
}

/// V6: the root lock is canonical JSON of its own format, and equal inputs give equal bytes on
/// two machines: nothing of the machine, the path or the user who wrote it.
#[test]
fn equal_inputs_give_identical_canonical_bytes() {
    let one = with_1_4_locks("root-lock-bytes-one");
    let two = with_1_4_locks("root-lock-bytes-two");
    let pinned = json(&fixture("pins.lock"))["pins"]["tzdata"]["requested"]
        .as_str()
        .unwrap()
        .to_string();
    for v in [&one, &two] {
        ok(&v.run("pin", &["tzdata", "--to", &pinned, &v.hosts()]));
    }
    let (a, b) = (one.root_lock().unwrap(), two.root_lock().unwrap());
    assert_eq!(a, b);
    let parsed = json(&a);
    let mut canonical = serde_json::to_string_pretty(&parsed).unwrap();
    canonical.push('\n');
    assert_eq!(String::from_utf8(a.clone()).unwrap(), canonical);
    assert_eq!(parsed["format"], "lodi-repository-lock/1");
    assert_eq!(parsed["version"], 1);
    let text = String::from_utf8(a).unwrap();
    for machine in [one.root(), two.root()] {
        assert!(
            !text.contains(&machine),
            "a path of the machine is in the lock"
        );
    }
    let (uid, _) = hostroot::ids(&one.case.root);
    assert!(!text.contains(&format!("\"{uid}\"")));
}

/// V6: the root lock and a project's `lodi.lock` refuse each other by name; a root lock with an
/// unknown field is refused; and a directory with a project's `lodi.toml` is no repository.
#[test]
fn the_root_lock_and_the_project_lock_refuse_each_other_by_name() {
    let v = with_1_4_locks("root-lock-formats");
    let pinned = json(&fixture("pins.lock"))["pins"]["tzdata"]["requested"]
        .as_str()
        .unwrap()
        .to_string();
    ok(&v.run("pin", &["tzdata", "--to", &pinned, &v.hosts()]));
    let root = v.root_lock().unwrap();

    let project = v.case.base().join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("lodi.toml"), "[project]\nversion = \"1\"\n").unwrap();
    fs::write(project.join("lodi.lock"), &root).unwrap();
    let checked = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["lock", "--check"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert_ne!(checked.status.code(), Some(0));
    assert!(
        err(&checked).contains("lodi-repository-lock/1"),
        "{}",
        story(&checked)
    );

    let hosts = v.case.root.path("hosts/lodi.lock");
    let project_lock = serde_json::json!({"version": 1, "format": "lodi-spike-lock/1"});
    fs::write(&hosts, serde_json::to_string_pretty(&project_lock).unwrap()).unwrap();
    let refused_project = v.run("plan", &[&v.hosts(), "--no-home"]);
    refused(&refused_project, "E_LOCK_VERSION");
    assert!(
        err(&refused_project).contains("lodi-spike-lock/1"),
        "{}",
        story(&refused_project)
    );

    let mut unknown = json(&root);
    unknown["extra"] = serde_json::json!(true);
    fs::write(&hosts, serde_json::to_string_pretty(&unknown).unwrap()).unwrap();
    refused(&v.run("plan", &[&v.hosts(), "--no-home"]), "E_LOCK_VERSION");

    fs::write(&hosts, &root).unwrap();
    fs::write(
        v.case.root.path("hosts/lodi.toml"),
        "[project]\nversion = \"1\"\n",
    )
    .unwrap();
    let _spawning = fakehost::spawning();
    let from_here = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["plan", "--yes", "--root"])
        .arg(&v.case.root.dir)
        .current_dir(v.case.root.path("hosts"))
        .env("PATH", v.case.fake_path())
        .env("LODI_HOST_REQUIRE_ROOT", "1")
        .env_remove("LODI_REPO")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    refused(&from_here, "E_NO_MANIFEST");
}

/// V6, Q3: `lodi update` fetches a `signed_by_url` key, verifies it against the declared digest
/// and locks it; a declaration that moves on afterwards is a stale lock.
#[test]
fn update_locks_a_key_by_url_and_a_changed_declaration_is_stale() {
    let v = Verbs::debian("root-lock-keys", &[("bc", BC)]);
    let key = {
        let mut out = vec![0xc6, 29];
        out.extend_from_slice(b"\x04invented public key material");
        out.extend_from_slice(&[0xcd, 17]);
        out.extend_from_slice(b"invented test key");
        out
    };
    let sha = lodi::util::sha256_hex(&key);
    v.server
        .files
        .lock()
        .unwrap()
        .insert("https://keys.example.invalid/vendor.gpg".into(), key);
    let source = |sha: &str| {
        format!(
            "\n[sources.vendor]\nuris = [\"https://packages.example.invalid/debian\"]\n\
             suites = [\"bookworm\"]\ncomponents = [\"main\"]\n\
             signed_by_url = \"https://keys.example.invalid/vendor.gpg\"\n\
             signed_by_sha256 = \"{sha}\"\n"
        )
    };
    let text = host("debian", None, &["bc"], &[]);
    v.set_host(&format!("{text}{}", source(&sha)));
    let updated = lodi(&v, &["update", &v.hosts()]);
    ok(&updated);
    let lock = root_lock(&v).expect("the key is locked");
    assert_eq!(
        lock["hosts"]["box"]["keys"]["vendor"],
        format!("sha256:{sha}")
    );
    v.set_host(&format!("{text}{}", source(&"0".repeat(64))));
    refused(&v.run("plan", &[&v.hosts(), "--no-home"]), "E_LOCK_STALE");
}

/// The fixtures are 1.4.1's own bytes: its format identifiers, and no writer this build names.
#[test]
fn the_1_4_fixtures_are_1_4_1_s_formats() {
    let pins = json(&fixture("pins.lock"));
    assert_eq!(pins["format"], "lodi-host-pins/1");
    let home = json(&fixture("home.lock"));
    assert_eq!(home["format"], "lodi-spike-lock/1");
    assert_eq!(home["generatedBy"], "lodi 1.4.1");
}
