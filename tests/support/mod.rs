//! Shared helpers for the S-3 and S-4 checks: a raw tar writer (so that hostile archives can be
//! built byte by byte), synthetic tool archives, a loopback artifact server and project writers.
//! Everything here is synthetic, except the S-4 mirror ([`Server::mirror`]), which fetches the
//! real locked artifacts once and serves them from a cache below the target directory.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use lodi::lock::{
    ArtifactEntry, LOCK_FORMAT, LOCK_VERSION, LockFile, Profile, RecipeRef, ToolEntry, ToolRequest,
    ToolSpec,
};
use lodi::util::{sha256_hex, sha256_tagged};

/// One tar member.
#[derive(Debug, Clone)]
pub enum Kind {
    File(Vec<u8>, u32),
    Dir,
    Symlink(String),
    Hardlink(String),
    Fifo,
    CharDevice,
}

#[derive(Debug, Clone)]
pub struct Member {
    pub path: Vec<u8>,
    pub kind: Kind,
}

pub fn file(path: &str, body: &str, mode: u32) -> Member {
    Member {
        path: path.as_bytes().to_vec(),
        kind: Kind::File(body.as_bytes().to_vec(), mode),
    }
}

pub fn dir(path: &str) -> Member {
    Member {
        path: path.as_bytes().to_vec(),
        kind: Kind::Dir,
    }
}

pub fn symlink(path: &str, target: &str) -> Member {
    Member {
        path: path.as_bytes().to_vec(),
        kind: Kind::Symlink(target.into()),
    }
}

pub fn hardlink(path: &str, target: &str) -> Member {
    Member {
        path: path.as_bytes().to_vec(),
        kind: Kind::Hardlink(target.into()),
    }
}

fn octal(field: &mut [u8], value: u64) {
    let text = format!("{:0width$o}", value, width = field.len() - 1);
    field[..text.len()].copy_from_slice(text.as_bytes());
    field[field.len() - 1] = 0;
}

fn header(name: &[u8], typeflag: u8, size: u64, mode: u32, link: &[u8]) -> [u8; 512] {
    let mut h = [0u8; 512];
    let n = name.len().min(100);
    h[..n].copy_from_slice(&name[..n]);
    octal(&mut h[100..108], u64::from(mode));
    octal(&mut h[108..116], 1000);
    octal(&mut h[116..124], 1000);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], 1_700_000_000);
    h[156] = typeflag;
    let l = link.len().min(100);
    h[157..157 + l].copy_from_slice(&link[..l]);
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    let text = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(text.as_bytes());
    h
}

fn padded(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(data);
    let rem = data.len() % 512;
    if rem != 0 {
        out.extend(std::iter::repeat_n(0u8, 512 - rem));
    }
}

/// A tar stream of `members`, written byte by byte (GNU long-name members for paths over 100
/// bytes). No path is sanitized: hostile archives are the point.
pub fn tar(members: &[Member]) -> Vec<u8> {
    let mut out = Vec::new();
    for m in members {
        if m.path.len() > 100 {
            let mut long = m.path.clone();
            long.push(0);
            out.extend_from_slice(&header(
                b"././@LongLink",
                b'L',
                long.len() as u64,
                0o644,
                b"",
            ));
            padded(&mut out, &long);
        }
        match &m.kind {
            Kind::File(body, mode) => {
                out.extend_from_slice(&header(&m.path, b'0', body.len() as u64, *mode, b""));
                padded(&mut out, body);
            }
            Kind::Dir => out.extend_from_slice(&header(&m.path, b'5', 0, 0o755, b"")),
            Kind::Symlink(t) => {
                out.extend_from_slice(&header(&m.path, b'2', 0, 0o777, t.as_bytes()))
            }
            Kind::Hardlink(t) => {
                out.extend_from_slice(&header(&m.path, b'1', 0, 0o644, t.as_bytes()))
            }
            Kind::Fifo => out.extend_from_slice(&header(&m.path, b'6', 0, 0o644, b"")),
            Kind::CharDevice => out.extend_from_slice(&header(&m.path, b'3', 0, 0o644, b"")),
        }
    }
    out.extend(std::iter::repeat_n(0u8, 1024));
    out
}

pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

pub fn xz(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    lzma_rs::xz_compress(&mut BufReader::new(bytes), &mut out).unwrap();
    out
}

/// A synthetic python-build-standalone-shaped archive (`tar.gz`, strip 1).
pub fn python_archive(version: &str) -> Vec<u8> {
    gzip(&tar(&[
        dir("python/"),
        dir("python/bin/"),
        file(
            "python/bin/python3.12",
            &format!("#!/bin/sh\necho \"Python {version} (synthetic)\"\n"),
            0o755,
        ),
        symlink("python/bin/python3", "python3.12"),
        file("python/bin/pip3", "#!/bin/sh\necho pip synthetic\n", 0o755),
        dir("python/lib/"),
        file("python/lib/os.py", "# synthetic\n", 0o644),
    ]))
}

