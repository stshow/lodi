//! The 2.0 host plumbing (#705, LD-503): the elevator helper, the "may manage" marker and the
//! config's one `lodi.lock`. Every input is passed in: a scratch root, a scratch `PATH` holding
//! stub `sudo`, `doas` and `run0` that record how they were called, a scratch passwd and a scratch
//! config root. Nothing reads the ambient `HOME`, XDG, the real passwd or `/`.

mod support;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{RwLock, RwLockReadGuard};

use lodi::config::lock::{self as configlock, HomeSection, HostSection, Lock, Lookup, Mode};
use lodi::config::{Identity, User};
use lodi::diag::Diagnostic;
use lodi::elevate::{self, Elevation, Request};
use lodi::marker;
use lodi::progress::{Event, Sink};

/// The test's own ids: under a scratch root the invoking user stands for root.
fn ids() -> (u32, u32) {
    // SAFETY: neither call has a precondition.
    unsafe { (libc::geteuid(), libc::getegid()) }
}

/// A scratch root, mode 0755, holding nothing yet.
fn root(name: &str) -> PathBuf {
    let root = support::scratch(name);
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    root
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

// ---------------------------------------------------------------------------- the marker ---

#[test]
fn the_marker_is_written_empty_at_0644_owned_by_the_root_owner() {
    let root = root("marker-write");
    assert!(!marker::may_manage(&root, ids().0));
    assert!(marker::write(&root, ids().0).unwrap());
    let path = root.join(marker::MARKER);
    let meta = fs::symlink_metadata(&path).unwrap();
    assert!(meta.is_file());
    assert_eq!(meta.len(), 0);
    assert_eq!(mode(&path), 0o644);
    assert_eq!(meta.uid(), ids().0);
    assert!(marker::may_manage(&root, ids().0));
}

#[test]
fn a_valid_marker_is_left_byte_identical() {
    let root = root("marker-again");
    marker::write(&root, ids().0).unwrap();
    let path = root.join(marker::MARKER);
    let before = fs::symlink_metadata(&path).unwrap();
    assert!(!marker::write(&root, ids().0).unwrap());
    let after = fs::symlink_metadata(&path).unwrap();
    assert_eq!(
        (
            before.ino(),
            before.mtime_nsec(),
            before.mode(),
            before.len()
        ),
        (after.ino(), after.mtime_nsec(), after.mode(), after.len())
    );
}

#[test]
fn a_symlink_at_the_marker_is_never_followed() {
    let root = root("marker-link");
    fs::create_dir_all(root.join("etc/lodi")).unwrap();
    let target = root.join("victim");
    symlink(&target, root.join(marker::MARKER)).unwrap();
    let refused = marker::write(&root, ids().0).unwrap_err();
    assert_eq!(refused.code, "E_PATH_ESCAPE");
    assert!(!target.exists());
    assert!(!marker::may_manage(&root, ids().0));
}

#[test]
fn a_symlinked_folder_on_the_way_is_never_followed() {
    let root = root("marker-dir-link");
    let elsewhere = root.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::create_dir_all(root.join("etc")).unwrap();
    symlink(&elsewhere, root.join("etc/lodi")).unwrap();
    assert_eq!(
        marker::write(&root, ids().0).unwrap_err().code,
        "E_PATH_ESCAPE"
    );
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}

#[test]
fn the_1x_arm_marker_does_not_count() {
    let root = root("marker-1x");
    fs::create_dir_all(root.join("etc/lodi")).unwrap();
    let old = root.join("etc/lodi/host-allowed");
    fs::write(&old, "").unwrap();
    fs::set_permissions(&old, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!marker::may_manage(&root, ids().0));
}

#[test]
fn a_group_or_other_writable_marker_reads_as_not_yet() {
    for bits in [0o664, 0o646] {
        let root = root("marker-writable");
        marker::write(&root, ids().0).unwrap();
        let path = root.join(marker::MARKER);
        fs::set_permissions(&path, fs::Permissions::from_mode(bits)).unwrap();
        assert!(!marker::may_manage(&root, ids().0), "{bits:o}");
        assert_eq!(marker::write(&root, ids().0).unwrap_err().code, "E_EXISTS");
        assert_eq!(mode(&path), bits, "left as found");
    }
}

#[test]
fn a_marker_owned_by_someone_else_or_not_a_file_reads_as_not_yet() {
    let root = root("marker-other");
    fs::create_dir_all(root.join(marker::MARKER)).unwrap();
    assert!(!marker::may_manage(&root, ids().0));
    let root = root_with_marker("marker-owner");
    assert!(!marker::may_manage(&root, ids().0 + 1));
}

fn root_with_marker(name: &str) -> PathBuf {
    let root = root(name);
    marker::write(&root, ids().0).unwrap();
    root
}

// -------------------------------------------------------------------------- the elevator ---

/// Held for writing while a stub is written and for reading around every spawn, so that no child
/// inherits a stub's open write descriptor ("Text file busy", as `tests/support/fakehost.rs`).
static STUBS: RwLock<()> = RwLock::new(());

fn spawning() -> RwLockReadGuard<'static, ()> {
    STUBS.read().unwrap_or_else(|e| e.into_inner())
}

