//! Scratch roots for the host scope.
//!
//! Every host test in this repository runs against a directory below `CARGO_TARGET_TMPDIR` that
//! the test creates, arms and destroys. **No host code in this repository is ever run against
//! `/`** (`AGENTS.md` §8): `--root DIR` is the product's own documented flag, not a test seam,
//! and `python3 -B scripts/check-host-safety.py` is a gate step that fails when a committed
//! script or test acquires a way around it.
//!
//! No real package manager is ever executed here: this machine has none and must never get one
//! (`AGENTS.md` §8). The gate resolves them by bare name through `PATH`, so the tests put their
//! own inert shims first and that is the whole seam (design call D3). The suites that actually
//! drive a backend — `tests/host_apt.rs` and `tests/host_pacman.rs` — put their own recording
//! shims ahead of these.
#![allow(dead_code)]

/// The suite's one wait ceiling. This file is included by `#[path]`, so it takes the helper from
/// the declaration every test binary that uses it makes at its own root — one copy per binary.
use crate::wait;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Once;
use std::time::Duration;

use lodi::hostscope::Options;

/// The directory holding the package-manager shims, first on this process's `PATH`.
///
/// `PATH` is set exactly once per test binary, behind a `Once`, and every test that needs it
/// goes through this function, so no test observes it half-written.
pub fn shims() -> PathBuf {
    static SETUP: Once = Once::new();
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let dir = DIR
        .get_or_init(|| crate::support::scratch("host-shims"))
        .clone();
    SETUP.call_once(|| {
        fs::create_dir_all(&dir).expect("the shim directory");
        // One per program an apt root's safety gate requires. `pacman` is deliberately **not**
        // here: `tests/host_safety.rs` proves `E_NO_RUNTIME` with an Arch root on a machine that
        // has no pacman, and a shim on this `PATH` would quietly take that proof away. The
        // suites that drive pacman put their own recording shim ahead of this directory.
        for name in ["apt-get", "dpkg-query"] {
            let path = dir.join(name);
            fs::write(
                &path,
                "#!/bin/sh\n# An inert shim. This build runs no package manager.\nexit 0\n",
            )
            .expect("a shim");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("a shim mode");
        }
        let existing = std::env::var("PATH").unwrap_or_default();
        let joined = format!("{}:{existing}", dir.display());
        // SAFETY: every `PATH` access in this test binary goes through this `Once`, which is
        // complete before any caller returns from `shims()`.
        unsafe { std::env::set_var("PATH", joined) };
    });
    dir
}

thread_local! {
    /// Where [`Root::new`] puts the roots of this thread while a [`Traversable`] lives.
    static BASE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// A `lodi-tmp.*` directory of mode 0755 in a temporary directory every uid can reach, under which every
/// [`Root`] this test's thread makes goes while it lives. Another uid than the invoking user's,
/// such as the person a switch under sudo drops to in a user namespace, cannot reach the target
/// directory of a checkout below a home of mode 0700 (LD-541); it can reach this. It carries the
/// owner mark of `scripts/reap-lodi.py --mark` (`AGENTS.md` §6.1) and is removed when dropped.
pub struct Traversable {
    pub dir: PathBuf,
}

impl Traversable {
    pub fn new() -> Traversable {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let pid = std::process::id();
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // A development shell's `TMPDIR` is often a folder of mode 0700 too.
        let base = [
            std::env::temp_dir(),
            PathBuf::from("/var/tmp"),
            PathBuf::from("/tmp"),
        ]
        .into_iter()
        .find(|dir| reachable_by_anyone(dir))
        .expect("a temporary directory every uid can reach");
        let dir = base.join(format!("lodi-tmp.{pid}-{n}"));
        remove(&dir);
        fs::create_dir(&dir).expect("a directory in the temporary directory");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("its mode");
        let stat = fs::read_to_string("/proc/self/stat").expect("this process's stat");
        let start = stat[stat.rfind(')').expect("a stat line") + 2..]
            .split_whitespace()
            .nth(19)
            .expect("its start time")
            .to_string();
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").expect("the boot id");
        fs::write(
            dir.join(".lodi-owner"),
            format!(
                "{{\"boot_id\": \"{}\", \"pid\": {pid}, \"schema\": 1, \"start\": {start}}}\n",
                boot.trim()
            ),
        )
        .expect("the owner mark");
        let dir = fs::canonicalize(&dir).expect("the directory resolves");
        BASE.with_borrow_mut(|base| *base = Some(dir.clone()));
        Traversable { dir }
    }
}

/// Whether `dir` and every folder above it may be searched by any uid.
fn reachable_by_anyone(dir: &Path) -> bool {
    fs::canonicalize(dir).is_ok_and(|dir| {
        dir.ancestors().all(|d| {
            fs::metadata(d).is_ok_and(|m| m.is_dir() && m.permissions().mode() & 0o001 != 0)
        })
    })
}

impl Drop for Traversable {
    fn drop(&mut self) {
        BASE.with_borrow_mut(|base| *base = None);
        remove(&self.dir);
    }
}

/// A scratch root, created empty and removed when the test drops it.
pub struct Root {
    pub dir: PathBuf,
}

impl Drop for Root {
    fn drop(&mut self) {
        remove(&self.dir);
    }
}

/// Remove a scratch directory, a store's read-only entries included, so nothing a test leaves
/// blocks the removal of the checkout it ran in.
pub fn remove(dir: &Path) {
    fn writable(path: &Path) {
        if fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
            for e in fs::read_dir(path).into_iter().flatten().flatten() {
                writable(&e.path());
            }
        }
    }
    writable(dir);
    let _ = fs::remove_dir_all(dir);
}

