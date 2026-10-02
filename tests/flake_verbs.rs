//! The top-level verbs of a repository (M-Flake fv-1, LD-416; DONE D1–D3; M150 V1–V5, V8, V10):
//! `lodi plan`, `apply`, `import` and `update`, the repository LD-447 resolves, and every
//! declared home applied as its own user. `lodi plan` and `lodi apply` went in 2.0 (#699,
//! LD-500), and their rows with them; so did the 1.x host import's hostname directories
//! (LD-522: 2.0's import writes a flat config, `tests/config_import.rs`); the URL refusals stay.
//!
//! Every case is the real binary against a scratch `--root` with the fake machine of
//! `tests/support/fakehost.rs` behind it, run from a scratch current directory, with a decoy
//! `HOME` and XDG and `LODI_HOST_REQUIRE_ROOT=1`. The recorded dated archives of
//! `tests/fixtures/host/pin/` are served on loopback through `LODI_FETCH_REWRITE`; nothing
//! reaches the network or `/`.

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
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use fakehost::{Case, Machine, err, story};
use hostroot::ids;
use pinverbs::*;

/// The child half of every held apply of this binary (`tests/support/hostroot.rs`).
#[test]
fn child_apply() {
    if !hostroot::is_child() {
        return;
    }
    hostroot::child_apply();
}

/// A scratch directory of this binary's own, emptied first.
fn scratch(case: &Case, name: &str) -> PathBuf {
    let dir = case.base().join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// `lodi VERB EXTRA… --root <the case's root>`, run from `cwd` with a decoy `HOME` and XDG that
/// hold a repository of their own, `LODI_REPO` unset unless `envs` sets it, and no terminal.
// check-host-safety: refusal — one place, and it appends this scratch root as --root.
fn top(case: &Case, verb: &str, extra: &[&str], cwd: &Path, envs: &[(&str, &str)]) -> Output {
    top_with(case, verb, extra, cwd, envs, Stdio::null(), Stdio::piped())
}

// check-host-safety: refusal — one place, and it appends this scratch root as --root.
fn top_with(
    case: &Case,
    verb: &str,
    extra: &[&str],
    cwd: &Path,
    envs: &[(&str, &str)],
    stdin: Stdio,
    stderr: Stdio,
) -> Output {
    let decoy = case.base().join("decoy-home");
    for dir in ["lodi/box", ".config/lodi", ".local/share"] {
        fs::create_dir_all(decoy.join(dir)).unwrap();
    }
    fs::write(decoy.join("lodi/box/host.toml"), "not a manifest [\n").unwrap();
    let _spawning = fakehost::spawning();
    Command::new(env!("CARGO_BIN_EXE_lodi"))
        .arg(verb)
        .args(extra)
        .arg("--root")
        .arg(&case.root.dir)
        .current_dir(cwd)
        .env("PATH", case.fake_path())
        .env("HOME", &decoy)
        .env("XDG_CONFIG_HOME", decoy.join(".config"))
        .env("XDG_DATA_HOME", decoy.join(".local/share"))
        .env("LODI_HOME", decoy.join("lodi-home"))
        .env_remove("LODI_REPO")
        .env_remove("SUDO_UID")
        .env_remove("SUDO_GID")
        .env("LODI_HOST_REQUIRE_ROOT", "1")
        .envs(envs.iter().copied())
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(stderr)
        .output()
        .expect("lodi runs")
}

/// A machine called `box` whose root's passwd names `alice` and `bob` (both the test's own
/// uid, so the child's drop is to who it already is), and a directory of hosts at `<root>/hosts`
/// whose `box/` declares the homes of `alice`, `bob` and `carol`, whom passwd does not name.
fn repository(name: &str) -> Case {
    repository_on(name, Machine::debian(), "debian")
}

/// [`repository`] on another distribution's machine, whose `box/` declares it.
fn repository_on(name: &str, machine: Machine, distro: &str) -> Case {
    let case = Case::new(name, machine);
    let (uid, gid) = ids(&case.root);
    case.root.write("etc/hostname", "box\n");
    case.root.write(
        "etc/passwd",
        &format!(
            "root:x:0:0::/root:/bin/sh\nalice:x:{uid}:{gid}::/people/alice:/bin/sh\n\
             bob:x:{uid}:{gid}::/people/bob:/bin/sh\n"
        ),
    );
    case.root.write(
        "hosts/box/host.toml",
        &format!("[host]\ndistro = \"{distro}\"\n"),
    );
    for login in ["alice", "bob", "carol"] {
        case.root.write(
            &format!("hosts/box/home/{login}/home.toml"),
            &format!("[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"{login}\"\n"),
        );
    }
    for login in ["alice", "bob"] {
        fs::create_dir_all(case.root.path(&format!("people/{login}"))).unwrap();
    }
    case
}

#[track_caller]
fn status(output: &Output, code: &str) {
    refused(output, code);
}

// ------------------------------------------------------------------------- V5 safety ---

/// V8, V10: `update` of a URL is refused naming a checkout, before any request.
#[test]
fn update_and_import_refuse_a_url() {
    let case = repository("verbs-url-writers");
    let cwd = scratch(&case, "cwd");
    for verb in ["update", "import"] {
        let refused = top(&case, verb, &["github:owner/hosts"], &cwd, &[]);
        status(&refused, "E_UNSUPPORTED");
        assert!(err(&refused).contains("checkout"), "{}", story(&refused));
    }
}
