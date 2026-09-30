//! H1: only the invoking uid's home in the selected host participates in a combined plan.
//! Every host invocation goes through the fake machine's scratch --root and the lane's
//! LODI_HOST_REQUIRE_ROOT=1 guard; nothing inspects the real machine's home.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
#[path = "support/wait.rs"]
mod wait;

mod support;

use fakehost::{Case, Machine, out, story};
use hostroot::ids;
use std::os::unix::ffi::OsStrExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn h8_named_import_creates_home_stub_but_in_place_keeps_host_files() {
    named_import_creates_home_stub("h8", Machine::debian);
}

/// H8 on Fedora (ff-1).
#[test]
fn h8_named_import_creates_home_stub_on_fedora() {
    named_import_creates_home_stub("h8-fedora", Machine::fedora);
}

fn named_import_creates_home_stub(name: &str, machine: fn() -> Machine) {
    let named = Case::new(&format!("{name}-named-import"), machine());
    let (uid, gid) = ids(&named.root);
    named.root.write("etc/hostname", "box\n");
    named.root.write(
        "etc/passwd",
        &format!("sample:x:{uid}:{gid}::/people/sample:/bin/sh\n"),
    );
    std::fs::create_dir_all(named.root.path("hosts")).unwrap();
    let hosts = named.root.path("hosts");
    let result = named.verb("import", &[hosts.to_str().unwrap()]);
    assert_eq!(result.status.code(), Some(0), "{}", story(&result));
    let selected = ["hosts", "box", "home", "sample", "home.toml"].join("/");
    assert!(named.root.exists(&selected));

    let in_place = Case::new(&format!("{name}-in-place-import"), machine());
    in_place.root.write("etc/hostname", "box\n");
    let result = in_place.import();
    assert_eq!(result.status.code(), Some(0), "{}", story(&result));
    assert!(!in_place.root.exists("etc/lodi/home"));
}

#[test]
fn h8_named_reimport_keeps_existing_home_manifest() {
    let case = Case::new("h8-reimport", Machine::debian());
    case.root.write("etc/hostname", "box\n");
    case.root
        .write("hosts/box/host.toml", "[host]\ndistro = \"debian\"\n");
    let selected = ["hosts", "box", "home", "sample", "home.toml"].join("/");
    case.root.write(&selected, "[home]\nversion = \"1\"\n");
    let hosts = case.root.path("hosts");
    let result = case.verb("import", &[hosts.to_str().unwrap()]);
    assert_eq!(result.status.code(), Some(0), "{}", story(&result));
    assert_eq!(case.root.read(&selected), "[home]\nversion = \"1\"\n");
    assert!(out(&result).contains("kept"), "{}", story(&result));
}

#[test]
fn h8_named_import_does_not_open_a_home_fifo() {
    let case = Case::new("h8-no-home-open", Machine::debian());
    case.root.write("etc/hostname", "box\n");
    let home = case.root.path("people/sample");
    std::fs::create_dir_all(&home).unwrap();
    let fifo = home.join(".gitconfig");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: the path is NUL-terminated and belongs to this scratch root.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    std::fs::create_dir_all(case.root.path("hosts")).unwrap();
    // Held only around `spawn`, not the wait loop below: this child deliberately blocks on the
    // FIFO for up to `wait::ceiling()`, and a guard held that long would stall every other
    // thread's `Case::new` in this binary.
    let spawning = fakehost::spawning();
    let mut child = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .arg("host")
        .arg("import")
        .arg(case.root.path("hosts"))
        .arg("--root")
        .arg(&case.root.dir)
        .env("PATH", case.fake_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(spawning);
    let deadline = Instant::now() + wait::ceiling();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("named import blocked on a home FIFO");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(0));
    let stub = ["hosts", "box", "home", "sample", "home.toml"].join("/");
    assert!(case.root.exists(&stub));
}

#[test]
fn h3_apply() {
    applies_host_then_home(Case::new("h3-combined", Machine::debian()), "debian");
}

/// H3 on Fedora (ff-1).
#[test]
fn h3_apply_on_fedora() {
    applies_host_then_home(Case::new("h3-combined-fedora", Machine::fedora()), "fedora");
}

