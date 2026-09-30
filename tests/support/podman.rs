//! A test's own rootless Podman storage, and the one checked removal of it (#327, #334).
//!
//! Every container test has a storage of its own, `target/tmp/spike-podman-<pid>-<test>/`,
//! reached through `CONTAINERS_STORAGE_CONF`. Its image layers hold files of mapped ids, so only
//! `podman unshare rm -rf` removes it, and one attempt fails while a process of the storage
//! (`conmon`, `crun`, `podman container cleanup`) is still ending. [`remove_storage`] is the one
//! removal every suite uses: it waits for those processes, tries again until the storage is gone
//! and returns what is left at the suite's ceiling, Podman's error included. [`TestStorage`]
//! fails its test with that. Storage left by a test process that no longer exists (one killed
//! outright) is removed by the next run's first [`TestStorage::new`].
//!
//! The binary that includes this file declares `fixture`, `support` and `wait` at its root.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::{fixture, support, wait};

/// A Podman storage: its directory and the `storage.conf` that points Podman at it.
#[derive(Clone)]
pub struct Storage {
    pub dir: PathBuf,
    pub conf: PathBuf,
}

/// The first `podman` on this process's `PATH`.
pub fn podman_program() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join("podman"))
        .find(|p| p.is_file())
        .expect("these checks need rootless Podman on PATH (see the module documentation)")
}

/// Run podman against the storage `conf` configures.
pub fn podman_with(conf: &Path, args: &[&str]) -> Output {
    Command::new(podman_program())
        .args(args)
        .env("CONTAINERS_STORAGE_CONF", conf)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// A `storage.conf` whose graph and run roots are `graph/` and `run/` below `dir`.
fn conf_text(dir: &Path) -> String {
    format!(
        "[storage]\ndriver = \"overlay\"\ngraphroot = \"{}\"\nrunroot = \"{}\"\n",
        dir.join("graph").display(),
        dir.join("run").display()
    )
}

/// Every live process other than this one whose command line names a path inside `dir`: the
/// `conmon` of a container in the storage there, the `crun` it runs and the `podman container
/// cleanup` it starts when the container exits all carry the storage's paths in their arguments,
/// as `dir` spells it or with its links [`resolved`] (#544).
pub fn processes_of(dir: &Path) -> Vec<i32> {
    let mut needles = vec![format!("{}/", dir.display()).into_bytes()];
    if let Some(real) = resolved(dir)
        && real != dir
    {
        needles.push(format!("{}/", real.display()).into_bytes());
    }
    let me = std::process::id() as i32;
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|&pid| pid != me && !fixture::gone(pid))
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| {
                needles
                    .iter()
                    .any(|n| c.windows(n.len()).any(|w| w == n.as_slice()))
            })
        })
        .collect()
}

/// `dir` with every symlink resolved, or, once `dir` is removed, its parent resolved and its name
/// joined back; `None` when neither resolves. Podman resolves a storage's graph and run roots, so
/// the processes of a storage below a linked target directory (the release clone's `target/`)
/// name its real path, not the path through the link (#544).
fn resolved(dir: &Path) -> Option<PathBuf> {
    fs::canonicalize(dir).ok().or_else(|| {
        let parent = fs::canonicalize(dir.parent()?).ok()?;
        Some(parent.join(dir.file_name()?))
    })
}

/// The `conmon` and command pids of every container in the storage `conf` configures.
fn container_pids(conf: &Path) -> Vec<i32> {
    let ids = out(&podman_with(conf, &["ps", "--all", "--quiet"]));
    let ids: Vec<&str> = ids.split_whitespace().collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let mut args = vec!["inspect", "--format", "{{.State.ConmonPid}} {{.State.Pid}}"];
    args.extend(ids);
    out(&podman_with(conf, &args))
        .split_whitespace()
        .filter_map(|p| p.parse::<i32>().ok())
        .filter(|&p| p > 0)
        .collect()
}

/// Wait, with the suite's ceiling, until every process of the storage at `dir` — the pids in
/// `pids` and whatever names the storage — is gone, and return those still alive at the deadline.
/// It never panics, so that a test that is already unwinding can wait too. The pids that are gone
/// are dropped before each look for the storage's processes, not after it: a process of the
/// storage starts the next one before it ends (`conmon` starts `podman container cleanup` as the
/// container exits), so a look taken after a pid is seen gone finds that process, while a look
/// taken before could miss it and let the storage be removed under it (#327).
fn wait_processes_gone(dir: &Path, mut pids: Vec<i32>) -> Vec<i32> {
    let ceiling = wait::ceiling();
    let start = Instant::now();
    let mut step = Duration::from_millis(5);
    loop {
        pids.retain(|&pid| !fixture::gone(pid));
        for pid in processes_of(dir) {
            if !pids.contains(&pid) {
                pids.push(pid);
            }
        }
        if pids.is_empty() || start.elapsed() >= ceiling {
            return pids;
        }
        std::thread::sleep(step);
        step = (step * 2).min(Duration::from_millis(100));
    }
}

