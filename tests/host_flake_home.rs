//! H1: only the invoking user's home in the selected host participates in a switch, and the
//! host part runs before it. Every host invocation goes through the fake machine's scratch
//! --root and the lane's LODI_HOST_REQUIRE_ROOT=1 guard; the home is the scratch `HOME`, and
//! nothing inspects the real machine's home.
//!
//! The config is the 2.0 one at the scratch `~/.config/lodi` (#699): `host.toml`, the invoking
//! user's `home.toml` and another login's `other/home.toml`, named by a `config.toml`.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
#[path = "support/wait.rs"]
mod wait;

mod support;

use fakehost::{Case, Machine, err, nothing, story};
use hostroot::ids;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// `lodi switch ARGS --root ROOT`: the host part, then the invoking user's home part, through
/// the fake machine's harness (scratch `HOME`, stub elevators, offline archive).
fn switch(case: &Case, args: &[&str]) -> Output {
    case.verb("switch", args)
}

/// The "may manage" marker `lodi import` leaves once the owner agreed (#705).
fn allow(case: &Case) {
    if !case.root.exists("etc/lodi/may-manage") {
        case.root
            .write("etc/lodi/may-manage", "")
            .chmod("etc/lodi/may-manage", 0o644);
    }
}

/// The passwd and group of a root whose invoking user `sample` is the test's own uid.
fn sample(case: &Case) {
    let (uid, gid) = ids(&case.root);
    case.root.write(
        "etc/passwd",
        &format!("root:x:0:0::/root:/bin/sh\nsample:x:{uid}:{gid}::/home:/bin/sh\n"),
    );
    case.root
        .write("etc/group", &format!("root:x:0:\nsample:x:{gid}:\n"));
    case.root.write("etc/hostname", "box\n");
}

/// A host declaring `/etc/combined` with `body`, owned by the test's own user.
fn combined(case: &Case, distro: &str, body: &str) -> String {
    let (uid, gid) = ids(&case.root);
    format!(
        "[host]\ndistro = \"{distro}\"\n\n[files.\"/etc/combined\"]\n\
         content = \"{body}\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
    )
}

