//! M-Spike S-4: container environments through real rootless Podman, run as the real binary.
//!
//! These checks need `podman` on `PATH` working rootless for this user, and network access on
//! the first run: the real locked artifacts (the debuerreotype rootfs layer, the Debian
//! packages from snapshot.debian.org and the upstream Python and Node archives of
//! `tests/fixtures/spike/lodi.lock`) are fetched once into `target/tmp/spike-mirror/` by a
//! loopback mirror ([`Server::mirror`]) that the binary reaches through `LODI_FETCH_REWRITE`,
//! so that every request it makes is counted. A missing Podman or a Podman failure fails these
//! tests; nothing is mocked except where a test says so (the missing-runtime case uses a
//! `PATH` without `podman`).
//!
//! Every test has a Podman storage of its own, `target/tmp/spike-podman-<pid>-<test>/`, through
//! `CONTAINERS_STORAGE_CONF`, made by its [`Finish`] when it starts and torn down when it ends,
//! pass, failure or panic: every container in it is removed, its `conmon`, its command and every
//! process that names the storage are waited for by pid with the suite's ceiling, and the storage
//! is removed (with `podman unshare`, since image layers hold files of mapped ids), so nothing a
//! test starts in Podman outlives it (LD-372). Storage left by a test process that no longer
//! exists (one killed outright) is removed on the next run.
//! The removal is `tests/support/podman.rs`'s, shared with `tests/spike_faults.rs`, and runs under
//! an empty janitor storage of this process's own. The developer's own Podman
//! storage, home and containers are never touched. Every user of the binary has its own
//! `HOME` (a synthetic home, bound into the container by `home = "shared"`), `LODI_HOME` and
//! `XDG_CONFIG_HOME` under the target directory.

/// Long-lived children that end with their test, pass or panic (LD-372).
#[path = "support/fixture.rs"]
mod fixture;
/// A test's own Podman storage and the one checked removal of it (#334).
#[path = "support/podman.rs"]
mod podman;
mod support;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Instant;

use fixture::Fixture;
use lodi::lock::{LockFile, closure_hash, parse_lock};
use podman::{Storage, TestStorage, podman_program, podman_with, processes_of};
use support::*;

const LODI: &str = env!("CARGO_BIN_EXE_lodi");
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/spike");

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn both(o: &Output) -> String {
    format!(
        "status {:?}\n--- stdout\n{}--- stderr\n{}",
        o.status,
        out(o),
        err(o)
    )
}

fn host_path() -> OsString {
    std::env::var_os("PATH").unwrap_or_default()
}

thread_local! {
    /// The storage of the test running on this thread, set by its [`Finish`]. The test harness
    /// runs each test on a thread of its own.
    static CURRENT: RefCell<Option<Storage>> = const { RefCell::new(None) };
}

/// The storage of the test running on this thread.
fn storage() -> Storage {
    CURRENT.with(|c| c.borrow().clone()).expect(
        "a test that uses Podman storage creates its Finish first (Finish::new at its start)",
    )
}

/// A container test's own Podman storage ([`TestStorage`], `target/tmp/spike-podman-<pid>-<name>`),
/// the storage of the test running on this thread while it lives. It is torn down when the test
/// ends, on every exit path, by the one checked removal the suites share (`tests/support/podman.rs`):
/// every container of the storage is removed, each container's `conmon`, its command and every
/// process that names the storage are waited for by pid with the suite's ceiling, and the storage
/// is removed (LD-372). What is still there at the deadline fails the test.
struct Finish {
    own: TestStorage,
}

impl Finish {
    /// `name` names this test's storage; it is unique among the tests of this file.
    fn new(name: &str) -> Finish {
        let own = TestStorage::new(name);
        CURRENT.with(|c| *c.borrow_mut() = Some(own.storage().clone()));
        Finish { own }
    }

    /// The directory of this test's storage.
    fn dir(&self) -> &Path {
        self.own.dir()
    }
}

impl Drop for Finish {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = None);
    }
}

/// A mirror of its own for each user (the tests run in parallel and count requests), all
/// sharing one cache of the real upstream files.
fn mirror_server() -> Server {
    Server::mirror(&mirror_cache())
}

/// A cold run's downloads (LD-390): every URL of `expected` came through `mirror` whole exactly
/// once and nothing else was asked for. A request the binary made again because the first
/// failed — the mirror's own upstream fetch outlasting the binary's wait for the response
/// headers, or failing — is LD-386's retry: it is named here and not counted as a download.
fn each_downloaded_once(mirror: &Server, expected: &BTreeSet<String>) {
    let served = mirror.served(wait::ceiling());
    match downloaded_once(&served, expected) {
        Ok(retried) => {
            for attempt in retried {
                eprintln!("spike-container: a failed attempt LD-386 tried again: {attempt}");
            }
        }
        Err(e) => panic!("{e}"),
    }
}

/// Run podman against this test's storage.
fn podman(args: &[&str]) -> Output {
    podman_with(&storage().conf, args)
}

/// Containers of environment `name` (Lodi names them `lodi-<name>-<pid>-<n>`).
fn lodi_containers(name: &str) -> Vec<String> {
    let prefix = format!("lodi-{name}-");
    out(&podman(&["ps", "--all", "--format", "{{.Names}}"]))
        .lines()
        .filter(|n| n.starts_with(&prefix))
        .map(str::to_string)
        .collect()
}

/// One isolated user of the binary.
struct User {
    base: PathBuf,
    mirror: Server,
    /// `PATH` of the binary (the host's, or a recording `podman` before it).
    path: OsString,
    /// The Podman storage the binary uses: that of the test that made this user.
    storage_conf: PathBuf,
}

