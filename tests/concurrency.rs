//! M-1.0 T-4: the concurrency cases the roadmap names, driven as real processes.
//!
//! Two projects entering at once against one store, two `lodi shell` sessions at once, a
//! collection during an apply and during a live `lodi develop` (and the reverse order), and a
//! collection meeting a session whose process is gone.
//!
//! **The synchronization rule (design call D10).** No case here waits on a clock. Each one either
//! takes the *exact* lock the product takes — `store/.lock`, `store/.locks/<entry>.lock`,
//! `<data>/home-scope/.lock`, through the product's own entry points — before either process
//! starts and releases it only after both have reported what they must, or blocks on a pipe the
//! product writes to: the contention line it prints, the marker the child shell echoes, or the
//! request the loopback artifact server logs ([`support::Server::await_request`], which blocks on
//! a condition variable the serving thread signals). The rule is enforced, not merely stated:
//! [`the_two_concurrency_files_never_wait_on_a_clock`] scans both source files and fails if the
//! name of the wall-clock wait appears in either of them.
//!
//! Offline and deterministic throughout: synthetic artifacts over a loopback server, scratch
//! `LODI_HOME`, `HOME` and `XDG_*` roots under `CARGO_TARGET_TMPDIR`, no guest and no network.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

use lodi::home::fsops::{self, RelPath};
use lodi::roots::Roots;
use lodi::store::{FileLock, Store, art_name};
use support::*;

const LODI: &str = env!("CARGO_BIN_EXE_lodi");

/// The line `lodi gc` prints on standard error when somebody else holds the store lock.
const STORE_WAIT: &str = "lodi: waiting for the store lock";
/// The line `lodi home apply` prints when somebody else holds the home lock.
const HOME_WAIT: &str = "waiting for the home lock";

// --------------------------------------------------------------------------- blocking helpers ---

/// Block until `reader` yields a line containing `needle`, and return every line read. End of
/// file means the process gave up instead of reporting: that is a failure, reported with what it
/// did say. This is a blocking read on a pipe the product writes to — there is no retry loop and
/// no deadline of our own.
fn read_until(reader: &mut impl BufRead, needle: &str, what: &str) -> String {
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("the pipe is readable");
        if n == 0 {
            panic!("{what}: the pipe closed before `{needle}` appeared; it said:\n{seen}");
        }
        seen.push_str(&line);
        if line.contains(needle) {
            return seen;
        }
    }
}

/// A child of this test, with the pipes the cases block on.
struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<std::process::ChildStdout>>,
    stderr: Option<BufReader<std::process::ChildStderr>>,
    name: String,
}

impl Proc {
    fn start(name: &str, mut command: Command) -> Proc {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("{name} starts: {e}"));
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().map(BufReader::new);
        let stderr = child.stderr.take().map(BufReader::new);
        Proc {
            child,
            stdin,
            stdout,
            stderr,
            name: name.to_string(),
        }
    }

    fn out_until(&mut self, needle: &str) -> String {
        let name = self.name.clone();
        read_until(self.stdout.as_mut().unwrap(), needle, &name)
    }

    fn err_until(&mut self, needle: &str) -> String {
        let name = self.name.clone();
        read_until(self.stderr.as_mut().unwrap(), needle, &name)
    }

    fn say(&mut self, text: &str) {
        self.stdin
            .as_mut()
            .expect("the child still has a standard input")
            .write_all(text.as_bytes())
            .unwrap_or_else(|e| panic!("{}: writing to its input: {e}", self.name));
    }

    /// Assert that the child is still running. While the test holds the lock it is blocked on,
    /// an exit is always a failure, and the failure names what it printed.
    fn assert_running(&mut self) {
        let status = self
            .child
            .try_wait()
            .unwrap_or_else(|e| panic!("{}: {e}", self.name));
        assert!(
            status.is_none(),
            "{} exited ({status:?}) while the test held the lock it must wait for",
            self.name
        );
    }

    /// Close the child's standard input, read what is left of both pipes and wait for it.
    fn finish(mut self) -> (i32, String, String) {
        drop(self.stdin.take());
        let mut out = String::new();
        let mut err = String::new();
        if let Some(reader) = self.stdout.as_mut() {
            reader.read_to_string(&mut out).ok();
        }
        if let Some(reader) = self.stderr.as_mut() {
            reader.read_to_string(&mut err).ok();
        }
        let status = self.child.wait().unwrap();
        (
            status.code().unwrap_or_else(|| {
                panic!("{} was killed by a signal: {status:?}\n{err}", self.name)
            }),
            out,
            err,
        )
    }
}

// ------------------------------------------------------------------------------- the harness ---

/// One machine: one `$LODI_HOME`, one loopback artifact server, scratch `HOME` and config roots.
struct Lane {
    base: PathBuf,
    server: Server,
}

