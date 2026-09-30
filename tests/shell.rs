//! M-0.3 T-2: `lodi shell` and interactive `lodi develop`, run as the real binary.
//!
//! Offline throughout. The upstream release metadata is a synthetic
//! python-build-standalone-shaped fixture built here, and the artifact is the synthetic archive
//! of `tests/support`; both are served by a loopback server the binary reaches through
//! `LODI_FETCH_REWRITE`, with its own `LODI_HOME`, `XDG_CONFIG_HOME` and `HOME` under the target
//! directory. Every child environment is built from a cleared environment, so an outer Lodi
//! activation of the developer cannot leak in, and `SHELL=/bin/sh` makes the interactive child
//! a shell that reads the script on its standard input.
//!
//! Acceptance rows A06-A11 of `docs/milestones/m-0.3/ACCEPTANCE.md` are checked here, except the
//! container half of A09 (real rootless Podman, `tests/spike_container.rs`) and the real-network
//! half of A08 (`scripts/m03-local.sh --case shell`).

mod support;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use support::*;

const LODI: &str = env!("CARGO_BIN_EXE_lodi");
/// The one python the synthetic upstream offers, and the tag it was released under.
const PYTHON: &str = "3.12.14";
const TAG: &str = "20260901";

/// The upstream files `lodi shell python` needs: the two newest releases of the recipe's
/// repository, the checksum file of the release it picks, and the artifact itself. The digests
/// are the real digests of the synthetic archive, so the binary verifies them as it would a
/// real download.
fn upstream() -> (BTreeMap<String, Vec<u8>>, String) {
    let bytes = python_archive(PYTHON);
    let hex = lodi::util::sha256_hex(&bytes);
    let name = format!("cpython-{PYTHON}+{TAG}-x86_64-unknown-linux-gnu-install_only.tar.gz");
    let url = format!(
        "https://github.com/astral-sh/python-build-standalone/releases/download/{TAG}/{name}"
    );
    let releases = serde_json::json!([{
        "tag_name": TAG,
        "name": TAG,
        "draft": false,
        "prerelease": false,
        "assets": [
            { "name": name, "size": bytes.len(), "digest": format!("sha256:{hex}"),
              "browser_download_url": url },
            { "name": "SHA256SUMS", "size": 64,
              "browser_download_url": format!(
                  "https://github.com/astral-sh/python-build-standalone/releases/download/{TAG}/SHA256SUMS") },
        ],
    }]);
    let mut files = BTreeMap::new();
    files.insert(
        "https://api.github.com/repos/astral-sh/python-build-standalone/releases?per_page=2"
            .to_string(),
        serde_json::to_vec(&releases).unwrap(),
    );
    files.insert(
        format!(
            "https://github.com/astral-sh/python-build-standalone/releases/download/{TAG}/SHA256SUMS"
        ),
        format!("{hex}  {name}\n").into_bytes(),
    );
    files.insert(url, bytes);
    (files, hex)
}

/// One isolated user: a store, a config directory, a home and the upstream server above.
struct User {
    base: PathBuf,
    server: Server,
}

