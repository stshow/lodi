//! `lodi update`, `lodi pin` and `lodi unpin` on the config discovery finds (#696, LD-499).
//!
//! Every case is the real binary against a scratch `--root` with the fake machine of
//! `tests/support/fakehost.rs` behind it (`LODI_HOST_REQUIRE_ROOT=1`), a scratch `HOME` whose
//! `~/.config/lodi` is the config, a decoy current folder, and the recorded dated archives of
//! `tests/fixtures/host/pin/` served on loopback. Nothing reaches `/` or the network.

#[path = "support/elevators.rs"]
mod elevators;
#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
#[path = "support/pinverbs.rs"]
mod pinverbs;
mod support;
#[path = "support/wait.rs"]
mod wait;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use elevators::Elevators;
use fakehost::{err, out, story};
use pinverbs::*;

/// The child half of every held apply of this binary (`tests/support/hostroot.rs`).
#[test]
fn child_apply() {
    if !hostroot::is_child() {
        return;
    }
    hostroot::child_apply();
}

/// One machine, its config at `~/.config/lodi`, and a decoy current folder.
struct Cfg {
    v: Verbs,
}

fn mkdir(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

impl Cfg {
    fn debian(name: &str) -> Cfg {
        let v = Verbs::debian(name, &[("bc", BC), ("tzdata", TZ_OLDER)]);
        let cfg = Cfg { v };
        mkdir(&cfg.config());
        mkdir(&cfg.decoy());
        cfg
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.v.case.root.path(rel)
    }

    fn config(&self) -> PathBuf {
        self.path("home/.config/lodi")
    }

    fn decoy(&self) -> PathBuf {
        self.path("decoy")
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.config().join(rel);
        mkdir(path.parent().unwrap());
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    fn read(&self, rel: &str) -> Option<String> {
        fs::read_to_string(self.config().join(rel)).ok()
    }

    fn lock(&self) -> serde_json::Value {
        serde_json::from_str(&self.read("lodi.lock").expect("lodi.lock")).unwrap()
    }

    fn remembered(&self) -> Option<String> {
        fs::read_to_string(
            self.path("home/.local/state/lodi")
                .join(lodi::config::REMEMBERED),
        )
        .ok()
    }

    fn run_in(&self, dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let rewrite = self.v.server.rewrite();
        let _spawning = fakehost::spawning();
        Command::new(env!("CARGO_BIN_EXE_lodi"))
            .args(args)
            .arg("--root")
            .arg(&self.v.case.root.dir)
            .current_dir(dir)
            .env("PATH", self.v.case.fake_path())
            .env("HOME", self.path("home"))
            .env_remove("XDG_STATE_HOME")
            .env_remove("LODI_REPO")
            .env_remove("SUDO_UID")
            .env_remove("DOAS_USER")
            .env("LODI_HOME", self.path("lodi-home"))
            .env("LODI_HOST_REQUIRE_ROOT", "1")
            .env("LODI_FETCH_REWRITE", rewrite)
            .envs(envs.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("lodi runs")
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_in(&self.decoy(), args, &[])
    }
}

/// The last ended day's instant, served as the newer recorded day.
fn today(cfg: &Cfg) -> String {
    cfg.v.alias_now()[0].clone()
}

// ------------------------------------------------------------------------- update ---

#[test]
fn update_moves_the_snapshot_keeps_a_pin_and_names_switch() {
    let cfg = Cfg::debian("cfg-update");
    let day = today(&cfg);
    cfg.write(
        "host.toml",
        &host(
            "debian",
            Some(OLDER),
            &["bc", "tzdata"],
            &[("tzdata", "2025-03-01")],
        ),
    );
    let output = cfg.run(&["update"]);
    ok(&output);
    assert!(
        out(&output).ends_with("next: lodi switch\n"),
        "{}",
        story(&output)
    );
    assert!(err(&output).contains("[1/1]"), "{}", story(&output));
    let text = cfg.read("host.toml").unwrap();
    assert!(text.contains(&format!("snapshot = \"{day}\"")), "{text}");
    let lock = cfg.lock();
    let section = &lock["hosts"]["host.toml"];
    assert_eq!(section["snapshot"]["requested"], day.as_str());
    assert_eq!(section["pins"]["tzdata"]["requested"], "2025-03-01");
    assert_eq!(section["pins"]["tzdata"]["version"], TZ_OLDER);
    assert_eq!(lock["format"], "lodi-config-lock");

    let before = census(&cfg.config());
    let again = cfg.run(&["update"]);
    ok(&again);
    assert!(err(&again).contains("up to date"), "{}", story(&again));
    assert_eq!(census(&cfg.config()), before);
}

#[test]
fn update_gives_a_host_without_a_snapshot_none() {
    let cfg = Cfg::debian("cfg-update-float");
    let text = host("debian", None, &["bc"], &[]);
    cfg.write("host.toml", &text);
    ok(&cfg.run(&["update"]));
    assert_eq!(cfg.read("host.toml").unwrap(), text);
    let lock = cfg.lock();
    assert!(
        lock["hosts"]["host.toml"].get("snapshot").is_none(),
        "{lock}"
    );
}

#[test]
fn update_with_a_failed_lookup_changes_neither_file() {
    let cfg = Cfg::debian("cfg-update-fail");
    cfg.v.alias_now();
    let text = host(
        "debian",
        Some(OLDER),
        &["bc", "nosuch"],
        &[("nosuch", "2025-03-01")],
    );
    cfg.write("host.toml", &text);
    let failed = cfg.run(&["update"]);
    refused(&failed, "E_NO_MATCH");
    assert_eq!(cfg.read("host.toml").unwrap(), text);
    assert!(cfg.read("lodi.lock").is_none());
}

#[test]
fn update_locks_the_homes_too_and_a_home_only_config() {
    let cfg = Cfg::debian("cfg-update-home");
    cfg.write("host.toml", &host("debian", None, &["bc"], &[]));
    cfg.write("home.toml", "[home]\nversion = \"1\"\n");
    ok(&cfg.run(&["update"]));
    let lock = cfg.lock();
    assert!(lock["homes"]["home.toml"]["tools"].is_object(), "{lock}");

    fs::remove_file(cfg.config().join("host.toml")).unwrap();
    fs::remove_file(cfg.config().join("lodi.lock")).unwrap();
    let home_only = cfg.run(&["update"]);
    ok(&home_only);
    let lock = cfg.lock();
    assert!(lock.get("hosts").is_none(), "{lock}");
    assert!(lock["homes"]["home.toml"].is_object(), "{lock}");
}

#[test]
fn a_held_config_stops_a_second_writer() {
    let cfg = Cfg::debian("cfg-busy");
    let text = host("debian", None, &["bc"], &[]);
    cfg.write("host.toml", &text);
    let held = fs::File::open(cfg.config()).unwrap();
    // SAFETY: flock on a descriptor this test owns.
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    refused(&cfg.run(&["update"]), "E_SYSTEM_BUSY");
    refused(
        &cfg.run(&["pin", "bc", "--to", "2026-09-01"]),
        "E_SYSTEM_BUSY",
    );
    assert_eq!(cfg.read("host.toml").unwrap(), text);
    assert!(cfg.read("lodi.lock").is_none());
}

/// A `signed_by_url` key is fetched, checked against the declared digest and locked; a digest
/// the fetched key does not match stops the update with neither file written.
#[test]
fn update_locks_a_key_by_url() {
    let cfg = Cfg::debian("cfg-keys");
    let key = {
        let mut out = vec![0xc6, 29];
        out.extend_from_slice(b"\x04invented public key material");
        out.extend_from_slice(&[0xcd, 17]);
        out.extend_from_slice(b"invented test key");
        out
    };
    let sha = lodi::util::sha256_hex(&key);
    cfg.v
        .server
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
    cfg.write("host.toml", &format!("{text}{}", source(&sha)));
    ok(&cfg.run(&["update"]));
    assert_eq!(
        cfg.lock()["hosts"]["host.toml"]["keys"]["vendor"],
        format!("sha256:{sha}")
    );
    cfg.write("host.toml", &format!("{text}{}", source(&"0".repeat(64))));
    let wrong = cfg.run(&["update"]);
    assert_ne!(code(&wrong), Some(0), "{}", story(&wrong));
    let lock = cfg.lock();
    assert_eq!(
        lock["hosts"]["host.toml"]["keys"]["vendor"],
        format!("sha256:{sha}")
    );
}

// ---------------------------------------------------------------------- discovery ---

#[test]
fn discovery_order_and_remembering_a_typed_path() {
    let cfg = Cfg::debian("cfg-find");
    let text = host("debian", None, &["bc"], &[]);
    // The decoy current folder holds a config of its own, which is never read.
    fs::write(cfg.decoy().join("host.toml"), "not toml [").unwrap();
    let none = cfg.run(&["update"]);
    refused(&none, "E_NO_MANIFEST");
    assert!(err(&none).contains("lodi import"), "{}", story(&none));

    let other = cfg.path("other");
    mkdir(&other);
    fs::write(other.join("host.toml"), &text).unwrap();
    let by_env = cfg.run_in(
        &cfg.decoy(),
        &["update"],
        &[("LODI_REPO", other.to_str().unwrap())],
    );
    ok(&by_env);
    assert!(other.join("lodi.lock").exists());
    assert!(cfg.remembered().is_none());

    let typo = cfg.run(&["update", cfg.path("missing").to_str().unwrap()]);
    refused(&typo, "E_NO_MANIFEST");
    assert!(cfg.remembered().is_none());
    ok(&cfg.run(&["update", other.to_str().unwrap()]));
    assert_eq!(cfg.remembered().unwrap(), format!("{}\n", other.display()));

    cfg.write("host.toml", &text);
    ok(&cfg.run(&["update"]));
    assert!(
        cfg.read("lodi.lock").is_none(),
        "the remembered path comes first"
    );
}

#[test]
fn a_url_config_stops() {
    let cfg = Cfg::debian("cfg-url");
    let output = cfg.run(&[
        "pin",
        "bc",
        "--to",
        "2026-09-01",
        "https://example.org/config.git",
    ]);
    refused(&output, "E_UNSUPPORTED");
}

// ---------------------------------------------------------------------- the hosts ---

#[test]
fn hosts_flat_and_named() {
    let cfg = Cfg::debian("cfg-hosts");
    cfg.write("host.toml", &host("debian", None, &["bc"], &[]));
    let flat = cfg.run(&["update", "--host", "box"]);
    assert_eq!(flat.status.code(), Some(2), "{flat:?}");
    fs::remove_file(cfg.config().join("host.toml")).unwrap();
    cfg.write("box/host.toml", &host("debian", None, &["bc"], &[]));
    cfg.write("work/host.toml", &host("debian", None, &["bc"], &[]));
    cfg.write(
        "config.toml",
        "[hosts.box]\nhost = \"box/host.toml\"\n\n[hosts.work]\nhost = \"work/host.toml\"\n",
    );
    let unchosen = cfg.run(&["update"]);
    refused(&unchosen, "E_NO_MANIFEST");
    assert!(err(&unchosen).contains("box, work"), "{}", story(&unchosen));
    refused(&cfg.run(&["update", "--host", "nosuch"]), "E_NO_MANIFEST");
    // A switch takes its host from the hostname (this machine is `box`), and its `--host` is
    // the host part only: the next step names the machine, never `--host NAME`.
    let work = cfg.run(&["pin", "bc", "--to", "2026-09-01", "--host", "work"]);
    ok(&work);
    assert!(
        out(&work).ends_with("next: lodi switch, run on work\n"),
        "{}",
        story(&work)
    );
    let lock = cfg.lock();
    assert!(
        lock["hosts"]["work/host.toml"]["pins"]["bc"].is_object(),
        "{lock}"
    );
    assert!(lock["hosts"].get("box/host.toml").is_none(), "{lock}");
    let this = cfg.run(&["pin", "bc", "--to", "2026-09-01", "--host", "box"]);
    ok(&this);
    assert!(
        out(&this).ends_with("next: lodi switch\n"),
        "{}",
        story(&this)
    );
}

// --------------------------------------------------------------------------- pin ---

#[test]
fn pin_to_a_date_writes_both_files_and_again_writes_nothing() {
    let cfg = Cfg::debian("cfg-pin");
    let text = host("debian", None, &["bc", "tzdata"], &[]);
    cfg.write("host.toml", &text);
    let pinned = cfg.run(&["pin", "tzdata", "--to", "2025-03-01"]);
    ok(&pinned);
    assert!(
        out(&pinned).ends_with("next: lodi switch\n"),
        "{}",
        story(&pinned)
    );
    let (removed, added) = changed_lines(&text, &cfg.read("host.toml").unwrap());
    assert!(removed.is_empty(), "{removed:?}");
    let added: Vec<&String> = added.iter().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        added,
        ["[packages.pin]", "tzdata = \"2025-03-01\""],
        "{added:?}"
    );
    assert_eq!(
        cfg.lock()["hosts"]["host.toml"]["pins"]["tzdata"]["version"],
        TZ_OLDER
    );
    let before = census(&cfg.config());
    let again = cfg.run(&["pin", "tzdata", "--to", "2025-03-01"]);
    ok(&again);
    assert!(out(&again).contains("nothing written"), "{}", story(&again));
    assert_eq!(census(&cfg.config()), before);
}

#[test]
fn pin_refuses_an_unknown_package_and_a_failed_lookup_writes_nothing() {
    let cfg = Cfg::debian("cfg-pin-fail");
    let text = host("debian", None, &["bc", "nosuch"], &[]);
    cfg.write("host.toml", &text);
    refused(
        &cfg.run(&["pin", "curl", "--to", "2025-03-01"]),
        "E_UNKNOWN_PACKAGE",
    );
    refused(
        &cfg.run(&["pin", "nosuch", "--to", "2025-03-01"]),
        "E_NO_MATCH",
    );
    assert_eq!(cfg.read("host.toml").unwrap(), text);
    assert!(cfg.read("lodi.lock").is_none());
}

#[test]
fn pin_without_to_lists_the_versions_and_writes_nothing() {
    let cfg = Cfg::debian("cfg-versions");
    cfg.v.alias_now();
    cfg.write("host.toml", &host("debian", None, &["bc", "tzdata"], &[]));
    let before = census(&cfg.config());
    let listed = cfg.run(&["pin", "tzdata"]);
    ok(&listed);
    let text = out(&listed);
    assert!(text.contains(TZ_RECENT), "{text}");
    assert!(text.contains("installed"), "{text}");
    let copy = text.lines().last().unwrap();
    assert!(copy.starts_with("lodi pin tzdata --to "), "{text}");
    assert!(!copy.contains("--host"), "{text}");
    assert_eq!(census(&cfg.config()), before);
}

/// A package the host does not declare cannot be pinned, so its listing says so instead of
/// ending with a `lodi pin` line that would fail.
#[test]
fn pin_without_to_on_an_undeclared_package_says_it_is_not_declared() {
    let cfg = Cfg::debian("cfg-versions-undeclared");
    cfg.v.alias_now();
    cfg.write("host.toml", &host("debian", None, &["bc"], &[]));
    let listed = cfg.run(&["pin", "tzdata"]);
    ok(&listed);
    let text = out(&listed);
    assert!(text.contains(TZ_RECENT), "{text}");
    assert!(!text.contains("lodi pin tzdata --to"), "{text}");
    assert!(text.contains("tzdata is not in the package list"), "{text}");
}

/// A name the archive lacks is never offered as its own nearest name.
#[test]
fn pin_never_suggests_the_exact_name_it_refused() {
    let cfg = Cfg::debian("cfg-pin-self");
    cfg.v.alias_now();
    cfg.v.case.install_by_hand(fakehost::Pkg::new("jqx"));
    cfg.write("host.toml", &host("debian", None, &["bc", "jqx"], &[]));
    let output = cfg.run(&["pin", "jqx"]);
    refused(&output, "E_UNKNOWN_PACKAGE");
    assert!(!err(&output).contains("`jqx`?"), "{}", story(&output));
}

#[test]
fn pin_all_sets_the_snapshot_and_refuses_a_version() {
    let cfg = Cfg::debian("cfg-pin-all");
    let day = today(&cfg);
    cfg.write("host.toml", &host("debian", None, &["bc"], &[]));
    ok(&cfg.run(&["pin", "--all"]));
    assert!(
        cfg.read("host.toml")
            .unwrap()
            .contains(&format!("snapshot = \"{day}\""))
    );
    let to = cfg.run(&["pin", "--all", "--to", "2026-09-01"]);
    ok(&to);
    assert!(
        cfg.read("host.toml")
            .unwrap()
            .contains(&format!("snapshot = \"{RECENT}\""))
    );
    assert_eq!(
        cfg.lock()["hosts"]["host.toml"]["snapshot"]["instant"],
        RECENT
    );
    let before = census(&cfg.config());
    ok(&cfg.run(&["pin", "--all", "--to", "2026-09-01"]));
    assert_eq!(census(&cfg.config()), before);
    refused(&cfg.run(&["pin", "--all", "--to", "1.2-3"]), "E_TYPE");
    let usage = cfg.run(&["pin", "--all", "bc"]);
    assert_eq!(code(&usage), Some(2), "{}", story(&usage));
}

#[test]
fn pin_all_on_a_home_only_config_stops() {
    let cfg = Cfg::debian("cfg-pin-home");
    cfg.write("home.toml", "[home]\nversion = \"1\"\n");
    refused(&cfg.run(&["pin", "--all"]), "E_NO_MANIFEST");
}

// ------------------------------------------------------------------------- unpin ---

#[test]
fn unpin_removes_the_key_and_its_entry_and_nothing_is_exit_0() {
    let cfg = Cfg::debian("cfg-unpin");
    let text = host("debian", None, &["bc", "tzdata"], &[]);
    cfg.write("host.toml", &text);
    ok(&cfg.run(&["pin", "tzdata", "--to", "2025-03-01"]));
    let unpinned = cfg.run(&["unpin", "tzdata"]);
    ok(&unpinned);
    assert!(
        out(&unpinned).ends_with("next: lodi switch\n"),
        "{}",
        story(&unpinned)
    );
    assert_eq!(cfg.read("host.toml").unwrap(), text);
    assert!(
        cfg.lock()["hosts"]["host.toml"].get("pins").is_none(),
        "{}",
        cfg.lock()
    );
    let before = census(&cfg.config());
    let none = cfg.run(&["unpin", "tzdata"]);
    ok(&none);
    assert!(out(&none).contains("nothing written"), "{}", story(&none));
    assert_eq!(census(&cfg.config()), before);
}

#[test]
fn unpin_all_removes_the_snapshot_and_every_pin() {
    let cfg = Cfg::debian("cfg-unpin-all");
    let floating = host("debian", None, &["bc", "tzdata"], &[]);
    cfg.write(
        "host.toml",
        &host(
            "debian",
            Some(RECENT),
            &["bc", "tzdata"],
            &[("tzdata", "2025-03-01")],
        ),
    );
    ok(&cfg.run(&["unpin", "--all"]));
    assert_eq!(cfg.read("host.toml").unwrap(), floating);
    let section = &cfg.lock()["hosts"]["host.toml"];
    assert!(
        section.get("snapshot").is_none() && section.get("pins").is_none(),
        "{section}"
    );
    let before = census(&cfg.config());
    ok(&cfg.run(&["unpin", "--all"]));
    assert_eq!(census(&cfg.config()), before);
}

// ------------------------------------------------------------------- no elevation ---

/// The same three commands with the recording stub elevators first on `PATH`, as the user and
/// then as `sudo` runs them (root in a user namespace of its own, `SUDO_UID` naming the person,
/// `HOME` root's own): no elevator is ever called, and under `sudo` the person's own config is
/// written, as theirs.
#[test]
fn update_pin_and_unpin_never_elevate_and_under_sudo_write_the_person_s_config() {
    let cfg = Cfg::debian("cfg-elevators");
    let (uid, gid) = hostroot::ids(&cfg.v.case.root);
    cfg.v.case.root.write(
        "etc/passwd",
        &format!("root:x:0:0::/root:/bin/sh\nsample:x:{uid}:{gid}::/home:/bin/sh\n"),
    );
    let root_home = cfg.path("root-home");
    mkdir(&root_home);
    let el = Elevators::new(&cfg.v.case.root.dir);
    let path = el.path(&[&cfg.v.case.base().join("bin")]);
    let text = host("debian", None, &["bc", "tzdata"], &[]);
    cfg.write("host.toml", &text);
    let as_user = |args: &[&str]| cfg.run_in(&cfg.decoy(), args, &[("PATH", path.as_str())]);
    ok(&as_user(&["update"]));
    ok(&as_user(&["pin", "tzdata", "--to", "2025-03-01"]));
    assert!(
        cfg.read("host.toml")
            .unwrap()
            .contains("tzdata = \"2025-03-01\"")
    );
    ok(&as_user(&["unpin", "tzdata"]));
    assert_eq!(cfg.read("host.toml").unwrap(), text);

    let (suid, sgid) = (uid.to_string(), gid.to_string());
    // Found on the test's own `PATH`: the child's names only the stubs, shims and tools.
    let unshare = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("unshare"))
        .find(|file| file.is_file())
        .expect("unshare");
    let under_sudo = |args: &[&str]| {
        let rewrite = cfg.v.server.rewrite();
        let _spawning = fakehost::spawning();
        Command::new(&unshare)
            .args(["--user", "--map-root-user"])
            .arg(env!("CARGO_BIN_EXE_lodi"))
            .args(args)
            .arg("--root")
            .arg(&cfg.v.case.root.dir)
            .current_dir(cfg.decoy())
            .env("PATH", &path)
            .env("HOME", &root_home)
            .env("SUDO_UID", &suid)
            .env("SUDO_GID", &sgid)
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("LODI_REPO")
            .env_remove("DOAS_USER")
            .env("LODI_HOME", cfg.path("lodi-home"))
            .env("LODI_HOST_REQUIRE_ROOT", "1")
            .env("LODI_FETCH_REWRITE", rewrite)
            .stdin(Stdio::null())
            .output()
            .expect("lodi runs")
    };
    ok(&under_sudo(&["update"]));
    ok(&under_sudo(&["pin", "tzdata", "--to", "2025-03-01"]));
    assert!(
        cfg.read("host.toml")
            .unwrap()
            .contains("tzdata = \"2025-03-01\"")
    );
    ok(&under_sudo(&["unpin", "tzdata"]));
    assert_eq!(cfg.read("host.toml").unwrap(), text);
    for file in ["host.toml", "lodi.lock"] {
        let meta = fs::metadata(cfg.config().join(file)).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (uid, gid), "{file}");
    }
    assert!(census(&root_home).is_empty(), "root's HOME is never used");
    assert_eq!(el.calls(), Vec::<String>::new(), "no elevator");
}