/// A scratch `PATH` folder holding a stub for each of `names`. A stub records its own name and
/// its arguments in `<dir>/<name>.args`, then, as an elevator does, clears the environment and
/// runs the rest of its command line; a `refuse` stub exits 1 at once, as a refused password.
fn stubs(names: &[&str], refuse: bool) -> PathBuf {
    let dir = support::scratch("elevators");
    let _held = STUBS.write().unwrap_or_else(|e| e.into_inner());
    for name in names {
        let record = dir.join(format!("{name}.args"));
        let run = if refuse {
            "exit 1".to_string()
        } else {
            "[ \"$1\" = -- ] && shift\nexec env -i \"$@\"".to_string()
        };
        let body = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n{run}\n",
            record.display()
        );
        let stub = dir.join(name);
        fs::write(&stub, body).unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// The arguments the stub `name` in `dir` was called with, if it was.
fn called(dir: &Path, name: &str) -> Option<Vec<String>> {
    let text = fs::read_to_string(dir.join(format!("{name}.args"))).ok()?;
    Some(text.lines().map(str::to_string).collect())
}

/// The person lodi acts for: the test's own uid, from a scratch passwd under `root`.
fn person(root: &Path) -> User {
    let (uid, gid) = ids();
    fs::create_dir_all(root.join("etc")).unwrap();
    fs::write(
        root.join("etc/passwd"),
        format!("root:x:0:0::/root:/bin/sh\nalice:x:{uid}:{gid}::/home/alice:/bin/sh\n"),
    )
    .unwrap();
    User::of(&identity(root, uid, None)).unwrap()
}

fn identity(root: &Path, euid: u32, doas_user: Option<&str>) -> Identity {
    Identity {
        euid,
        sudo_uid: None,
        doas_user: doas_user.map(OsString::from),
        home: Some(root.join("home/alice")),
        state_home: None,
        config_home: None,
        data_home: None,
        root: root.to_path_buf(),
        hostname: "box".into(),
    }
}

fn lodi() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lodi"))
}

/// An elevation of `euid` for the test's own user, with `path` as `PATH` and `root` as `--root`.
fn elevation(root: &Path, path: &Path, euid: u32) -> Elevation {
    let user = person(root);
    let mut elevation = Elevation::new(&lodi(), Some(path.as_os_str()), &user, euid);
    elevation.root = Some(root.to_path_buf());
    elevation
}

/// Every event it was given, in order.
#[derive(Default)]
struct Recorded(Vec<Event>);

impl Sink for Recorded {
    fn event(&mut self, event: Event) {
        self.0.push(event);
    }
}

#[test]
fn the_elevator_is_the_first_of_sudo_doas_and_run0_found() {
    let all = stubs(&["run0", "doas", "sudo"], false);
    let found = |dir: &Path| elevate::choose(Some(dir.as_os_str()));
    assert_eq!(found(&all).unwrap(), all.join("sudo"));
    let doas = stubs(&["doas"], false);
    assert_eq!(found(&doas).unwrap(), doas.join("doas"));
    let run0 = stubs(&["run0"], false);
    assert_eq!(found(&run0).unwrap(), run0.join("run0"));
    let both = std::env::join_paths([run0.clone(), doas.clone()]).unwrap();
    assert_eq!(
        elevate::choose(Some(&both)).unwrap(),
        doas.join("doas"),
        "the order is by program, not by PATH entry"
    );
    let none = support::scratch("no-elevator");
    assert_eq!(found(&none).unwrap_err().code, "E_NEED_ROOT");
    assert_eq!(elevate::choose(None).unwrap_err().code, "E_NEED_ROOT");
}