impl User {
    fn new(name: &str) -> User {
        let base = scratch(name);
        for d in ["home", "config", "lodi-home"] {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        User {
            base,
            mirror: mirror_server(),
            path: host_path(),
            storage_conf: storage().conf,
        }
    }

    /// Put a `podman` first on this user's `PATH` that logs its `TMPDIR` and argv (one line
    /// per call, to `podman-calls`) and then runs the real Podman with them unchanged.
    fn record_podman(&mut self) -> PathBuf {
        let dir = self.base.join("shim");
        fs::create_dir_all(&dir).unwrap();
        let log = self.base.join("podman-calls");
        let shim = dir.join("podman");
        fs::write(
            &shim,
            format!(
                "#!/bin/sh\n{{ printf '%s' \"${{TMPDIR-unset}}\"; for a in \"$@\"; do printf ' %s' \"$a\" \
                 | tr '\\n' ' '; done; echo; }} >> '{}'\nexec '{}' \"$@\"\n",
                log.display(),
                podman_program().display()
            ),
        )
        .unwrap();
        fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let mut path = OsString::from(dir.as_os_str());
        path.push(":");
        path.push(host_path());
        self.path = path;
        log
    }

    fn home(&self) -> PathBuf {
        self.base.join("home")
    }

    fn lodi_home(&self) -> PathBuf {
        self.base.join("lodi-home")
    }

    fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut c = Command::new(LODI);
        c.args(args)
            .current_dir(dir)
            .env_clear()
            .env("PATH", &self.path)
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.base.join("config"))
            .env("LODI_HOME", self.lodi_home())
            .env("LODI_FETCH_REWRITE", self.mirror.rewrite())
            .env("CONTAINERS_STORAGE_CONF", &self.storage_conf)
            .env("LODI_TRUST", "1")
            .stdin(Stdio::null());
        // CONTAINERS_CONF_OVERRIDE: the gate's cgroup placement inside lodi-work.slice (#367).
        for key in [
            "XDG_RUNTIME_DIR",
            "DBUS_SESSION_BUS_ADDRESS",
            "USER",
            "LOGNAME",
            "CONTAINERS_CONF_OVERRIDE",
        ] {
            if let Some(v) = std::env::var_os(key) {
                c.env(key, v);
            }
        }
        c
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.command(dir, args).output().unwrap()
    }

    fn run_with(&self, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = self.command(dir, args);
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    /// The image record of the store (exactly one is expected).
    fn image_record(&self) -> Option<serde_json::Value> {
        let meta = self.lodi_home().join("store/.meta");
        let entries = fs::read_dir(meta).ok()?;
        let mut records: Vec<serde_json::Value> = entries
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("img-"))
            .map(|e| serde_json::from_slice(&fs::read(e.path()).unwrap()).unwrap())
            .collect();
        assert!(records.len() <= 1, "{records:?}");
        records.pop()
    }

    fn sessions(&self) -> Vec<String> {
        session_listing(&self.lodi_home().join("gcroots/sessions"))
    }
}

fn fixture_lock() -> LockFile {
    parse_lock(&fs::read(Path::new(FIXTURE).join("lodi.lock")).unwrap()).unwrap()
}

/// The acceptance project: the fixture's manifest, lock, Makefile and C source.
fn openssl_project(user: &User) -> PathBuf {
    let project = user.base.join("spike-openssl");
    fs::create_dir_all(&project).unwrap();
    for f in ["lodi.toml", "lodi.lock", "Makefile", "openssl-version.c"] {
        fs::copy(Path::new(FIXTURE).join(f), project.join(f)).unwrap();
    }
    project
}

/// A small project derived from the fixture lock: the same pinned base and snapshot, the
/// requested `packages` (their closure: the base plus exactly `install`), no tools. `change`
/// may alter the closure before its hash is recomputed.
fn small_project(
    user: &User,
    name: &str,
    packages: &[&str],
    install: &[&str],
    change: impl FnOnce(&mut Vec<lodi::lock::ClosureEntry>),
) -> PathBuf {
    let mut lock = fixture_lock();
    lock.packages.clear();
    for p in lock.profiles.values_mut() {
        p.packages.clear();
    }
    let base = lock.base.as_mut().unwrap();
    let mut requested: Vec<String> = packages.iter().map(|s| s.to_string()).collect();
    requested.sort();
    base.requested.insert("default".into(), requested);
    let mut closure: Vec<_> = base
        .closure
        .packages
        .iter()
        .filter(|p| p.origin == "base" || install.contains(&p.name.as_str()))
        .cloned()
        .collect();
    assert_eq!(
        closure.iter().filter(|p| p.origin == "install").count(),
        install.len(),
        "every requested install package is in the fixture closure"
    );
    change(&mut closure);
    base.closure.hash = closure_hash(&closure);
    base.closure.packages = closure;
    let project = user.base.join(name);
    fs::create_dir_all(&project).unwrap();
    let list: Vec<String> = packages.iter().map(|p| format!("\"{p}\"")).collect();
    fs::write(
        project.join("lodi.toml"),
        format!(
            "[project]\nname = \"{name}\"\n\n[container]\ndistro = \"debian\"\n\
             release = \"bookworm\"\nsnapshot = \"{}\"\n\n[packages]\ncommon = [{}]\n",
            lock.base.as_ref().unwrap().snapshot,
            list.join(", ")
        ),
    )
    .unwrap();
    fs::write(project.join("lodi.lock"), lock.to_canonical_json()).unwrap();
    // The derived lock is fresh: entering keeps it as it is (LD-496).
    if let Err(f) = lodi::lock::frozen(&project) {
        panic!("derived lock: {f}");
    }
    project
}

fn url_of(lock: &LockFile, package: &str) -> String {
    let base = lock.base.as_ref().unwrap();
    let p = base
        .closure
        .packages
        .iter()
        .find(|p| p.name == package)
        .unwrap();
    let repo = base
        .repositories
        .iter()
        .find(|r| Some(&r.name) == p.repository.as_ref())
        .unwrap();
    format!("{}{}", repo.url, p.filename.as_ref().unwrap())
}

/// The installed packages of `image` read independently of Lodi: `name -> (version, arch)`.
fn installed(image: &str) -> BTreeMap<String, (String, String)> {
    let o = podman(&[
        "run",
        "--rm",
        "--network=none",
        "--entrypoint",
        "dpkg-query",
        image,
        "-W",
        "-f",
        "${db:Status-Abbrev} ${Package} ${Version} ${Architecture}\n",
    ]);
    assert!(o.status.success(), "{}", both(&o));
    out(&o)
        .lines()
        .filter(|l| l.starts_with("ii "))
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f[1].to_string(), (f[2].to_string(), f[3].to_string()))
        })
        .collect()
}

/// The `[container] snapshot` the Arch case pins: one fixed past day, so the mirror's cache of
/// the real archive stays valid across runs and the test is not a function of today's date.
const ARCH_SNAPSHOT: &str = "2026-09-01T00:00:00Z";

/// The installed packages of an Arch `image`, read independently of Lodi: `name -> version`.
fn installed_pacman(image: &str) -> BTreeMap<String, String> {
    let o = podman(&[
        "run",
        "--rm",
        "--network=none",
        "--entrypoint",
        "pacman",
        image,
        "-Q",
    ]);
    assert!(o.status.success(), "{}", both(&o));
    out(&o)
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect()
}

/// The package names of a `pacman -Qi` record stream, as the generated build recorded the base
/// set inside the image before it installed anything.
fn recorded_names(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|l| l.split_once(" : "))
        .filter(|(k, _)| k.trim_end() == "Name")
        .map(|(_, v)| v.trim().to_string())
        .collect()
}