/// A synthetic nodejs.org-shaped archive (`tar.xz`, strip 1).
pub fn node_archive(version: &str) -> Vec<u8> {
    let top = format!("node-v{version}-linux-x64");
    xz(&tar(&[
        dir(&format!("{top}/")),
        dir(&format!("{top}/bin/")),
        file(
            &format!("{top}/bin/node"),
            &format!("#!/bin/sh\necho v{version}\n"),
            0o755,
        ),
        symlink(
            &format!("{top}/bin/npm"),
            "../lib/node_modules/npm/bin/npm-cli.js",
        ),
        symlink(
            &format!("{top}/bin/npx"),
            "../lib/node_modules/npm/bin/npx-cli.js",
        ),
        dir(&format!("{top}/lib/node_modules/npm/bin/")),
        file(
            &format!("{top}/lib/node_modules/npm/bin/npm-cli.js"),
            "#!/bin/sh\necho npm synthetic\n",
            0o755,
        ),
        file(
            &format!("{top}/lib/node_modules/npm/bin/npx-cli.js"),
            "#!/bin/sh\necho npx synthetic\n",
            0o755,
        ),
    ]))
}

/// A locked tool with its synthetic archive.
#[derive(Debug, Clone)]
pub struct Tool {
    pub label: String,
    pub constraint: String,
    pub version: String,
    pub url: String,
    pub format: String,
    pub bytes: Vec<u8>,
    pub bin: Vec<String>,
}

impl Tool {
    pub fn python(version: &str) -> Tool {
        Tool {
            label: "python".into(),
            constraint: "3.12".into(),
            version: version.into(),
            url: format!("https://artifacts.test/python/cpython-{version}-install_only.tar.gz"),
            format: "tar.gz".into(),
            bytes: python_archive(version),
            bin: vec!["bin/python3".into(), "bin/pip3".into()],
        }
    }

    pub fn node(constraint: &str, version: &str) -> Tool {
        Tool {
            label: "nodejs".into(),
            constraint: constraint.into(),
            version: version.into(),
            url: format!("https://artifacts.test/node/node-v{version}-linux-x64.tar.xz"),
            format: "tar.xz".into(),
            bytes: node_archive(version),
            bin: vec!["bin/node".into(), "bin/npm".into(), "bin/npx".into()],
        }
    }

    pub fn entry(&self) -> ToolEntry {
        ToolEntry {
            request: ToolRequest {
                constraint: self.constraint.clone(),
                name: self.label.clone(),
                provider: "binary".into(),
                declaration: None,
            },
            provider: "binary".into(),
            version: self.version.clone(),
            tag: None,
            recipe: RecipeRef {
                input: "builtin".into(),
                path: format!("{}.toml", self.label),
                sha256: sha256_tagged(format!("synthetic recipe {}", self.label).as_bytes()),
            },
            artifacts: vec![ArtifactEntry {
                url: self.url.clone(),
                sha256: sha256_tagged(&self.bytes),
                size: Some(self.bytes.len() as u64),
                format: self.format.clone(),
                strip_components: 1,
                subdir: String::new(),
                exclude: Vec::new(),
            }],
            spec: ToolSpec {
                arch: "x86_64".into(),
                bin: self.bin.clone(),
                path: vec!["bin".into()],
                env: BTreeMap::new(),
            },
        }
    }

    pub fn sha256_hex(&self) -> String {
        sha256_hex(&self.bytes)
    }
}

/// Write `lodi.toml` (as given) and a lock for `tools` into `dir`.
pub fn write_project(dir: &Path, manifest: &str, tools: &[Tool]) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("lodi.toml"), manifest).unwrap();
    let packages: BTreeMap<String, ToolEntry> =
        tools.iter().map(|t| (t.label.clone(), t.entry())).collect();
    let lock = LockFile {
        version: LOCK_VERSION,
        format: LOCK_FORMAT.into(),
        generated_by: "lodi 0.1.0".into(),
        manifest_hash: sha256_tagged(manifest.as_bytes()),
        base: None,
        profiles: BTreeMap::from([(
            "default".to_string(),
            Profile {
                packages: packages.keys().cloned().collect(),
            },
        )]),
        packages,
    };
    lodi::lock::validate(&lock).unwrap();
    fs::write(dir.join("lodi.lock"), lock.to_canonical_json()).unwrap();
}

/// A manifest with `[tools]` for `tools`, optional extra text (env, tasks).
pub fn manifest(tools: &[Tool], extra: &str) -> String {
    let mut text = String::from("[project]\nname = \"demo\"\n\n[tools]\n");
    for t in tools {
        text.push_str(&format!("{} = \"{}\"\n", t.label, t.constraint));
    }
    text.push('\n');
    text.push_str(extra);
    text
}