fn applies_host_then_home(case: Case, distro: &str) {
    let (uid, gid) = ids(&case.root);
    case.root.write("etc/hostname", "box\n");
    case.root.write(
        "etc/passwd",
        &format!("sample:x:{uid}:{gid}::/people/sample:/bin/sh\n"),
    );
    case.root.write(
        "hosts/box/host.toml",
        &format!(
            "[host]\ndistro = \"{distro}\"\n\n[files.\"/etc/combined\"]\n\
             content = \"host\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
        ),
    );
    let home = ["hosts", "box", "home", "sample", "home.toml"].join("/");
    case.root.write(
        &home,
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"home\"\n",
    );
    std::fs::create_dir_all(case.root.path("people/sample")).unwrap();
    let hosts = case.root.path("hosts");
    let applied = case.apply(&[hosts.to_str().unwrap()]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    assert_eq!(case.root.read("etc/combined"), "host\n");
    assert_eq!(case.root.read("people/sample/note"), "home");
    let raw = case
        .root
        .read("people/sample/.local/share/lodi/home-scope/state.json");
    let state: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let source = ["hosts", "box", "home", "sample"].join("/");
    assert_eq!(
        state["source"],
        case.root.path(&source).display().to_string()
    );
}

#[test]
fn h4_combined_apply_ignores_decoy_home_roots() {
    let case = Case::new("h4-decoy-roots", Machine::debian());
    let (uid, gid) = ids(&case.root);
    case.root.write("etc/hostname", "box\n");
    case.root.write(
        "etc/passwd",
        &format!("sample:x:{uid}:{gid}::/people/sample:/bin/sh\n"),
    );
    case.root
        .write("hosts/box/host.toml", "[host]\ndistro = \"debian\"\n");
    let manifest = ["hosts", "box", "home", "sample", "home.toml"].join("/");
    case.root.write(
        &manifest,
        "[home]\nversion = \"1\"\n\n[home.xdg_config.\"demo/app\"]\ntext = \"owned\"\n",
    );
    std::fs::create_dir_all(case.root.path("people/sample")).unwrap();
    let decoy = case.root.path("decoy").display().to_string();
    let hosts = case.root.path("hosts");
    let applied = case.verb_env(
        "apply",
        &[hosts.to_str().unwrap()],
        &[
            ("HOME", &decoy),
            ("XDG_CONFIG_HOME", &decoy),
            ("XDG_DATA_HOME", &decoy),
            ("XDG_STATE_HOME", &decoy),
            ("LODI_HOME", &decoy),
        ],
    );
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    assert_eq!(case.root.read("people/sample/.config/demo/app"), "owned");
    assert!(!case.root.exists("decoy/demo/app"));
    assert!(!case.root.exists("decoy/home-scope/state.json"));
}

#[test]
fn h2_group_writable_home_source_refuses_before_host_change() {
    let case = Case::new("h2-writable-source", Machine::debian());
    let (uid, gid) = ids(&case.root);
    case.root.write("etc/hostname", "box\n");
    case.root.write(
        "etc/passwd",
        &format!("sample:x:{uid}:{gid}::/people/sample:/bin/sh\n"),
    );
    case.root.write(
        "hosts/box/host.toml",
        &format!(
            "[host]\ndistro = \"debian\"\n\n[files.\"/etc/combined\"]\n\
             content = \"host\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
        ),
    );
    let home = ["hosts", "box", "home", "sample"].join("/");
    case.root.write(
        &format!("{home}/home.toml"),
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\nsource = \"inputs/note\"\n",
    );
    case.root.write(&format!("{home}/inputs/note"), "untrusted");
    case.root.chmod(&format!("{home}/inputs"), 0o775);
    std::fs::create_dir_all(case.root.path("people/sample")).unwrap();
    let hosts = case.root.path("hosts");
    let applied = case.apply(&[hosts.to_str().unwrap()]);
    assert_eq!(applied.status.code(), Some(3), "{}", story(&applied));
    assert!(fakehost::err(&applied).contains("E_PATH_ESCAPE"));
    assert!(!case.root.exists("etc/combined"));
    assert!(!case.has_lock());
}

#[test]
fn h2_invalid_home_lock_refuses_before_host_change() {
    let case = Case::new("h2-lock", Machine::debian());
    let (uid, gid) = ids(&case.root);
    case.root.write("etc/hostname", "box\n");
    case.root.write(
        "etc/passwd",
        &format!("sample:x:{uid}:{gid}::/people/sample:/bin/sh\n"),
    );
    case.root.write(
        "hosts/box/host.toml",
        &format!(
            "[host]\ndistro = \"debian\"\n\n[files.\"/etc/combined\"]\n\
             content = \"host\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
        ),
    );
    let home = ["hosts", "box", "home", "sample"].join("/");
    case.root
        .write(&format!("{home}/home.toml"), "[home]\nversion = \"1\"\n");
    case.root.write(&format!("{home}/home.lock"), "broken");
    std::fs::create_dir_all(case.root.path("people/sample")).unwrap();
    let hosts = case.root.path("hosts");
    let applied = case.apply(&[hosts.to_str().unwrap()]);
    assert_ne!(applied.status.code(), Some(0), "{}", story(&applied));
    assert!(!case.root.exists("etc/combined"));
    assert!(!case.has_lock());
}

#[test]
fn h5_home_drift_reports_host_applied_and_home_refused() {
    let case = Case::new("h5-home-drift", Machine::debian());
    let (uid, gid) = ids(&case.root);
    case.root.write("etc/hostname", "box\n");
    case.root.write(
        "etc/passwd",
        &format!("sample:x:{uid}:{gid}::/people/sample:/bin/sh\n"),
    );
    let manifest = |body: &str| {
        format!(
            "[host]\ndistro = \"debian\"\n\n[files.\"/etc/combined\"]\n\
             content = \"{body}\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
        )
    };
    case.root.write("hosts/box/host.toml", &manifest("first"));
    let home = ["hosts", "box", "home", "sample", "home.toml"].join("/");
    case.root.write(
        &home,
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"managed\"\n",
    );
    std::fs::create_dir_all(case.root.path("people/sample")).unwrap();
    let hosts = case.root.path("hosts");
    let named = hosts.to_str().unwrap();
    let first = case.apply(&[named]);
    assert_eq!(first.status.code(), Some(0), "{}", story(&first));
    case.root.write("hosts/box/host.toml", &manifest("second"));
    case.root.write("people/sample/note", "edited");
    let failed = case.apply(&[named]);
    assert_eq!(failed.status.code(), Some(8), "{}", story(&failed));
    assert_eq!(case.root.read("etc/combined"), "second\n");
    let error = fakehost::err(&failed);
    assert!(error.contains("host applied"), "{error}");
    assert!(
        error.contains("W_DRIFT") && error.contains("note"),
        "{error}"
    );
    let plan = case.plan_with(&[named]);
    assert_eq!(plan.status.code(), Some(0), "{}", story(&plan));
    assert!(out(&plan).contains("note"), "{}", story(&plan));
    case.root.write("people/sample/note", "managed");
    let again = case.apply(&[named]);
    assert_eq!(again.status.code(), Some(0), "{}", story(&again));
    assert_eq!(case.root.read("etc/combined"), "second\n");
    assert!(out(&again).contains("nothing to do"), "{}", story(&again));
}

#[test]
fn h1_host_without_home_applies_without_passwd_entry() {
    let case = Case::new("h1-no-home", Machine::debian());
    let (uid, _) = ids(&case.root);
    case.root.write("etc/passwd", "root:x:0:0::/root:/bin/sh\n");
    case.root.write("etc/hostname", "box\n");
    case.root
        .write("hosts/box/host.toml", "[host]\ndistro = \"debian\"\n");
    let hosts = case.root.path("hosts");
    let path = hosts.to_str().expect("scratch path is UTF-8");
    let applied = case.apply(&[path]);
    assert_eq!(
        applied.status.code(),
        Some(0),
        "uid {uid}: {}",
        story(&applied)
    );
    assert!(
        out(&applied).contains("nothing to do"),
        "{}",
        story(&applied)
    );
}

#[test]
fn h1_no_home() {
    let case = Case::new("h1-no-home-flag", Machine::debian());
    case.root.write("etc/passwd", "root:x:0:0::/root:/bin/sh\n");
    case.root.write("etc/hostname", "box\n");
    case.root
        .write("hosts/box/host.toml", "[host]\ndistro = \"debian\"\n");
    case.root.write(
        &format!("hosts/box/home/{}/home.toml", "other"),
        "[home]\nversion = \"1\"\n",
    );
    let hosts = case.root.path("hosts");
    let applied = case.apply(&[hosts.to_str().expect("scratch path is UTF-8"), "--no-home"]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    assert!(
        out(&applied).contains("home skipped"),
        "{}",
        story(&applied)
    );
}

#[test]
fn h1_uid() {
    let case = Case::new("h1-missing-uid", Machine::debian());
    let (uid, _) = ids(&case.root);
    case.root.write("etc/passwd", "root:x:0:0::/root:/bin/sh\n");
    case.root.write("etc/hostname", "box\n");
    case.root
        .write("hosts/box/host.toml", "[host]\ndistro = \"debian\"\n");
    case.root.write(
        &format!("hosts/box/home/{}/home.toml", "other"),
        "[home]\nversion = \"1\"\n",
    );
    let hosts = case.root.path("hosts");
    let applied = case.apply(&[hosts.to_str().expect("scratch path is UTF-8")]);
    assert_eq!(applied.status.code(), Some(3), "{}", story(&applied));
    assert!(
        fakehost::err(&applied).contains("E_CONFIG"),
        "{}",
        story(&applied)
    );
    assert!(
        fakehost::err(&applied).contains(&uid.to_string()),
        "{}",
        story(&applied)
    );
    assert!(
        !case.has_lock(),
        "host must not be applied before home preflight"
    );
}

#[test]
fn h1_plan() {
    plans_only_the_selected_home(Case::new("h1-selected-home", Machine::debian()), "debian");
}

/// H1 on Fedora (ff-1).
#[test]
fn h1_plan_on_fedora() {
    let case = Case::new("h1-selected-home-fedora", Machine::fedora());
    plans_only_the_selected_home(case, "fedora");
}

fn plans_only_the_selected_home(case: Case, distro: &str) {
    let (uid, gid) = ids(&case.root);
    case.root.write(
        "etc/passwd",
        &format!("sample:x:{uid}:{gid}::/people/sample:/bin/sh\n"),
    );
    case.root.write("etc/group", &format!("sample:x:{gid}:\n"));
    case.root.write("etc/hostname", "box\n");
    case.root.write(
        "hosts/box/host.toml",
        &format!("[host]\ndistro = \"{distro}\"\n"),
    );
    case.root.write(
        &format!("hosts/box/home/{}/home.toml", "sample"),
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"only this home\"\n",
    );
    case.root.write(
        &format!("hosts/box/home/{}/home.toml", "other"),
        "[home]\nversion = \"1\"\n\n[home.file.\"secret\"]\ntext = \"not selected\"\n",
    );
    let hosts = case.root.path("hosts");
    let plan = case.plan_with(&[hosts.to_str().expect("scratch path is UTF-8")]);
    assert_eq!(plan.status.code(), Some(0), "{}", story(&plan));
    assert!(
        out(&plan).contains("home sample"),
        "H1: selected home absent from combined plan: {}",
        story(&plan)
    );
    assert!(
        out(&plan).contains("note"),
        "H1: selected entry absent: {}",
        story(&plan)
    );
    assert!(
        !out(&plan).contains("secret"),
        "H1: another home was opened: {}",
        story(&plan)
    );
}

// ------------------------------------------ the fixture of tests/fixtures/host/flake/ ---

fn repo() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fill(case: &Case, text: &str) -> String {
    let (uid, gid) = ids(&case.root);
    text.replace("{uid}", &uid.to_string())
        .replace("{gid}", &gid.to_string())
        .replace("{other}", &(uid + 1).to_string())
}

fn copy_tree(case: &Case, from: &std::path::Path, to: &str) {
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = format!("{to}/{name}");
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(case.root.path(&rel)).unwrap();
            copy_tree(case, &entry.path(), &rel);
        } else {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            case.root.write(&rel, &fill(case, &text));
        }
    }
}