impl Lane {
    fn new(name: &str, files: BTreeMap<String, Vec<u8>>) -> Lane {
        let base = scratch(name);
        for d in ["home", "config", "lodi-home"] {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        Lane {
            base,
            server: Server::start(files),
        }
    }

    fn for_tools(name: &str, tools: &[Tool]) -> Lane {
        Lane::new(
            name,
            tools
                .iter()
                .map(|t| (t.url.clone(), t.bytes.clone()))
                .collect(),
        )
    }

    fn lodi_home(&self) -> PathBuf {
        self.base.join("lodi-home")
    }

    fn store(&self) -> Store {
        Store::open(&self.lodi_home()).expect("the scratch store opens")
    }

    /// Bring the scratch store to the current layout **before** any child starts, through the
    /// product's own writing entry point.
    ///
    /// A store with no `.layout.json` marker is layout 0 (M-1.0 T-2), so the first writing
    /// command in it takes the **exclusive** `store/.lock` for the once-only migration. A case
    /// whose barrier is a *per-entry* lock must not leave that pending: realization holds the
    /// shared `store/.lock` across the per-entry acquisition (`src/host.rs`), so one process
    /// blocked on the barrier while holding the store lock shared will block the other out of
    /// its own migration, and a rendezvous that waits for the second process deadlocks — which
    /// one does, depending on which process wins the start, so the case would also depend on
    /// scheduling order (design call D10 forbids both). Migrating first removes the question.
    /// The two-shells case deliberately does **not** call this: there the migration's use of the
    /// store lock is the barrier (LD-250).
    fn at_current_layout(&self) -> Store {
        Store::open_for_write(&self.lodi_home()).expect("the scratch store migrates")
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
            .env("LODI_TRUST", "1")
            .env("SHELL", "/bin/sh");
        c
    }

    fn sessions(&self) -> Vec<String> {
        session_listing(&self.lodi_home().join("gcroots/sessions"))
    }

    fn entries(&self) -> Vec<String> {
        listing(&self.lodi_home().join("store"))
            .into_iter()
            .filter(|n| !n.contains('/') && (n.starts_with("art-") || n.starts_with("env-")))
            .collect()
    }

    fn staging(&self) -> Vec<String> {
        listing(&self.lodi_home().join("store/tmp"))
    }
}

/// The script a case gives `lodi develop`: report, then block on its own standard input until
/// the test closes it. `cat` returning is the only way out, so the session stays live — and
/// rooted — for exactly as long as the test needs it.
fn hold_script(extra: &str) -> String {
    format!("{extra}printf 'ready\\n'\ncat > /dev/null\n")
}

// ------------------------------------------------------- (a) two projects at once, one store ---

/// **Case 1.** Two project directories with overlapping tool sets enter one `$LODI_HOME` at the
/// same time. The barrier is the **exact** per-entry lock `Store::publish` takes for the shared
/// artifact, held by this test before either binary starts, so the two are provably unable to
/// publish it while it is held; the rendezvous that says both have got that far is the loopback
/// server, which has by then served each project's *private* artifact once.
///
/// Realized exactly once is derived from the store's own state, never from timing: each process
/// copies the shared entry's sidecar *at the moment its own realization finished*, and the two
/// copies must be byte-identical to each other and to the file on disk — a second realization
/// would have replaced the tree and written a sidecar with its own `created` timestamp. The
/// product's own realization report is counted too: `lodi: realized …` names the shared entry
/// exactly once across both runs.
#[test]
fn two_projects_at_once_realize_the_shared_entry_exactly_once() {
    let shared = Tool::python("3.12.14");
    let first_only = Tool::node("22", "22.23.2");
    let second_only = Tool::node("20", "20.20.2");
    let lane = Lane::for_tools(
        "concurrency-projects",
        &[shared.clone(), first_only.clone(), second_only.clone()],
    );

    let shared_entry = art_name(&shared.entry()).expect("the shared artifact names its entry");
    let meta = lane
        .lodi_home()
        .join("store/.meta")
        .join(format!("{shared_entry}.json"));

    // The barrier: the per-entry lock of the shared artifact, taken through the product's own
    // `Store::key_lock`, before either process starts. The store is brought to the current
    // layout first, so that neither child wants the exclusive store lock on the way in and the
    // case cannot turn on which of them starts first.
    let store = lane.at_current_layout();
    let barrier = store
        .key_lock(&shared_entry)
        .expect("the per-entry lock of the shared artifact");

    let mut procs = Vec::new();
    for (name, private) in [("first", &first_only), ("second", &second_only)] {
        let dir = lane.base.join(format!("project-{name}"));
        let tools = vec![private.clone(), shared.clone()];
        write_project(&dir, &manifest(&tools, ""), &tools);
        let script = hold_script(&format!(
            "node > {}\ncp {} {}\n",
            dir.join("node.out").display(),
            meta.display(),
            dir.join("sidecar.json").display(),
        ));
        let command = lane.command(&dir, &["develop", "--", "sh", "-c", &script]);
        procs.push((dir, Proc::start(name, command)));
    }

    // Both are past their own private artifact and can go no further than the lock this test
    // holds. The rendezvous is the server's own log, waited on by a condition variable.
    lane.server.await_request(&first_only.url, 1);
    lane.server.await_request(&second_only.url, 1);
    for (_, proc) in &mut procs {
        proc.assert_running();
    }
    assert!(
        !meta.exists(),
        "the shared entry was published while its lock was held"
    );
    assert!(
        !lane.lodi_home().join("store").join(&shared_entry).exists(),
        "the shared entry's tree appeared while its lock was held"
    );

    // Release it: the two now serialize on it, exactly as two real projects would.
    drop(barrier);
    for (_, proc) in &mut procs {
        proc.out_until("ready");
    }

    // Both sessions are live at the same time, and each is rooted.
    let sessions = lane.sessions();
    assert_eq!(sessions.len(), 2, "both sessions are rooted: {sessions:?}");
    for (_, proc) in &procs {
        assert!(
            sessions.contains(&proc.child.id().to_string()),
            "{} has no session root: {sessions:?}",
            proc.name
        );
    }
    // Neither holds the shared store lock while its command runs (M-0.3 T-2).
    assert!(
        store
            .try_exclusive_lock()
            .expect("the store lock is askable")
            .is_some(),
        "a running session still holds the shared store lock"
    );

    let mut outcomes = Vec::new();
    for (dir, proc) in procs {
        let name = proc.name.clone();
        let (code, _, err) = proc.finish();
        assert_eq!(code, 0, "{name}: {err}");
        outcomes.push((name, dir, err));
    }

    // Each got its own environment: its own node, its own `env-` entry.
    let versions: Vec<String> = outcomes
        .iter()
        .map(|(_, dir, _)| fs::read_to_string(dir.join("node.out")).unwrap())
        .collect();
    assert_eq!(versions, vec!["v22.23.2\n", "v20.20.2\n"], "{versions:?}");
    let envs: Vec<String> = lane
        .entries()
        .into_iter()
        .filter(|n| n.starts_with("env-"))
        .collect();
    assert_eq!(
        envs.len(),
        2,
        "each project has its own environment: {envs:?}"
    );

    // Exactly once, from the store's own state: the sidecar each process saw when its own
    // realization returned, and the sidecar on disk now, are the same bytes.
    let on_disk = fs::read(&meta).expect("the shared entry's sidecar");
    for (name, dir, _) in &outcomes {
        let seen = fs::read(dir.join("sidecar.json")).expect("the sidecar the process saw");
        assert_eq!(
            seen, on_disk,
            "{name} saw a different realization of the shared entry"
        );
    }
    // And from the product's own report: one realization of that entry across both runs.
    let realizations: usize = outcomes
        .iter()
        .map(|(_, _, err)| {
            err.lines()
                .filter(|line| line.starts_with("lodi: realized") && line.contains(&shared_entry))
                .count()
        })
        .sum();
    assert_eq!(
        realizations,
        1,
        "the shared entry was realized {realizations} times:\n{}",
        outcomes
            .iter()
            .map(|(n, _, e)| format!("{n}: {e}"))
            .collect::<Vec<_>>()
            .join("")
    );

    assert!(lane.staging().is_empty(), "{:?}", lane.staging());
    assert!(lane.sessions().is_empty(), "{:?}", lane.sessions());
    assert!(
        lane.entries().contains(&shared_entry),
        "the shared entry is not in the store"
    );
}

/// **Negative control for case 1**, free of any build flag and of the product's own locking: the
/// same check-then-realize shape over a scratch directory, run by two threads. With the
/// exclusive lock the product takes, exactly one of them realizes; with no lock at all — and the
/// two threads made to look before either writes, so that the reproduction does not depend on
/// which thread the operating system runs first — both do. That second count is the failure the
/// per-entry lock in `Store::publish` prevents above.
#[test]
fn without_the_per_entry_lock_two_realizations_of_one_entry_happen() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    /// The shape of `Store::publish`: look for a complete entry, and realize it when there is
    /// none. `lock` is the path of the exclusive lock held across the whole of it, or `None` for
    /// the unlocked reproduction; `observe` is the rendezvous only that reproduction needs.
    fn reproduce(
        dir: &Path,
        lock: Option<&Path>,
        count: &AtomicUsize,
        start: &Barrier,
        observe: Option<&Barrier>,
    ) {
        // Both threads are running before either takes anything.
        start.wait();
        let held = lock.map(|path| FileLock::at(path, true).expect("the scratch lock"));
        let done = dir.join("entry");
        let seen = done.exists();
        if let Some(observe) = observe {
            // The unlocked reproduction only: both have looked before either writes.
            observe.wait();
        }
        if !seen {
            count.fetch_add(1, Ordering::SeqCst);
            fs::write(&done, b"realized\n").unwrap();
        }
        drop(held);
    }