/// A fresh scratch directory under the target directory (never a home path), named
/// `NAME-PID-N`. The first call in a process removes the scratch directories of test processes
/// that have exited ([`sweep_dead_scratch`]; their stores are read-only, which `cargo clean`
/// alone could not remove), and arranges for this process's own to be removed when it exits
/// ([`remove_own_scratch_at_exit`], #348). It is every test's one way to a directory under the
/// target directory: a fixed name there is deleted under a second run of the same binary, and
/// `tests/concurrency.rs` fails on a path built there by hand (#350).
pub fn scratch(name: &str) -> PathBuf {
    static CLEAN: std::sync::Once = std::sync::Once::new();
    CLEAN.call_once(|| {
        sweep_dead_scratch(Path::new(env!("CARGO_TARGET_TMPDIR")));
        // SAFETY: registers a plain `extern "C"` function that takes no arguments and never
        // unwinds; `atexit` has no other precondition.
        unsafe { libc::atexit(remove_own_scratch_at_exit) };
    });
    scratch_in(Path::new(env!("CARGO_TARGET_TMPDIR")), name)
}

/// [`scratch`]'s fresh `NAME-PID-N` below `base`, itself a directory below the target directory,
/// spelled canonical (#427): the host gate canonicalizes `--root` and reports every path below
/// that spelling, so a `target/tmp` reached through a symbolic link must not give a test's root
/// another one.
pub fn scratch_in(base: &Path, name: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = base.join(format!(
        "{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    if dir.exists() {
        make_writable(&dir);
        fs::remove_dir_all(&dir).unwrap();
    }
    fs::create_dir_all(&dir).unwrap();
    fs::canonicalize(&dir).unwrap()
}

/// The mirror's cache of the real upstream files ([`Server::mirror`]), `target/tmp/spike-mirror/`:
/// the one fixed name a test uses under the target directory, shared by every run on purpose so
/// that each file is fetched once. No test removes it, and the mirror adds a file only whole, by
/// renaming a part file of its own process (#350). `LODI_TEST_MIRROR`, an absolute directory,
/// names another place for it: the gate sets one machine-wide cache, so a new checkout does not
/// fetch the same locked files again (#393).
pub fn mirror_cache() -> PathBuf {
    match std::env::var_os("LODI_TEST_MIRROR").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("spike-mirror"),
    }
}

/// Remove the directories below `root` that [`scratch`] made for a test process that has exited:
/// only the names `NAME-PID-N` ([`scratch_pid`]). The gate runs the test binaries at once (#308)
/// over one `CARGO_TARGET_TMPDIR`, so anything else there may be another binary's and in use
/// right now: a fixed-name scratch such as `surface-init-alias-0-import`, whose `0` names no live
/// process (#344), or a Podman storage `spike-podman-PID-TEST`, which its own test removes.
pub fn sweep_dead_scratch(root: &Path) {
    remove_scratch_where(root, |pid| !lodi::store::pid_alive(pid));
}

/// Remove, as the process exits, every scratch directory [`scratch`] made for it (#348). A store
/// makes its entries read-only, and the gate's last run of each test binary used to leave its
/// scratch behind until a later run's sweep: `git worktree remove` and a plain `rm -rf` of the
/// worktree then failed with `Permission denied`. The write bits are restored first
/// ([`make_writable`]). A process that is killed never gets here; the next run's
/// [`sweep_dead_scratch`] removes what it left. Registered with `atexit` by the first [`scratch`]
/// call, so it runs after the test harness has finished every test, on a pass and a failure alike.
extern "C" fn remove_own_scratch_at_exit() {
    let own = std::process::id();
    let _ = std::panic::catch_unwind(|| {
        remove_scratch_where(Path::new(env!("CARGO_TARGET_TMPDIR")), |pid| {
            u32::try_from(pid) == Ok(own)
        });
    });
}

/// Remove the directories below `root` named as [`scratch`] names them whose process id `pick`
/// selects, restoring the write bits of a read-only store first.
fn remove_scratch_where(root: &Path, pick: impl Fn(i32) -> bool) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(pid) = scratch_pid(&name)
            && pick(pid)
        {
            make_writable(&e.path());
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

/// The process id in a name [`scratch`] makes, `NAME-PID-N` with PID and N all digits and PID
/// above 0; `None` for any other name.
fn scratch_pid(name: &str) -> Option<i32> {
    let digits = |word: &str| !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit());
    let mut words = name.rsplitn(3, '-');
    let (n, pid, stem) = (words.next()?, words.next()?, words.next()?);
    if stem.is_empty() || !digits(n) || !digits(pid) {
        return None;
    }
    pid.parse().ok().filter(|&pid| pid > 0)
}