/// M-Arch-Base T-6, acceptance (a), (b) and (c): an Arch project locked from the real archive
/// builds offline from the pinned bootstrap rootfs and the verified package files, enters and
/// runs the requested package, reads its closure back, and a second entry downloads and builds
/// nothing. The rootfs and the package files come through the same counting mirror as the
/// Debian cases, so every request is counted and the second run makes none.
#[test]
fn an_arch_project_builds_offline_from_its_lock_and_a_warm_entry_downloads_nothing() {
    const NAME: &str = "spike-arch";
    let _finish = Finish::new("arch");
    let user = User::new("container-arch");
    let project = user.base.join(NAME);
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("lodi.toml"),
        format!(
            "[project]\nname = \"{NAME}\"\n\n[container]\ndistro = \"arch\"\n\
             release = \"rolling\"\nsnapshot = \"{ARCH_SNAPSHOT}\"\n\n\
             [packages]\ncommon = [\"jq\"]\n"
        ),
    )
    .unwrap();

    // Lock: metadata only. No rootfs, no package file, no signature and no keyring is fetched.
    // `develop` locks by itself (LD-496); with no Podman on `PATH` it stops right after.
    let empty = user.base.join("empty-path");
    fs::create_dir_all(&empty).unwrap();
    user.mirror.clear();
    let o = user.run_with(
        &project,
        &["develop", "--", "true"],
        &[("PATH", empty.to_str().unwrap())],
    );
    assert_eq!(o.status.code(), Some(7), "{}", both(&o));
    assert!(
        err(&o).contains("lodi: wrote lodi.lock (resolved base)"),
        "{}",
        both(&o)
    );
    for url in user.mirror.requests() {
        assert!(
            !url.ends_with(".pkg.tar.zst") && !url.ends_with(".sig") && !url.contains("bootstrap"),
            "locking fetched {url}"
        );
    }
    let lock = parse_lock(&fs::read(project.join("lodi.lock")).unwrap()).unwrap();
    let base = lock.base.as_ref().unwrap();
    assert_eq!(
        (base.distro.as_str(), base.release.as_str()),
        ("arch", "rolling")
    );
    assert_eq!(base.deb_arch, "x86_64", "the family's own vocabulary (D11)");
    assert!(base.pinned);
    assert_eq!(base.requested["default"], ["jq"]);
    assert_eq!(
        base.repositories
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        ["core", "extra"]
    );
    assert!(
        base.closure.packages.iter().all(|p| p.origin == "install"),
        "an Arch lock names only what it resolved: {:?}",
        base.closure.packages
    );
    assert!(base.closure.packages.iter().any(|p| p.name == "jq"));

    // Cold: exactly the locked rootfs and the locked package files, each fetched once, then an
    // offline build, and the requested package runs inside the environment.
    user.mirror.clear();
    let start = Instant::now();
    let o = user.run(&project, &["develop", "--", "jq", "--version"]);
    let cold = start.elapsed();
    assert_eq!(o.status.code(), Some(0), "{}", both(&o));
    assert!(
        err(&o).contains("building the container image"),
        "{}",
        both(&o)
    );
    let jq = base
        .closure
        .packages
        .iter()
        .find(|p| p.name == "jq")
        .unwrap();
    let upstream = jq
        .version
        .rsplit_once('-')
        .map_or(jq.version.as_str(), |(v, _)| v);
    assert!(
        out(&o).contains(&format!("jq-{upstream}")),
        "the locked jq {} did not report itself: {}",
        jq.version,
        both(&o)
    );
    let expected: BTreeSet<String> = base
        .closure
        .packages
        .iter()
        .map(|p| url_of(&lock, &p.name))
        .chain([base.rootfs.url.clone()])
        .collect();
    each_downloaded_once(&user.mirror, &expected);

    // The image's installed set equals the lock's closure together with the base set the build
    // recorded inside the image before installing anything, read back independently of Lodi.
    let record = user.image_record().expect("an image record");
    assert_eq!(record["complete"], true);
    assert_eq!(record["closureHash"], base.closure.hash);
    let tag = record["tag"].as_str().unwrap().to_string();
    let packages = installed_pacman(&tag);
    for p in &base.closure.packages {
        assert_eq!(packages.get(&p.name), Some(&p.version), "{}", p.name);
    }
    let o = podman(&[
        "run",
        "--rm",
        "--network=none",
        "--entrypoint",
        "cat",
        &tag,
        "/lodi-base-packages",
    ]);
    assert!(o.status.success(), "the recorded base set: {}", both(&o));
    let recorded = recorded_names(&out(&o));
    assert!(
        recorded.len() > 100,
        "{} recorded base packages",
        recorded.len()
    );
    let locked: BTreeSet<String> = base
        .closure
        .packages
        .iter()
        .map(|p| p.name.clone())
        .collect();
    assert_eq!(
        packages.keys().cloned().collect::<BTreeSet<_>>(),
        recorded.union(&locked).cloned().collect::<BTreeSet<_>>(),
        "installed is exactly the recorded base plus the lock"
    );

    // Warm: no request, no build, and it still works with every fetch sent to a closed port.
    user.mirror.clear();
    let start = Instant::now();
    let o = user.run(&project, &["develop", "--", "jq", "--version"]);
    let warm = start.elapsed();
    assert_eq!(o.status.code(), Some(0), "{}", both(&o));
    assert!(!err(&o).contains("building"), "{}", both(&o));
    assert!(
        user.mirror.requests().is_empty(),
        "{:?}",
        user.mirror.requests()
    );
    let o = user.run_with(
        &project,
        &["develop", "--", "jq", "--version"],
        &[("LODI_FETCH_REWRITE", "https://=http://127.0.0.1:9/")],
    );
    assert_eq!(o.status.code(), Some(0), "network denied: {}", both(&o));
    assert_eq!(user.image_record().unwrap()["imageId"], record["imageId"]);
    assert!(
        warm < cold,
        "warm {warm:?} was not faster than cold {cold:?}"
    );
    assert!(
        lodi_containers(NAME).is_empty(),
        "{:?}",
        lodi_containers(NAME)
    );
    assert!(user.sessions().is_empty());
    // The project directory holds the manifest and the lock and nothing else: an Arch
    // environment writes no file of its own here.
    assert_eq!(listing(&project), ["lodi.lock", "lodi.toml"]);
}