    for (what, locked, expected) in [("locked", true, 1), ("unlocked", false, 2)] {
        let dir = scratch(&format!("concurrency-control-{what}"));
        let lock = dir.join("entry.lock");
        let count = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(2));
        let observe = (!locked).then(|| Arc::new(Barrier::new(2)));
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let (dir, lock, count, start, observe) = (
                    dir.clone(),
                    lock.clone(),
                    Arc::clone(&count),
                    Arc::clone(&start),
                    observe.clone(),
                );
                scope.spawn(move || {
                    reproduce(
                        &dir,
                        locked.then_some(lock.as_path()),
                        &count,
                        &start,
                        observe.as_deref(),
                    );
                });
            }
        });
        assert_eq!(
            count.load(Ordering::SeqCst),
            expected,
            "the {what} reproduction realized the entry the wrong number of times"
        );
    }
}

// ------------------------------------------------------------------- (b) two shells at once ---

/// The upstream files an ad-hoc `lodi shell python` resolves against, served by the loopback
/// server: the recipe's release page, the checksum file of the release it picks and the
/// artifact. The digests are the real digests of the synthetic archive, so the binary verifies
/// them as it would a real download.
fn python_upstream() -> BTreeMap<String, Vec<u8>> {
    const VERSION: &str = "3.12.14";
    const TAG: &str = "20260901";
    let bytes = python_archive(VERSION);
    let hex = lodi::util::sha256_hex(&bytes);
    let name = format!("cpython-{VERSION}+{TAG}-x86_64-unknown-linux-gnu-install_only.tar.gz");
    let base = "https://github.com/astral-sh/python-build-standalone/releases/download";
    let url = format!("{base}/{TAG}/{name}");
    let releases = serde_json::json!([{
        "tag_name": TAG,
        "name": TAG,
        "draft": false,
        "prerelease": false,
        "assets": [
            { "name": name, "size": bytes.len(), "digest": format!("sha256:{hex}"),
              "browser_download_url": url },
            { "name": "SHA256SUMS", "size": 64,
              "browser_download_url": format!("{base}/{TAG}/SHA256SUMS") },
        ],
    }]);
    BTreeMap::from([
        (RELEASES.to_string(), serde_json::to_vec(&releases).unwrap()),
        (
            format!("{base}/{TAG}/SHA256SUMS"),
            format!("{hex}  {name}\n").into_bytes(),
        ),
        (url, bytes),
    ])
}