/// Removes its directory when dropped, a failed assertion's unwinding included, read-only store
/// entries and all.
pub struct RemoveOnDrop(pub PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        make_writable(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn make_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(m) = fs::symlink_metadata(path)
        && m.is_dir()
    {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
        if let Ok(entries) = fs::read_dir(path) {
            for e in entries.flatten() {
                make_writable(&e.path());
            }
        }
    }
}

/// The live-session roots under `gcroots/sessions`, by name.
///
/// `Store::add_session_root` publishes a root atomically through `.<pid>.tmp` (`src/store.rs`),
/// so a bare directory listing can catch that temporary name while another process is mid-write
/// and report a root that does not exist. The product's own reader, `Store::prune_sessions`,
/// already ignores any name that is not a pid; this is the same rule for the tests, and it is
/// why a session assertion here is not a race (M-1.0 T-4).
///
/// A harness that lists `gcroots/sessions` calls this rather than [`listing`], and every harness
/// in this repository that lists that directory does: `tests/concurrency.rs` (M-1.0 T-4), and
/// `tests/shell.rs`, `tests/spike_host.rs`, `tests/spike_faults.rs` and `tests/spike_container.rs`
/// (M-1.0 R-3, LD-262, closing the follow-up LD-257 left open). A new one does too — listing that
/// directory raw is the bug, not the exception.
pub fn session_listing(root: &Path) -> Vec<String> {
    listing(root)
        .into_iter()
        .filter(|name| !name.starts_with('.'))
        .collect()
}

/// Every path below `root`, relative, sorted.
pub fn listing(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            out.push(p.strip_prefix(root).unwrap().to_string_lossy().into_owned());
            if e.file_type().unwrap().is_dir() {
                walk(root, &p, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// What became of one request a [`Server`] logged (LD-390).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Served {
    /// Logged, and no answer begun: a mirror still fetching the file upstream, or a held request.
    Waiting,
    /// The answer is being written.
    Answering,
    /// A 200 whose every byte was written to a client that was still waiting when the answer
    /// began: one download.
    Complete,
    /// Anything else, and what: a 404, the mirror's 502 when its own upstream fetch failed, a
    /// client that had stopped waiting before the answer began, a body cut off or stalled.
    Failed(String),
}

/// Every request a [`Server`] logged, in arrival order, with what became of it (LD-390). The
/// request log says what a client asked for; this says which answers were whole downloads.
#[derive(Default)]
pub struct Ledger {
    /// The generation ([`Server::clear`] starts a new one) and its entries.
    entries: Mutex<(u64, Vec<(String, Served)>)>,
    changed: Condvar,
}

/// One request's place in the [`Ledger`].
#[derive(Clone, Copy)]
struct Entry {
    generation: u64,
    index: usize,
}

impl Ledger {
    fn open(&self, url: &str) -> Entry {
        let mut entries = self.entries.lock().unwrap();
        entries.1.push((url.to_string(), Served::Waiting));
        Entry {
            generation: entries.0,
            index: entries.1.len() - 1,
        }
    }

    /// Record `served` for `at`, unless the ledger was cleared since `at` was logged.
    fn set(&self, at: Entry, served: Served) {
        let mut entries = self.entries.lock().unwrap();
        if entries.0 == at.generation {
            entries.1[at.index].1 = served;
        }
        drop(entries);
        self.changed.notify_all();
    }

    fn clear(&self) {
        let mut entries = self.entries.lock().unwrap();
        entries.0 += 1;
        entries.1.clear();
    }
}

/// A loopback HTTP server for `https://<host>/<path>` URLs reached through
/// `LODI_FETCH_REWRITE=https://=http://127.0.0.1:<port>/`. It logs every request and records
/// what became of it ([`Server::served`]), can delay responses, can stall a URL after half of
/// its body until the client goes away, and can hold a request until its client gives up.
pub struct Server {
    pub port: u16,
    pub log: Arc<Mutex<Vec<String>>>,
    /// Signalled after every request is logged, so a test can **block** until a request it is
    /// waiting for has arrived instead of polling a clock (M-1.0 T-4, design call D10).
    pub logged: Arc<Condvar>,
    /// What became of every logged request (LD-390).
    pub ledger: Arc<Ledger>,
    pub files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    pub stall: Arc<Mutex<Vec<String>>>,
    pub stalled: Arc<AtomicBool>,
    pub delay: Arc<Mutex<Duration>>,
    /// [`Server::mirror`]: the cache directory of real upstream files.
    pub upstream: Arc<Mutex<Option<PathBuf>>>,
    /// URLs answered with 404 (a vanished upstream artifact), even in a mirror.
    pub missing: Arc<Mutex<Vec<String>>>,
    /// URLs whose next request is answered only after its client has stopped waiting, one
    /// request per entry: what the mirror does when its own upstream fetch outlasts the client's
    /// wait for the response headers, which LD-386 tries again (LD-390).
    pub outwait: Arc<Mutex<Vec<String>>>,
}

impl Server {
    pub fn start(files: BTreeMap<String, Vec<u8>>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = Server {
            port,
            log: Arc::new(Mutex::new(Vec::new())),
            logged: Arc::new(Condvar::new()),
            ledger: Arc::new(Ledger::default()),
            files: Arc::new(Mutex::new(files)),
            stall: Arc::new(Mutex::new(Vec::new())),
            stalled: Arc::new(AtomicBool::new(false)),
            delay: Arc::new(Mutex::new(Duration::ZERO)),
            upstream: Arc::new(Mutex::new(None)),
            missing: Arc::new(Mutex::new(Vec::new())),
            outwait: Arc::new(Mutex::new(Vec::new())),
        };
        let shared = server.shared();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let shared = shared.clone();
                std::thread::spawn(move || serve(stream, &shared));
            }
        });
        server
    }

    /// The handles a serving thread works with.
    fn shared(&self) -> Shared {
        Shared {
            log: Arc::clone(&self.log),
            logged: Arc::clone(&self.logged),
            ledger: Arc::clone(&self.ledger),
            files: Arc::clone(&self.files),
            stall: Arc::clone(&self.stall),
            stalled: Arc::clone(&self.stalled),
            delay: Arc::clone(&self.delay),
            upstream: Arc::clone(&self.upstream),
            missing: Arc::clone(&self.missing),
            outwait: Arc::clone(&self.outwait),
        }
    }

    /// A caching mirror of the real upstream: a URL not in `files` is served from
    /// `cache/<sha256 of the URL>`, which is fetched over HTTPS on first use (Lodi's own
    /// fetcher, no rewrite) and kept for later runs. The bytes are the real artifacts; the
    /// binary under test verifies each against its lock as it would from the network, and the
    /// request log counts what the binary asked for.
    pub fn mirror(cache: &Path) -> Server {
        fs::create_dir_all(cache).unwrap();
        let server = Server::start(BTreeMap::new());
        *server.upstream.lock().unwrap() = Some(cache.to_path_buf());
        server
    }

    pub fn for_tools(tools: &[Tool]) -> Server {
        Server::start(
            tools
                .iter()
                .map(|t| (t.url.clone(), t.bytes.clone()))
                .collect(),
        )
    }

    pub fn rewrite(&self) -> String {
        format!("https://=http://127.0.0.1:{}/", self.port)
    }

    pub fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    pub fn count(&self, url: &str) -> usize {
        self.requests().iter().filter(|u| *u == url).count()
    }

    pub fn clear(&self) {
        let mut log = self.log.lock().unwrap();
        log.clear();
        self.ledger.clear();
    }

    /// Every request logged since the last [`Server::clear`], in arrival order, with what became
    /// of it (LD-390), once no answer is still being written. An answer to a client that has gone
    /// ends at once, so `ceiling` is only the backstop against a hang; a request not yet answered
    /// is returned as [`Served::Waiting`]. A gate test counts downloads here, not requests
    /// ([`downloaded_once`]).
    pub fn served(&self, ceiling: Duration) -> Vec<(String, Served)> {
        let entries = self.ledger.entries.lock().unwrap();
        let (entries, waited) = self
            .ledger
            .changed
            .wait_timeout_while(entries, ceiling, |entries| {
                entries.1.iter().any(|(_, s)| *s == Served::Answering)
            })
            .unwrap();
        assert!(
            !waited.timed_out(),
            "an answer was still being written after {ceiling:?}: {:?}",
            entries.1
        );
        entries.1.clone()
    }

    /// Block until `url` has been requested at least `n` times. This is the deterministic
    /// rendezvous these tests use instead of waiting on a clock: the condition variable is
    /// signalled by the serving thread itself, so the call returns exactly when the `n`th
    /// request has been logged and not a moment later (M-1.0 T-4, design call D10).
    pub fn await_request(&self, url: &str, n: usize) {
        let mut log = self.log.lock().unwrap();
        while log.iter().filter(|u| *u == url).count() < n {
            log = self.logged.wait(log).unwrap();
        }
    }
}