/// The `storage.conf` of this process's empty janitor storage, a [`support::scratch`] directory
/// (#350), which the removals run under: Podman mounts the graph root of the storage it is
/// configured with, which would keep that directory busy.
fn janitor_conf() -> &'static Path {
    static CONF: OnceLock<PathBuf> = OnceLock::new();
    CONF.get_or_init(|| {
        let dir = support::scratch("spike-podman-janitor");
        let conf = dir.join("storage.conf");
        fs::write(&conf, conf_text(&dir)).unwrap();
        conf
    })
}

/// Remove a test storage with `podman unshare rm -rf`, and return what is left of it at the
/// suite's ceiling: its processes still alive, and the storage itself if it still exists, with
/// Podman's own error. Every container in it is removed first, under the storage's own
/// configuration, and its `conmon`, its command and every process that names the storage are
/// waited for by pid, so that none of them outlives the storage. A removal that leaves the
/// storage in place (a Podman process of it still ending, a `podman unshare` that failed) is
/// tried again, after another wait for the storage's processes, until the storage is gone or the
/// ceiling has passed; it is never left behind unreported (#327, #334).
pub fn remove_storage(dir: &Path) -> Vec<String> {
    if !dir.exists() {
        return Vec::new();
    }
    let start = Instant::now();
    let own = dir.join("storage.conf");
    let mut pids = Vec::new();
    if own.is_file() {
        pids = container_pids(&own);
        let _ = podman_with(&own, &["rm", "--all", "--force", "--time", "0"]);
    }
    let conf = janitor_conf();
    let mut step = Duration::from_millis(50);
    loop {
        pids = wait_processes_gone(dir, pids);
        let removal = Command::new(podman_program())
            .args(["unshare", "rm", "-rf"])
            .arg(dir)
            .env("CONTAINERS_STORAGE_CONF", conf)
            .stdin(Stdio::null())
            .output();
        support::make_writable(dir);
        let _ = fs::remove_dir_all(dir);
        if !dir.exists() || start.elapsed() >= wait::ceiling() {
            let mut left: Vec<String> = pids.iter().map(|pid| format!("pid {pid}")).collect();
            if dir.exists() {
                let why = match removal {
                    Ok(o) => format!(
                        "{}: {}",
                        o.status,
                        String::from_utf8_lossy(&o.stderr).trim()
                    ),
                    Err(e) => e.to_string(),
                };
                left.push(format!("the storage itself (podman unshare rm -rf: {why})"));
            }
            return left;
        }
        std::thread::sleep(step);
        step = (step * 2).min(Duration::from_secs(1));
    }
}

/// Remove, once per test process, the storage of test processes that no longer exist (an
/// interrupted run): `spike-podman-<pid>-<test>`, or an earlier layout's `spike-podman-<pid>`.
fn remove_dead_storage() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        let Ok(entries) = fs::read_dir(env!("CARGO_TARGET_TMPDIR")) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let pid = name
                .strip_prefix("spike-podman-")
                .and_then(|p| p.split('-').next())
                .and_then(|p| p.parse::<i32>().ok());
            if pid.is_some_and(|p| !lodi::store::pid_alive(p)) {
                remove_storage(&e.path());
            }
        }
    });
}

/// A test's own Podman storage, `target/tmp/spike-podman-<pid>-<name>`, fresh and empty when the
/// test starts and torn down when it ends, on every exit path: a pass, a failed assertion and a
/// panic before the test's own checks alike ([`remove_storage`], LD-372). What is still there at
/// the deadline, a process or the storage itself, fails the test; a test that is already failing
/// is not failed twice: it is then reported on stderr instead.
pub struct TestStorage {
    own: Storage,
}

impl TestStorage {
    /// `name` names this test's storage; it is unique among the tests of its binary.
    pub fn new(name: &str) -> TestStorage {
        remove_dead_storage();
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("spike-podman-{}-{name}", std::process::id()));
        let left = remove_storage(&dir);
        assert!(left.is_empty(), "{} was left: {left:?}", dir.display());
        for d in ["graph", "run"] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        let conf = dir.join("storage.conf");
        fs::write(&conf, conf_text(&dir)).unwrap();
        TestStorage {
            own: Storage { dir, conf },
        }
    }

    pub fn storage(&self) -> &Storage {
        &self.own
    }

    /// The directory of this test's storage.
    pub fn dir(&self) -> &Path {
        &self.own.dir
    }

    /// The `storage.conf` that points Podman at this storage.
    pub fn conf(&self) -> &Path {
        &self.own.conf
    }

    /// Run podman against this storage.
    pub fn podman(&self, args: &[&str]) -> Output {
        podman_with(&self.own.conf, args)
    }
}

impl Drop for TestStorage {
    fn drop(&mut self) {
        let left = remove_storage(&self.own.dir);
        if left.is_empty() {
            return;
        }
        let message = format!(
            "what the test storage {} left after {:?}: {left:?}",
            self.own.dir.display(),
            wait::ceiling()
        );
        if std::thread::panicking() {
            eprintln!("{message}");
        } else {
            panic!("{message}");
        }
    }
}