#[test]
fn not_root_runs_lodi_again_through_the_elevator_with_everything_on_its_command_line() {
    let root = root("elevate-sudo");
    let path = stubs(&["sudo", "doas", "run0"], false);
    let mut elevation = elevation(&root, &path, ids().0);
    elevation.config = Some(root.join("home/alice/.config/lodi"));
    elevation.verbose = true;
    let mut sink = Recorded::default();
    let result = {
        let _spawn = spawning();
        elevation.run(&Request::WriteMarker, &mut sink).unwrap()
    };
    assert_eq!(result, serde_json::json!({"created": true}));
    assert!(marker::may_manage(&root, ids().0));
    assert!(called(&path, "doas").is_none() && called(&path, "run0").is_none());
    let args = called(&path, "sudo").expect("sudo was called");
    assert_eq!(args[0], "--");
    assert_eq!(Path::new(&args[1]), lodi());
    assert_eq!(args[2], elevate::ENTRY);
    let after = |flag: &str| {
        let at = args
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("{flag}"));
        args[at + 1].clone()
    };
    assert_eq!(Path::new(&after("--root")), root);
    assert_eq!(
        Path::new(&after("--config")),
        root.join("home/alice/.config/lodi")
    );
    assert_eq!(after("--user"), ids().0.to_string());
    assert_eq!(after("--request"), "write-marker");
    assert!(args.iter().any(|a| a == "--verbose"));
    // The child's progress reached this process over the relay.
    assert!(
        matches!(sink.0.first(), Some(Event::Begin { .. })),
        "{:?}",
        sink.0
    );
    assert!(matches!(sink.0.last(), Some(Event::Finish { .. })));
}

#[test]
fn a_refused_password_is_need_root_and_nothing_changes() {
    let root = root("elevate-refused");
    let path = stubs(&["sudo"], true);
    let mut sink = Recorded::default();
    let refused = {
        let _spawn = spawning();
        elevation(&root, &path, ids().0)
            .run(&Request::WriteMarker, &mut sink)
            .unwrap_err()
    };
    assert_eq!(refused.code, "E_NEED_ROOT");
    assert!(called(&path, "sudo").is_some());
    assert!(!root.join(marker::MARKER).exists());
    assert!(sink.0.is_empty());
}

#[test]
fn already_root_runs_the_request_in_place_with_no_elevator() {
    let root = root("elevate-root");
    let path = stubs(&["sudo", "doas", "run0"], false);
    let mut sink = Recorded::default();
    let result = elevation(&root, &path, 0)
        .run(&Request::WriteMarker, &mut sink)
        .unwrap();
    assert_eq!(result, serde_json::json!({"created": true}));
    for name in ["sudo", "doas", "run0"] {
        assert!(called(&path, name).is_none(), "{name} was called");
    }
    assert!(root.join(marker::MARKER).exists());
    assert!(matches!(sink.0.first(), Some(Event::Begin { .. })));
}

#[test]
fn no_elevator_at_all_stops_before_anything_runs() {
    let root = root("elevate-none");
    let none = support::scratch("no-elevator");
    let refused = elevation(&root, &none, ids().0)
        .run(&Request::WriteMarker, &mut Recorded::default())
        .unwrap_err();
    assert_eq!(refused.code, "E_NEED_ROOT");
    assert!(!root.join(marker::MARKER).exists());
}

/// `lodi ENTRY ARGS` run by hand, with `stdin` as its standard input.
fn by_hand(args: &[&OsStr], stdin: &str) -> std::process::Output {
    use std::io::Write;
    let _spawn = spawning();
    let mut child = Command::new(lodi())
        .arg(elevate::ENTRY)
        .args(args)
        .env_clear()
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
    child.wait_with_output().unwrap()
}