/// The rule a gate test checks a cold run's downloads by (LD-390), over [`Server::served`]: every
/// URL of `expected` was served whole exactly once, and nothing else was asked for. Any other
/// request of an expected URL is an attempt that LD-386 tried again: it must come before the
/// whole download (a fetch is tried again only until it has its answer), there are fewer of
/// them than one fetch's attempts, and each is returned, named with what became of it — never
/// counted as a second download. A second whole download of a URL fails the count.
pub fn downloaded_once(
    served: &[(String, Served)],
    expected: &BTreeSet<String>,
) -> Result<Vec<String>, String> {
    let attempts = lodi::fetch::Retry::default().attempts as usize;
    let mut wrong = Vec::new();
    let mut retried = Vec::new();
    for (url, what) in served {
        if !expected.contains(url) {
            wrong.push(format!(
                "{url} was asked for ({what:?}), and it is not an expected download"
            ));
        }
    }
    for url in expected {
        let of: Vec<&Served> = served
            .iter()
            .filter(|(u, _)| u == url)
            .map(|(_, what)| what)
            .collect();
        let whole = of.iter().filter(|what| ***what == Served::Complete).count();
        let first_whole = of.iter().position(|what| **what == Served::Complete);
        match whole {
            1 => {}
            0 => wrong.push(format!("{url} was never served whole")),
            n => wrong.push(format!("{url} was downloaded {n} times")),
        }
        if let Some(first) = first_whole
            && of[first + 1..]
                .iter()
                .any(|what| **what != Served::Complete)
        {
            wrong.push(format!(
                "{url} was asked for again after its whole download"
            ));
        }
        let failed: Vec<String> = of
            .iter()
            .filter(|what| ***what != Served::Complete)
            .map(|what| match what {
                Served::Failed(why) => format!("{url}: {why}"),
                Served::Waiting => format!("{url}: never answered while its client waited"),
                other => format!("{url}: {other:?}"),
            })
            .collect();
        if failed.len() >= attempts {
            wrong.push(format!(
                "{url} failed {} times: one fetch makes at most {attempts} attempts",
                failed.len()
            ));
        }
        retried.extend(failed);
    }
    if wrong.is_empty() {
        Ok(retried)
    } else {
        Err(format!("{}\nevery request: {served:?}", wrong.join("\n")))
    }
}