#[test]
fn the_openssl_fixture_builds_and_runs_with_python_and_a_warm_entry_downloads_nothing() {
    const NAME: &str = "spike-openssl";
    let _finish = Finish::new("openssl");
    let mut user = User::new("container-openssl");
    let calls = user.record_podman();
    let project = openssl_project(&user);
    let lock = fixture_lock();
    let base = lock.base.as_ref().unwrap();
    let before = listing(&project);

    // Cold: every locked artifact is fetched exactly once, the image is built and verified,
    // and `make` builds and runs the libssl-linked program inside the container.
    let start = Instant::now();
    user.mirror.clear();
    let o = user.run(&project, &["develop", "--", "make"]);
    let cold = start.elapsed();
    assert_eq!(o.status.code(), Some(0), "{}", both(&o));
    assert!(
        err(&o).contains("building the container image"),
        "{}",
        both(&o)
    );
    let expected: BTreeSet<String> = base
        .closure
        .packages
        .iter()
        .filter(|p| p.origin == "install")
        .map(|p| url_of(&lock, &p.name))
        .chain([base.rootfs.url.clone()])
        .chain(lock.packages.values().map(|t| t.artifacts[0].url.clone()))
        .collect();
    each_downloaded_once(&user.mirror, &expected);

    // The program printed the OpenSSL version of the locked libssl3, and ldd resolves libssl
    // and libcrypto inside the realized environment (not on this host).
    let libssl = base
        .closure
        .packages
        .iter()
        .find(|p| p.name == "libssl3")
        .unwrap();
    let upstream = libssl.version.split('-').next().unwrap();
    let stdout = out(&o);
    assert!(
        stdout.contains(&format!("runtime: OpenSSL {upstream} ")),
        "locked libssl3 {} not reported: {stdout}",
        libssl.version
    );
    assert!(
        stdout.contains(&format!("headers: OpenSSL {upstream} ")),
        "{stdout}"
    );
    for lib in ["libssl.so.3", "libcrypto.so.3"] {
        assert!(
            stdout.contains(&format!("{lib} => /lib/x86_64-linux-gnu/{lib}")),
            "{stdout}"
        );
    }
    // Build products are below .lodi, owned by the caller (user = same).
    let program = project.join(".lodi/build/openssl-version");
    let meta = fs::metadata(&program).unwrap();
    assert_eq!(meta.uid(), unsafe { libc::getuid() });
    let after = listing(&project);
    let added: Vec<&String> = after.iter().filter(|p| !before.contains(p)).collect();
    assert_eq!(
        added,
        [".lodi", ".lodi/build", ".lodi/build/openssl-version"],
        "the fixture writes only below .lodi"
    );

    // The image's installed closure equals the lock, read independently of Lodi.
    let record = user.image_record().expect("an image record");
    assert_eq!(record["complete"], true);
    assert_eq!(record["closureHash"], base.closure.hash);
    let tag = record["tag"].as_str().unwrap().to_string();
    let packages = installed(&tag);
    let locked: BTreeMap<String, String> = base
        .closure
        .packages
        .iter()
        .map(|p| (p.name.clone(), p.version.clone()))
        .collect();
    assert_eq!(
        packages
            .iter()
            .map(|(k, (v, _))| (k.clone(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
        locked
    );

    // Python 3.12 and Node 22 from the lock run inside the container.
    let o = user.run(&project, &["run", "versions"]);
    assert_eq!(o.status.code(), Some(0), "{}", both(&o));
    let python = &lock.packages["python"].version;
    let node = &lock.packages["nodejs"].version;
    assert!(python.starts_with("3.12."), "{python}");
    assert!(
        out(&o).contains(&format!("Python {python}")),
        "{}",
        both(&o)
    );
    assert!(out(&o).contains(&format!("v{node}")), "{}", both(&o));

    // Warm: no request at all, no build; with every fetch sent to a closed port it still works.
    user.mirror.clear();
    let start = Instant::now();
    let o = user.run(&project, &["develop", "--", "make"]);
    let warm = start.elapsed();
    assert_eq!(o.status.code(), Some(0), "{}", both(&o));
    assert!(!err(&o).contains("building"), "{}", both(&o));
    assert!(!err(&o).contains("realized"), "{}", both(&o));
    assert!(
        user.mirror.requests().is_empty(),
        "{:?}",
        user.mirror.requests()
    );
    let o = user.run_with(
        &project,
        &["develop", "--", "make"],
        &[("LODI_FETCH_REWRITE", "https://=http://127.0.0.1:9/")],
    );
    assert_eq!(o.status.code(), Some(0), "network denied: {}", both(&o));
    assert_eq!(user.image_record().unwrap()["imageId"], record["imageId"]);
    assert!(
        lodi_containers(NAME).is_empty(),
        "{:?}",
        lodi_containers(NAME)
    );
    assert!(user.sessions().is_empty());

    // Every Podman command of the cold build, the closure check and the warm entries recorded
    // no events (the flag before the subcommand), kept its temporary image copies in LODI_HOME
    // (TMPDIR, the documented override of containers.conf image_copy_tmp_dir, default
    // /var/tmp), and every `run` stored no output (LD-24, LD-25).
    let calls = fs::read_to_string(&calls).unwrap();
    let tmp = user.lodi_home().join("store/tmp");
    let mut subcommands = BTreeSet::new();
    for call in calls.lines() {
        let (tmpdir, args) = call.split_once(' ').unwrap();
        let tmpdir = Path::new(tmpdir);
        assert!(
            tmpdir.parent() == Some(tmp.as_path())
                && tmpdir
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .ends_with("-podman"),
            "TMPDIR of {call:?}"
        );
        let args: Vec<&str> = args.split(' ').collect();
        assert_eq!(args[0], "--events-backend=none", "{call:?}");
        if args[1] == "run" {
            assert!(args.contains(&"--log-driver=none"), "{call:?}");
        }
        subcommands.insert(args[1].to_string());
    }
    for want in ["import", "build", "run", "image", "untag"] {
        assert!(subcommands.contains(want), "no {want} in {subcommands:?}");
    }
    // The temporary directory is removed when the entry ends.
    let left: Vec<String> = listing(&tmp);
    assert!(left.is_empty(), "{left:?}");
    // And this Podman takes TMPDIR over its image_copy_tmp_dir.
    let o = Command::new(podman_program())
        .args([
            "--events-backend=none",
            "info",
            "--format",
            "{{.Store.ImageCopyTmpDir}}",
        ])
        .env("CONTAINERS_STORAGE_CONF", &storage().conf)
        .env("TMPDIR", &tmp)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out(&o).trim(), tmp.to_str().unwrap(), "{}", both(&o));
    eprintln!("spike-container: openssl cold {cold:?}, warm {warm:?}");
}

#[test]
fn rebuilds_verify_the_same_closure_and_tampered_or_missing_caches_never_pass() {
    const NAME: &str = "rebuild";
    let _finish = Finish::new("rebuild");
    let user = User::new("container-rebuild");
    let project = small_project(&user, NAME, &["xz-utils"], &["xz-utils"], |_| {});
    let lock = parse_lock(&fs::read(project.join("lodi.lock")).unwrap()).unwrap();
    let rootfs = lock.base.as_ref().unwrap().rootfs.url.clone();
    let xz = url_of(&lock, "xz-utils");
    let xz_sha = lock
        .base
        .as_ref()
        .unwrap()
        .closure
        .packages
        .iter()
        .find(|p| p.name == "xz-utils")
        .unwrap()
        .sha256
        .clone()
        .unwrap();
    let cached_xz = user
        .lodi_home()
        .join("cache/dl")
        .join(&xz_sha["sha256:".len()..]);
    let enter = |what: &str| {
        user.mirror.clear();
        let o = user.run(&project, &["develop", "--", "xz", "--version"]);
        assert_eq!(o.status.code(), Some(0), "{what}: {}", both(&o));
        assert!(out(&o).contains("xz (XZ Utils)"), "{what}: {}", both(&o));
        o
    };

    let o = enter("cold");
    assert!(err(&o).contains("building"));
    assert_eq!(
        user.mirror.requests().into_iter().collect::<BTreeSet<_>>(),
        [rootfs.clone(), xz.clone()].into_iter().collect()
    );
    let first = user.image_record().unwrap();
    let tag = first["tag"].as_str().unwrap().to_string();

    // A missing image is rebuilt from the verified download cache: no request, and the new
    // image's closure is verified to be the same locked closure (not the same bytes).
    assert!(podman(&["rmi", "--force", &tag]).status.success());
    let o = enter("image missing");
    assert!(err(&o).contains("building"), "{}", both(&o));
    assert!(
        user.mirror.requests().is_empty(),
        "{:?}",
        user.mirror.requests()
    );
    let second = user.image_record().unwrap();
    assert_eq!(second["closureHash"], first["closureHash"]);
    assert_eq!(second["identity"], first["identity"]);
    assert_eq!(
        installed(&tag).len(),
        lock.base.as_ref().unwrap().closure.packages.len()
    );

    // The image, not the download cache, is what a warm entry uses.
    fs::remove_dir_all(user.lodi_home().join("cache/dl")).unwrap();
    let o = enter("download cache missing, image present");
    assert!(!err(&o).contains("building"));
    assert!(user.mirror.requests().is_empty());

    // A tampered cached package is discarded and fetched again: exactly that one request.
    assert!(podman(&["rmi", "--force", &tag]).status.success());
    enter("cache missing, image missing");
    assert_eq!(
        user.mirror.requests().len(),
        2,
        "{:?}",
        user.mirror.requests()
    );
    let mut bytes = fs::read(&cached_xz).unwrap();
    bytes[100] ^= 0xff;
    fs::write(&cached_xz, bytes).unwrap();
    assert!(podman(&["rmi", "--force", &tag]).status.success());
    let o = enter("tampered cache");
    assert!(err(&o).contains("building"));
    assert_eq!(user.mirror.requests(), std::slice::from_ref(&xz));

    // An image replaced under Lodi's tag is never used: it is rebuilt from verified inputs.
    let o = podman(&[
        "run",
        "--name",
        "spike-tamper",
        "--entrypoint",
        "sh",
        &tag,
        "-c",
        "echo x > /tampered",
    ]);
    assert!(o.status.success(), "{}", both(&o));
    assert!(
        podman(&["commit", "--quiet", "spike-tamper", &tag])
            .status
            .success()
    );
    assert!(podman(&["rm", "spike-tamper"]).status.success());
    let tampered = out(&podman(&["image", "inspect", "--format", "{{.Id}}", &tag]));
    user.mirror.clear();
    let o = user.run(
        &project,
        &["develop", "--", "sh", "-c", "test ! -e /tampered"],
    );
    assert_eq!(
        o.status.code(),
        Some(0),
        "the tampered image was used: {}",
        both(&o)
    );
    assert!(err(&o).contains("building"), "{}", both(&o));
    assert!(user.mirror.requests().is_empty());
    let rebuilt = user.image_record().unwrap();
    assert_ne!(rebuilt["imageId"].as_str().unwrap(), tampered.trim());
    assert!(
        lodi_containers(NAME).is_empty(),
        "{:?}",
        lodi_containers(NAME)
    );
}

/// The pid of a direct child of `parent` whose command name contains `name` (a packaged
/// `podman` may be a wrapper that runs `.podman-wrapped`).
fn child_named(parent: u32, name: &str) -> Option<i32> {
    for e in fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(e.path().join("stat")) else {
            continue;
        };
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let comm = &stat[stat.find('(').unwrap_or(0) + 1..close];
        let ppid = stat[close + 2..].split_whitespace().nth(1);
        if comm.contains(name) && ppid == Some(&parent.to_string()) {
            return Some(pid);
        }
    }
    None
}

/// Wait for something Podman does. The ceiling is the suite's shared one
/// (`tests/support/wait.rs`): how long a container takes to appear is not a performance budget,
/// and on a machine running several lanes' gates a fixed minute was not enough. A container that
/// never appears still fails the test.
fn wait_for(what: &str, check: impl FnMut() -> bool) {
    wait::until(what, check);
}

/// Start `develop -- <argv>` in the background and wait until its one container of environment
/// `name` runs (#718). The work whose length only the disk decides — the cold image build and
/// Podman's one-time ID-mapped copy of the image for `--userns=keep-id` (`storage-chown-by-maps`,
/// every file of the rootfs) — is done first by a synchronous `develop -- true`, with no clock
/// over it, like every `user.run` here. The timed wait then covers only a warm `podman run`, and
/// fails at once, with Lodi's status, if Lodi exits instead.
fn start_session(user: &User, dir: &Path, name: &str, argv: &[&str]) -> Fixture {
    let o = user.run(dir, &["develop", "--", "true"]);
    assert_eq!(o.status.code(), Some(0), "warming the image: {}", both(&o));
    let mut args = vec!["develop", "--"];
    args.extend_from_slice(argv);
    let mut session = spawn(user, dir, &args);
    wait_for("the container to run", || {
        if let Some(status) = session.child().try_wait().unwrap() {
            panic!("lodi develop exited ({status}) before its container ran");
        }
        lodi_containers(name).len() == 1
    });
    session
}

/// Start a background `lodi` whose caller then waits, by the clock, for its container. Only on a
/// realized image (#718): a cold build inside such a wait made the test's outcome depend on how
/// busy the disk was, so a cold spawn fails here at once, on every machine.
fn spawn(user: &User, dir: &Path, args: &[&str]) -> Fixture {
    assert!(
        user.image_record().is_some_and(|r| r["complete"] == true),
        "a background session starts on a realized image (#718): run `develop -- true` first, \
         or use start_session"
    );
    Fixture::spawn(
        "lodi",
        user.command(dir, args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
}

#[test]
fn the_runtime_keeps_argv_status_user_home_and_cleans_up() {
    const NAME: &str = "runtime";
    let _finish = Finish::new("runtime");
    let user = User::new("container-runtime");
    let project = small_project(&user, NAME, &[], &[], |_| {});
    let develop = |argv: &[&str]| {
        let mut args = vec!["develop", "--"];
        args.extend_from_slice(argv);
        user.run(&project, &args)
    };

    // argv arrives unchanged; no shell is involved.
    let o = develop(&[
        "printf", "[%s]", "a b", "\"q\"", "$HOME", "", "*", "--", "x\ny",
    ]);
    assert_eq!(o.status.code(), Some(0), "{}", both(&o));
    assert_eq!(out(&o), "[a b][\"q\"][$HOME][][*][--][x\ny]");

    // Status: the command's, 128 + n for a signal, 127 for a missing program.
    assert_eq!(develop(&["sh", "-c", "exit 7"]).status.code(), Some(7));
    assert_eq!(develop(&["sh", "-c", "exit 0"]).status.code(), Some(0));
    assert_eq!(
        develop(&["sh", "-c", "kill -TERM $$"]).status.code(),
        Some(143)
    );
    assert_eq!(develop(&["no-such-program-here"]).status.code(), Some(127));

    // user = same, the working directory is the project root, the environment is the
    // container's with the activation, and the host's other variables do not enter.
    let o = user.run_with(
        &project,
        &[
            "develop",
            "--",
            "sh",
            "-c",
            "id -u; id -g; pwd; hostname; echo $LODI_MODE; \
          echo $LANG; echo ${SPIKE_SECRET:-unset}; echo $PATH",
        ],
        &[("SPIKE_SECRET", "leak")],
    );
    assert_eq!(o.status.code(), Some(0), "{}", both(&o));
    let lines: Vec<String> = out(&o).lines().map(str::to_string).collect();
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    assert_eq!(lines[0], uid.to_string());
    assert_eq!(lines[1], gid.to_string());
    assert_eq!(Path::new(&lines[2]), project.canonicalize().unwrap());
    assert_eq!(lines[3], "runtime");
    assert_eq!(lines[4], "container");
    assert_eq!(lines[5], "C.UTF-8");
    assert_eq!(lines[6], "unset");
    assert!(
        lines[7]
            .ends_with("/tree/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
    );

    // home = shared: the (synthetic) home is the caller's, read-write, at the same path.
    let o = develop(&[
        "sh",
        "-c",
        "echo from-container > \"$HOME/shared-home\"; echo $HOME",
    ]);
    assert_eq!(o.status.code(), Some(0), "{}", both(&o));
    assert_eq!(out(&o).trim(), user.home().to_string_lossy());
    assert_eq!(
        fs::read_to_string(user.home().join("shared-home")).unwrap(),
        "from-container\n"
    );
    // writable = ephemeral: writes outside the binds are gone at the next entry.
    assert_eq!(
        develop(&["sh", "-c", "echo x > /tmp/ephemeral"])
            .status
            .code(),
        Some(0)
    );
    assert_eq!(
        develop(&["test", "!", "-e", "/tmp/ephemeral"])
            .status
            .code(),
        Some(0)
    );
    // The store is read-only inside, root of the namespace is not reached, stdin passes.
    assert_ne!(
        develop(&["sh", "-c", "touch \"$LODI_ENV/x\""])
            .status
            .code(),
        Some(0)
    );
    assert_ne!(
        develop(&["sh", "-c", "touch /etc/owned"]).status.code(),
        Some(0)
    );
    let mut child = user
        .command(&project, &["develop", "--", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"through stdin\n")
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(out(&o), "through stdin\n");
    assert!(
        lodi_containers(NAME).is_empty(),
        "{:?}",
        lodi_containers(NAME)
    );

    // A live-session root exists exactly while the container runs; SIGTERM to Lodi reaches
    // the command (143) and the container is gone afterwards.
    let child = spawn(&user, &project, &["develop", "--", "sleep", "60"]);
    let pid = child.id();
    wait_for("the container", || !lodi_containers(NAME).is_empty());
    let root: serde_json::Value = serde_json::from_slice(
        &fs::read(user.lodi_home().join(format!("gcroots/sessions/{pid}"))).unwrap(),
    )
    .unwrap();
    assert_eq!(root["activation"]["runtime"], "container");
    assert!(
        root["image"]["tag"]
            .as_str()
            .unwrap()
            .starts_with("localhost/lodi-env:")
    );
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(143), "{}", both(&o));
    assert!(
        lodi_containers(NAME).is_empty(),
        "{:?}",
        lodi_containers(NAME)
    );
    assert!(user.sessions().is_empty());

    // Podman itself killed: Lodi reports the signal and removes the container it left.
    let child = spawn(&user, &project, &["develop", "--", "sleep", "60"]);
    let pid = child.id();
    wait_for("the container", || !lodi_containers(NAME).is_empty());
    let podman_pid = child_named(pid, "podman").expect("podman is Lodi's child");
    unsafe { libc::kill(podman_pid, libc::SIGKILL) };
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(137), "{}", both(&o));
    assert!(o.status.signal().is_none());
    assert!(
        lodi_containers(NAME).is_empty(),
        "{:?}",
        lodi_containers(NAME)
    );
    assert!(user.sessions().is_empty());
}

#[test]
fn build_failures_closure_drift_and_podman_failures_are_reported() {
    let _finish = Finish::new("failures");
    let user = User::new("container-failures");

    // A closure apt cannot install (libssl-dev without libssl3): E_LAYER_BUILD with the log.
    let broken = small_project(&user, "broken", &["libssl-dev"], &["libssl-dev"], |_| {});
    let o = user.run(&broken, &["develop", "--", "true"]);
    assert_eq!(o.status.code(), Some(6), "{}", both(&o));
    assert!(err(&o).contains("E_LAYER_BUILD"), "{}", both(&o));
    assert!(
        err(&o).contains("libssl3"),
        "the log tail names the problem: {}",
        both(&o)
    );
    assert!(user.image_record().is_none());

    // A lock whose closure the base does not have (a base package at another version):
    // E_CLOSURE_DRIFT names it, and the built image is discarded.
    let drift = small_project(&user, "drift", &[], &[], |c| {
        let p = c.iter_mut().find(|p| p.name == "hostname").unwrap();
        p.version = "9.99".into();
    });
    let o = user.run(&drift, &["develop", "--", "true"]);
    assert_eq!(o.status.code(), Some(6), "{}", both(&o));
    assert!(err(&o).contains("E_CLOSURE_DRIFT"), "{}", both(&o));
    assert!(
        err(&o).contains("changed: hostname locked 9.99"),
        "{}",
        both(&o)
    );
    assert!(user.image_record().is_none());
    let lock = parse_lock(&fs::read(drift.join("lodi.lock")).unwrap()).unwrap();
    let (tag, _) = lodi::container::image_names(lock.base.as_ref().unwrap());
    assert_eq!(podman(&["image", "exists", &tag]).status.code(), Some(1));

    // A Podman that fails (its storage cannot be created): E_ENTER, nothing downloaded.
    let bad = user.base.join("bad-storage.conf");
    let file = user.base.join("not-a-directory");
    fs::write(&file, "").unwrap();
    fs::write(
        &bad,
        format!(
            "[storage]\ndriver = \"overlay\"\ngraphroot = \"{0}/graph\"\nrunroot = \"{0}/run\"\n",
            file.display()
        ),
    )
    .unwrap();
    let plain = small_project(&user, "plain", &[], &[], |_| {});
    user.mirror.clear();
    let o = user.run_with(
        &plain,
        &["develop", "--", "true"],
        &[("CONTAINERS_STORAGE_CONF", bad.to_str().unwrap())],
    );
    assert_eq!(o.status.code(), Some(7), "{}", both(&o));
    assert!(
        err(&o).contains("E_ENTER") && err(&o).contains("podman"),
        "{}",
        both(&o)
    );
    assert!(
        user.mirror.requests().is_empty(),
        "{:?}",
        user.mirror.requests()
    );
    assert!(lodi_containers("plain").is_empty());
}

#[test]
fn a_missing_podman_untrusted_text_and_nesting_are_refused_before_anything_is_fetched() {
    // No real Podman here: PATH has no podman at all.
    let _finish = Finish::new("refusals");
    let user = User::new("container-refusals");
    let project = small_project(&user, "refused", &[], &[], |_| {});
    let empty = user.base.join("empty-path");
    fs::create_dir_all(&empty).unwrap();
    user.mirror.clear();
    let o = user.run_with(
        &project,
        &["develop", "--", "true"],
        &[("PATH", empty.to_str().unwrap())],
    );
    assert_eq!(o.status.code(), Some(7), "{}", both(&o));
    assert!(err(&o).contains("E_NO_RUNTIME"), "{}", both(&o));
    assert!(
        err(&o).contains("install it once with root privileges"),
        "{}",
        both(&o)
    );
    assert!(
        !user.lodi_home().join("store").exists(),
        "nothing was realized"
    );

    // Task text is trusted before the runtime is even looked for (ADR-014).
    let mut manifest = fs::read_to_string(project.join("lodi.toml")).unwrap();
    manifest.push_str("\n[tasks.hello]\nrun = \"echo hello\"\n");
    fs::write(project.join("lodi.toml"), manifest).unwrap();
    let mut c = user.command(&project, &["run", "hello"]);
    c.env_remove("LODI_TRUST");
    let o = c.output().unwrap();
    assert_eq!(o.status.code(), Some(11), "{}", both(&o));

    // Another Lodi environment in the caller: container environments do not nest.
    let o = user.run_with(
        &project,
        &["develop", "--", "true"],
        &[("LODI_MODE", "host"), ("LODI_ACTIVATION", "sha256:x")],
    );
    assert_eq!(o.status.code(), Some(7), "{}", both(&o));
    assert!(err(&o).contains("E_NESTED"), "{}", both(&o));
    assert!(
        user.mirror.requests().is_empty(),
        "{:?}",
        user.mirror.requests()
    );
    assert!(!user.lodi_home().join("store").exists());
}

/// A lock for `lodi shell --base debian:bookworm <packages>`: the fixture's pinned base with
/// exactly those packages requested, so the ad-hoc resolution below is the fixture's and the
/// real snapshot archive is not resolved again here.
fn shell_lock(packages: &[&str]) -> LockFile {
    let mut lock = fixture_lock();
    lock.packages.clear();
    for p in lock.profiles.values_mut() {
        p.packages.clear();
    }
    let base = lock.base.as_mut().unwrap();
    let mut requested: Vec<String> = packages.iter().map(|s| s.to_string()).collect();
    requested.sort();
    base.requested.insert("default".into(), requested);
    let closure: Vec<_> = base
        .closure
        .packages
        .iter()
        .filter(|p| p.origin == "base" || packages.contains(&p.name.as_str()))
        .cloned()
        .collect();
    assert_eq!(
        closure.iter().filter(|p| p.origin == "install").count(),
        packages.len(),
        "every requested package is in the fixture closure"
    );
    base.closure.hash = closure_hash(&closure);
    base.closure.packages = closure;
    lock
}

// M-0.3 T-2, acceptance (d): `lodi shell --base` enters a real container that has the packages,
// with no manifest and no file anywhere but `$LODI_HOME`.
#[test]
fn shell_with_a_base_enters_a_container_that_has_the_packages() {
    const PACKAGE: &str = "make";
    let _finish = Finish::new("shell");
    let user = User::new("container-shell");

    // The request resolves to the manifest `[container] debian/bookworm` + `[packages] make`.
    // Its resolution is seeded in `$LODI_HOME/cache/shell/<h32>.lock`, exactly where a first
    // `lodi shell` would have left it, so this check enters from the cache as the second one
    // does (LD-49) and resolves nothing against the real archive here.
    let request = lodi::shell::Request {
        base: Some("debian:bookworm".into()),
        items: vec![PACKAGE.into()],
    };
    let resolved = lodi::shell::resolve_request(&request).unwrap();
    let lock = shell_lock(&[PACKAGE]);
    assert!(
        lodi::lock::staleness(&resolved.manifest, &lock).is_empty(),
        "the seeded lock is fresh for the request: {:?}",
        lodi::lock::staleness(&resolved.manifest, &lock)
    );
    let cache = user.lodi_home().join("cache/shell");
    fs::create_dir_all(&cache).unwrap();
    fs::write(
        cache.join(format!("{}.lock", resolved.h32())),
        lock.to_canonical_json(),
    )
    .unwrap();

    // Any directory at all, with no manifest in it: `lodi shell` never reads one.
    let dir = user.base.join("elsewhere");
    fs::create_dir_all(&dir).unwrap();
    let before = listing(&dir);

    let mut child = user
        .command(&dir, &["shell", "--base", "debian:bookworm", PACKAGE])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            b"dpkg -l make
make --version
exit 6
",
        )
        .unwrap();
    let o = child.wait_with_output().unwrap();

    // The shell's own status comes back, and the package is installed in the image it entered.
    assert_eq!(o.status.code(), Some(6), "{}", both(&o));
    let text = out(&o);
    assert!(
        text.lines()
            .any(|l| l.starts_with("ii ") && l.contains(" make ")),
        "dpkg -l does not show {PACKAGE}: {}",
        both(&o)
    );
    assert!(text.contains("GNU Make"), "{}", both(&o));

    // Independently of Lodi: the image really holds the package, and nothing else was asked for.
    let record = user.image_record().expect("an image record");
    assert_eq!(record["complete"], true);
    assert_eq!(
        record["closureHash"],
        lock.base.as_ref().unwrap().closure.hash
    );
    let packages = installed(record["tag"].as_str().unwrap());
    assert!(packages.contains_key(PACKAGE), "{packages:?}");

    // Nothing was written outside $LODI_HOME, and the session root is gone with the shell.
    assert_eq!(listing(&dir), before, "a file appeared in the directory");
    assert!(!dir.join("lodi.toml").exists() && !dir.join("lodi.lock").exists());
    assert!(user.sessions().is_empty());
    assert!(
        lodi_containers("lodi-shell").is_empty(),
        "no container left"
    );
}

/// LD-372: an ordinary container test that fails while a container it started is running leaves
/// nothing behind. It is built like every test above — [`Finish::new`] at its start, a
/// [`User::new`] in the test's storage, a real `lodi develop` — and panics; the panic is caught
/// only so that this test can look afterwards. By the time the failed test has unwound, with no
/// further wait here, the container's `conmon` and command, every process that names the test's
/// storage, and the storage itself are gone.
#[test]
fn a_container_of_a_test_that_panics_does_not_outlive_it() {
    const NAME: &str = "hygiene";
    let seen = std::sync::Mutex::new((PathBuf::new(), Vec::<(String, i32)>::new()));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let finish = Finish::new(NAME);
        let user = User::new("container-hygiene");
        let project = small_project(&user, NAME, &[], &[], |_| {});
        let _session = start_session(&user, &project, NAME, &["sleep", "600"]);
        let name = lodi_containers(NAME).remove(0);
        let state = out(&podman(&[
            "inspect",
            "--format",
            "{{.State.ConmonPid}} {{.State.Pid}}",
            &name,
        ]));
        let mut pids = state.split_whitespace().map(|p| p.parse::<i32>().unwrap());
        let mut seen = seen.lock().unwrap();
        seen.0 = finish.dir().to_path_buf();
        seen.1.push(("conmon".into(), pids.next().unwrap()));
        seen.1
            .push(("the container's command".into(), pids.next().unwrap()));
        for (what, pid) in seen.1.iter() {
            assert!(*pid > 0 && !fixture::gone(*pid), "{what} runs");
        }
        assert!(
            !processes_of(&seen.0).is_empty(),
            "conmon names the storage"
        );
        drop(seen);
        panic!("a failure while the container runs");
    }));
    assert!(outcome.is_err(), "the test failed as staged");
    let (dir, seen) = seen.into_inner().unwrap();
    assert_eq!(seen.len(), 2, "both pids were seen before the failure");
    for (what, pid) in seen {
        assert!(
            fixture::gone(pid),
            "{what} (pid {pid}) outlived the failed test"
        );
    }
    assert_eq!(
        processes_of(&dir),
        Vec::<i32>::new(),
        "processes of the failed test's storage outlived it"
    );
    assert!(
        !dir.exists(),
        "the failed test's storage {} outlived it",
        dir.display()
    );
}

