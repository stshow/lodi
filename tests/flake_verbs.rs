//! The top-level verbs of a repository (M-Flake fv-1, LD-416; DONE D1–D3; M150 V1–V5, V8, V10):
//! `lodi plan`, `apply`, `import` and `update`, the repository LD-447 resolves, and every
//! declared home applied as its own user.
//!
//! Every case is the real binary against a scratch `--root` with the fake machine of
//! `tests/support/fakehost.rs` behind it, run from a scratch current directory, with a decoy
//! `HOME` and XDG and `LODI_HOST_REQUIRE_ROOT=1`. The recorded dated archives of
//! `tests/fixtures/host/pin/` are served on loopback through `LODI_FETCH_REWRITE`, and a git URL
//! is served from gu-1's recorded exchanges; nothing reaches the network or `/`.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/git_host.rs"]
mod git_host;
#[path = "support/hostroot.rs"]
mod hostroot;
#[path = "support/pinverbs.rs"]
mod pinverbs;
mod support;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::fs;
use std::io::Write as _;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use fakehost::{Case, Machine, err, out, story};
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

fn hosts(case: &Case) -> String {
    case.root.path("hosts").display().to_string()
}

fn note(case: &Case, login: &str) -> Option<String> {
    fs::read_to_string(case.root.path(&format!("people/{login}/note"))).ok()
}

/// Everything below the root and the fake machine's own state, for "nothing changed".
fn everything(case: &Case) -> (BTreeMapCensus, serde_json::Value) {
    (census(&case.root.dir), case.machine())
}

type BTreeMapCensus = std::collections::BTreeMap<PathBuf, (u32, u64, String, i128)>;

#[track_caller]
fn status(output: &Output, code: &str) {
    refused(output, code);
}

// ------------------------------------------------------------------- V1 resolution ---

/// V1, step 1: an explicit path wins over a current directory and `LODI_REPO` that both hold a
/// repository — here broken ones, which would refuse if they were read.
#[test]
fn an_explicit_path_wins_over_the_current_directory_and_lodi_repo() {
    let case = repository("verbs-explicit");
    let broken = scratch(&case, "broken");
    fs::create_dir_all(broken.join("box")).unwrap();
    fs::write(broken.join("box/host.toml"), "not a manifest [\n").unwrap();
    let env = broken.display().to_string();
    let plan = top(
        &case,
        "plan",
        &[&hosts(&case), "--yes"],
        &broken,
        &[("LODI_REPO", env.as_str())],
    );
    ok(&plan);
    assert!(out(&plan).contains("home alice"), "{}", story(&plan));
}

/// V1, step 2: an implicit current directory is asked about on the terminal, and a "no" is
/// `E_DECLINED` with nothing read or written; a "yes" proceeds.
#[test]
fn implicit_current_directory_asks_and_declining_changes_nothing() {
    let case = repository("verbs-ask");
    let here = case.root.path("hosts");
    let before = everything(&case);
    let (declined, shown) = on_terminal(&case, "apply", &here, "n\n");
    // Standard error is the terminal, so the refusal is read there.
    let shown = String::from_utf8_lossy(&shown);
    assert_eq!(declined.status.code(), Some(11), "{shown}");
    assert!(shown.contains("E_DECLINED"), "{shown}");
    assert!(
        shown.contains(&here.display().to_string()),
        "the question named no directory"
    );
    assert_eq!(everything(&case), before, "{}", story(&declined));
    assert_eq!(note(&case, "alice"), None);

    let (confirmed, _) = on_terminal(&case, "apply", &here, "y\n");
    ok(&confirmed);
    assert_eq!(note(&case, "alice").as_deref(), Some("alice"));
}