/// The fixture's root: `etc/passwd`, `etc/group`, `etc/hostname` and the directory of hosts,
/// with `sample`'s passwd home made. Returns the directory of hosts, as a `SOURCE`.
fn flake(case: &Case) -> String {
    let fixture = repo().join("tests/fixtures/host/flake");
    for (from, to) in [("passwd", "etc/passwd"), ("group", "etc/group")] {
        let text = std::fs::read_to_string(fixture.join(from)).unwrap();
        case.root.write(to, &fill(case, &text));
    }
    case.root.write("etc/hostname", "box\n");
    copy_tree(case, &fixture.join("hosts"), "hosts");
    std::fs::create_dir_all(case.root.path("people/sample")).unwrap();
    case.root.path("hosts").display().to_string()
}

const SAMPLE: &str = "hosts/box/home/sample";

/// `<dir>/home/<login>/<rel>`, spelled without a home path in the source.
fn home_in(dir: &str, login: &str, rel: &str) -> String {
    [dir, "home", login, rel].join("/")
}

/// Every path below `dir`, never following a link, with its kind, mode, owner, bytes and
/// modification time: two equal censuses are two identical trees.
fn census(dir: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    use std::os::unix::fs::MetadataExt;
    let mut found = std::collections::BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(at) = pending.pop() {
        for entry in std::fs::read_dir(&at).unwrap().flatten() {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let bytes = if meta.is_file() {
                std::fs::read(&path).unwrap()
            } else {
                Vec::new()
            };
            if meta.is_dir() {
                pending.push(path.clone());
            }
            let rel = path.strip_prefix(dir).unwrap().display().to_string();
            let what = format!(
                "{:o} {}:{} {} {:?}",
                meta.mode(),
                meta.uid(),
                meta.gid(),
                meta.mtime_nsec() + meta.mtime() * 1_000_000_000,
                bytes
            );
            found.insert(rel, what);
        }
    }
    found
}