/// The entry refuses unless it may act as root: run by hand by a person, it does nothing. It
/// carries no token: whoever may run it as root is root already, so a token the parent writes on
/// both sides proves nothing (#705).
#[test]
fn the_hidden_entry_started_by_hand_is_refused_and_does_nothing() {
    let uid = ids().0.to_string();
    let root = root("entry-by-hand");
    let common = |root: Option<&Path>| {
        let mut args: Vec<OsString> = ["--request", "write-marker", "--user", &uid]
            .iter()
            .map(OsString::from)
            .collect();
        if let Some(root) = root {
            args.push("--root".into());
            args.push(root.into());
        }
        args
    };
    // Not root, and no scratch root standing for one.
    let args = common(None);
    let refs: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    let out = by_hand(&refs, "");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty());
    // An unknown flag is refused too.
    let mut args = common(Some(&root));
    args.push("--token".into());
    args.push("0123456789abcdef0123456789abcdef".into());
    let refs: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    let out = by_hand(&refs, "");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(!root.join("etc").exists(), "nothing was created");
    // A root it may act on: it acts.
    let args = common(Some(&root));
    let refs: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    let out = by_hand(&refs, "");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(marker::may_manage(&root, ids().0));
}

#[test]
fn the_hidden_entry_is_not_in_the_help() {
    let _spawn = spawning();
    for args in [vec!["--help"], vec!["help"], vec![]] {
        let out = Command::new(lodi())
            .args(&args)
            .env_clear()
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(!text.contains(elevate::ENTRY), "{args:?}");
    }
}

#[test]
fn under_doas_the_person_is_doas_user_as_under_sudo() {
    let root = root("elevate-doas");
    let path = stubs(&["sudo"], false);
    person(&root);
    let user = User::of(&identity(&root, 0, Some("alice"))).unwrap();
    assert!(user.sudo);
    let elevation = Elevation::new(&lodi(), Some(path.as_os_str()), &user, 0);
    assert_eq!(elevation.user, ids().0);
    let unknown = User::of(&identity(&root, 0, Some("mallory"))).unwrap_err();
    assert_eq!(unknown.code, "E_CONFIG");
}

// ------------------------------------------------------------------------------ the lock ---

/// A pin record for `name` at `version`, requested as `requested`.
fn pin(requested: &str, version: &str) -> serde_json::Value {
    serde_json::json!({
        "policy": "version", "requested": requested, "version": version,
        "sha256": format!("sha256:{}", "0".repeat(64)),
        "filename": format!("pool/{version}.deb"), "repository": "debian",
    })
}

/// A host section on `snapshot` (requested as given) with `pins` and one key.
fn host(snapshot: Option<(&str, &str)>, pins: &[(&str, &str, &str)], key: &str) -> HostSection {
    let pins: serde_json::Map<String, serde_json::Value> = pins
        .iter()
        .map(|(name, requested, version)| (name.to_string(), pin(requested, version)))
        .collect();
    let mut value = serde_json::json!({
        "distro": "debian", "release": "bookworm", "arch": "x86_64",
        "pins": pins, "keys": {"extra": key},
    });
    if let Some((requested, instant)) = snapshot {
        value["snapshot"] = serde_json::json!({
            "requested": requested, "instant": instant, "indexes": {},
        });
    }
    serde_json::from_value(value).unwrap()
}

fn home(tools: &[support::Tool]) -> HomeSection {
    HomeSection {
        tools: tools.iter().map(|t| (t.label.clone(), t.entry())).collect(),
        profiles: Default::default(),
    }
}

/// A lookup that answers from fixed sections and records what it was asked; `fail` makes every
/// lookup fail, as an unreachable archive.
#[derive(Default)]
struct Fixed {
    hosts: std::collections::BTreeMap<String, HostSection>,
    homes: std::collections::BTreeMap<String, HomeSection>,
    asked: Vec<(String, Mode)>,
    fail: bool,
}

impl Lookup for Fixed {
    fn host(
        &mut self,
        key: &str,
        _: Option<&HostSection>,
        mode: Mode,
    ) -> Result<HostSection, Diagnostic> {
        self.asked.push((key.to_string(), mode));
        if self.fail {
            return Err(Diagnostic::new("E_FETCH", "the archive is unreachable"));
        }
        Ok(self.hosts[key].clone())
    }

    fn home(
        &mut self,
        key: &str,
        _: Option<&HomeSection>,
        mode: Mode,
    ) -> Result<HomeSection, Diagnostic> {
        self.asked.push((key.to_string(), mode));
        Ok(self.homes[key].clone())
    }
}