/// Run `lodi VERB` from `cwd` with standard input and standard error on a pseudo-terminal whose
/// input already holds `answer`. Returns the output and what the terminal was shown.
fn on_terminal(case: &Case, verb: &str, cwd: &Path, answer: &str) -> (Output, Vec<u8>) {
    // SAFETY: plain pseudo-terminal calls on descriptors this test owns.
    let (master, slave) = unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        assert!(master >= 0, "no pseudo-terminal");
        assert_eq!(libc::grantpt(master), 0);
        assert_eq!(libc::unlockpt(master), 0);
        let mut name = [0 as libc::c_char; 128];
        assert_eq!(libc::ptsname_r(master, name.as_mut_ptr(), name.len()), 0);
        let slave = libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        );
        assert!(slave >= 0, "the terminal's other end");
        (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave))
    };
    let mut writer = fs::File::from(master.try_clone().unwrap());
    writer.write_all(answer.as_bytes()).unwrap();
    let output = top_with(
        case,
        verb,
        &[],
        cwd,
        &[],
        Stdio::from(slave.try_clone().unwrap()),
        Stdio::from(slave),
    );
    // The child has exited and every copy of the terminal's other end is closed, so the master
    // reads all the child wrote and then fails. One non-blocking read can run ahead of the
    // terminal's buffer under load; poll bounds each wait should a stray copy stay open.
    let fd = std::os::fd::AsRawFd::as_raw_fd(&master);
    let mut shown = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let mut ready = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: a poll of, then a read from, the master end this function owns.
        let n = unsafe {
            if libc::poll(&mut ready, 1, 10_000) <= 0 {
                break;
            }
            libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len())
        };
        let Ok(n) = usize::try_from(n) else { break };
        if n == 0 {
            break;
        }
        shown.extend_from_slice(&chunk[..n]);
    }
    (output, shown)
}

/// V1: without a terminal the implicit current directory cannot be confirmed: refused, and
/// nothing changes, unless `--yes` answers it.
#[test]
fn non_tty_refuses_the_implicit_current_directory() {
    let case = repository("verbs-non-tty");
    let here = case.root.path("hosts");
    let before = everything(&case);
    for verb in ["plan", "apply", "update", "import"] {
        let refused = top(&case, verb, &[], &here, &[]);
        status(&refused, "E_DECLINED");
    }
    assert_eq!(everything(&case), before);
    let yes = top(&case, "apply", &["--yes"], &here, &[]);
    ok(&yes);
    assert_eq!(note(&case, "bob").as_deref(), Some("bob"));
}

/// V1, step 3: `LODI_REPO` names the repository when the current directory holds none.
#[test]
fn lodi_repo_is_used_when_the_current_directory_holds_no_repository() {
    let case = repository("verbs-env");
    let elsewhere = scratch(&case, "elsewhere");
    let env = hosts(&case);
    let applied = top(
        &case,
        "apply",
        &[],
        &elsewhere,
        &[("LODI_REPO", env.as_str())],
    );
    ok(&applied);
    assert_eq!(note(&case, "alice").as_deref(), Some("alice"));
}