impl Root {
    /// A fresh, empty root named after the test. `AGENTS.md` §6.1: scratch belongs under the
    /// target directory, never in a home path and never in `/tmp`.
    pub fn new(name: &str) -> Root {
        shims();
        let name = format!("host-{name}");
        let dir = match BASE.with_borrow(Clone::clone) {
            Some(base) => crate::support::scratch_in(&base, &name),
            None => crate::support::scratch(&name),
        };
        Root { dir }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel.trim_start_matches('/'))
    }

    pub fn write(&self, rel: &str, body: &str) -> &Root {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
        fs::write(&path, body).expect("a file");
        self
    }

    pub fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path(rel)).unwrap_or_default()
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.path(rel).symlink_metadata().is_ok()
    }

    pub fn chmod(&self, rel: &str, mode: u32) -> &Root {
        fs::set_permissions(self.path(rel), fs::Permissions::from_mode(mode)).expect("a mode");
        self
    }

    /// Write the "may manage" marker the product refuses without (#705). On a real machine
    /// `lodi import` writes it when the person says yes; here a test writes it for a root it
    /// owns, so that every other host test does not depend on that command.
    pub fn may_manage(&self) -> &Root {
        self.write(lodi::marker::MARKER, "")
    }

    /// Give the root the identity of a Debian 12 machine.
    pub fn debian(&self) -> &Root {
        self.write(
            "etc/os-release",
            "PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\nNAME=\"Debian GNU/Linux\"\n\
             VERSION_ID=\"12\"\nVERSION=\"12 (bookworm)\"\nVERSION_CODENAME=bookworm\n\
             ID=debian\nHOME_URL=\"https://www.debian.org/\"\n",
        )
    }

    /// Give the root the identity of an Arch machine, whose package manager the tests do not
    /// put on `PATH` — the case `E_NO_RUNTIME` exists for.
    pub fn arch(&self) -> &Root {
        self.write(
            "etc/os-release",
            "NAME=\"Arch Linux\"\nID=arch\nBUILD_ID=rolling\n",
        )
    }

    /// An armed Debian root with a host file in its config folder, [`CONFIG`]. This is what
    /// almost every test wants.
    pub fn debian_with(&self, manifest: &str) -> &Root {
        self.may_manage()
            .debian()
            .write(&format!("{CONFIG}/host.toml"), manifest)
    }

    /// The 2.0 form of a host part: the root, its config folder named, and the config's pins.
    pub fn options(&self) -> Options {
        options_for(&self.dir)
    }

    /// A named pipe inside the root's own scratch area, used as a `[files]` `source` so that a
    /// test can hold a real apply still at a real point in a real action and kill it there.
    /// Nothing in the product knows about it: `source` is read as a stream, as any file is.
    pub fn fifo(&self, rel: &str) -> PathBuf {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
        let status = Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo runs");
        assert!(status.success(), "mkfifo {}", path.display());
        path
    }
}

/// The uid and gid a file this test user creates actually gets, so that a manifest can declare
/// an owner this process is allowed to set. Declaring `root` here would be `E_NEED_ROOT`, which
/// is its own test — and a real `chown` to root only ever happens inside a disposable guest.
pub fn ids(root: &Root) -> (u32, u32) {
    let probe = root.path(".ids");
    fs::write(&probe, "").expect("a probe file");
    let meta = fs::metadata(&probe).expect("the probe's metadata");
    let ids = (meta.uid(), meta.gid());
    let _ = fs::remove_file(&probe);
    ids
}

