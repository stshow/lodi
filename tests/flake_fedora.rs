//! ff-1 (LD-436): the repository loop of `lodi import`, `plan` and `apply` on Fedora 44.
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
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use fakehost::{Case, Machine, Pkg, out, story};
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

/// `lodi VERB EXTRA… --root <the case's root>` from a scratch directory, as `sudo` runs it for
/// the test's own user: `SUDO_UID` and `SUDO_GID` name that user, `HOME` is a decoy holding a
/// broken repository of its own, and there is no terminal.
// check-host-safety: refusal — one place, and it appends this scratch root as --root.
fn top(case: &Case, verb: &str, extra: &[&str], envs: &[(&str, &str)]) -> Output {
    run(
        case,
        Command::new(env!("CARGO_BIN_EXE_lodi")),
        verb,
        extra,
        envs,
    )
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

/// Take `name`'s line out of the file, as the owner did.
fn delete_line(path: &Path, name: &str) {
    let before = fs::read_to_string(path).unwrap();
    let quoted = format!("\"{name}\"");
    let after: String = before
        .lines()
        .filter(|line| line.trim().trim_end_matches(',') != quoted)
        .map(|line| format!("{line}\n"))
        .collect();
    assert_ne!(before, after, "no line for {name}:\n{before}");
    fs::write(path, after).unwrap();
}

/// A1: `lodi import ~/lodi` as root on Fedora writes `~/lodi/box/` with `[packages.fedora]` and the
/// home stub of `sample`, every file and directory owned by the user, and changes no package.
#[test]
fn import_writes_the_fedora_host_folder_owned_by_the_user() {
    let case = fedora("ff-import");
    let repo = repository(&case);
    let owner = fs::metadata(&repo).unwrap().uid();
    let before = case.installed();
    let imported = as_root(&case, "import", &[&repo.display().to_string()]);
    ok(&imported);
    let manifest = fs::read_to_string(repo.join("box/host.toml")).unwrap();
    assert!(manifest.contains("distro = \"fedora\""), "{manifest}");
    assert_eq!(
        declared(&manifest),
        ["bash", "dnf5", "htop", "kernel", "tree"],
        "{manifest}"
    );
    assert!(!manifest.contains("\n[packages.debian]"), "{manifest}");
    assert!(repo.join("box").join("home/sample/home.toml").is_file());
    assert!(!repo.join(".git").exists());
    for (path, _) in census(&repo) {
        let meta = fs::symlink_metadata(&path).unwrap();
        assert_eq!(meta.uid(), owner, "{}", path.display());
    }
    assert_eq!(case.installed(), before, "an import changed the machine");
}

/// A2: after the owner deletes `tree` from the imported `host.toml`, `lodi apply ~/lodi` as root
/// removes it with dnf, then applies the home of `sample` as that user — the host first.
#[test]
fn apply_runs_the_host_then_each_home_as_its_user() {
    let case = fedora("ff-apply");
    let repo = repository(&case);
    ok(&as_root(&case, "import", &[&repo.display().to_string()]));
    delete_line(&repo.join("box/host.toml"), "tree");
    fs::write(
        repo.join("box").join("home/sample/home.toml"),
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"home\\n\"\n",
    )
    .unwrap();
    let applied = top(&case, "apply", &[&repo.display().to_string()], &[]);
    ok(&applied);
    let installed = case.installed();
    assert!(!installed.contains("tree"), "{installed:?}");
    assert!(installed.contains("htop"), "{installed:?}");
    let removal = case
        .log()
        .into_iter()
        .find(|line| line.starts_with("dnf5 ") && line.contains(" remove "))
        .unwrap_or_else(|| panic!("dnf5 removed nothing: {:?}", case.log()));
    assert!(removal.ends_with(" tree"), "{removal}");
    let note = case.root.path("people/sample/note");
    assert_eq!(fs::read_to_string(&note).unwrap(), "home\n");
    assert_eq!(fs::metadata(&note).unwrap().uid(), ids(&case.root).0);
    let text = out(&applied);
    let at = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {}", story(&applied)))
    };
    assert!(at("- package tree") < at("home sample"), "{text}");
    let again = top(&case, "apply", &[&repo.display().to_string()], &[]);
    ok(&again);
    assert!(
        out(&again).matches("nothing to do").count() >= 2,
        "{}",
        story(&again)
    );
}