impl User {
    fn new(name: &str) -> User {
        let base = scratch(name);
        for d in ["home", "config", "lodi-home"] {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        User {
            base,
            server: Server::start(upstream().0),
        }
    }

    fn lodi_home(&self) -> PathBuf {
        self.base.join("lodi-home")
    }

    fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut c = Command::new(LODI);
        c.args(args)
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.base.join("home"))
            .env("XDG_CONFIG_HOME", self.base.join("config"))
            .env("LODI_HOME", self.lodi_home())
            .env("LODI_FETCH_REWRITE", self.server.rewrite())
            // The interactive child is a plain `sh` reading the script on its standard input.
            .env("SHELL", "/bin/sh");
        c
    }

    /// Run `args` with `script` on the shell's standard input.
    fn shell(&self, dir: &Path, args: &[&str], script: &str) -> Output {
        self.shell_with(dir, args, script, &[])
    }

    fn shell_with(&self, dir: &Path, args: &[&str], script: &str, env: &[(&str, &str)]) -> Output {
        let mut c = self.command(dir, args);
        for (k, v) in env {
            c.env(k, v);
        }
        let mut child = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// Start an interactive shell and leave its standard input open.
    fn open(&self, dir: &Path, args: &[&str]) -> Child {
        self.command(dir, args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.command(dir, args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn sessions(&self) -> Vec<String> {
        session_listing(&self.lodi_home().join("gcroots/sessions"))
    }

    fn shell_locks(&self) -> Vec<String> {
        listing(&self.lodi_home().join("cache/shell"))
    }

    fn store_entries(&self) -> Vec<String> {
        listing(&self.lodi_home().join("store"))
            .into_iter()
            .filter(|n| !n.contains('/') && (n.starts_with("art-") || n.starts_with("env-")))
            .collect()
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Wait for something a child process does, under the suite's shared ceiling
/// (`tests/support/wait.rs`).
fn wait_for(what: &str, cond: impl Fn() -> bool) {
    wait::until(what, cond);
}

// A06, A07, A08, A11: one tool into one shell, nothing on the parent's PATH afterwards, the
// live session named by its root, the second entry offline, and the shell's status passed back.
#[test]
fn shell_python_enters_once_online_and_again_with_the_network_refused() {
    let user = User::new("shell-python");
    let dir = user.base.join("elsewhere");
    fs::create_dir_all(&dir).unwrap();

    // (a) the shell has the resolved python on its PATH, and its exit status is passed through.
    let o = user.shell(&dir, &["shell", "python"], "python3 --version\nexit 7\n");
    assert_eq!(o.status.code(), Some(7), "{}", err(&o));
    assert_eq!(
        out(&o),
        format!("Python {PYTHON} (synthetic)\n"),
        "{}",
        err(&o)
    );

    // The tool lives in the store and nowhere else: it is not on the caller's PATH afterwards.
    assert!(
        user.store_entries().iter().any(|n| n.starts_with("art-")),
        "{:?}",
        user.store_entries()
    );
    let parent = Command::new("/bin/sh")
        .arg("-c")
        .arg("command -v python3 || true")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .output()
        .unwrap();
    assert!(
        !out(&parent).contains("lodi-home"),
        "the tool leaked onto the caller's PATH: {}",
        out(&parent)
    );
    // Nothing was written outside $LODI_HOME.
    assert!(listing(&dir).is_empty(), "{:?}", listing(&dir));

    // The resolution was cached under $LODI_HOME by its request, and nowhere else (LD-49).
    let locks = user.shell_locks();
    assert_eq!(locks.len(), 1, "{locks:?}");
    assert!(
        locks[0].len() == 32 + ".lock".len() && locks[0].ends_with(".lock"),
        "{locks:?}"
    );

    // (c) a second identical shell with the network refused still enters, and asks for nothing.
    // The rewrite target is a port nothing listens on, kept closed until the shell has exited.
    user.server.clear();
    let closed = ClosedPort::bind();
    let o = user.shell_with(
        &dir,
        &["shell", "python"],
        "python3 --version\n",
        &[("LODI_FETCH_REWRITE", &closed.rewrite())],
    );
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(out(&o), format!("Python {PYTHON} (synthetic)\n"));
    assert!(
        user.server.requests().is_empty(),
        "the second shell asked upstream for {:?}",
        user.server.requests()
    );
    assert_eq!(user.shell_locks().len(), 1, "the cached lock was reused");

    // A signal in the shell becomes 128 + n, as for any child.
    let o = user.shell(&dir, &["shell", "python"], "kill -TERM $$\n");
    assert_eq!(o.status.code(), Some(128 + 15), "{}", err(&o));
}

// A07: while the shell is open the session root names the entries, every one of them exists,
// and the store lock is not held for the life of the shell — a second shell enters meanwhile.
#[test]
fn a_live_shell_is_rooted_and_does_not_hold_the_store_lock() {
    let user = User::new("shell-session");
    let dir = user.base.join("elsewhere");
    fs::create_dir_all(&dir).unwrap();

    let mut child = user.open(&dir, &["shell", "python"]);
    wait_for("the session root", || !user.sessions().is_empty());
    let sessions = user.sessions();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    assert_eq!(sessions[0], child.id().to_string());

    let record: serde_json::Value = serde_json::from_slice(
        &fs::read(user.lodi_home().join("gcroots/sessions").join(&sessions[0])).unwrap(),
    )
    .unwrap();
    assert_eq!(record["pid"].as_u64(), Some(u64::from(child.id())));
    let activation = &record["activation"];
    assert!(
        activation["id"].as_str().unwrap().starts_with("sha256:"),
        "{record}"
    );
    assert_eq!(activation["runtime"].as_str(), Some("host"), "{record}");
    assert_eq!(activation["profile"].as_str(), Some("default"), "{record}");
    let entries = record["entries"].as_array().unwrap();
    assert!(!entries.is_empty(), "{record}");
    for entry in entries {
        // Every entry the record names must be a directory that is there now: this is what a
        // collector reads to know what it may not remove (ADR-016, D7).
        let path = user.lodi_home().join("store").join(entry.as_str().unwrap());
        assert!(
            fs::metadata(&path).unwrap().is_dir(),
            "{} is named but absent",
            path.display()
        );
    }

    // The shared store lock was released before the shell started: another entry proceeds.
    let o = user.shell(&dir, &["shell", "python"], "exit 0\n");
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(user.sessions(), sessions, "only the open shell is rooted");

    child.stdin.take().unwrap().write_all(b"exit 4\n").unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(4), "{}", err(&o));
    wait_for("the session root to go", || user.sessions().is_empty());
}

// A10: a directory with a manifest in it is untouched — `lodi shell` neither reads nor writes it.
#[test]
fn shell_writes_no_file_in_a_directory_that_has_a_manifest() {
    let user = User::new("shell-cwd");
    let project = user.base.join("project");
    fs::create_dir_all(&project).unwrap();
    // A manifest `lodi shell` must not read: it asks for a tool the upstream does not have, so
    // reading it would fail the resolution instead of resolving the argument.
    fs::write(
        project.join("lodi.toml"),
        "[project]\nname = \"other\"\n\n[tools]\nnodejs = \"22\"\n",
    )
    .unwrap();
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&project)
        .status()
        .is_ok_and(|s| s.success());
    let status = |()| {
        Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&project)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    let before = listing(&project);
    let before_status = status(());

    let o = user.shell(&project, &["shell", "python"], "python3 --version\n");
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(out(&o), format!("Python {PYTHON} (synthetic)\n"));

    assert_eq!(
        listing(&project),
        before,
        "a file appeared in the directory"
    );
    if git {
        assert_eq!(status(()), before_status, "the working tree changed");
    }
    assert!(!project.join("lodi.lock").exists());
    // What it did write is under $LODI_HOME, named by the request and not by the directory.
    assert_eq!(user.shell_locks().len(), 1);
}

// A10: `lodi develop` with no `-- COMMAND` is the project's own environment with a shell in it.
#[test]
fn develop_with_no_command_enters_an_interactive_shell_and_returns_its_status() {
    let user = User::new("develop-interactive");
    let tools = vec![Tool::python(PYTHON)];
    let project = user.base.join("project");
    // The project's lock is fresh and pins the same synthetic artifact the server holds.
    write_project(&project, &manifest(&tools, ""), &tools);
    let server = Server::for_tools(&tools);
    let rewrite = server.rewrite();
    // A manifest with tools and no tasks is trusted before its environment is entered (LD-359).
    let o = user.run(&project, &["trust"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));

    let o = user.shell_with(
        &project,
        &["develop"],
        "python3 --version\nprintf '%s\\n' \"${LODI_MODE}\"\nexit 5\n",
        &[("LODI_FETCH_REWRITE", &rewrite)],
    );
    assert_eq!(o.status.code(), Some(5), "{}", err(&o));
    assert_eq!(
        out(&o),
        format!("Python {PYTHON} (synthetic)\nhost\n"),
        "{}",
        err(&o)
    );
    // Nothing new in the project: `develop` reads the lock it was given and writes no file.
    assert!(!project.join("lodi.lock.tmp").exists());
    assert!(user.sessions().is_empty());
}

// M-Arch-Base T-6: `--base` resolves to the distribution the argument named, for each of the
// three this build supports, and the request's identity is the base it will actually resolve.
// Until T-6 the container request discarded the parsed distro and always said Debian, so
// `--base ubuntu:noble` asked for a Debian base at release `noble`.
#[test]
fn a_base_request_carries_the_distribution_the_argument_named() {
    let mut hashes = Vec::new();
    for (base, distro, release) in [
        ("arch:rolling", lodi::manifest::Distro::Arch, "rolling"),
        (
            "debian:bookworm",
            lodi::manifest::Distro::Debian,
            "bookworm",
        ),
        ("ubuntu:noble", lodi::manifest::Distro::Ubuntu, "noble"),
    ] {
        let request = lodi::shell::Request {
            base: Some(base.to_string()),
            items: vec!["make".to_string()],
        };
        let resolved = lodi::shell::resolve_request(&request).expect(base);
        let container = resolved
            .manifest
            .container
            .as_ref()
            .unwrap_or_else(|| panic!("{base} is not container mode"));
        assert_eq!(container.distro, distro, "{base}");
        assert_eq!(container.release, release, "{base}");
        assert_eq!(resolved.manifest.packages, ["make"], "{base}");
        assert!(resolved.manifest.tools.is_empty(), "{base}");
        // The canonical request names the same base, so the cache key of one base is never the
        // cache key of another.
        assert!(
            resolved
                .request
                .contains(&format!("\"distro\": \"{}\"", container.distro.name())),
            "{base}: {}",
            resolved.request
        );
        hashes.push(resolved.h32());
    }
    hashes.sort();
    hashes.dedup();
    assert_eq!(hashes.len(), 3, "two bases share one cache key");

    // The alias each base accepts resolves to the same canonical release, so `--base arch` and
    // `--base arch:rolling` are one request.
    for (alias, canonical) in [
        ("arch", "arch:rolling"),
        ("debian", "debian:bookworm"),
        ("debian:12", "debian:bookworm"),
        ("ubuntu", "ubuntu:noble"),
        ("ubuntu:24.04", "ubuntu:noble"),
    ] {
        let of = |base: &str| {
            lodi::shell::resolve_request(&lodi::shell::Request {
                base: Some(base.to_string()),
                items: vec!["make".to_string()],
            })
            .expect(base)
            .h32()
        };
        assert_eq!(of(alias), of(canonical), "{alias}");
    }

    // A release outside the supported set of the distribution asked for is refused here, before
    // anything is resolved, and the message names that distribution's set.
    for base in [
        "arch:2026.09.01",
        "arch:latest",
        "debian:trixie",
        "fedora:43",
    ] {
        let err = lodi::shell::resolve_request(&lodi::shell::Request {
            base: Some(base.to_string()),
            items: vec!["make".to_string()],
        })
        .expect_err(base);
        assert_eq!(err.code, "E_UNSUPPORTED", "{base}: {}", err.message);
    }
}

// A11: every documented exit status of `lodi shell`, produced here.
#[test]
fn every_documented_exit_status_is_produced() {
    let user = User::new("shell-statuses");
    let dir = user.base.join("elsewhere");
    fs::create_dir_all(&dir).unwrap();

    // 2 — no tool named, and a flag without its value. Refused before anything is resolved.
    for args in [
        &["shell"][..],
        &["shell", "--base"],
        &["shell", "--base", "debian:bookworm"],
        &["shell", "python", "--", "true"],
    ] {
        let o = user.run(&dir, args);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", err(&o));
        assert!(err(&o).contains("unsupported"), "{args:?}");
    }

    // 4 — an unknown tool is E_NO_RECIPE and names the tools that do exist; no fallback.
    // (The name used to be `go`, which M-0.4 T-4 shipped as a recipe.)
    let o = user.run(&dir, &["shell", "notatool"]);
    assert_eq!(o.status.code(), Some(4), "{}", err(&o));
    assert!(err(&o).contains("E_NO_RECIPE"), "{}", err(&o));
    // Every built-in tool, in order, not two that happened to sort next to each other.
    let names = lodi::catalogue::builtin_tool_names().join(", ");
    assert!(
        err(&o).contains(&format!("this build resolves {names};")),
        "{}",
        err(&o)
    );

    // 4 — no version satisfies the constraint is E_NO_MATCH, listing what is available.
    let o = user.run(&dir, &["shell", "python@99.1"]);
    assert_eq!(o.status.code(), Some(4), "{}", err(&o));
    assert!(err(&o).contains("E_NO_MATCH"), "{}", err(&o));
    assert!(err(&o).contains(PYTHON), "{}", err(&o));

    // 7 — --base with no container runtime on PATH is E_NO_RUNTIME; lodi installs no Podman.
    let o = user
        .command(&dir, &["shell", "--base", "debian:bookworm", "make"])
        .env("PATH", "/nonexistent")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(7), "{}", err(&o));
    assert!(err(&o).contains("E_NO_RUNTIME"), "{}", err(&o));

    // 7 — nesting, unchanged from ADR-015: --no-nest refuses to stack, and another runtime is
    // refused outright.
    for env in [
        &[("LODI_ACTIVATION", "sha256:outer"), ("LODI_MODE", "host")][..],
        &[("LODI_MODE", "container")],
    ] {
        let mut c = user.command(&dir, &["shell", "--no-nest", "python"]);
        for (k, v) in env {
            c.env(k, v);
        }
        let o = c.stdin(Stdio::null()).output().unwrap();
        assert_eq!(o.status.code(), Some(7), "{env:?}: {}", err(&o));
        assert!(err(&o).contains("E_NESTED"), "{env:?}: {}", err(&o));
    }

    // 7 — E_SHELL_NOT_FOUND is the status of a machine with no usable shell at all. The binary
    // cannot be put on one here; the code's status is checked where it is decided.
    assert_eq!(lodi::diag::exit_status("E_SHELL_NOT_FOUND"), 7);
    assert_eq!(
        lodi::shell::shell_program(None, Path::new("/nonexistent/sh"))
            .unwrap_err()
            .code,
        "E_SHELL_NOT_FOUND"
    );
}
