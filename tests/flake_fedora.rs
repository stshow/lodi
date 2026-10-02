//! ff-1 (LD-436): `lodi import` on Fedora 44, as 2.0 runs it (LD-522): a flat config.
//! `lodi plan` and `lodi apply` went in 2.0 (#699, LD-500), and their rows with them.
//!
//! Every case is the real binary against a scratch `--root` with the fake machine of
//! `tests/support/fakehost.rs` behind it, whose dnf5 and rpm answer as a Fedora 44 guest printed,
//! run from a scratch current directory with a decoy `HOME` and `LODI_HOST_REQUIRE_ROOT=1`. A git
//! URL is served from a repository the test commits, by `scripts/git-http-loopback.py` on
//! loopback through `LODI_FETCH_REWRITE`; nothing reaches the network or `/`.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/fixture.rs"]
mod fixture;
#[path = "support/git_loopback.rs"]
mod git_loopback;
#[path = "support/hostroot.rs"]
mod hostroot;
#[path = "support/pinverbs.rs"]
mod pinverbs;
mod support;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use fakehost::{Case, Machine, Pkg};
use hostroot::ids;
use pinverbs::{census, ok};

/// The child half of every held apply of this binary (`tests/support/hostroot.rs`).
#[test]
fn child_apply() {
    if !hostroot::is_child() {
        return;
    }
    hostroot::child_apply();
}

/// A package at a Fedora 44 build.
fn fc44(name: &str, version: &str) -> Pkg {
    let mut pkg = Pkg::new(name);
    pkg.version = format!("0:{version}.fc44");
    pkg
}

/// A Fedora 44 machine called `box` a person chose `htop` and `tree` on, whose passwd names
/// `sample`, the test's own uid, so the drop of a home's apply is to who it already is.
fn fedora(name: &str) -> Case {
    let machine = Machine::fedora()
        .with(fc44("htop", "3.4.1-3").depends(&["glibc"]))
        .with(fc44("tree", "2.2.1-4").depends(&["glibc"]));
    let case = Case::new(name, machine);
    case.root.write("etc/hostname", "box\n");
    fs::create_dir_all(case.root.path("people/sample")).unwrap();
    case
}

/// A directory of this case's own, mode 0755, emptied first.
fn scratch(case: &Case, name: &str) -> PathBuf {
    let dir = case.base().join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// [`top`] as root: in a user namespace of its own whose root is the test's user, as `sudo`
/// runs it, so the import writes the machine's record that only root writes (LD-379).
// check-host-safety: refusal — one place, and it appends this scratch root as --root.
fn as_root(case: &Case, verb: &str, extra: &[&str]) -> Output {
    let mut command = Command::new("unshare");
    command
        .args(["--user", "--map-root-user"])
        .arg(env!("CARGO_BIN_EXE_lodi"));
    run(case, command, verb, extra, &[])
}

fn run(
    case: &Case,
    mut command: Command,
    verb: &str,
    extra: &[&str],
    envs: &[(&str, &str)],
) -> Output {
    let cwd = scratch(case, "cwd");
    let decoy = case.base().join("decoy-home");
    fs::create_dir_all(decoy.join("lodi/box")).unwrap();
    fs::write(decoy.join("lodi/box/host.toml"), "not a manifest [\n").unwrap();
    let (uid, gid) = ids(&case.root);
    let _spawning = fakehost::spawning();
    command
        .arg(verb)
        .args(extra)
        .arg("--root")
        .arg(&case.root.dir)
        .current_dir(&cwd)
        .env("PATH", case.fake_path())
        .env("HOME", &decoy)
        .env("XDG_CONFIG_HOME", decoy.join(".config"))
        .env("XDG_DATA_HOME", decoy.join(".local/share"))
        .env("LODI_HOME", decoy.join("lodi-home"))
        .env("SUDO_UID", uid.to_string())
        .env("SUDO_GID", gid.to_string())
        .env_remove("LODI_REPO")
        .env("LODI_HOST_REQUIRE_ROOT", "1")
        // The import locks what it can: on the fake's offline archive, never the network.
        .env("LODI_FETCH_REWRITE", case.archive_server.rewrite())
        .env("LODI_FETCH_ATTEMPTS", "1")
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("lodi runs")
}

/// `~/lodi` of the case: a directory the test's user owns, where `import` writes `box/`.
fn repository(case: &Case) -> PathBuf {
    let repo = case.root.path("people/sample/lodi");
    fs::create_dir_all(&repo).unwrap();
    repo
}

/// The names a `[packages.fedora] add` list holds.
fn declared(manifest: &str) -> Vec<String> {
    let table = manifest
        .split("\n[packages.fedora]\n")
        .nth(1)
        .unwrap_or_else(|| panic!("no [packages.fedora]:\n{manifest}"));
    table
        .lines()
        .skip_while(|line| *line != "add = [")
        .skip(1)
        .take_while(|line| *line != "]")
        .map(|line| {
            line.trim()
                .trim_end_matches(',')
                .trim_matches('"')
                .to_string()
        })
        .collect()
}

/// A1: `lodi import ~/lodi` as root on Fedora writes a flat config in `~/lodi` with
/// `[packages.fedora]` and the home of `sample`, every file and directory owned by the user, and
/// changes no package.
#[test]
fn import_writes_the_fedora_host_folder_owned_by_the_user() {
    let case = fedora("ff-import");
    let repo = repository(&case);
    let owner = fs::metadata(&repo).unwrap().uid();
    let before = case.installed();
    let imported = as_root(&case, "import", &[&repo.display().to_string(), "--yes"]);
    ok(&imported);
    let manifest = fs::read_to_string(repo.join("host.toml")).unwrap();
    assert!(manifest.contains("distro = \"fedora\""), "{manifest}");
    assert_eq!(
        declared(&manifest),
        ["bash", "dnf5", "htop", "kernel", "tree"],
        "{manifest}"
    );
    assert!(!manifest.contains("\n[packages.debian]"), "{manifest}");
    assert!(repo.join("home.toml").is_file());
    assert!(
        !repo.join(".git").exists(),
        "a folder that existed is not git-inited"
    );
    for (path, _) in census(&repo) {
        let meta = fs::symlink_metadata(&path).unwrap();
        assert_eq!(meta.uid(), owner, "{}", path.display());
    }
    assert_eq!(case.installed(), before, "an import changed the machine");
}