/// The one page of release metadata an ad-hoc `lodi shell python` reads.
const RELEASES: &str =
    "https://api.github.com/repos/astral-sh/python-build-standalone/releases?per_page=2";

/// **Case 2.** Two `lodi shell python` sessions run at once. The barrier is the **exact** shared
/// store lock, held exclusively by this test before either binary starts; the rendezvous that
/// says both have reached it is the **contention line the product itself prints** — each shell
/// reports `lodi: waiting for the store lock` on its standard error and then blocks, so the test
/// knows both are at the barrier without waiting on a clock. (A `lodi shell` against a store with
/// no layout marker takes that lock on the way in, for the once-only 0 → 1 migration of M-1.0
/// T-2; the release page is fetched only afterwards, which is why the rendezvous is the line and
/// not the request.) Once both shells are at their prompts the test takes that same lock
/// exclusively and **succeeds**: neither shell holds it for the life of the shell (M-0.3 T-2), so
/// neither blocks the other. Each has its tool on `PATH`, and when both exit their roots are gone.
#[test]
fn two_shells_at_once_never_block_each_other_on_the_store_lock() {
    let lane = Lane::new("concurrency-shells", python_upstream());
    let dir = lane.base.join("elsewhere");
    fs::create_dir_all(&dir).unwrap();
    let store = lane.store();

    let barrier = store.exclusive_lock().expect("the shared store lock");
    // The negative control of this case's own assertion: while the lock is held, asking for it
    // says no. The success below is therefore evidence, not an empty check.
    assert!(
        store
            .try_exclusive_lock()
            .expect("the lock is askable")
            .is_none(),
        "the store lock is not exclusive"
    );

    let mut shells = Vec::new();
    for name in ["first", "second"] {
        let mut proc = Proc::start(name, lane.command(&dir, &["shell", "python"]));
        // The script the interactive shell will run as soon as it starts.
        proc.say(&format!(
            "command -v python3 > {}\nprintf 'ready\\n'\n",
            dir.join(format!("{name}.path")).display()
        ));
        shells.push(proc);
    }
    // Both have reached the exact lock this test is holding, each having said so itself.
    for proc in &mut shells {
        proc.err_until(STORE_WAIT);
        proc.assert_running();
    }
    assert!(
        lane.entries().is_empty(),
        "an entry was published while the store lock was held: {:?}",
        lane.entries()
    );

    drop(barrier);
    for proc in &mut shells {
        proc.out_until("ready");
    }

    // Both are live at once, each rooted, and neither holds the store lock.
    let sessions = lane.sessions();
    assert_eq!(sessions.len(), 2, "both shells are rooted: {sessions:?}");
    for proc in &shells {
        assert!(
            sessions.contains(&proc.child.id().to_string()),
            "{} has no session root: {sessions:?}",
            proc.name
        );
    }
    assert!(
        store
            .try_exclusive_lock()
            .expect("the lock is askable")
            .is_some(),
        "an open shell still holds the shared store lock, so the other would have blocked"
    );

    for proc in &mut shells {
        let path = fs::read_to_string(dir.join(format!("{}.path", proc.name))).unwrap();
        assert!(
            Path::new(path.trim()).starts_with(lane.lodi_home().join("store")),
            "{}: python3 is not the store's: {path}",
            proc.name
        );
        proc.say("exit 0\n");
    }
    for proc in shells {
        let name = proc.name.clone();
        let (code, _, err) = proc.finish();
        assert_eq!(code, 0, "{name}: {err}");
    }
    assert!(
        lane.sessions().is_empty(),
        "a root outlived its shell: {:?}",
        lane.sessions()
    );
    assert!(lane.staging().is_empty(), "{:?}", lane.staging());
}

// ------------------------------------------------ (c) a collection during an apply, and back ---