/// #718: a background session is never started on a cold image. Its caller waits for the
/// container by the clock, and a cold build plus Podman's ID-mapped copy of the image inside that
/// wait made the outcome depend on how busy the disk was. The refusal comes before anything is
/// started, so it holds on an idle machine and a loaded one alike.
#[test]
fn a_background_session_on_a_cold_image_is_refused_before_it_starts() {
    const NAME: &str = "cold";
    let _finish = Finish::new(NAME);
    let user = User::new("container-cold");
    let project = small_project(&user, NAME, &[], &[], |_| {});
    let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        spawn(&user, &project, &["develop", "--", "sleep", "600"])
    }));
    let Err(message) = refused else {
        panic!("a cold spawn is refused");
    };
    let message = match message.downcast_ref::<&str>() {
        Some(text) => text.to_string(),
        None => message
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default(),
    };
    assert!(message.contains("#718"), "{message}");
    assert!(user.image_record().is_none(), "nothing was built");
    assert!(lodi_containers(NAME).is_empty(), "nothing was started");
}

/// #544: a target directory reached through a symlink (the release clone's `target/` links to a
/// persistent directory) still finds the processes of a storage in it. Podman resolves the
/// storage's graph and run roots, so `conmon` names the storage by its real path, while the test
/// knows it by the path through the link; either spelling of the storage finds the process, and
/// a storage that has been removed still does.
#[test]
fn the_processes_of_a_storage_reached_through_a_symlink_are_found() {
    let base = scratch("symlinked-storage");
    let real = base.join("real");
    fs::create_dir_all(real.join("spike-podman-1-x/graph")).unwrap();
    let link = base.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let real = fs::canonicalize(&real).unwrap();
    let named = real.join("spike-podman-1-x/graph/userdata");
    let mut child = Command::new("sh")
        .args(["-c", "read -r line", "sh"])
        .arg(&named)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    let through_link = link.join("spike-podman-1-x");
    let found = processes_of(&through_link).contains(&pid);
    let by_real_path = processes_of(&real.join("spike-podman-1-x")).contains(&pid);
    fs::remove_dir_all(real.join("spike-podman-1-x")).unwrap();
    let after_removal = processes_of(&through_link).contains(&pid);
    let _ = child.kill();
    let _ = child.wait();
    assert!(by_real_path, "the real path finds the process");
    assert!(found, "the path through the link finds the process");
    assert!(
        after_removal,
        "a removed storage's path through the link finds the process"
    );
}