/// The handles of a [`Server`] one serving thread works with.
#[derive(Clone)]
struct Shared {
    log: Arc<Mutex<Vec<String>>>,
    logged: Arc<Condvar>,
    ledger: Arc<Ledger>,
    files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    stall: Arc<Mutex<Vec<String>>>,
    stalled: Arc<AtomicBool>,
    delay: Arc<Mutex<Duration>>,
    upstream: Arc<Mutex<Option<PathBuf>>>,
    missing: Arc<Mutex<Vec<String>>>,
    outwait: Arc<Mutex<Vec<String>>>,
}

fn serve(mut stream: TcpStream, s: &Shared) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
            break;
        }
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let url = format!("https://{}", &path[1..]);
    let at = {
        let mut log = s.log.lock().unwrap();
        log.push(url.clone());
        s.ledger.open(&url)
    };
    s.logged.notify_all();
    let held = {
        let mut outwait = s.outwait.lock().unwrap();
        let found = outwait.iter().position(|u| *u == url);
        found.map(|i| outwait.remove(i)).is_some()
    };
    if held {
        // Answer nothing until the client closes the connection: it sends nothing more while it
        // waits, so the read returns when it gives up (the minute is a backstop).
        let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
        let mut sink = [0u8; 1];
        let _ = stream.read(&mut sink);
        let _ = stream.set_read_timeout(None);
    }
    std::thread::sleep(*s.delay.lock().unwrap());
    if s.missing.lock().unwrap().contains(&url) {
        refuse(
            &mut stream,
            &s.ledger,
            at,
            "404 Not Found",
            "answered 404 Not Found".into(),
        );
        return;
    }
    let body = s.files.lock().unwrap().get(&url).cloned();
    let cache = s.upstream.lock().unwrap().clone();
    if let (None, Some(cache)) = (&body, cache) {
        serve_mirrored(stream, &url, &cache, &s.ledger, at);
        return;
    }
    let Some(body) = body else {
        refuse(
            &mut stream,
            &s.ledger,
            at,
            "404 Not Found",
            "answered 404 Not Found".into(),
        );
        return;
    };
    if s.stall.lock().unwrap().contains(&url) {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        if stream.write_all(head.as_bytes()).is_err() {
            s.ledger
                .set(at, Served::Failed("the answer could not be written".into()));
            return;
        }
        let half = body.len() / 2;
        let _ = stream.write_all(&body[..half]);
        let _ = stream.flush();
        s.ledger.set(
            at,
            Served::Failed(format!(
                "stalled on purpose after {half} of {} bytes",
                body.len()
            )),
        );
        s.stalled.store(true, Ordering::SeqCst);
        // Hold the connection open until the client goes away (or a minute passes).
        let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
        let mut sink = [0u8; 1];
        let _ = stream.read(&mut sink);
        return;
    }
    deliver(
        &mut stream,
        &s.ledger,
        at,
        &mut body.as_slice(),
        body.len() as u64,
    );
}

/// Whether the client of `stream` has gone: it closed or reset the connection. A client that is
/// waiting for its answer sends nothing more, so a read that would block means it is still there.
fn client_gone(stream: &TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return false;
    }
    let mut byte = [0u8; 1];
    let gone = match stream.peek(&mut byte) {
        Ok(n) => n == 0,
        Err(e) => !matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        ),
    };
    let _ = stream.set_nonblocking(false);
    gone
}

/// Answer 200 with `body`, `len` bytes, and record what became of it (LD-390): [`Served::Complete`]
/// only when the client was still there as the answer began and every byte was written.
fn deliver<R: Read>(stream: &mut TcpStream, ledger: &Ledger, at: Entry, body: &mut R, len: u64) {
    ledger.set(at, Served::Answering);
    let gone = client_gone(stream);
    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n");
    let sent = stream
        .write_all(head.as_bytes())
        .and_then(|()| std::io::copy(body, stream));
    let served = match sent {
        _ if gone => {
            Served::Failed("the client had stopped waiting before the answer began".into())
        }
        Ok(n) if n == len => Served::Complete,
        Ok(n) => Served::Failed(format!("the body ended after {n} of {len} bytes")),
        Err(e) => Served::Failed(format!("the answer was cut off: {e}")),
    };
    ledger.set(at, served);
}

/// Answer `status` with no body, and record it as [`Served::Failed`] with `why`.
fn refuse(stream: &mut TcpStream, ledger: &Ledger, at: Entry, status: &str, why: String) {
    let _ = stream.write_all(
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
    );
    ledger.set(at, Served::Failed(why));
}