/// V1, step 4: nothing resolves — no argument, no repository here, no `LODI_REPO` — and the
/// refusal is `E_NO_MANIFEST` with its hint, though the decoy `HOME` holds F-20's old default
/// `~/lodi/<hostname>/`. Nothing is read or written, no `git` runs and no lock appears.
#[test]
fn nothing_resolving_gives_the_hint_and_writes_nothing() {
    let case = repository("verbs-nothing");
    let empty = scratch(&case, "empty");
    let shims = scratch(&case, "git-shim");
    let marker = case.base().join("git-ran");
    {
        let _writing = fakehost::writing();
        fs::write(
            shims.join("git"),
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(shims.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!("{}:{}", shims.display(), case.fake_path());
    let before = everything(&case);
    for verb in ["plan", "apply", "import", "update"] {
        let refused = top(&case, verb, &[], &empty, &[("PATH", path.as_str())]);
        status(&refused, "E_NO_MANIFEST");
        assert!(err(&refused).contains("hint"), "{}", story(&refused));
    }
    assert_eq!(everything(&case), before);
    assert!(!marker.exists(), "a git process ran");
    assert!(!case.root.exists("hosts/lodi.lock"));
    assert!(!empty.join("lodi.lock").exists());
}

/// #294: `import --yes` in an empty current directory (hidden entries aside) imports the machine
/// there, owned by the directory's owner, and so does a "yes" on the terminal. A directory that
/// holds something else, and every other verb, resolve as before, with nothing written.
#[test]
fn import_yes_writes_into_an_empty_current_directory() {
    let case = Case::new("verbs-import-here", Machine::debian());
    case.root.write("etc/hostname", "box\n");
    // Below the root, where `--root` trusts a host repository, as V2's is.
    let dir = |name: &str| {
        let dir = case.root.path(name);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        dir
    };
    let here = dir("here");
    fs::create_dir(here.join(".git")).unwrap();
    let owner = fs::metadata(&here).unwrap().uid();
    for verb in ["plan", "apply", "update"] {
        status(&top(&case, verb, &["--yes"], &here, &[]), "E_NO_MANIFEST");
    }
    let imported = top(&case, "import", &["--yes"], &here, &[]);
    ok(&imported);
    assert!(here.join("box/host.toml").is_file(), "{}", story(&imported));
    assert!(here.join("box").join("home/sample/home.toml").is_file());
    for (path, _) in census(&here) {
        assert_eq!(fs::symlink_metadata(&path).unwrap().uid(), owner);
    }

    let asked = dir("asked");
    let (confirmed, shown) = on_terminal(&case, "import", &asked, "y\n");
    ok(&confirmed);
    let shown = String::from_utf8_lossy(&shown);
    assert!(shown.contains(&asked.display().to_string()), "{shown}");
    assert!(asked.join("box/host.toml").is_file(), "{shown}");

    let busy = dir("busy");
    fs::write(busy.join("notes.txt"), "mine\n").unwrap();
    let before = census(&busy);
    let refused = top(&case, "import", &["--yes"], &busy, &[]);
    status(&refused, "E_NO_MANIFEST");
    assert!(err(&refused).contains("hint"), "{}", story(&refused));
    assert_eq!(census(&busy), before);
}

// ------------------------------------------------------------------------- V2 import ---

/// V2: `lodi import REPO` writes `REPO/<hostname>/` (or `--host NAME`) with fh-1's home stub,
/// everything owned by the repository's owner, and no git repository.
#[test]
fn import_writes_the_hostname_directory_owned_by_the_repository_owner() {
    imports_the_hostname_directory(&Case::new("verbs-import", Machine::debian()));
}

/// V2 on Fedora (ff-1): the same directories, the host's packages under `[packages.fedora]`.
#[test]
fn import_writes_the_hostname_directory_on_fedora() {
    let case = Case::new("verbs-import-fedora", Machine::fedora());
    let repo = imports_the_hostname_directory(&case);
    let manifest = fs::read_to_string(repo.join("box/host.toml")).unwrap();
    assert!(manifest.contains("\n[packages.fedora]\n"), "{manifest}");
}

fn imports_the_hostname_directory(case: &Case) -> PathBuf {
    case.root.write("etc/hostname", "box\n");
    let repo = case.root.path("lodi");
    fs::create_dir_all(&repo).unwrap();
    fs::set_permissions(&repo, fs::Permissions::from_mode(0o755)).unwrap();
    let cwd = scratch(case, "cwd");
    let owner = fs::metadata(&repo).unwrap().uid();
    let imported = top(case, "import", &[&repo.display().to_string()], &cwd, &[]);
    ok(&imported);
    let named = top(
        case,
        "import",
        &[&repo.display().to_string(), "--host", "other"],
        &cwd,
        &[],
    );
    ok(&named);
    for host in ["box", "other"] {
        assert!(repo.join(host).join("host.toml").is_file(), "{host}");
        assert!(
            repo.join(host).join("home/sample/home.toml").is_file(),
            "{host}"
        );
    }
    assert!(!repo.join(".git").exists());
    for (path, _) in census(&repo) {
        assert_eq!(
            fs::symlink_metadata(&path).unwrap().uid(),
            owner,
            "{}",
            path.display()
        );
    }
    repo
}

// -------------------------------------------------------------------------- V3 homes ---

/// V3: one apply does the host, then each declared home in login order as its user; a login
/// the root's passwd lacks is skipped with a line of its own; a re-apply has nothing to do.
#[test]
fn apply_reaches_every_declared_home_in_order_and_skips_a_missing_login() {
    reaches_every_declared_home(repository("verbs-homes"));
}

/// V3 on Fedora (ff-1).
#[test]
fn apply_reaches_every_declared_home_on_fedora() {
    reaches_every_declared_home(repository_on(
        "verbs-homes-fedora",
        Machine::fedora(),
        "fedora",
    ));
}

fn reaches_every_declared_home(case: Case) {
    let cwd = scratch(&case, "cwd");
    let applied = top(&case, "apply", &[&hosts(&case)], &cwd, &[]);
    ok(&applied);
    let text = out(&applied);
    let at = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {text}"))
    };
    assert!(at("home alice") < at("home bob"), "{text}");
    assert!(at("home bob") < at("carol"), "{text}");
    assert_eq!(note(&case, "alice").as_deref(), Some("alice"));
    assert_eq!(note(&case, "bob").as_deref(), Some("bob"));
    assert!(!case.root.exists("people/carol"));
    let again = top(&case, "apply", &[&hosts(&case)], &cwd, &[]);
    ok(&again);
    assert!(
        out(&again).matches("nothing to do").count() >= 3,
        "{}",
        story(&again)
    );
}

/// V3: a home that cannot be applied does not stop the next one, and the exit status says so.
#[test]
fn one_home_s_failure_does_not_stop_the_others() {
    let case = repository("verbs-one-fails");
    let cwd = scratch(&case, "cwd");
    let alice = case.root.path("people/alice");
    fs::set_permissions(&alice, fs::Permissions::from_mode(0o555)).unwrap();
    let applied = top(&case, "apply", &[&hosts(&case)], &cwd, &[]);
    fs::set_permissions(&alice, fs::Permissions::from_mode(0o755)).unwrap();
    assert_ne!(applied.status.code(), Some(0), "{}", story(&applied));
    assert!(err(&applied).contains("alice"), "{}", story(&applied));
    assert_eq!(note(&case, "alice"), None);
    assert_eq!(note(&case, "bob").as_deref(), Some("bob"));
}

/// V3, F-21: every input is read and checked before the first change: a home that does not
/// parse stops the apply with the host and every other home untouched.
#[test]
fn every_input_is_preflighted_before_the_first_mutation() {
    let case = repository("verbs-preflight");
    let (uid, gid) = ids(&case.root);
    case.root.write(
        "hosts/box/host.toml",
        &format!(
            "[host]\ndistro = \"debian\"\n\n[files.\"/etc/marker\"]\ncontent = \"x\\n\"\n\
             owner = \"{uid}\"\ngroup = \"{gid}\"\n"
        ),
    );
    let bob = ["hosts", "box", "home", "bob", "home.toml"].join("/");
    case.root.write(&bob, "[home]\nversion = \"1\"\nbad = [\n");
    let cwd = scratch(&case, "cwd");
    let before = everything(&case);
    let applied = top(&case, "apply", &[&hosts(&case)], &cwd, &[]);
    status(&applied, "E_SYNTAX");
    assert_eq!(everything(&case), before, "{}", story(&applied));
}

/// A home manifest with the one inline tool `hello`, served from `https://fixtures.test/hello`.
fn home_with_hello(body: &[u8]) -> String {
    format!(
        "[home]\nversion = \"1\"\n\n[tools.hello]\nversion = \"1.0.0\"\n\
         url = \"https://fixtures.test/hello\"\nsha256 = \"{}\"\nformat = \"binary\"\n\
         path = [\"bin\"]\n",
        lodi::util::sha256_hex(body)
    )
}

/// #295: a second user's home that declares a tool with no lock entry has it locked into the
/// repository's one `lodi.lock` by the apply itself, in one write before any home runs as its
/// user; that home's own process then finds its section fresh and writes none, the home
/// applies, and the lock keeps its owner and mode. A re-apply writes nothing.
#[test]
fn apply_locks_another_users_home_tools_before_that_user_runs() {
    let v = Verbs::debian("verbs-other-tools", &[]);
    let body = b"#!/bin/sh\necho hello\n";
    v.server
        .files
        .lock()
        .unwrap()
        .insert("https://fixtures.test/hello".into(), body.to_vec());
    let (uid, gid) = ids(&v.case.root);
    v.case.root.write(
        "etc/passwd",
        &format!(
            "root:x:0:0::/root:/bin/sh\nalice:x:{uid}:{gid}::/people/alice:/bin/sh\n\
             bob:x:{uid}:{gid}::/people/bob:/bin/sh\n"
        ),
    );
    v.set_host("[host]\ndistro = \"debian\"\n");
    let manifest = |login: &str| format!("hosts/box/home/{login}/home.toml");
    for login in ["alice", "bob"] {
        v.case
            .root
            .write(&manifest(login), "[home]\nversion = \"1\"\n");
        fs::create_dir_all(v.case.root.path(&format!("people/{login}"))).unwrap();
    }
    v.case
        .root
        .write(&manifest("alice"), &home_with_hello(body));
    let cwd = scratch(&v.case, "cwd");
    let rewrite = v.server.rewrite();
    let envs = [("LODI_FETCH_REWRITE", rewrite.as_str())];
    ok(&top(&v.case, "update", &[&v.hosts()], &cwd, &envs));
    let root_lock = v.case.root.path("hosts/lodi.lock");
    fs::set_permissions(&root_lock, fs::Permissions::from_mode(0o640)).unwrap();
    let owner = fs::metadata(&root_lock).unwrap().uid();

    v.case.root.write(&manifest("bob"), &home_with_hello(body));
    let applied = top(&v.case, "apply", &[&v.hosts()], &cwd, &envs);
    ok(&applied);
    let text = out(&applied);
    let wrote = format!("wrote {}", root_lock.display());
    let at = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {text}"))
    };
    assert_eq!(text.matches(&wrote).count(), 1, "{}", story(&applied));
    assert!(at(&wrote) < at("home alice"), "{}", story(&applied));
    let lock: serde_json::Value = serde_json::from_slice(&v.root_lock().unwrap()).unwrap();
    assert!(
        lock["homes"]["box/home/bob"]["tools"]["hello"].is_object(),
        "{lock}"
    );
    assert!(
        v.case
            .root
            .path("people/bob")
            .read_dir()
            .unwrap()
            .next()
            .is_some()
    );
    let meta = fs::metadata(&root_lock).unwrap();
    assert_eq!((meta.uid(), meta.mode() & 0o7777), (owner, 0o640));

    let bytes = v.root_lock().unwrap();
    let again = top(&v.case, "apply", &[&v.hosts()], &cwd, &envs);
    ok(&again);
    assert!(!out(&again).contains(&wrote), "{}", story(&again));
    assert_eq!(v.root_lock().unwrap(), bytes);
}

// ---------------------------------------------------------------------------- V4 CLI ---

/// V4: `lodi plan` shows what `lodi apply` would do — the host, then every declared home, with
/// `--host` choosing among the hosts — and writes nothing; the scope-first guesses stay usage
/// errors.
#[test]
fn plan_mirrors_apply_and_writes_nothing() {
    plans_as_apply_would(repository("verbs-plan"), "debian");
}

/// V4 on Fedora (ff-1).
#[test]
fn plan_mirrors_apply_on_fedora() {
    let case = repository_on("verbs-plan-fedora", Machine::fedora(), "fedora");
    plans_as_apply_would(case, "fedora");
}

fn plans_as_apply_would(case: Case, distro: &str) {
    case.root.write(
        "hosts/other/host.toml",
        &format!("[host]\ndistro = \"{distro}\"\n"),
    );
    case.root.write(
        &["hosts", "other", "home", "alice", "home.toml"].join("/"),
        "[home]\nversion = \"1\"\n\n[home.file.\"other\"]\ntext = \"o\"\n",
    );
    let cwd = scratch(&case, "cwd");
    let before = everything(&case);
    let plan = top(&case, "plan", &[&hosts(&case)], &cwd, &[]);
    ok(&plan);
    for needle in ["home alice", "home bob", "carol"] {
        assert!(out(&plan).contains(needle), "{needle}: {}", story(&plan));
    }
    let other = top(
        &case,
        "plan",
        &[&hosts(&case), "--host", "other"],
        &cwd,
        &[],
    );
    ok(&other);
    assert!(out(&other).contains("other"), "{}", story(&other));
    assert!(!out(&other).contains("home bob"), "{}", story(&other));
    assert_eq!(everything(&case), before);
    for words in [&["setup"][..], &["init", "host"], &["init", "home"]] {
        let usage = Command::new(env!("CARGO_BIN_EXE_lodi"))
            .args(words)
            .current_dir(&cwd)
            .output()
            .unwrap();
        assert_eq!(usage.status.code(), Some(2), "{words:?}");
    }
}

// ------------------------------------------------------------------------- V5 safety ---

/// V5: `LODI_HOST_REQUIRE_ROOT=1` makes `--root` mandatory for every new verb, `update` with no
/// `SOURCE` included; each is refused before anything is read.
#[test]
fn every_new_verb_needs_root_under_lodi_host_require_root() {
    let case = repository("verbs-guard");
    let here = case.root.path("hosts");
    let before = everything(&case);
    for verb in ["plan", "apply", "import", "update"] {
        for source in [None, Some(hosts(&case))] {
            // check-host-safety: refusal — the guard must refuse this before anything is read.
            let _spawning = fakehost::spawning();
            let refused = Command::new(env!("CARGO_BIN_EXE_lodi"))
                .arg(verb)
                .args(source.iter())
                .arg("--yes")
                .current_dir(&here)
                .env("LODI_HOST_REQUIRE_ROOT", "1")
                .env("PATH", case.fake_path())
                .stdin(Stdio::null())
                .output()
                .unwrap();
            status(&refused, "E_HOST_ROOT_REQUIRED");
        }
    }
    assert_eq!(everything(&case), before);
}

// ------------------------------------------------------------------------- V8 update ---

/// V8: `update` moves the file-level snapshot to the day the archive last published and keeps an
/// explicit pin exactly; the root lock records both.
#[test]
fn update_retains_an_explicit_pin() {
    let v = Verbs::debian("verbs-update", &[("bc", BC), ("tzdata", TZ_OLDER)]);
    let now = v.alias_now();
    v.set_host(&host(
        "debian",
        Some(OLDER),
        &["bc", "tzdata"],
        &[("tzdata", "2025-03-01")],
    ));
    let cwd = scratch(&v.case, "cwd");
    let rewrite = v.server.rewrite();
    let updated = top(
        &v.case,
        "update",
        &[&v.hosts()],
        &cwd,
        &[("LODI_FETCH_REWRITE", rewrite.as_str())],
    );
    ok(&updated);
    let snapshot = v.host_toml();
    assert!(
        now.iter()
            .any(|at| snapshot.contains(&format!("snapshot = \"{at}\""))),
        "{snapshot}"
    );
    assert!(snapshot.contains("tzdata = \"2025-03-01\""), "{snapshot}");
    let lock = v.lock().expect("the root lock records the host");
    assert_eq!(lock["pins"]["tzdata"]["version"], TZ_OLDER);
    assert!(
        now.iter().any(|at| lock["snapshot"]["instant"] == *at),
        "{lock}"
    );
    let again = top(
        &v.case,
        "update",
        &[&v.hosts()],
        &cwd,
        &[("LODI_FETCH_REWRITE", rewrite.as_str())],
    );
    ok(&again);
    assert_eq!(v.lock().unwrap(), lock);
}

/// V8: a floating host gets no snapshot from `update`.
#[test]
fn update_never_adds_a_snapshot_to_a_floating_host() {
    let v = Verbs::debian("verbs-float", &[("bc", BC), ("tzdata", TZ_OLDER)]);
    let text = host("debian", None, &["bc", "tzdata"], &[]);
    v.set_host(&text);
    let cwd = scratch(&v.case, "cwd");
    let rewrite = v.server.rewrite();
    let updated = top(
        &v.case,
        "update",
        &[&v.hosts()],
        &cwd,
        &[("LODI_FETCH_REWRITE", rewrite.as_str())],
    );
    ok(&updated);
    assert_eq!(v.host_toml(), text);
    assert!(v.lock().is_none_or(|lock| lock["snapshot"].is_null()));
}

/// V8: the manifest and the root lock change together or not at all: a host directory that
/// cannot take the new `host.toml` leaves both byte-identical.
#[test]
fn update_changes_the_manifest_and_the_lock_all_or_nothing() {
    let v = Verbs::debian("verbs-atomic", &[("bc", BC), ("tzdata", TZ_OLDER)]);
    v.alias_now();
    v.set_host(&host("debian", Some(OLDER), &["bc", "tzdata"], &[]));
    let cwd = scratch(&v.case, "cwd");
    let rewrite = v.server.rewrite();
    let env = [("LODI_FETCH_REWRITE", rewrite.as_str())];
    ok(&v.run("pin", &["--all", "--to", OLDER, &v.hosts()]));
    let before = (v.host_toml(), v.root_lock());
    fs::set_permissions(v.dir(), fs::Permissions::from_mode(0o555)).unwrap();
    let failed = top(&v.case, "update", &[&v.hosts()], &cwd, &env);
    fs::set_permissions(v.dir(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_ne!(failed.status.code(), Some(0), "{}", story(&failed));
    assert_eq!((v.host_toml(), v.root_lock()), before, "{}", story(&failed));
    ok(&top(&v.case, "update", &[&v.hosts()], &cwd, &env));
    assert_ne!(v.host_toml(), before.0);
}

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

// ---------------------------------------------------------------- V10 compatibility ---

/// V10: top-level `plan` and `apply` take a git URL through gu-1's reader: the recorded
/// repository resolves on loopback, the plan writes no lock and the apply locks the commit.
#[test]
fn top_level_plan_and_apply_take_a_git_url() {
    const URL: &str = "git+https://git.example.test/hosts.git";
    let served = git_host::Server::start("single", "first");
    let case = Case::new(
        "verbs-url",
        Machine::debian().offering(fakehost::Pkg::new("jq")),
    );
    case.root.write("etc/hostname", "box\n");
    let cwd = scratch(&case, "cwd");
    let rewrite = served.rewrite("https://git.example.test/hosts.git", "/hosts.git");
    let env = [("LODI_FETCH_REWRITE", rewrite.as_str())];
    let plan = top(&case, "plan", &[URL], &cwd, &env);
    ok(&plan);
    assert!(out(&plan).contains(git_host::FIRST), "{}", story(&plan));
    assert!(!case.root.exists("etc/lodi/host.lock"));
    let applied = top(&case, "apply", &[URL], &cwd, &env);
    ok(&applied);
    assert!(
        case.root
            .read("etc/lodi/host.lock")
            .contains(git_host::FIRST)
    );
}

/// LD-456: a fetched tree that holds no host is refused naming the commit it resolved and
/// verified and how many git requests that took, which the live guest step records.
#[test]
fn a_url_with_no_host_names_its_commit_and_its_requests() {
    const URL: &str = "git+https://git.example.test/hosts.git";
    let served = git_host::Server::start("dir", "only");
    let case = Case::new("verbs-url-none", Machine::debian());
    case.root.write("etc/hostname", "box\n");
    let cwd = scratch(&case, "cwd");
    let rewrite = served.rewrite("https://git.example.test/hosts.git", "/hosts.git");
    let env = [("LODI_FETCH_REWRITE", rewrite.as_str())];
    let plan = top(&case, "plan", &[URL, "--host", "absent"], &cwd, &env);
    status(&plan, "E_NO_MANIFEST");
    let said = format!("verified; {} git requests)", served.lines().len());
    assert!(err(&plan).contains(&said), "{said}: {}", story(&plan));
    let commit = err(&plan)
        .split("(commit ")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .unwrap_or_default()
        .to_string();
    assert!(
        commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "{}",
        story(&plan)
    );
    assert!(!case.root.exists("etc/lodi/host.lock"));
}

/// V10, F-23: a fetched repository's root `lodi.lock` is read from the tree, as a checkout's is:
/// one of a later format is refused by name before anything is planned.
#[test]
fn a_fetched_repository_reads_its_root_lock() {
    const URL: &str = "git+https://git.example.test/hosts.git";
    let served = git_host::Server::start("rootlock", "only");
    let case = Case::new(
        "verbs-url-rootlock",
        Machine::debian().offering(fakehost::Pkg::new("jq")),
    );
    case.root.write("etc/hostname", "box\n");
    let cwd = scratch(&case, "cwd");
    let rewrite = served.rewrite("https://git.example.test/hosts.git", "/hosts.git");
    let env = [("LODI_FETCH_REWRITE", rewrite.as_str())];
    let plan = top(&case, "plan", &[URL], &cwd, &env);
    status(&plan, "E_LOCK_VERSION");
    assert!(err(&plan).contains("lodi.lock"), "{}", story(&plan));
    assert!(!case.root.exists("etc/lodi/host.lock"));
}