/// The data root of a scratch home environment, as the binary computes it.
fn data_root(env: &HomeEnv) -> lodi::home::fsops::Root {
    let home = OsString::from(env.home());
    let store = OsString::from(env.data().join("lodi"));
    Roots::from_vars(Some(home.as_os_str()), None, None, Some(store.as_os_str()))
        .expect("the scratch roots")
        .data_root()
}

/// Take the home scope's own `<data>/home-scope/.lock` exclusively, through the one entry point
/// the binary uses (`LD-187`).
fn hold_home_lock(env: &HomeEnv) -> fsops::Lock {
    let data = data_root(env);
    let scope = RelPath::new(lodi::home::state::HOME_SCOPE_DIR).unwrap();
    let lock = RelPath::new(lodi::home::state::LOCK_FILE).unwrap();
    fsops::mkdir_p(&data, &scope).expect("the scope directory");
    fsops::lock(&data, &lock, true).expect("the home lock, held by the test")
}

const FILES_ONLY: &str = r#"[home]
version = "1"

[files.".inputrc"]
content = "set editing-mode vi\n"
"#;

fn write_home_manifest(env: &HomeEnv, text: &str) {
    let dir = env.config().join("lodi");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("home.toml"), text).unwrap();
}

/// **Case 3.** A `lodi gc` started while a `lodi home apply` holds `<data>/home-scope/.lock` and
/// while a `lodi develop` holds a live session root. The collection reports the contention on the
/// exclusive `store/.lock` with the one line it already prints — that line is this case's
/// negative control, because it is printed only when the lock really is held — removes nothing
/// the live root names, and the apply finishes unaffected.
#[test]
fn a_collection_waits_for_the_store_while_an_apply_and_a_session_hold_theirs() {
    let env = home_env("concurrency-gc-apply");
    let decoy = env.decoy_listing();
    write_home_manifest(&env, FILES_ONLY);

    // The store this case shares is the home environment's own `LODI_HOME`.
    let tools = vec![Tool::python("3.12.14")];
    let server = Server::for_tools(&tools);
    let lodi_home = env.data().join("lodi");
    let project = env.root().join("project");
    write_project(&project, &manifest(&tools, ""), &tools);
    let store = Store::open(&lodi_home).expect("the scratch store opens");

    // 1. The apply is blocked on the exact home lock this test holds, and says so.
    let home_lock = hold_home_lock(&env);
    let mut apply = Proc::start("the apply", {
        let mut c = env.command();
        c.args(["home", "apply"]);
        c
    });
    apply.err_until(HOME_WAIT);
    assert!(
        !env.data().join("lodi/home-scope/state.json").exists(),
        "a blocked apply wrote the state file"
    );

    // 2. A live `lodi develop` session, rooted, with its command blocked on its own input.
    let script = hold_script("");
    let mut develop = Proc::start("the session", {
        let mut c = Command::new(LODI);
        c.args(["develop", "--", "sh", "-c", &script])
            .current_dir(&project)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", env.home())
            .env("XDG_CONFIG_HOME", env.config())
            .env("LODI_HOME", &lodi_home)
            .env("LODI_FETCH_REWRITE", server.rewrite())
            .env("LODI_TRUST", "1")
            .env("SHELL", "/bin/sh");
        c
    });
    develop.out_until("ready");
    let live: Vec<String> = session_listing(&lodi_home.join("gcroots/sessions"));
    assert_eq!(live, vec![develop.child.id().to_string()], "{live:?}");
    let rooted: Vec<String> = serde_json::from_slice::<serde_json::Value>(
        &fs::read(lodi_home.join("gcroots/sessions").join(&live[0])).unwrap(),
    )
    .unwrap()["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(!rooted.is_empty(), "the live root names no entry");

    // 3. The collection meets the exclusive store lock this test now holds, and reports it.
    let store_lock = store.exclusive_lock().expect("the exclusive store lock");
    let mut gc = Proc::start("the collection", {
        let mut c = Command::new(LODI);
        c.args(["gc", "-v"])
            .current_dir(env.root())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", env.home())
            .env("LODI_HOME", &lodi_home);
        c
    });
    gc.err_until(STORE_WAIT);
    apply.assert_running();
    develop.assert_running();

    // Release the store: the collection runs, and keeps everything the live root names.
    drop(store_lock);
    let (code, out, err) = gc.finish();
    assert_eq!(code, 0, "{err}");
    for entry in &rooted {
        assert!(
            lodi_home.join("store").join(entry).is_dir(),
            "the collection removed {entry}, which the live session root names\n{out}"
        );
    }
    assert!(
        lodi_home.join("gcroots/sessions").join(&live[0]).is_file(),
        "the collection pruned a live session root"
    );

    // Release the home lock: the apply finishes, unaffected by any of it.
    drop(home_lock);
    let (code, out, err) = apply.finish();
    assert_eq!(code, 0, "{err}");
    assert!(out.contains(".inputrc"), "{out}");
    assert_eq!(
        fs::read_to_string(env.home().join(".inputrc")).unwrap(),
        "set editing-mode vi\n"
    );

    develop.assert_running();
    let name = develop.name.clone();
    let (code, _, err) = develop.finish();
    assert_eq!(code, 0, "{name}: {err}");
    contained_in(&env, &decoy);
}

/// The reverse order of case 3: the **apply** starts while the collection holds the exclusive
/// `store/.lock`. A home manifest with a `[tools]` table realizes through the project pipeline,
/// which takes the shared store lock, so the apply cannot get past it — proven by the artifact
/// the loopback server has *not* been asked for and by the state file that does not exist — and
/// completes as soon as the lock is released.
#[test]
fn an_apply_waits_for_the_store_while_a_collection_holds_it() {
    let env = home_env("concurrency-apply-gc");
    let decoy = env.decoy_listing();
    let body = b"#!/bin/sh\necho home tool\n";
    let url = "https://artifacts.test/home/demo-1.0.0";
    let server = Server::start(BTreeMap::from([(url.to_string(), body.to_vec())]));
    write_home_manifest(
        &env,
        &format!(
            "[home]\nversion = \"1\"\n\n[files.\".inputrc\"]\ncontent = \"set editing-mode vi\\n\"\n\n\
             [tools.demo]\nversion = \"1.0.0\"\nurl = \"{url}\"\nsha256 = \"{}\"\n\
             format = \"binary\"\n",
            lodi::util::sha256_hex(body)
        ),
    );
    let lodi_home = env.data().join("lodi");
    let store = Store::open(&lodi_home).expect("the scratch store opens");

    // The collection's lock, and the home lock, both held before the apply starts.
    let store_lock = store.exclusive_lock().expect("the exclusive store lock");
    let home_lock = hold_home_lock(&env);
    let mut apply = Proc::start("the apply", {
        let mut c = env.command();
        c.args(["home", "apply"])
            .env("LODI_FETCH_REWRITE", server.rewrite());
        c
    });
    // It is at the home lock; that line is the product's own contention report.
    apply.err_until(HOME_WAIT);

    // Let it past the home lock. Its next stop is the shared store lock, which the collection
    // holds: it can therefore not have begun realizing, which the server's empty log shows.
    drop(home_lock);
    apply.assert_running();
    assert_eq!(server.count(url), 0, "{:?}", server.requests());
    assert!(
        !env.data().join("lodi/home-scope/state.json").exists(),
        "the apply wrote its state while the store lock was held"
    );

    drop(store_lock);
    let name = apply.name.clone();
    let (code, out, err) = apply.finish();
    assert_eq!(code, 0, "{name}: {err}");
    assert_eq!(server.count(url), 1, "{:?}", server.requests());
    assert!(out.contains(".inputrc"), "{out}");
    assert!(
        env.data().join("lodi/home-scope/state.json").is_file(),
        "the apply did not write its state"
    );
    assert!(
        lodi_home.join("gcroots/home").is_file(),
        "the realized home set was not rooted"
    );
    contained_in(&env, &decoy);
}

// ------------------------------------------------- (d) a collection against a session that dies ---

/// **Case 4.** A collection under contention meets three session roots: one whose process is
/// gone, one whose process is alive, and one it cannot read. The dead one is pruned, the live
/// one and everything it names are kept, and the unreadable one is `W_ROOT_UNREADABLE` with
/// nothing collected rather than a guess. The exclusive `store/.lock` is held by this test
/// before the collection starts, and the collection's own contention line — this case's negative
/// control — is what says it waited for it.
#[test]
fn a_collection_prunes_a_dead_root_keeps_a_live_one_and_refuses_an_unreadable_one() {
    let tools = vec![Tool::python("3.12.14")];
    let lane = Lane::for_tools("concurrency-roots", &tools);
    let project = lane.base.join("project");
    write_project(&project, &manifest(&tools, ""), &tools);
    let store = lane.store();

    // A live session, rooted, blocked on its own input.
    let script = hold_script("");
    let mut develop = Proc::start(
        "the session",
        lane.command(&project, &["develop", "--", "sh", "-c", &script]),
    );
    develop.out_until("ready");
    let live_pid = develop.child.id().to_string();
    let entries = lane.entries();
    assert!(!entries.is_empty(), "nothing was realized");

    // A dead session root: a process that has been started, waited for and reaped, whose root
    // names every entry in the store.
    let dead = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let dead_pid = dead.id().to_string();
    let mut dead = dead;
    dead.wait().unwrap();
    let sessions = lane.lodi_home().join("gcroots/sessions");
    fs::write(
        sessions.join(&dead_pid),
        serde_json::to_vec(&serde_json::json!({
            "version": 1, "pid": dead_pid.parse::<u64>().unwrap(), "entries": entries,
        }))
        .unwrap(),
    )
    .unwrap();

    // The collection waits for the exclusive store lock this test holds, and says so.
    let store_lock = store.exclusive_lock().expect("the exclusive store lock");
    let mut gc = Proc::start("the collection", lane.command(&project, &["gc", "-v"]));
    gc.err_until(STORE_WAIT);
    develop.assert_running();
    drop(store_lock);
    let (code, out, err) = gc.finish();
    assert_eq!(code, 0, "{err}");

    assert!(
        !sessions.join(&dead_pid).exists(),
        "the root of a process that is gone was kept\n{out}"
    );
    assert!(
        sessions.join(&live_pid).is_file(),
        "the root of a live process was pruned\n{out}"
    );
    for entry in &entries {
        assert!(
            lane.lodi_home().join("store").join(entry).is_dir(),
            "{entry} was collected although the live root names it\n{out}"
        );
    }

    // The same collection against a root it cannot read: nothing is collected, and it says why.
    let root = sessions.join(&live_pid);
    let mode = fs::metadata(&root).unwrap().permissions();
    fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
    let store_lock = store.exclusive_lock().expect("the exclusive store lock");
    let mut gc = Proc::start(
        "the blind collection",
        lane.command(&project, &["gc", "-v"]),
    );
    gc.err_until(STORE_WAIT);
    drop(store_lock);
    let (code, out, err) = gc.finish();
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("W_ROOT_UNREADABLE") || out.contains("W_ROOT_UNREADABLE"),
        "an unreadable root was guessed at rather than reported\n{out}\n{err}"
    );
    for entry in &entries {
        assert!(
            lane.lodi_home().join("store").join(entry).is_dir(),
            "{entry} was collected while a root could not be read\n{out}"
        );
    }
    fs::set_permissions(&root, mode).unwrap();

    let name = develop.name.clone();
    let (code, _, err) = develop.finish();
    assert_eq!(code, 0, "{name}: {err}");
}