/// A config root: a 0755 folder of the test's own.
fn config(name: &str) -> PathBuf {
    root(name)
}

/// The person, acting through sudo (`SUDO_UID` is the test's own uid) or not.
fn acting(root: &Path, sudo: bool) -> User {
    let user = person(root);
    if !sudo {
        return user;
    }
    let mut identity = identity(root, 0, None);
    identity.sudo_uid = Some(ids().0.to_string().into());
    let user = User::of(&identity).unwrap();
    assert!(user.sudo);
    user
}

#[test]
fn a_missing_lock_is_empty_and_one_file_at_the_root_holds_every_section() {
    let root = config("lock-one");
    assert_eq!(configlock::read(&root).unwrap(), Lock::default());
    let mut lookup = Fixed::default();
    let snap = Some(("2026-09-01", "2026-09-01T00:00:00Z"));
    lookup
        .hosts
        .insert("host.toml".into(), host(snap, &[], "sha256:a"));
    lookup
        .hosts
        .insert("server/host.toml".into(), host(snap, &[], "sha256:a"));
    lookup
        .homes
        .insert("home.toml".into(), home(&[support::Tool::python("3.12.1")]));
    // Two hosts share one home.toml: it is locked once, in one section.
    let filled = configlock::fill(
        &Lock::default(),
        &["host.toml", "server/host.toml"],
        &["home.toml", "home.toml"],
        &mut lookup,
    )
    .unwrap();
    assert_eq!(lookup.asked.len(), 3, "{:?}", lookup.asked);
    let held = configlock::hold(&root).unwrap();
    assert!(held.write(&filled, &acting(&root, false)).unwrap());
    let back = configlock::read(&root).unwrap();
    assert_eq!(back, filled);
    let keys: Vec<&String> = back.hosts.keys().chain(back.homes.keys()).collect();
    assert_eq!(keys, ["host.toml", "server/host.toml", "home.toml"]);
    let text = fs::read_to_string(root.join("lodi.lock")).unwrap();
    assert_eq!(text.matches("\"home.toml\"").count(), 1);
    let names: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|name| name != "etc")
        .collect();
    assert_eq!(names, ["lodi.lock"], "one file, no temporary left");
}

#[test]
fn fill_adds_what_is_missing_and_leaves_every_existing_entry_as_it_was() {
    let old = Lock {
        hosts: [(
            "host.toml".to_string(),
            host(
                Some(("2026-01-01", "2026-01-01T00:00:00Z")),
                &[("vim", "2:9.0", "2:9.0.1")],
                "sha256:a",
            ),
        )]
        .into(),
        homes: [(
            "home.toml".to_string(),
            home(&[support::Tool::python("3.12.1")]),
        )]
        .into(),
        ..Lock::default()
    };
    let mut lookup = Fixed::default();
    // What a fresh lookup would give: a newer snapshot and newer versions of everything.
    lookup.hosts.insert(
        "host.toml".into(),
        host(
            Some(("2026-01-01", "2026-09-30T00:00:00Z")),
            &[("vim", "2:9.0", "2:9.0.9"), ("git", "1:2.39", "1:2.39.5")],
            "sha256:a",
        ),
    );
    lookup.homes.insert(
        "home.toml".into(),
        home(&[
            support::Tool::python("3.12.9"),
            support::Tool::node("20", "20.1.0"),
        ]),
    );
    let filled = configlock::fill(&old, &["host.toml"], &["home.toml"], &mut lookup).unwrap();
    assert!(lookup.asked.iter().all(|(_, mode)| *mode == Mode::Fill));
    let (before, after) = (&old.hosts["host.toml"], &filled.hosts["host.toml"]);
    assert_eq!(after.snapshot, before.snapshot, "the snapshot did not move");
    assert_eq!(
        after.pins["vim"], before.pins["vim"],
        "an existing pin did not move"
    );
    assert_eq!(after.pins["git"].version, "1:2.39.5", "a new pin was added");
    let (before, after) = (&old.homes["home.toml"], &filled.homes["home.toml"]);
    assert_eq!(after.tools["python"], before.tools["python"]);
    assert_eq!(after.tools["nodejs"].version, "20.1.0");
}