/// Serve `url` from the mirror cache, fetching it upstream first when it is not cached.
fn serve_mirrored(mut stream: TcpStream, url: &str, cache: &Path, ledger: &Ledger, at: Entry) {
    use lodi::fetch::Fetcher;
    let path = cache.join(sha256_hex(url.as_bytes()));
    if !path.exists() {
        static N: AtomicU64 = AtomicU64::new(0);
        let part = cache.join(format!(
            ".{}-{}.part",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let fetched = (|| {
            let mut file = fs::File::create(&part).map_err(|e| e.to_string())?;
            lodi::fetch::HttpFetcher::new(Vec::new())
                .download(url, &mut file, 4 << 30)
                .map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            fs::rename(&part, &path).map_err(|e| e.to_string())
        })();
        if let Err(e) = fetched {
            let _ = fs::remove_file(&part);
            let why = format!("answered 502: the mirror's own upstream fetch failed: {e}");
            refuse(&mut stream, ledger, at, "502 Bad Gateway", why);
            return;
        }
    }
    let Ok(mut file) = fs::File::open(&path) else {
        ledger.set(
            at,
            Served::Failed("the cached file could not be opened".into()),
        );
        return;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    deliver(&mut stream, ledger, at, &mut file, len);
}

/// A loopback port nothing listens on, kept for as long as this value lives (LD-393). Its
/// socket is bound and never listens: a connection to the port is refused, exactly as when
/// nothing is bound there, and the kernel gives the port to no other socket meanwhile. A port
/// bound only to learn its number and then released can be given to a concurrent test's
/// [`Server`], and a fetch meant to be refused would reach it. `std` binds a TCP socket only on
/// the way to listening on it, hence `libc`; `SO_REUSEADDR` stays off, so that not even a socket
/// that sets it can bind the port.
pub struct ClosedPort(TcpListener);

impl ClosedPort {
    pub fn bind() -> ClosedPort {
        use std::os::fd::{FromRawFd, OwnedFd};
        let loopback = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: 0,
            sin_addr: libc::in_addr {
                s_addr: u32::from(std::net::Ipv4Addr::LOCALHOST).to_be(),
            },
            sin_zero: [0; 8],
        };
        let size = std::mem::size_of_val(&loopback) as libc::socklen_t;
        // SAFETY: `socket` returns a new descriptor that `OwnedFd` then owns alone, and `bind` is
        // given a whole `sockaddr_in` and its size.
        let socket = unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
            let socket = OwnedFd::from_raw_fd(fd);
            let bound = libc::bind(fd, (&raw const loopback).cast(), size);
            assert_eq!(bound, 0, "bind: {}", std::io::Error::last_os_error());
            socket
        };
        // Bound and never listening: `std`'s type for the socket, whose `listen` is never called.
        ClosedPort(TcpListener::from(socket))
    }

    pub fn port(&self) -> u16 {
        self.0.local_addr().unwrap().port()
    }

    /// `LODI_FETCH_REWRITE` sending every HTTPS fetch to this port, where it is refused.
    pub fn rewrite(&self) -> String {
        format!("https://=http://127.0.0.1:{}/", self.port())
    }
}

// ------------------------------------------------ the home scope's scratch roots (M-0.5 T-1) ---

/// The one place a test process of this repository touches its **own** environment (design call
/// D15, `LD-111`). It allocates the process-wide root every [`HomeEnv`] lives under, points
/// `LODI_FS_LEDGER` at one ledger inside it, and remembers the ambient `HOME` so that
/// [`home_env`] can refuse to hand it to a child.
pub struct HomeGate {
    root: PathBuf,
    ledger: PathBuf,
    ambient_home: Option<PathBuf>,
}

impl HomeGate {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ledger(&self) -> &Path {
        &self.ledger
    }

    /// The ledger's lines, `(op, path)`, in the order they were appended.
    pub fn ledger_lines(&self) -> Vec<(String, PathBuf)> {
        let Ok(text) = fs::read_to_string(&self.ledger) else {
            return Vec::new();
        };
        text.lines()
            .map(|line| {
                let (op, path) = line
                    .split_once('\t')
                    .unwrap_or_else(|| panic!("a ledger line is `<op>\\t<path>`: {line:?}"));
                (op.to_string(), PathBuf::from(path))
            })
            .collect()
    }
}

/// Initialize the gate exactly once. Every test that touches the home scope calls this **first**,
/// so the single environment write below happens before any read anywhere in the process: the
/// `OnceLock` blocks every other caller until it has returned.
pub fn home_gate() -> &'static HomeGate {
    static GATE: std::sync::OnceLock<HomeGate> = std::sync::OnceLock::new();
    GATE.get_or_init(|| {
        let root = scratch("home-gate");
        let ledger = root.join("ledger.tsv");
        let ambient_home = std::env::var_os("HOME").map(PathBuf::from);
        // SAFETY: this closure is the only code in a test process of this repository that writes
        // to its own environment. `OnceLock::get_or_init` serializes it against every other
        // thread, and every environment read in these tests happens after a call to this
        // function, so no read can run concurrently with this write.
        unsafe { std::env::set_var("LODI_FS_LEDGER", &ledger) };
        HomeGate {
            root,
            ledger,
            ambient_home,
        }
    })
}