// ---------------------------------------------------------------------- projects ---

/// A project with no tools and no base: its lock needs no network.
fn project(cfg: &Cfg) -> PathBuf {
    let dir = cfg.path("project");
    mkdir(&dir);
    fs::write(dir.join("lodi.toml"), "[project]\nname = \"demo\"\n").unwrap();
    dir
}

#[test]
fn update_in_a_project_updates_its_lock_whatever_else_is_set() {
    let cfg = Cfg::debian("cfg-project");
    let text = host("debian", None, &["bc"], &[]);
    cfg.write("host.toml", &text);
    let dir = project(&cfg);
    let manifest = fs::read(dir.join("lodi.toml")).unwrap();
    let envs = [("LODI_REPO", cfg.config().to_str().unwrap().to_string())];
    let envs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let output = cfg.run_in(&dir, &["update"], &envs);
    ok(&output);
    assert!(dir.join("lodi.lock").exists(), "{}", story(&output));
    assert_eq!(fs::read(dir.join("lodi.toml")).unwrap(), manifest);
    assert!(
        cfg.read("lodi.lock").is_none(),
        "the config was not updated"
    );

    let usage = cfg.run_in(&dir, &["update", "--host", "box"], &[]);
    assert_eq!(code(&usage), Some(2), "{}", story(&usage));
    refused(&cfg.run(&["update", dir.to_str().unwrap()]), "E_CONFIG");
    ok(&cfg.run_in(&dir, &["update", cfg.config().to_str().unwrap()], &[]));
    assert!(
        cfg.read("lodi.lock").is_some(),
        "a typed path picks the config"
    );

    // `pin` acts on the config even from inside a project.
    ok(&cfg.run_in(&dir, &["pin", "bc", "--to", "2026-09-01"], &[]));
    assert!(
        cfg.read("host.toml")
            .unwrap()
            .contains("bc = \"2026-09-01\"")
    );
}

#[test]
fn a_project_in_a_parent_folder_is_never_used() {
    let cfg = Cfg::debian("cfg-project-parent");
    cfg.write("host.toml", &host("debian", None, &["bc"], &[]));
    let dir = project(&cfg);
    let below = dir.join("src");
    mkdir(&below);
    ok(&cfg.run_in(&below, &["update"], &[]));
    assert!(!dir.join("lodi.lock").exists());
    assert!(cfg.read("lodi.lock").is_some());
}