/// A4: `lodi apply github:you/lodi` as root, whose repository holds a Fedora host folder `box/`, is
/// applied as from a clone: `tree`, which the pushed folder leaves out, is removed with dnf, and
/// the machine's `host.lock` records the commit.
#[test]
fn apply_from_a_git_url_works_on_fedora() {
    let case = fedora("ff-url");
    let repo = repository(&case);
    ok(&as_root(&case, "import", &[&repo.display().to_string()]));
    delete_line(&repo.join("box/host.toml"), "tree");
    let commit = git_loopback::commit_all(&repo, "box");
    let server = git_loopback::Served::start(&scratch(&case, "served"));
    server.push(&repo, "lodi");
    let rewrite = server.rewrite("https://github.com/you/lodi.git", "lodi");
    let env = [("LODI_FETCH_REWRITE", rewrite.as_str())];
    let plan = top(&case, "plan", &["github:you/lodi"], &env);
    ok(&plan);
    assert!(out(&plan).contains("- package tree"), "{}", story(&plan));
    assert!(case.installed().contains("tree"));
    let applied = top(&case, "apply", &["github:you/lodi"], &env);
    ok(&applied);
    assert!(!case.installed().contains("tree"), "{}", story(&applied));
    let lock = case.root.read("etc/lodi/host.lock");
    assert!(lock.contains(&commit), "{lock}");
    assert!(server.requests() > 0);
}

/// A5: one repository holds a Debian host folder `deb/` and a Fedora host folder `box/`, each
/// declaring both distributions' tables. On a Fedora machine `box/` plans with
/// `[packages.fedora]`; on a Debian machine `deb/` plans with `[packages.debian]`; the other
/// table is ignored, and nothing is written.
#[test]
fn each_host_folder_plans_with_its_own_distribution() {
    let tables = "[packages.debian]\nadd = [\"jq\"]\n\n[packages.fedora]\nadd = [\"cowsay\"]\n";
    let lay_out = |case: &Case| -> PathBuf {
        let repo = repository(case);
        for (dir, distro) in [("box", "fedora"), ("deb", "debian")] {
            fs::create_dir_all(repo.join(dir)).unwrap();
            fs::write(
                repo.join(dir).join("host.toml"),
                format!("[host]\ndistro = \"{distro}\"\n\n{tables}"),
            )
            .unwrap();
        }
        repo
    };
    let offered = || Pkg::new("jq");
    let fedora_case = Case::new(
        "ff-plan-fedora",
        Machine::fedora()
            .offering(fc44("cowsay", "3.8.4-5"))
            .offering(fc44("jq", "1.8.1-1")),
    );
    let debian_case = Case::new(
        "ff-plan-debian",
        Machine::debian()
            .offering(offered())
            .offering(Pkg::new("cowsay")),
    );
    for (case, host, wanted, ignored) in [
        (&fedora_case, "box", "cowsay", "jq"),
        (&debian_case, "deb", "jq", "cowsay"),
    ] {
        let repo = lay_out(case);
        let before = (census(&case.root.dir), case.machine());
        let plan = top(
            case,
            "plan",
            &[&repo.display().to_string(), "--host", host],
            &[],
        );
        ok(&plan);
        let text = out(&plan);
        assert!(
            text.contains(&format!("+ package {wanted}")),
            "{}",
            story(&plan)
        );
        assert!(
            !text.contains(&format!("package {ignored}")),
            "{}",
            story(&plan)
        );
        assert_eq!((census(&case.root.dir), case.machine()), before);
    }
}

const PV_OLDER: &str = "0:1.10.4-1.fc44.x86_64";
const PV_NEWER: &str = "0:1.10.5-1.fc44.x86_64";

/// `tests/fixtures/host/dnf/pin/FILE`: a real package file's lead and signature (fk-1).
fn pin_fixture(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/host/dnf/pin")
        .join(file)
}