/// A flat config: `host.toml`, and `home.toml` when given.
fn flat(case: &Case, host: &str, home: Option<&str>) {
    case.set_manifest(host);
    if let Some(home) = home {
        case.write_beside("home.toml", home);
    }
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
    sample(&case);
    flat(
        &case,
        &combined(&case, distro, "host"),
        Some("[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"home\"\n"),
    );
    let applied = switch(&case, &[]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    assert_eq!(case.root.read("etc/combined"), "host\n");
    assert_eq!(case.root.read("home/note"), "home");
}

#[test]
fn h2_invalid_lock_refuses_before_host_change() {
    let case = Case::new("h2-lock", Machine::debian());
    sample(&case);
    flat(
        &case,
        &combined(&case, "debian", "host"),
        Some("[home]\nversion = \"1\"\n"),
    );
    case.write_beside("lodi.lock", "broken");
    let applied = switch(&case, &[]);
    assert_ne!(applied.status.code(), Some(0), "{}", story(&applied));
    assert!(!case.root.exists("etc/combined"));
    assert!(!case.has_lock());
}

#[test]
fn h5_home_drift_reports_host_applied_and_home_refused() {
    let case = Case::new("h5-home-drift", Machine::debian());
    sample(&case);
    flat(
        &case,
        &combined(&case, "debian", "first"),
        Some("[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"managed\"\n"),
    );
    let first = switch(&case, &[]);
    assert_eq!(first.status.code(), Some(0), "{}", story(&first));
    case.set_manifest(&combined(&case, "debian", "second"));
    case.root.write("home/note", "edited");
    let failed = switch(&case, &[]);
    assert_ne!(failed.status.code(), Some(0), "{}", story(&failed));
    assert_eq!(case.root.read("etc/combined"), "second\n");
    assert_eq!(case.root.read("home/note"), "edited");
    let error = err(&failed);
    assert!(error.contains("note"), "{error}");
    let plan = switch(&case, &["--dry-run"]);
    assert!(err(&plan).contains("note"), "{}", story(&plan));
    case.root.write("home/note", "managed");
    let again = switch(&case, &[]);
    assert_eq!(again.status.code(), Some(0), "{}", story(&again));
    assert_eq!(case.root.read("etc/combined"), "second\n");
    assert!(nothing(&again), "{}", story(&again));
}

#[test]
fn h1_host_without_home_applies_without_passwd_entry() {
    let case = Case::new("h1-no-home", Machine::debian());
    case.root.write("etc/passwd", "root:x:0:0::/root:/bin/sh\n");
    case.root.write("etc/hostname", "box\n");
    flat(&case, "[host]\ndistro = \"debian\"\n", None);
    let applied = switch(&case, &[]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    assert!(nothing(&applied), "{}", story(&applied));
}

#[test]
fn h1_plan() {
    plans_only_the_invoking_home(Case::new("h1-selected-home", Machine::debian()), "debian");
}

/// H1 on Fedora (ff-1).
#[test]
fn h1_plan_on_fedora() {
    let case = Case::new("h1-selected-home-fedora", Machine::fedora());
    plans_only_the_invoking_home(case, "fedora");
}

fn plans_only_the_invoking_home(case: Case, distro: &str) {
    sample(&case);
    let (_, gid) = ids(&case.root);
    let (uid, _) = ids(&case.root);
    case.root.write(
        "etc/passwd",
        &format!(
            "root:x:0:0::/root:/bin/sh\nsample:x:{uid}:{gid}::/home:/bin/sh\n\
             other:x:{}:{gid}::/people/other:/bin/sh\n",
            uid + 1
        ),
    );
    flat(
        &case,
        &format!("[host]\ndistro = \"{distro}\"\n"),
        Some("[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"only this home\"\n"),
    );
    case.write_beside(
        "other/home.toml",
        "[home]\nversion = \"1\"\n\n[home.file.\"secret\"]\ntext = \"not selected\"\n",
    );
    case.write_beside(
        "config.toml",
        "[hosts.box]\nhost = \"host.toml\"\n\
         homes = { sample = \"home.toml\", other = \"other/home.toml\" }\n",
    );
    let plan = switch(&case, &["--dry-run"]);
    assert_eq!(plan.status.code(), Some(0), "{}", story(&plan));
    assert!(
        err(&plan).contains("note"),
        "H1: the invoking user's entry absent: {}",
        story(&plan)
    );
    assert!(
        !err(&plan).contains("secret"),
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

/// Copy the fixture tree `from` to `to` beside the manifest.
fn copy_beside(case: &Case, from: &Path, to: &str) {
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = format!("{to}/{name}");
        if entry.file_type().unwrap().is_dir() {
            copy_beside(case, &entry.path(), &rel);
        } else {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            case.write_beside(&rel, fill(case, &text));
        }
    }
}

/// The fixture's root: `etc/passwd`, `etc/group`, `etc/hostname`, and its host `box` as a 2.0
/// config: `host.toml`, `sample`'s `home.toml` with its `inputs/`, `other/home.toml`, and a
/// `config.toml` naming them.
fn config_flake(case: &Case) {
    let fixture = repo().join("tests/fixtures/host/flake");
    for (from, to) in [("passwd", "etc/passwd"), ("group", "etc/group")] {
        let text = std::fs::read_to_string(fixture.join(from)).unwrap();
        case.root
            .write(to, &fill(case, &text).replace("/people/sample", "/home"));
    }
    case.root.write("etc/hostname", "box\n");
    allow(case);
    let host = fixture.join("hosts/box");
    case.set_manifest(&fill(
        case,
        &std::fs::read_to_string(host.join("host.toml")).unwrap(),
    ));
    let read = |rel: &str| std::fs::read_to_string(host.join(rel)).unwrap();
    case.write_beside("home.toml", read("home/sample/home.toml"));
    copy_beside(case, &host.join("home/sample/inputs"), "inputs");
    case.write_beside("other/home.toml", read("home/other/home.toml"));
    case.write_beside(
        "config.toml",
        "[hosts.box]\nhost = \"host.toml\"\n\
         homes = { sample = \"home.toml\", other = \"other/home.toml\" }\n",
    );
}

/// Every path below `dir`, never following a link, with its kind, mode, owner, bytes and
/// modification time: two equal censuses are two identical trees. The run logs in the scratch
/// home's state are left out; the config's lock is not: a refusal writes none.
fn census(dir: &Path) -> std::collections::BTreeMap<String, String> {
    use std::os::unix::fs::MetadataExt;
    let mut found = std::collections::BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(at) = pending.pop() {
        for entry in std::fs::read_dir(&at).unwrap().flatten() {
            let path = entry.path();
            let rel = path.strip_prefix(dir).unwrap().display().to_string();
            if rel.contains(".local/state") {
                continue;
            }
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let bytes = if meta.is_file() {
                std::fs::read(&path).unwrap()
            } else {
                Vec::new()
            };
            if meta.is_dir() {
                pending.push(path.clone());
                if rel.ends_with(".config/lodi") || rel.ends_with(".local") {
                    // Its modification time moves when the lock or the state is written.
                    found.insert(
                        rel,
                        format!("{:o} {}:{}", meta.mode(), meta.uid(), meta.gid()),
                    );
                    continue;
                }
            }
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

fn make_fifo(path: &Path) {
    let _ = std::fs::remove_file(path);
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: the path is NUL-terminated and belongs to this test's scratch root.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
}

/// H1: another login's home is never opened. `other/` is writable by everyone and its manifest
/// a FIFO, so a walk into it would be `E_PATH_ESCAPE` or would block; preview and switch both
/// succeed.
#[test]
fn h1_census_no_other_home_is_opened() {
    let case = Case::new("h1-census", Machine::debian());
    config_flake(&case);
    make_fifo(&case.beside("other/home.toml"));
    std::fs::set_permissions(
        case.beside("other"),
        std::os::unix::fs::PermissionsExt::from_mode(0o777),
    )
    .unwrap();
    let plan = switch(&case, &["--dry-run"]);
    assert_eq!(plan.status.code(), Some(0), "{}", story(&plan));
    assert!(!err(&plan).contains("secret"), "{}", story(&plan));
    let applied = switch(&case, &[]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    assert_eq!(case.root.read("home/note"), "home\n");
    assert!(!case.root.exists("home/secret"));
    assert!(!case.root.exists("people/other"));
}

/// H1: a uid with no passwd entry still switches: the home is `HOME` (not under sudo), so the
/// config names it by nothing and the host part runs.
#[test]
fn h1_missing_uid_switches_the_host() {
    let case = Case::new("h1-missing-uid", Machine::debian());
    config_flake(&case);
    case.root.write("etc/passwd", "root:x:0:0::/root:/bin/sh\n");
    let host = switch(&case, &["--host"]);
    assert_eq!(host.status.code(), Some(0), "{}", story(&host));
    assert_eq!(case.root.read("etc/combined"), "host\n");
}

/// H2: a directory `source` of the home expands under the config's walk.
#[test]
fn h2_a_directory_source_expands_under_the_walk() {
    let case = Case::new("h2-directory-source", Machine::debian());
    config_flake(&case);
    let applied = switch(&case, &[]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    assert_eq!(case.root.read("home/config/nvim/init.vim"), "set number\n");
    assert_eq!(
        case.root.read("home/config/nvim/lua/plugins.lua"),
        "return {}\n"
    );
}

/// H2: a symbolic link below a directory `source` is `E_PATH_ESCAPE`, and the whole root is
/// byte for byte what it was: nothing written, the host included.
#[test]
fn h2_a_link_in_the_home_refuses_with_nothing_written() {
    let case = Case::new("h2-link", Machine::debian());
    config_flake(&case);
    let lua = case.beside("inputs/nvim/lua/plugins.lua");
    std::fs::remove_file(&lua).unwrap();
    std::os::unix::fs::symlink("../init.vim", &lua).unwrap();
    let before = census(&case.root.dir);
    let applied = switch(&case, &[]);
    assert_eq!(applied.status.code(), Some(3), "{}", story(&applied));
    assert!(
        err(&applied).contains("E_PATH_ESCAPE"),
        "{}",
        story(&applied)
    );
    assert_eq!(census(&case.root.dir), before);
}

/// H2: a home manifest that does not parse stops the whole switch before the host's first
/// mutation.
#[test]
fn h2_a_home_that_does_not_parse_writes_nothing() {
    let case = Case::new("h2-unparsed", Machine::debian());
    config_flake(&case);
    case.write_beside("home.toml", "[home\n");
    let before = census(&case.root.dir);
    let applied = switch(&case, &[]);
    assert_ne!(applied.status.code(), Some(0), "{}", story(&applied));
    assert_eq!(census(&case.root.dir), before);
}

/// H3: every file, directory, state and backup the home part creates belongs to the user (the
/// test's own uid here; the guest row proves real uids), and the switch says nothing of
/// W_HOME_SUDO.
#[test]
fn h3_everything_the_home_part_creates_belongs_to_the_user() {
    use std::os::unix::fs::MetadataExt;
    let case = Case::new("h3-owners", Machine::debian());
    config_flake(&case);
    case.root.write("home/note", "mine before lodi\n");
    let applied = switch(&case, &[]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    assert!(
        !err(&applied).contains("W_HOME_SUDO"),
        "{}",
        story(&applied)
    );
    assert_eq!(case.root.read("home/note"), "home\n");
    let (uid, gid) = ids(&case.root);
    let mut pending = vec![case.root.path("home")];
    let mut seen = 0;
    while let Some(at) = pending.pop() {
        for entry in std::fs::read_dir(&at).unwrap().flatten() {
            let meta = std::fs::symlink_metadata(entry.path()).unwrap();
            if meta.is_dir() {
                pending.push(entry.path());
            }
            assert_eq!(
                (meta.uid(), meta.gid()),
                (uid, gid),
                "{}",
                entry.path().display()
            );
            seen += 1;
        }
    }
    assert!(seen > 3, "the home part created nothing");
}

/// H5: a home the user cannot write after the host changed leaves the host applied, fails,
/// and a re-run converges.
#[test]
fn h5_store_permission_after_the_host_leaves_the_host_applied() {
    let case = Case::new("h5-store-perm", Machine::debian());
    config_flake(&case);
    // The config lives in the home, so only the folders the home part writes are closed.
    for dir in ["home/config/nvim", "home/config/demo"] {
        std::fs::create_dir_all(case.root.path(dir)).unwrap();
        case.root.chmod(dir, 0o555);
    }
    let failed = switch(&case, &[]);
    for dir in ["home/config/nvim", "home/config/demo"] {
        case.root.chmod(dir, 0o755);
    }
    assert_ne!(failed.status.code(), Some(0), "{}", story(&failed));
    assert_eq!(case.root.read("etc/combined"), "host\n");
    assert!(case.has_lock(), "{}", story(&failed));
    let again = switch(&case, &[]);
    assert_eq!(again.status.code(), Some(0), "{}", story(&again));
    assert_eq!(case.root.read("home/note"), "home\n");
    let settled = switch(&case, &[]);
    assert!(nothing(&settled), "{}", story(&settled));
}

/// H6: a switch writes nothing into the config but its lock: root never writes there.
#[test]
fn h6_census_a_switch_leaves_the_config_alone() {
    let case = Case::new("h6-host-dir-census", Machine::debian());
    config_flake(&case);
    let before = census(&case.config());
    let applied = switch(&case, &[]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    let mut after = census(&case.config());
    after
        .remove("lodi.lock")
        .expect("the first switch locks the config");
    assert_eq!(after, before);
}

/// H8: an import of the home does not open a FIFO in it.
#[test]
fn h8_import_does_not_open_a_home_fifo() {
    let case = Case::new("h8-no-home-open", Machine::debian());
    sample(&case);
    make_fifo(&case.root.path("home/.gitconfig"));
    // Held only around `spawn`, not the wait loop below: this child deliberately blocks on the
    // FIFO for up to `wait::ceiling()`, and a guard held that long would stall every other
    // thread's `Case::new` in this binary.
    let spawning = fakehost::spawning();
    let mut child = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["import", "--home", "--root"])
        .arg(&case.root.dir)
        .current_dir(case.base())
        .env("PATH", case.elevating_path())
        .env("HOME", case.root.path("home"))
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("LODI_REPO")
        .env_remove("SUDO_UID")
        .env_remove("DOAS_USER")
        .env("LODI_HOST_REQUIRE_ROOT", "1")
        .env("LODI_FETCH_REWRITE", case.archive_server.rewrite())
        .env("LODI_FETCH_ATTEMPTS", "1")
        .env("GIT_CEILING_DIRECTORIES", &case.root.dir)
        // `git init` reads the person's ~/.gitconfig; this case is about what lodi opens.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(case.base().join("import.err")).unwrap())
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
            panic!("import blocked on a home FIFO");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let said = std::fs::read_to_string(case.base().join("import.err")).unwrap();
    assert_eq!(status.code(), Some(0), "{said}");
    assert!(case.config().join("home.toml").is_file());
}

/// H2: a home `source` in a folder a group the person shares may write is `E_PATH_ESCAPE` before
/// the host's first change, and nothing is written (1.x's rule for a host's sources).
#[test]
fn h2_group_writable_home_source_refuses_before_host_change() {
    let case = Case::new("h2-writable-source", Machine::debian());
    sample(&case);
    // The person's group lists another member, so its write is no private group's (#684).
    let (_, gid) = ids(&case.root);
    case.root
        .write("etc/group", &format!("root:x:0:\nsample:x:{gid}:other\n"));
    flat(
        &case,
        &combined(&case, "debian", "host"),
        Some("[home]\nversion = \"1\"\n\n[home.file.\"note\"]\nsource = \"inputs/note\"\n"),
    );
    case.write_beside("inputs/note", "untrusted");
    std::fs::set_permissions(
        case.beside("inputs"),
        std::os::unix::fs::PermissionsExt::from_mode(0o775),
    )
    .unwrap();
    allow(&case);
    let before = census(&case.root.dir);
    for args in [&["--dry-run"][..], &[]] {
        let refused = switch(&case, args);
        assert_eq!(refused.status.code(), Some(3), "{}", story(&refused));
        assert!(
            err(&refused).contains("E_PATH_ESCAPE"),
            "{}",
            story(&refused)
        );
        assert!(err(&refused).contains("inputs"), "{}", story(&refused));
        assert_eq!(census(&case.root.dir), before, "{}", story(&refused));
    }
    assert!(!case.root.exists("etc/combined"));
    assert!(!case.root.exists("home/note"));
}

/// H2, the other side: a home `source` folder and file that only the person's own private group
/// may write besides them (Fedora's umask 002, #684) are read, as the config's own folders are.
#[test]
fn h2_private_group_writable_home_source_is_read() {
    let case = Case::new("h2-private-source", Machine::debian());
    sample(&case);
    flat(
        &case,
        &combined(&case, "debian", "host"),
        Some("[home]\nversion = \"1\"\n\n[home.file.\"note\"]\nsource = \"inputs/note\"\n"),
    );
    case.write_beside("inputs/note", "trusted\n");
    for (path, mode) in [("inputs", 0o775), ("inputs/note", 0o664)] {
        std::fs::set_permissions(
            case.beside(path),
            std::os::unix::fs::PermissionsExt::from_mode(mode),
        )
        .unwrap();
    }
    allow(&case);
    let preview = switch(&case, &["--dry-run"]);
    assert_eq!(preview.status.code(), Some(0), "{}", story(&preview));
    let output = switch(&case, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", story(&output));
    assert_eq!(case.root.read("home/note"), "trusted\n");
}

/// H5: `switch --host` applies the host alone and opens no home path: the home `config.toml`
/// names for the person is a FIFO in a folder everyone may write.
#[test]
fn h5_no_home_opens_no_home_path() {
    let case = Case::new("h5-no-home", Machine::debian());
    config_flake(&case);
    case.write_beside(
        "config.toml",
        "[hosts.box]\nhost = \"host.toml\"\nhomes = { sample = \"people/sample/home.toml\" }\n",
    );
    std::fs::create_dir_all(case.beside("people/sample")).unwrap();
    make_fifo(&case.beside("people/sample/home.toml"));
    std::fs::set_permissions(
        case.beside("people"),
        std::os::unix::fs::PermissionsExt::from_mode(0o777),
    )
    .unwrap();
    for args in [&["--host", "--dry-run"][..], &["--host"]] {
        let output = switch(&case, args);
        assert_eq!(output.status.code(), Some(0), "{}", story(&output));
        assert!(!err(&output).contains("people"), "{}", story(&output));
    }
    assert_eq!(case.root.read("etc/combined"), "host\n");
    assert!(!case.root.exists("home/note"));
}