/// The one `W_DEPRECATED` line, newline included, that `lodi home plan`, `apply` and `status`
/// print first on standard error from 1.2 when `home.toml` uses `[files]` (decision D5, LD-324).
/// Its text is pinned in `tests/surface.rs`.
pub fn files_warning() -> String {
    let warning =
        lodi::surface::manifest_deprecation(lodi::surface::DEPRECATIONS, "home.toml", "[files]")
            .expect("the committed table has the D5 row");
    format!("{warning}\n")
}

/// A throwaway set of home-scope roots, and the only way these tests run the binary against one.
pub struct HomeEnv {
    root: PathBuf,
    home: PathBuf,
    config: PathBuf,
    data: PathBuf,
    decoy: PathBuf,
}

/// The decoy tree: a small fixed set of files with known content, beside the roots and named by
/// nothing Lodi writes. A changed listing or a changed hash is a write that escaped.
const DECOY: &[(&str, &str)] = &[
    ("keep.txt", "the decoy is never touched\n"),
    ("sub/deeper.txt", "nor is this\n"),
    (".dotfile", "nor this\n"),
];

/// Allocate `<gate>/<name>-<n>` with `home/`, `home/.config/`, `data/` and `decoy/` inside it.
pub fn home_env(name: &str) -> HomeEnv {
    static N: AtomicU64 = AtomicU64::new(0);
    let gate = home_gate();
    let root = gate
        .root
        .join(format!("{name}-{}", N.fetch_add(1, Ordering::Relaxed)));
    let _ = fs::remove_dir_all(&root);
    let home = root.join("home");
    let config = home.join(".config");
    let data = root.join("data");
    let decoy = root.join("decoy");
    for dir in [&home, &config, &data, &decoy] {
        fs::create_dir_all(dir).unwrap();
    }
    for (rel, body) in DECOY {
        let path = decoy.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
    }
    let env = HomeEnv {
        root,
        home,
        config,
        data,
        decoy,
    };
    let target = fs::canonicalize(env!("CARGO_TARGET_TMPDIR")).unwrap();
    assert!(
        env.home.starts_with(&target),
        "the scratch HOME {} is outside {}",
        env.home.display(),
        target.display()
    );
    assert!(
        gate.ambient_home.as_deref() != Some(env.home.as_path()),
        "the scratch HOME is the ambient one; no test of this repository runs against a real home"
    );
    env
}

impl HomeEnv {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn config(&self) -> &Path {
        &self.config
    }

    pub fn data(&self) -> &Path {
        &self.data
    }

    pub fn decoy(&self) -> &Path {
        &self.decoy
    }

    /// The built binary with **nothing** inherited: `env_clear`, then exactly the seven variables
    /// the home scope may see.
    pub fn command(&self) -> std::process::Command {
        self.environ(std::process::Command::new(env!("CARGO_BIN_EXE_lodi")))
    }

    /// [`HomeEnv::command`] started under `umask <mask>`, through `/bin/sh`: the binary and the
    /// seven variables are the same, only the file-creation mask differs. For the rules whose
    /// point is a mode that must not depend on the umask.
    pub fn command_under_umask(&self, mask: &str) -> std::process::Command {
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!("umask {mask} && exec \"$0\" \"$@\""))
            .arg(env!("CARGO_BIN_EXE_lodi"));
        self.environ(command)
    }

    fn environ(&self, mut command: std::process::Command) -> std::process::Command {
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_DATA_HOME", &self.data)
            .env("LODI_HOME", self.data.join("lodi"))
            .env("LODI_FS_LEDGER", home_gate().ledger())
            .env("SHELL", "/bin/sh")
            .current_dir(&self.root);
        command
    }

    /// The decoy tree as `<relative path> <sha256>` lines, sorted: an exact listing **and** the
    /// content, so neither a new file nor an edited one can pass.
    pub fn decoy_listing(&self) -> String {
        let mut lines = Vec::new();
        let mut stack = vec![self.decoy.clone()];
        while let Some(at) = stack.pop() {
            for entry in fs::read_dir(&at).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                let rel = path
                    .strip_prefix(&self.decoy)
                    .unwrap()
                    .display()
                    .to_string();
                if meta.is_dir() {
                    stack.push(path);
                    lines.push(format!("{rel} dir"));
                } else {
                    lines.push(format!(
                        "{rel} {}",
                        lodi::util::sha256_hex(&fs::read(&path).unwrap())
                    ));
                }
            }
        }
        lines.sort();
        lines.join("\n")
    }
}

/// A host report with the one `W_LEGACY_FILES` line a manifest with the legacy `[files]` table
/// prints (si-1) taken out, after asserting it was there exactly once.
pub fn without_legacy(report: &str) -> String {
    let (legacy, rest): (Vec<&str>, Vec<&str>) = report
        .split_inclusive('\n')
        .partition(|line| line.starts_with("W_LEGACY_FILES: "));
    assert_eq!(legacy.len(), 1, "one W_LEGACY_FILES line:\n{report}");
    assert!(legacy[0].contains("[etc.\"PATH\"]"), "{report}");
    rest.concat()
}