/// `O_NONBLOCK` on Linux. Opening a fifo for writing with it fails with `ENXIO` while no
/// reader is there, which turns "wait for the process under test to reach this point" into a
/// poll that cannot hang the suite.
const O_NONBLOCK: i32 = 0o4000;

/// Open the writing end of a fifo, waiting until the process under test has opened the reading
/// end. That open is the signal that it has reached this point in this action.
pub fn open_writer(path: &Path) -> fs::File {
    use std::os::unix::fs::OpenOptionsExt;
    let start = std::time::Instant::now();
    let ceiling = wait::ceiling();
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .custom_flags(O_NONBLOCK)
            .open(path)
        {
            Ok(handle) => return handle,
            Err(_) if start.elapsed() < ceiling => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!(
                "no reader ever opened {}: {e}; {}",
                path.display(),
                wait::timed_out("that open", ceiling)
            ),
        }
    }
}

/// Deliver a whole value down the fifo and close it, so that the reader sees an end of file.
pub fn feed(path: &Path, body: &str) {
    let mut handle = open_writer(path);
    handle
        .write_all(body.as_bytes())
        .expect("the pipe takes it");
}

/// Wait for something the process under test does, rather than for a length of time.
pub fn wait_until(what: &str, ready: impl FnMut() -> bool) {
    wait::until(what, ready);
}

/// Every journal file in a root, oldest first.
pub fn journals(root: &Root) -> Vec<PathBuf> {
    let dir = root.path("var/lib/lodi/host/journal");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| {
            let when = fs::metadata(&p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (when, p)
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, p)| p).collect()
}

/// The one backup a replacing action keeps, if it has taken it yet.
pub fn backups(root: &Root) -> Vec<PathBuf> {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, out);
            } else if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains(".lodi-backup-"))
            {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    visit(&root.dir, &mut out);
    out.sort();
    out
}