/// #367: rootless Podman gives a container its own `libpod-*.scope` under `user.slice`, outside
/// the capped `lodi-work.slice` the gate runs in (#363). Inside that slice the gate exports
/// `CONTAINERS_CONF_OVERRIDE` naming a file with `cgroups = "disabled"` (`scripts/gate.sh`,
/// `podman_slice_conf`); Lodi hands its environment to Podman, and this suite passes the
/// variable through to Lodi, so the container's command runs inside the slice too. Outside the
/// slice, or with no override, there is nothing to hold and the test says so.
#[test]
fn a_container_started_inside_lodi_work_slice_stays_in_it() {
    const NAME: &str = "slice";
    let mine = fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    if !mine.contains("/lodi-work.slice/") || std::env::var_os("CONTAINERS_CONF_OVERRIDE").is_none()
    {
        eprintln!(
            "skipped: not inside lodi-work.slice with CONTAINERS_CONF_OVERRIDE set (scripts/gate.sh sets it there)"
        );
        return;
    }
    let _finish = Finish::new(NAME);
    let user = User::new("container-slice");
    let project = small_project(&user, NAME, &[], &[], |_| {});
    let _session = start_session(&user, &project, NAME, &["sleep", "600"]);
    let name = lodi_containers(NAME).remove(0);
    let pid = out(&podman(&["inspect", "--format", "{{.State.Pid}}", &name]));
    let pid: i32 = pid.trim().parse().unwrap();
    let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    assert!(
        cgroup.contains("/lodi-work.slice/"),
        "the container's command (pid {pid}) left lodi-work.slice: {cgroup}"
    );
}