fn make_fifo(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: the path is NUL-terminated and belongs to this test's scratch root.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
}

/// H1: another login's home is never opened. `home/other/` is writable by everyone and its
/// manifest a FIFO, so a walk into it would be `E_PATH_ESCAPE`; plan and apply both succeed.
#[test]
fn h1_census_no_other_home_is_opened() {
    let case = Case::new("h1-census", Machine::debian());
    let hosts = flake(&case);
    make_fifo(&case.root.path(&home_in("hosts/box", "other", "home.toml")));
    case.root.chmod("hosts/box/home/other", 0o777);
    let plan = case.plan_with(&[&hosts]);
    assert_eq!(plan.status.code(), Some(0), "{}", story(&plan));
    assert!(!out(&plan).contains("secret"), "{}", story(&plan));
    let applied = case.apply(&[&hosts]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    assert_eq!(case.root.read("people/sample/note"), "home\n");
    assert!(!case.root.exists("people/sample/secret"));
    assert!(!case.root.exists("people/other"));
}

/// H1: a login that is not one safe path component is `E_PATH_ESCAPE`, before the host.
#[test]
fn h1_unsafe_login_is_path_escape_before_the_host() {
    let case = Case::new("h1-unsafe-login", Machine::debian());
    let hosts = flake(&case);
    let (uid, gid) = ids(&case.root);
    case.root.write(
        "etc/passwd",
        &format!("..:x:{uid}:{gid}::/people/sample:/bin/sh\n"),
    );
    let applied = case.apply(&[&hosts]);
    assert_eq!(applied.status.code(), Some(3), "{}", story(&applied));
    assert!(
        fakehost::err(&applied).contains("E_PATH_ESCAPE"),
        "{}",
        story(&applied)
    );
    assert!(!case.root.exists("etc/combined"));
    assert!(!case.has_lock());
}

/// H1: a uid with no passwd entry names the uid and `--no-home`, the way on.
#[test]
fn h1_missing_uid_names_the_uid_and_no_home() {
    let case = Case::new("h1-missing-uid-hint", Machine::debian());
    let hosts = flake(&case);
    case.root.write("etc/passwd", "root:x:0:0::/root:/bin/sh\n");
    let applied = case.apply(&[&hosts]);
    let (uid, _) = ids(&case.root);
    let error = fakehost::err(&applied);
    assert_eq!(applied.status.code(), Some(3), "{}", story(&applied));
    assert!(
        error.contains("E_CONFIG") && error.contains(&uid.to_string()),
        "{error}"
    );
    assert!(error.contains("--no-home"), "{error}");
    let skipped = case.apply(&[&hosts, "--no-home"]);
    assert_eq!(skipped.status.code(), Some(0), "{}", story(&skipped));
    assert_eq!(case.root.read("etc/combined"), "host\n");
}

/// H2: a directory `source` of the home expands under the host directory's walk.
#[test]
fn h2_a_directory_source_expands_under_the_walk() {
    let case = Case::new("h2-directory-source", Machine::debian());
    let hosts = flake(&case);
    let applied = case.apply(&[&hosts]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    assert_eq!(
        case.root.read("people/sample/.config/nvim/init.vim"),
        "set number\n"
    );
    assert_eq!(
        case.root.read("people/sample/.config/nvim/lua/plugins.lua"),
        "return {}\n"
    );
}

/// H2: a symbolic link below a directory `source` is `E_PATH_ESCAPE`, and the whole root is
/// byte for byte what it was: nothing written, the host included.
#[test]
fn h2_a_link_in_the_home_refuses_with_nothing_written() {
    let case = Case::new("h2-link", Machine::debian());
    let hosts = flake(&case);
    let lua = case
        .root
        .path(&format!("{SAMPLE}/inputs/nvim/lua/plugins.lua"));
    std::fs::remove_file(&lua).unwrap();
    std::os::unix::fs::symlink("../init.vim", &lua).unwrap();
    let before = census(&case.root.dir);
    let applied = case.apply(&[&hosts]);
    assert_eq!(applied.status.code(), Some(3), "{}", story(&applied));
    assert!(
        fakehost::err(&applied).contains("E_PATH_ESCAPE"),
        "{}",
        story(&applied)
    );
    assert_eq!(census(&case.root.dir), before);
}

/// H2: a home manifest that does not parse stops the whole apply before the host's first
/// mutation.
#[test]
fn h2_a_home_that_does_not_parse_writes_nothing() {
    let case = Case::new("h2-unparsed", Machine::debian());
    let hosts = flake(&case);
    case.root.write(&format!("{SAMPLE}/home.toml"), "[home\n");
    let before = census(&case.root.dir);
    let applied = case.apply(&[&hosts]);
    assert_ne!(applied.status.code(), Some(0), "{}", story(&applied));
    assert_eq!(census(&case.root.dir), before);
}

/// H3: every file, directory, state and backup the home part creates belongs to the user (the
/// test's own uid here; the guest row proves real uids), and the combined apply says nothing
/// of W_HOME_SUDO.
#[test]
fn h3_everything_the_home_part_creates_belongs_to_the_user() {
    use std::os::unix::fs::MetadataExt;
    let case = Case::new("h3-owners", Machine::debian());
    let hosts = flake(&case);
    case.root.write("people/sample/note", "mine before lodi\n");
    let applied = case.apply(&[&hosts]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    assert!(
        !fakehost::err(&applied).contains("W_HOME_SUDO"),
        "{}",
        story(&applied)
    );
    let (uid, gid) = ids(&case.root);
    let home = census(&case.root.path("people/sample"));
    assert!(
        home.contains_key(".local/share/lodi/home-scope/state.json"),
        "{home:?}"
    );
    assert!(
        home.keys()
            .any(|path| path.starts_with(".local/share/lodi/home-scope/backups/")),
        "{home:?}"
    );
    for rel in home.keys() {
        let meta = std::fs::symlink_metadata(case.root.path("people/sample").join(rel)).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (uid, gid), "{rel}");
    }
}

/// H5: a home the user cannot write (`E_STORE_PERM`) after the host changed leaves the host
/// applied and locked, says so, exits with the home's status, and a re-run converges.
#[test]
fn h5_store_permission_after_the_host_leaves_the_host_applied() {
    let case = Case::new("h5-store-perm", Machine::debian());
    let hosts = flake(&case);
    case.root.chmod("people/sample", 0o555);
    let failed = case.apply(&[&hosts]);
    case.root.chmod("people/sample", 0o755);
    assert_eq!(failed.status.code(), Some(9), "{}", story(&failed));
    assert_eq!(case.root.read("etc/combined"), "host\n");
    assert!(case.has_lock());
    let error = fakehost::err(&failed);
    assert!(
        error.contains("E_STORE_PERM") && error.contains("host applied"),
        "{error}"
    );
    assert!(out(&failed).contains("/etc/combined"), "{}", story(&failed));
    let again = case.apply(&[&hosts]);
    assert_eq!(again.status.code(), Some(0), "{}", story(&again));
    assert!(out(&again).contains("nothing to do"), "{}", story(&again));
    assert_eq!(case.root.read("people/sample/note"), "home\n");
}

/// H5: `--no-home` prints one line, applies the host alone and opens no home path: the home
/// manifest is a FIFO in a directory everyone may write.
#[test]
fn h5_no_home_opens_no_home_path() {
    let case = Case::new("h5-no-home", Machine::debian());
    let hosts = flake(&case);
    make_fifo(&case.root.path(&format!("{SAMPLE}/home.toml")));
    case.root.chmod("hosts/box/home", 0o777);
    for verb in ["plan", "apply"] {
        let output = case.verb(verb, &[&hosts, "--no-home"]);
        assert_eq!(output.status.code(), Some(0), "{}", story(&output));
        let said = out(&output)
            .lines()
            .filter(|l| l.contains("--no-home"))
            .count();
        assert_eq!(said, 1, "{}", story(&output));
    }
    assert_eq!(case.root.read("etc/combined"), "host\n");
    assert!(!case.root.exists("people/sample/note"));
}

/// H6: a combined apply writes nothing into the host directory: no lock is due for a home
/// without tools, and root never writes there.
#[test]
fn h6_census_a_combined_apply_leaves_the_host_directory_alone() {
    let case = Case::new("h6-host-dir-census", Machine::debian());
    let hosts = flake(&case);
    let before = census(&case.root.path("hosts"));
    let applied = case.apply(&[&hosts]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    assert_eq!(census(&case.root.path("hosts")), before);
}

/// F-19: a bare host apply reads the manifest in place's `home/<login>/home.toml` as a host
/// directory's.
#[test]
fn h1_in_place_home_is_applied_by_a_bare_apply() {
    let case = Case::new("f19-in-place", Machine::debian());
    flake(&case);
    case.set_manifest(&case.root.read("hosts/box/host.toml"));
    case.root.write(
        &home_in("etc/lodi", "sample", "home.toml"),
        &case
            .root
            .read(&format!("{SAMPLE}/home.toml"))
            .replace("home\\n", "in place\\n"),
    );
    copy_tree(
        &case,
        &case.root.path(&format!("{SAMPLE}/inputs")),
        &home_in("etc/lodi", "sample", "inputs"),
    );
    let plan = case.plan();
    assert_eq!(plan.status.code(), Some(0), "{}", story(&plan));
    assert!(out(&plan).contains("home sample"), "{}", story(&plan));
    let applied = case.apply(&[]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    assert_eq!(case.root.read("people/sample/note"), "in place\n");
}

/// H8: the stub a named import writes is the file `lodi home init SOURCE` writes as the user,
/// both detecting programs on the fixed PATH, never the caller's.
#[test]
fn h8_the_import_stub_is_what_home_init_writes() {
    let case = Case::new("h8-same-stub", Machine::debian());
    let (uid, gid) = ids(&case.root);
    case.root.write("etc/hostname", "box\n");
    case.root.write(
        "etc/passwd",
        &format!("sample:x:{uid}:{gid}::/people/sample:/bin/sh\n"),
    );
    std::fs::create_dir_all(case.root.path("people/sample")).unwrap();
    std::fs::create_dir_all(case.root.path("hosts")).unwrap();
    // Every module's executable on the caller's PATH, and none detected from it.
    let bin = case.root.path("caller-bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in [
        "git", "hx", "nvim", "vim", "tmux", "kitty", "bash", "zsh", "fish", "ssh",
    ] {
        let path = bin.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        case.root.chmod(&format!("caller-bin/{name}"), 0o755);
    }
    let path = format!("{}:{}", bin.display(), case.fake_path());
    let hosts = case.root.path("hosts").display().to_string();
    let imported = case.verb_env("import", &[&hosts], &[("PATH", &path)]);
    assert_eq!(imported.status.code(), Some(0), "{}", story(&imported));
    let stub = case.root.read(&home_in("hosts/box", "sample", "home.toml"));
    assert!(!stub.is_empty());
    let login = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_string();
    let home = case.root.path("people/sample");
    let _spawning = fakehost::spawning();
    let init = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["home", "init", &hosts, "--host", "box"])
        .env("HOME", &home)
        .env("PATH", &path)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("LODI_HOME")
        .output()
        .unwrap();
    if login == "sample" {
        return;
    }
    assert_eq!(init.status.code(), Some(0), "{}", story(&init));
    assert_eq!(
        case.root.read(&home_in("hosts/box", &login, "home.toml")),
        stub
    );
}

// ------------------------------------------- H1: a host with no home is 1.4, byte for byte ---

#[path = "support/compat.rs"]
mod compat;

/// The goldens of `tests/fixtures/host/compat-1.4/` and the version the recording binary
/// reports (its README names the commit).
const COMPAT_1_4: &str = "tests/fixtures/host/compat-1.4";
const COMPAT_RECORDED: &str = "1.4.0";

fn files_manifest(case: &Case) -> String {
    let (uid, gid) = ids(&case.root);
    format!(
        "[host]\nversion = \"1\"\ndistro = \"debian\"\npackages = \"managed\"\n\n\
         [packages]\ncommon = [\"tree\"]\n\n\
         [files.\"/etc/motd\"]\ncontent = \"hello\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n\n\
         [files.\"/etc/app.conf\"]\nsource = \"files/app.conf\"\nmode = \"0600\"\n\
         owner = \"{uid}\"\ngroup = \"{gid}\"\n"
    )
}

fn compat_case(name: &str) -> Case {
    let case = Case::new(
        name,
        Machine::debian().offering(fakehost::Pkg::new("tree").repo(Some("main"))),
    );
    // H1(a): with no home/ the passwd file is never read, so it need not name the invoker.
    case.root.write("etc/passwd", "root:x:0:0::/root:/bin/sh\n");
    case.root.write("etc/app.conf", "the machine's own\n");
    case
}

/// H1: the manifest in place with no `home/` plans, applies and locks as 1.4 did.
#[test]
fn compat_1_4_h1_the_manifest_in_place_without_a_home_is_1_4() {
    let case = compat_case("compat14-in-place");
    case.root.write("etc/lodi/files/app.conf", "declared\n");
    case.set_manifest(&files_manifest(&case));
    let transcript = compat::walk(&case, COMPAT_RECORDED);
    compat::check(
        &repo().join(COMPAT_1_4),
        "in-place",
        &transcript,
        COMPAT_RECORDED,
    );
}

/// H1: a host directory with no `home/` plans, applies and locks as 1.4 did.
#[test]
fn compat_1_4_h1_a_host_directory_without_a_home_is_1_4() {
    let case = compat_case("compat14-directory");
    case.root.write("etc/hostname", "box\n");
    case.root
        .write("hosts/box/host.toml", &files_manifest(&case));
    case.root.write("hosts/box/files/app.conf", "declared\n");
    let hosts = case.root.path("hosts").display().to_string();
    let transcript = compat::walk_with(&case, COMPAT_RECORDED, &[&hosts]);
    compat::check(
        &repo().join(COMPAT_1_4),
        "directory",
        &transcript,
        COMPAT_RECORDED,
    );
}

/// H8(a): an import in place with no home configuration writes 1.4's file set, byte for byte,
/// but for the manifest's text and the report's lines, which si-1 changed (no file is copied from
/// `/etc`, and the typed settings are written), and the record of those settings, which a
/// converged apply writes too and an import writes since #585.
#[test]
fn compat_1_4_h8_an_import_in_place_keeps_the_1_4_file_set() {
    let case = compat_case("compat14-import");
    case.root.write("etc/hostname", "box\n");
    let output = case.verb_by(&compat::binary(), "import", &[]);
    let mut transcript = format!(
        "== import: exit {:?}\n-- stdout\n{}-- stderr\n{}== etc/lodi\n",
        output.status.code(),
        out(&output),
        fakehost::err(&output)
    );
    let lodi = case.root.path("etc/lodi");
    for rel in census(&lodi).into_keys() {
        let path = lodi.join(&rel);
        let meta = std::fs::symlink_metadata(&path).unwrap();
        let mode = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o7777;
        transcript.push_str(&format!("-- {rel} {mode:04o}\n"));
        if meta.is_file() {
            transcript.push_str(&std::fs::read_to_string(&path).unwrap());
        }
    }
    // Paths below the scratch root are written relative to it (the README says why).
    let transcript = compat::normalize(&case, &transcript, COMPAT_RECORDED).replace("<root>/", "");
    compat::check_masked(
        &repo().join(COMPAT_1_4),
        "import",
        &transcript,
        COMPAT_RECORDED,
        file_set,
    );
}

/// An import transcript with the body of its standard output and of `host.toml` left out: the
/// file set, each mode, the record and standard error stay.
fn file_set(transcript: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in transcript.split_inclusive('\n') {
        if line.starts_with("-- ") || line.starts_with("== ") {
            skipping = line == "-- stdout\n" || line.starts_with("-- host.toml ");
            out.push_str(line);
        } else if !skipping {
            out.push_str(line);
        }
    }
    without_basics_record(&out)
}

/// The record with its `basics` block taken out, and the schema that block needs (5) written as
/// the one the record has without it (2): the `[system]` settings an import declares are
/// recorded as a converged apply records them (#585).
fn without_basics_record(text: &str) -> String {
    let Some(start) = text.find(",\n  \"basics\": {\n") else {
        return text.to_string();
    };
    let Some(end) = text[start..].find("\n  }\n}\n").map(|at| start + at + 4) else {
        return text.to_string();
    };
    format!("{}{}", &text[..start], &text[end..])
        .replace("\"version\": 5,", "\"version\": 2,")
        .replace("\"lodi-host-lock/5\"", "\"lodi-host-lock/2\"")
}

/// H8 with H1: a named import whose uid has no passwd entry refuses before it writes, naming
/// `--no-home`; with it the import writes the host alone.
#[test]
fn h8_a_named_import_without_a_login_refuses_or_skips_the_home() {
    let case = Case::new("h8-no-login", Machine::debian());
    case.root.write("etc/hostname", "box\n");
    case.root.write("etc/passwd", "root:x:0:0::/root:/bin/sh\n");
    std::fs::create_dir_all(case.root.path("hosts")).unwrap();
    let hosts = case.root.path("hosts").display().to_string();
    let refused = case.verb("import", &[&hosts]);
    assert_eq!(refused.status.code(), Some(3), "{}", story(&refused));
    assert!(
        fakehost::err(&refused).contains("--no-home"),
        "{}",
        story(&refused)
    );
    assert!(!case.root.exists("hosts/box"));
    let skipped = case.verb("import", &[&hosts, "--no-home"]);
    assert_eq!(skipped.status.code(), Some(0), "{}", story(&skipped));
    assert!(case.root.exists("hosts/box/host.toml"));
    assert!(!case.root.exists("hosts/box/home"));
}

/// The oldest lodi that can use a folder the root `lodi import` writes (rel-1-release-1-5-0,
/// LD-402): lodi 1.4.1 reads a host directory's `host.toml` and `files/` and never its `home/`.
/// The folder 1.5.0 writes is therefore one 1.4.1 applies as 1.5.0 does exactly when its
/// `host.toml` is the file the 1.4 import writes, its home stub declares nothing, and it carries
/// no lock of any kind; then `IMPORT_MINIMUM` stays 1.4.0.
#[test]
fn an_import_folder_is_one_lodi_1_4_1_applies_as_1_5_0_does() {
    use lodi::hostscope::import::emit::IMPORT_MINIMUM;
    assert_eq!(IMPORT_MINIMUM, "1.4.0");
    let case = compat_case("import-minimum-150");
    let (uid, gid) = ids(&case.root);
    case.root.write("etc/hostname", "box\n");
    case.root.write(
        "etc/passwd",
        &format!("sample:x:{uid}:{gid}::/people/sample:/bin/sh\n"),
    );
    std::fs::create_dir_all(case.root.path("people/sample")).unwrap();
    std::fs::create_dir_all(case.root.path("hosts")).unwrap();
    let hosts = case.root.path("hosts").display().to_string();
    // The top-level `lodi import DIR`, under the fake machine's scratch root. Scoped to this one
    // call (#456): `twin.import()` below takes the same guard again on this thread, and a
    // recursive read held past it can deadlock against a writer-preferring RwLock.
    let imported = {
        let _spawning = fakehost::spawning();
        Command::new(env!("CARGO_BIN_EXE_lodi"))
            .args(["import", &hosts, "--root"])
            .arg(&case.root.dir)
            .env("PATH", case.fake_path())
            .env("HOME", case.root.path("home"))
            .env("XDG_CONFIG_HOME", case.root.path("home/config"))
            .env("XDG_DATA_HOME", case.root.path("home/data"))
            .env("LODI_HOME", case.root.path("lodi-home"))
            .env("LODI_HOST_REQUIRE_ROOT", "1")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    assert_eq!(imported.status.code(), Some(0), "{}", story(&imported));
    let written: Vec<String> = census(&case.root.path("hosts")).into_keys().collect();
    let stub_path = ["box", "home", "sample", "home.toml"];
    let expected: Vec<String> = (1..=stub_path.len())
        .map(|n| stub_path[..n].join("/"))
        .chain(["box/host.toml".to_string()])
        .collect();
    assert_eq!(
        written, expected,
        "the folder carries a host.toml, a home stub and no lock"
    );
    // The host part: the bytes the in-place import writes, which `compat_1_4_h8` holds to 1.4's.
    let twin = compat_case("import-minimum-150-in-place");
    twin.root.write("etc/hostname", "box\n");
    let in_place = twin.import();
    assert_eq!(in_place.status.code(), Some(0), "{}", story(&in_place));
    let host = case.root.read("hosts/box/host.toml");
    assert_eq!(host, twin.root.read("etc/lodi/host.toml"));
    assert!(
        host.contains(&format!("\nmin_lodi_version = \">={IMPORT_MINIMUM}\"\n")),
        "{host}"
    );
    // The home part: nothing but the table and its format, so a 1.4.1 that skips `home/` leaves
    // nothing undone that 1.5.0 would do.
    let stub = case.root.read(&home_in("hosts/box", "sample", "home.toml"));
    let declared: Vec<&str> = stub
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    assert_eq!(declared, ["[home]", "version = \"1\""], "{stub}");
}