/// [`fedora`] with `pv` installed at its older build, both builds offered as the guest's
/// repositories offered them, and every build's file served by `dnf5 download`.
fn fedora_with_pv(name: &str) -> Case {
    let case = fedora(name);
    let serves: serde_json::Map<String, serde_json::Value> = [
        ("pv", PV_OLDER, "pv-1.10.4-1.fc44.x86_64.rpm"),
        ("pv", PV_NEWER, "pv-1.10.5-1.fc44.x86_64.rpm"),
    ]
    .into_iter()
    .map(|(n, build, file)| {
        let path = pin_fixture(file).display().to_string();
        (format!("{n}-{build}"), serde_json::Value::String(path))
    })
    .collect();
    case.edit(|m| {
        let build = |b: &str| b.trim_end_matches(".x86_64").to_string();
        m["installed"]["pv"] = serde_json::json!(
            {"version": build(PV_OLDER), "explicit": true, "depends": ["glibc"]});
        m["available"]["pv"] = serde_json::json!(
            {"version": build(PV_NEWER), "repo": "updates", "depends": ["glibc"]});
        m["also"]["pv"] = serde_json::json!(
            [{"version": build(PV_OLDER), "repo": "fedora", "depends": ["glibc"]}]);
        m["serves"] = serde_json::Value::Object(serves);
    });
    case
}

/// `git` in the repository as its owner, under an identity with no address.
fn git(case: &Case, repo: &Path, args: &[&str]) {
    let done = Command::new("git")
        .args(["-c", "user.name=ff1", "-c", "user.email=ff1"])
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", case.base().join("git-home"))
        .output()
        .expect("git runs");
    assert!(done.status.success(), "git {args:?}: {}", story(&done));
}

/// The installed build of `pv`, as the lock names it.
fn pv(case: &Case) -> String {
    let m = case.machine();
    format!(
        "{}.x86_64",
        m["installed"]["pv"]["version"].as_str().unwrap_or("none")
    )
}

/// A3: pin `pv` at its installed build and commit; move the pin in `host.toml`, `lodi update`
/// records the newer build in the root `lodi.lock`, commit and apply; then `git revert HEAD` and
/// `lodi apply` put the older build back, from the lock of the earlier commit.
#[test]
fn update_then_revert_restores_the_earlier_builds() {
    let case = fedora_with_pv("ff-revert");
    let repo = repository(&case);
    let dir = repo.display().to_string();
    ok(&as_root(&case, "import", &[&dir]));
    git(&case, &repo, &["init", "-q"]);
    ok(&top(
        &case,
        "host",
        &["pin", "pv", &dir, "--host", "box"],
        &[],
    ));
    git(&case, &repo, &["add", "-A"]);
    git(&case, &repo, &["commit", "-qm", "pin pv"]);
    let lock = |repo: &Path| -> serde_json::Value {
        serde_json::from_slice(&fs::read(repo.join("lodi.lock")).unwrap()).unwrap()
    };
    let earlier = lock(&repo);
    assert_eq!(earlier["hosts"]["box"]["pins"]["pv"]["version"], "1.10.4");

    let manifest = repo.join("box/host.toml");
    let text = fs::read_to_string(&manifest).unwrap();
    assert!(text.contains(&format!("pv = \"{PV_OLDER}\"")), "{text}");
    fs::write(&manifest, text.replace(PV_OLDER, PV_NEWER)).unwrap();
    let updated = top(&case, "update", &[&dir, "--host", "box"], &[]);
    ok(&updated);
    let later = lock(&repo);
    assert_eq!(
        later["hosts"]["box"]["pins"]["pv"]["version"],
        "1.10.5",
        "{}",
        story(&updated)
    );
    assert!(!repo.join("box/host.lock").exists() && !repo.join("box/lodi.lock").exists());
    git(&case, &repo, &["commit", "-qam", "pv newer"]);
    ok(&top(&case, "apply", &[&dir], &[]));
    assert_eq!(pv(&case), PV_NEWER);

    git(&case, &repo, &["revert", "--no-edit", "HEAD"]);
    assert_eq!(lock(&repo), earlier);
    let applied = top(&case, "apply", &[&dir], &[]);
    ok(&applied);
    assert_eq!(pv(&case), PV_OLDER, "{}", story(&applied));
    let again = top(&case, "apply", &[&dir], &[]);
    ok(&again);
    assert!(out(&again).contains("nothing to do"), "{}", story(&again));
}