#[test]
fn refresh_moves_the_snapshot_and_keeps_package_pins() {
    let old = Lock {
        hosts: [(
            "host.toml".to_string(),
            host(
                Some(("2026-01-01", "2026-01-01T00:00:00Z")),
                &[("vim", "2:9.0.1", "2:9.0.1")],
                "sha256:a",
            ),
        )]
        .into(),
        ..Lock::default()
    };
    let mut lookup = Fixed::default();
    lookup.hosts.insert(
        "host.toml".into(),
        host(
            Some(("2026-09-30", "2026-09-30T00:00:00Z")),
            &[
                ("vim", "2:9.0.1", "2:9.0.1-other"),
                ("git", "1:2.39", "1:2.39.5"),
            ],
            "sha256:b",
        ),
    );
    let refreshed = configlock::refresh(&old, &["host.toml"], &[], &mut lookup).unwrap();
    assert_eq!(lookup.asked, [("host.toml".to_string(), Mode::Refresh)]);
    let after = &refreshed.hosts["host.toml"];
    assert_eq!(
        after.snapshot.as_ref().unwrap().instant,
        "2026-09-30T00:00:00Z"
    );
    assert_eq!(
        after.pins["vim"], old.hosts["host.toml"].pins["vim"],
        "the pin stays"
    );
    assert_eq!(after.pins["git"].version, "1:2.39.5");
    assert_eq!(after.keys["extra"], "sha256:b");
    // A host without a snapshot gets none.
    let mut lookup = Fixed::default();
    lookup
        .hosts
        .insert("host.toml".into(), host(None, &[], "sha256:b"));
    let floating = configlock::refresh(&old, &["host.toml"], &[], &mut lookup).unwrap();
    assert!(floating.hosts["host.toml"].snapshot.is_none());
}

#[test]
fn a_failed_lookup_writes_nothing() {
    let root = config("lock-failed");
    let mut lookup = Fixed {
        fail: true,
        ..Fixed::default()
    };
    let failed = configlock::refresh(&Lock::default(), &["host.toml"], &[], &mut lookup);
    assert_eq!(failed.unwrap_err().code, "E_FETCH");
    assert!(!root.join("lodi.lock").exists());
}

#[test]
fn a_lock_2_0_cannot_parse_stops_naming_it() {
    let root = config("lock-1x");
    let path = root.join("lodi.lock");
    for text in [
        "{\n  \"format\": \"lodi-repository-lock/1\",\n  \"version\": 1\n}\n",
        "{\"version\":1,\"format\":\"lodi-spike-lock/1\"}",
        "not json",
    ] {
        fs::write(&path, text).unwrap();
        let refused = configlock::read(&root).unwrap_err();
        assert_eq!(refused.code, "E_CONFIG");
        assert!(refused.message.contains(&path.display().to_string()));
        let held = configlock::hold(&root).unwrap();
        assert_eq!(held.read().unwrap_err().code, "E_CONFIG");
    }
}

#[test]
fn write_is_atomic_and_owned_by_the_config_folder_owner_under_sudo() {
    let root = config("lock-sudo");
    let held = configlock::hold(&root).unwrap();
    let lock = Lock {
        hosts: [("host.toml".to_string(), host(None, &[], "sha256:a"))].into(),
        ..Lock::default()
    };
    assert!(held.write(&lock, &acting(&root, true)).unwrap());
    let path = root.join("lodi.lock");
    let (file, dir) = (fs::metadata(&path).unwrap(), fs::metadata(&root).unwrap());
    assert_eq!((file.uid(), file.gid()), (dir.uid(), dir.gid()));
    assert_eq!(mode(&path), 0o644);
    // Writing the same lock again changes nothing on disk.
    let inode = file.ino();
    assert!(!held.write(&lock, &acting(&root, true)).unwrap());
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    // A link at the lock's path is never written through.
    let elsewhere = root.join("elsewhere");
    fs::remove_file(&path).unwrap();
    symlink(&elsewhere, &path).unwrap();
    assert!(held.write(&lock, &acting(&root, false)).is_err());
    assert!(!elsewhere.exists());
}

#[test]
fn a_second_writer_is_system_busy() {
    let root = config("lock-busy");
    let _held = configlock::hold(&root).unwrap();
    assert_eq!(configlock::hold(&root).unwrap_err().code, "E_SYSTEM_BUSY");
}