// ----------------------------------------------------------------------- the rule, enforced ---

/// The decoy tree beside a scratch home environment is untouched and the write ledger names
/// nothing outside its root — the containment check every home-scope test of this repository
/// ends with.
fn contained_in(env: &HomeEnv, decoy_before: &str) {
    let outside: Vec<PathBuf> = home_gate()
        .ledger_lines()
        .into_iter()
        .map(|(_, path)| path)
        .filter(|path| !path.starts_with(home_gate().root()))
        .collect();
    assert!(
        outside.is_empty(),
        "the ledger left the gate root: {outside:?}"
    );
    assert_eq!(env.decoy_listing(), decoy_before, "the decoy tree changed");
}

// ------------------------------------------------------- test binaries at once (#308, #344) ---

/// The gate runs the test binaries at once (#308), and they share `CARGO_TARGET_TMPDIR`. The
/// sweep that each binary's first [`support::scratch`] runs removes only a directory `scratch`
/// made, `NAME-PID-N`, whose process has exited. Anything else there belongs to another binary
/// that may be using it right now: a fixed-name scratch whose second-to-last word is a number
/// naming no live process (`surface-init-alias-0-import` vanished under a running test, #344), a
/// Podman storage `spike-podman-PID-TEST` its own test removes, a marker file. The negative
/// control: the dead `NAME-PID-N` directories do go.
#[test]
fn the_scratch_sweep_removes_only_the_dead_directories_scratch_made() {
    let root = scratch("sweep");
    let mut exited = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
    let dead = exited.id();
    exited.wait().unwrap();
    let live = std::process::id();
    let kept = [
        "surface-init-alias-0-import".to_string(),
        format!("surface-init-alias-{dead}-init"),
        format!("spike-podman-{dead}-arch"),
        "h6-home-source".to_string(),
        format!("tools-inline-{live}-0"),
        format!("text-index-go-0-{live}-3"),
    ];
    let removed = [
        format!("tools-inline-{dead}-0"),
        format!("text-index-go-0-{dead}-3"),
    ];
    for name in kept.iter().chain(&removed) {
        fs::create_dir_all(root.join(name).join("home/.config")).unwrap();
    }
    // A dead run's store makes its entries read-only (#348): the sweep removes them all the same.
    for name in &removed {
        fs::write(root.join(name).join("home/.config/entry"), "read-only").unwrap();
        for dir in ["home/.config", "home"] {
            fs::set_permissions(root.join(name).join(dir), fs::Permissions::from_mode(0o555))
                .unwrap();
        }
    }
    fs::write(root.join("wait-hung-marker"), "a file").unwrap();

    support::sweep_dead_scratch(&root);

    let left: BTreeSet<String> = fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let mut want: BTreeSet<String> = kept.into_iter().collect();
    want.insert("wait-hung-marker".to_string());
    assert_eq!(
        left, want,
        "the sweep removed another binary's scratch, or kept a dead one"
    );
    fs::remove_dir_all(&root).unwrap();
}