/// Re-run this very test binary, in a child process, on one `#[test]` function.
///
/// The child is a real `lodi` apply in a real process, and the parent kills it with `SIGKILL`.
/// The product has no idea it is under a test: the child reads only the environment the test
/// binary itself reads, and the library entry point it calls is the one a command would call.
pub fn spawn(test: &str, envs: &[(&str, &str)]) -> Child {
    let exe = std::env::current_exe().expect("the test binary");
    let mut command = Command::new(exe);
    command
        .arg("--exact")
        .arg(test)
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env("LODI_TEST_CHILD", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    command.spawn().expect("the child starts")
}

/// Kill a child with `SIGKILL` and wait for it (`Child::kill` sends `SIGKILL` on Unix).
pub fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Whether this process is the child half of [`spawn`].
/// The scratch config folder of a root, relative to it: the folder `lodi switch` names as the
/// host part's, which a host file's `source` paths are read below.
pub const CONFIG: &str = "etc/lodi";

/// [`Root::options`] for the root at `dir`.
pub fn options_for(dir: &Path) -> Options {
    Options {
        root: Some(dir.to_path_buf()),
        source: Some(dir.join(CONFIG)),
        config: Some(lodi::hostscope::safety::ConfigPins::default()),
        ..Options::default()
    }
}

pub fn is_child() -> bool {
    std::env::var_os("LODI_TEST_CHILD").is_some()
}

/// The child half of every interruption test: run one real apply over the root the parent
/// named, print what it said, and exit with the status a command would exit with.
pub fn child_apply() {
    let root = PathBuf::from(std::env::var("LODI_TEST_ROOT").expect("the parent names a root"));
    let options = Options {
        resolved: std::env::var("LODI_TEST_RESOLVED").ok(),
        overwrite_drift: std::env::var_os("LODI_TEST_OVERWRITE_DRIFT").is_some(),
        // A positional `SOURCE`, when the parent names one (LD-379).
        source: std::env::var_os(SOURCE_VAR)
            .map(PathBuf::from)
            .or_else(|| Some(root.join(CONFIG))),
        ..options_for(&root)
    };
    if let (Some(name), Some(dir)) = (
        std::env::var_os(HOLD_NAME_VAR),
        std::env::var_os(HOLD_DIR_VAR),
    ) {
        hold::install(
            name.to_string_lossy().into_owned(),
            std::env::var_os(HOLD_OPEN_VAR).is_some(),
            std::env::var(HOLD_NTH_VAR)
                .ok()
                .and_then(|nth| nth.parse().ok())
                .unwrap_or(1),
            PathBuf::from(dir),
            wait::ceiling() * 2,
        );
    }
    match lodi::hostscope::apply(&options) {
        Ok(report) => {
            print!("{report}");
            std::process::exit(0);
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(i32::from(error.exit_status()));
        }
    }
}

/// The `PATH` every package manager the host scope starts gets, and the only one (LD-357).
pub const FIXED_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// Variables a POSIX shell exports of its own accord when it starts, whatever it inherited.
const SHELL_OWN: &[&str] = &["PWD", "OLDPWD", "SHLVL", "_"];

/// Check what each shim recorded with `export -p` before it set anything itself: every program
/// the host scope started inherited exactly the names in `fixed` (and what the shell adds of its
/// own), with `PATH` the fixed path — none of the caller's environment. `inherited` is the shims'
/// log, one `== <program>` header before each program's listing. Returns how many programs were
/// checked, so a caller can insist that there were some.
pub fn assert_fixed_environment(inherited: &str, fixed: &[&str]) -> usize {
    let mut checked = 0;
    for block in inherited.split("== ").filter(|b| !b.trim().is_empty()) {
        let (program, listing) = block.split_once('\n').unwrap_or((block, ""));
        let mut names = std::collections::BTreeSet::new();
        for line in listing.lines() {
            let Some(rest) = line.strip_prefix("export ") else {
                continue;
            };
            let name = rest.split('=').next().unwrap_or(rest).trim().to_string();
            if name == "PATH" {
                let value = rest["PATH=".len()..].trim_matches(|c| c == '"' || c == '\'');
                assert_eq!(value, FIXED_PATH, "{program} was given PATH={value}");
            }
            names.insert(name);
        }
        let extra: Vec<&String> = names
            .iter()
            .filter(|n| !fixed.contains(&n.as_str()) && !SHELL_OWN.contains(&n.as_str()))
            .collect();
        assert!(
            extra.is_empty(),
            "{program} inherited {extra:?} beyond the fixed set {fixed:?}:\n{listing}"
        );
        for name in fixed {
            assert!(
                names.contains(*name),
                "{program} was not given {name}:\n{listing}"
            );
        }
        checked += 1;
    }
    checked
}

// ------------------------------------------------------------------ holding an apply still ---

/// The file name whose write a [`Hold`] stops a child apply at.
pub const HOLD_NAME_VAR: &str = "LODI_TEST_HOLD_NAME";
/// The directory a held child says `HELD` in and waits for `RELEASE` in.
pub const HOLD_DIR_VAR: &str = "LODI_TEST_HOLD_DIR";
/// Set when a [`Hold`] stops at the first open of the name itself rather than at its write.
pub const HOLD_OPEN_VAR: &str = "LODI_TEST_HOLD_OPEN";
/// Which matching open a [`Hold`] stops at, counting from 1; the first when unset.
pub const HOLD_NTH_VAR: &str = "LODI_TEST_HOLD_NTH";
/// The positional `SOURCE` of a child apply.
pub const SOURCE_VAR: &str = "LODI_TEST_SOURCE";

/// A real apply, held still in its own process at one real point: the instant it opens the
/// temporary file its write of a managed path `name` goes through, before that open happens.
/// At that instant every earlier action is done, the journal says this action has begun, any
/// backup it keeps is on disk, and nothing of the new bytes is.
///
/// It replaces the named pipe these tests once used as a `[files]` `source` (LD-379): a source
/// is now read once, into memory, before the first plan, and only as a regular file, so no
/// pipe can hold an apply any more. The product knows nothing of this either. The child half
/// of [`spawn`] is this test binary; before it calls the library it asks the kernel, by a
/// seccomp user-notification filter on its own thread, to hand every `open` it makes to a
/// thread of its own, which lets each one through at once except that one, which it lets
/// through only once the test writes `RELEASE` — or the test kills the child, or the bound
/// passes. The directory is beside the root, never inside it, so no census of the root sees it.
pub struct Hold {
    pub dir: PathBuf,
    name: String,
    /// Stop at the first `open` of a path whose last component is `name`, whatever it opens:
    /// [`Hold::at_open_of`].
    open: bool,
    /// Stop at this matching open rather than the first: [`Hold::nth`].
    nth: usize,
}

impl Drop for Hold {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Hold {
    /// Hold a child apply over `root` at its write of the file called `name`.
    pub fn at_write_of(root: &Root, name: &str) -> Hold {
        let dir = PathBuf::from(format!("{}-hold", root.dir.display()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the hold's directory");
        Hold {
            dir,
            name: name.to_string(),
            open: false,
            nth: 1,
        }
    }

    /// Stop at the `nth` matching open instead of the first: an apply that writes one path
    /// twice — a source file put back after a refresh failed (LD-365) — held at the second.
    pub fn nth(mut self, nth: usize) -> Hold {
        self.nth = nth;
        self
    }

    /// Hold a child apply over `root` at its first `open` of a path whose last component is
    /// `name` — a directory on the way to a host as well as a file — before that open happens.
    pub fn at_open_of(root: &Root, name: &str) -> Hold {
        let mut hold = Hold::at_write_of(root, name);
        hold.open = true;
        hold
    }

    /// Start a child apply over `root`, with this hold on it.
    pub fn spawn(&self, root: &Root) -> Child {
        self.spawn_with(root, &[])
    }

    /// [`Hold::spawn`] with more of the child's environment.
    pub fn spawn_with(&self, root: &Root, extra: &[(&str, &str)]) -> Child {
        let root_dir = root.dir.display().to_string();
        let dir = self.dir.display().to_string();
        let mut envs: Vec<(&str, &str)> = vec![
            ("LODI_TEST_ROOT", &root_dir),
            (HOLD_NAME_VAR, &self.name),
            (HOLD_DIR_VAR, &dir),
        ];
        if self.open {
            envs.push((HOLD_OPEN_VAR, "1"));
        }
        let nth = self.nth.to_string();
        envs.push((HOLD_NTH_VAR, &nth));
        envs.extend_from_slice(extra);
        spawn("child_apply", &envs)
    }

    /// Wait until the child is held.
    pub fn wait_held(&self) {
        let held = self.dir.join("HELD");
        wait_until(
            &format!("the apply to reach its write of {}", self.name),
            || held.exists(),
        );
    }

    /// Let the child go on.
    pub fn release(&self) {
        fs::write(self.dir.join("RELEASE"), "").expect("the release");
    }
}

/// The child half of [`Hold`]. x86_64 is the one architecture the suite runs on (`AGENTS.md`
/// §3); elsewhere a hold is refused loudly rather than silently not held.
mod hold {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// Whether `file` is the temporary name a write of `name` opens: `.NAME.lodi-PID-N`.
    pub fn is_the_write_of(file: &str, name: &str) -> bool {
        let Some(rest) = file
            .strip_prefix('.')
            .and_then(|rest| rest.strip_prefix(name))
            .and_then(|rest| rest.strip_prefix(".lodi-"))
        else {
            return false;
        };
        let mut parts = rest.split('-');
        let digits = |part: Option<&str>| {
            part.is_some_and(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        };
        digits(parts.next()) && digits(parts.next()) && parts.next().is_none()
    }

    #[cfg(target_arch = "x86_64")]
    pub fn install(name: String, open: bool, nth: usize, dir: PathBuf, bound: Duration) {
        use std::sync::mpsc;
        // The thread that answers is made first, so the filter below never applies to it.
        let (tx, rx) = mpsc::channel::<i32>();
        std::thread::spawn(move || {
            let fd = rx.recv().expect("the listener");
            serve(fd, &name, open, nth, &dir, bound);
        });
        // SAFETY: plain system calls on this thread; every pointer outlives the call.
        unsafe {
            assert_eq!(
                libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0),
                0,
                "no_new_privs: {}",
                std::io::Error::last_os_error()
            );
            const ARCH_OFFSET: u32 = 4;
            const NR_OFFSET: u32 = 0;
            const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
            let stmt = |code: u32, k: u32| libc::sock_filter {
                code: code as u16,
                jt: 0,
                jf: 0,
                k,
            };
            let jump = |k: u32, jt: u8, jf: u8| libc::sock_filter {
                code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
                jt,
                jf,
                k,
            };
            let load = libc::BPF_LD | libc::BPF_W | libc::BPF_ABS;
            let ret = libc::BPF_RET | libc::BPF_K;
            let filter = [
                stmt(load, ARCH_OFFSET),
                jump(AUDIT_ARCH_X86_64, 1, 0),
                stmt(ret, libc::SECCOMP_RET_ALLOW),
                stmt(load, NR_OFFSET),
                jump(libc::SYS_openat as u32, 2, 0),
                jump(libc::SYS_open as u32, 1, 0),
                stmt(ret, libc::SECCOMP_RET_ALLOW),
                stmt(ret, libc::SECCOMP_RET_USER_NOTIF),
            ];
            let program = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_ptr() as *mut libc::sock_filter,
            };
            let fd = libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                libc::SECCOMP_FILTER_FLAG_NEW_LISTENER,
                &program as *const libc::sock_fprog,
            );
            assert!(
                fd >= 0,
                "the seccomp listener: {}",
                std::io::Error::last_os_error()
            );
            tx.send(fd as i32).expect("the answering thread");
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    pub fn install(_name: String, _open: bool, _nth: usize, _dir: PathBuf, _bound: Duration) {
        panic!("a hold needs x86_64, the one architecture this suite runs on");
    }

    #[cfg(target_arch = "x86_64")]
    fn serve(fd: i32, name: &str, open: bool, nth: usize, dir: &Path, bound: Duration) {
        let mut held = false;
        let mut seen = 0usize;
        loop {
            // SAFETY: both structures are plain data the kernel fills or reads.
            let mut request: libc::seccomp_notif = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::ioctl(
                    fd,
                    libc::SECCOMP_IOCTL_NOTIF_RECV as _,
                    &mut request as *mut libc::seccomp_notif,
                )
            };
            if rc < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return;
            }
            let pointer = if request.data.nr == libc::SYS_openat as i32 {
                request.data.args[1]
            } else {
                request.data.args[0]
            };
            // The caller is stopped inside its `open`: a thread of this process, or a program the
            // apply started (a package manager inherits the filter), whose memory is not this
            // one's. Its path is read through its own `/proc/PID/mem`, which is right for both
            // (LD-365: a source's refresh runs a package manager before the write a test holds).
            let path = caller_path(request.pid, pointer).unwrap_or_default();
            let path = path.as_path();
            if !held
                && path.file_name().is_some_and(|file| {
                    let file = file.to_string_lossy();
                    if open {
                        file == name
                    } else {
                        is_the_write_of(&file, name)
                    }
                })
                && {
                    seen += 1;
                    seen == nth
                }
            {
                held = true;
                let _ = std::fs::write(dir.join("HELD"), path.display().to_string());
                let start = Instant::now();
                while !dir.join("RELEASE").exists() && start.elapsed() < bound {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let mut response: libc::seccomp_notif_resp = unsafe { std::mem::zeroed() };
            response.id = request.id;
            response.flags = libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE as u32;
            unsafe {
                libc::ioctl(
                    fd,
                    libc::SECCOMP_IOCTL_NOTIF_SEND as _,
                    &mut response as *mut libc::seccomp_notif_resp,
                )
            };
        }
    }

    /// The NUL-terminated path at `pointer` in the memory of the stopped caller `pid`, or `None`
    /// when it cannot be read.
    #[cfg(target_arch = "x86_64")]
    fn caller_path(pid: u32, pointer: u64) -> Option<PathBuf> {
        use std::io::{Read, Seek, SeekFrom};
        let mut memory = std::fs::File::open(format!("/proc/{pid}/mem")).ok()?;
        memory.seek(SeekFrom::Start(pointer)).ok()?;
        let mut out = Vec::new();
        let mut chunk = [0u8; 256];
        while out.len() <= 4096 {
            let read = memory.read(&mut chunk).ok()?;
            if read == 0 {
                return None;
            }
            if let Some(end) = chunk[..read].iter().position(|byte| *byte == 0) {
                out.extend_from_slice(&chunk[..end]);
                return Some(PathBuf::from(
                    <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(out),
                ));
            }
            out.extend_from_slice(&chunk[..read]);
        }
        None
    }

    #[test]
    fn a_hold_matches_only_the_write_of_its_name() {
        assert!(is_the_write_of(".a.conf.lodi-123-0", "a.conf"));
        assert!(!is_the_write_of(
            ".a.conf.lodi-backup-1.lodi-123-0",
            "a.conf"
        ));
        assert!(!is_the_write_of(".b.conf.lodi-123-0", "a.conf"));
        assert!(!is_the_write_of("a.conf", "a.conf"));
        assert!(!is_the_write_of(".a.conf.lodi-123", "a.conf"));
    }
}