/// #348: the child half of [`a_test_process_leaves_no_scratch_behind_read_only_store_and_all`].
/// Run on its own it does nothing; the parent re-runs this binary on it with
/// `LODI_TEST_SCRATCH_EXIT_REPORT` set, and then it makes a scratch holding a read-only tree, as
/// a store's entries are, and writes the scratch's path to that file.
#[test]
fn scratch_exit_child() {
    let Some(report) = std::env::var_os("LODI_TEST_SCRATCH_EXIT_REPORT") else {
        return;
    };
    let dir = scratch("scratch-exit");
    let entry = dir.join("store/art/entry");
    fs::create_dir_all(&entry).unwrap();
    fs::write(entry.join("file"), "read-only").unwrap();
    for ro in [&entry, &dir.join("store/art")] {
        fs::set_permissions(ro, fs::Permissions::from_mode(0o555)).unwrap();
    }
    fs::write(report, dir.as_os_str().as_encoded_bytes()).unwrap();
}

/// #348: a test process's scratch is gone once the process exits, read-only store entries and
/// all. The gate ran every test binary and left each one's last scratch under the target
/// directory, stores read-only, until a later run's sweep; `git worktree remove` and a plain
/// `rm -rf` of the worktree then failed with `Permission denied`. The child is this binary re-run
/// on [`scratch_exit_child`]; its scratch must not survive it.
#[test]
fn a_test_process_leaves_no_scratch_behind_read_only_store_and_all() {
    let report = scratch("scratch-exit-parent").join("report");
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "scratch_exit_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LODI_TEST_SCRATCH_EXIT_REPORT", &report)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the child failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let left = PathBuf::from(fs::read_to_string(&report).expect("the child's report"));
    assert!(
        left.file_name().is_some() && !left.exists(),
        "the child's scratch {} outlived it",
        left.display()
    );
}

/// The `.rs` files below `tests/` (the fixtures' trees aside), relative to the repository.
fn test_sources() -> Vec<String> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut found = Vec::new();
    let mut stack = vec![repo.join("tests")];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() && !path.ends_with("tests/fixtures") {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "rs") {
                found.push(path.strip_prefix(repo).unwrap().display().to_string());
            }
        }
    }
    found.sort();
    found
}

/// Every line of code (not a comment) in a test source outside `allowed` that contains `needle`
/// and none of `exempt`, as `file:line: text`.
fn code_naming(needle: &str, allowed: &[&str], exempt: &[&str]) -> Vec<String> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut hits = Vec::new();
    for name in test_sources() {
        // An entry that ends in `/` allows every file below that directory.
        if allowed
            .iter()
            .any(|a| *a == name || (a.ends_with('/') && name.starts_with(a)))
        {
            continue;
        }
        let text = fs::read_to_string(repo.join(&name)).unwrap();
        for (i, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if !code.starts_with("//")
                && code.contains(needle)
                && !exempt.iter().any(|e| code.contains(e))
            {
                hits.push(format!("{name}:{}: {code}", i + 1));
            }
        }
    }
    hits
}

/// #350: a test's scratch is [`support::scratch`]'s `NAME-PID-N`, which no other process makes.
/// A fixed name under the target directory's `tmp`, removed and made again by the test that owns
/// it, is deleted under a second run of the same binary over the same target directory (a hand
/// `cargo test` beside a queued gate). So only the helpers name that directory:
/// `tests/support/mod.rs` (`scratch`, its sweep and the mirror's shared download cache) and
/// `tests/support/podman.rs` (a test's Podman storage, which only `podman unshare` removes). A
/// line of code anywhere else under `tests/` that names it fails here; a check that an output does
/// not contain it builds no path and is let through.
#[test]
fn only_the_scratch_helpers_build_paths_under_the_target_tmpdir() {
    let var = concat!("CARGO_TARGET", "_TMPDIR");
    let leak_check = format!(".contains(env!(\"{var}\"))");
    let hits = code_naming(
        var,
        &["tests/support/mod.rs", "tests/support/podman.rs"],
        &[&leak_check],
    );
    assert!(
        hits.is_empty(),
        "these build a path under {var} by hand; take it from support::scratch (#350):\n{}",
        hits.join("\n")
    );
}

/// #355: the system temp directory is outside [`support::scratch`]'s per-process naming and its
/// dead-process sweep, so a killed run leaks what a test made there, and a fixed name there
/// collides between two runs. Only the helpers under `tests/support/` may name it; a line of code
/// anywhere else under `tests/` that does fails here.
#[test]
fn no_test_makes_scratch_in_the_system_temp_directory() {
    let call = concat!("temp", "_dir(");
    let hits = code_naming(call, &["tests/support/"], &[]);
    assert!(
        hits.is_empty(),
        "these make scratch in the system temp directory; take it from support::scratch \
         (#355):\n{}",
        hits.join("\n")
    );
}

/// #334: a Podman storage holds files of mapped ids, so only `podman unshare rm -rf` removes it,
/// and one attempt fails while a process of the storage is still ending (#327). The one removal
/// that waits for those processes, tries again and reports what is left at the ceiling is
/// `tests/support/podman.rs`'s; a suite that runs a removal of its own fails here.
#[test]
fn only_the_shared_podman_helper_removes_a_podman_storage() {
    let call = concat!("\"unshare\"", ", \"rm\"");
    let hits = code_naming(call, &["tests/support/podman.rs"], &[]);
    assert!(
        hits.is_empty(),
        "these remove a Podman storage unchecked; call support/podman.rs's remove_storage \
         (#334):\n{}",
        hits.join("\n")
    );
}

/// Design call D10, enforced rather than stated: neither this file nor `tests/faults_home.rs`
/// may wait on a wall clock. The name of that call is assembled here so that the scan cannot
/// match its own assertion.
#[test]
fn the_two_concurrency_files_never_wait_on_a_clock() {
    let banned = concat!("sle", "ep");
    for name in ["tests/concurrency.rs", "tests/faults_home.rs"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        let hits: Vec<usize> = text
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains(banned))
            .map(|(i, _)| i + 1)
            .collect();
        assert!(
            hits.is_empty(),
            "{name} waits on a wall clock at line(s) {hits:?}; design call D10 forbids it"
        );
    }
}
