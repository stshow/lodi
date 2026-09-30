//! M-Arch-Base T-6: `distro = "arch"` from an empty directory to an environment request.
//!
//! Offline, and through the real binary. The Arch world is the committed synthetic fixtures of
//! `tests/fixtures/arch/` (see its README), served over a loopback HTTP server that the binary
//! reaches through `LODI_FETCH_REWRITE`, so every request it makes is counted. The repository
//! day and the bootstrap directory are registered for **yesterday** and not today, as the real
//! Arch Linux Archive publishes a day only after that UTC day has ended; `lodi init --base arch`
//! followed by `lodi lock`, with no edit to the snapshot, still pins — acceptance (a) of this
//! task, minus the image build, and LD-381's rule that the Arch default is the previous UTC day.
//!
//! The image half needs a real Arch rootfs and real rootless Podman, and is
//! `tests/spike_container.rs`; the live measurement against `archive.archlinux.org` is
//! `sh scripts/march-local.sh --case arch`. What is here is everything in between: the manifest
//! `lodi init` writes, the lock `lodi lock` writes from it, `lodi lock --check`, staleness, the
//! documented refusals, and that a container request reaches image realization rather than
//! anything else.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lodi::util::{format_utc, now_utc, sha256_hex};
use support::{Server, file, gzip, tar};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/arch");
const ISO: &str = "https://archive.archlinux.org/iso/";
const REPOS: &str = "https://archive.archlinux.org/repos";

fn fixture(name: &str) -> Vec<u8> {
    fs::read(Path::new(FIXTURE).join(name)).unwrap()
}

/// `YYYY.MM.DD` and `YYYY/MM/DD` of one instant: the bootstrap directory name and the
/// repository day of the same UTC date.
fn day(at: i64) -> (String, String) {
    let date = format_utc(at)[..10].to_string();
    (date.replace('-', "."), date.replace('-', "/"))
}

/// One `<repo>.db`: the real gzip-tar of one `desc` stanza, built from the committed fixture.
fn database(desc: &str, version: &str) -> Vec<u8> {
    database_of(&String::from_utf8(fixture(desc)).unwrap(), version)
}

/// The same, from a stanza the caller has altered.
fn database_of(text: &str, version: &str) -> Vec<u8> {
    let name = text
        .split("%NAME%\n")
        .nth(1)
        .and_then(|rest| rest.lines().next())
        .expect("the stanza names its package")
        .to_string();
    gzip(&tar(&[file(
        &format!("{name}-{version}/desc"),
        text,
        0o644,
    )]))
}

/// The HTML directory index `archive.archlinux.org/iso/` serves.
fn listing(dirs: &[&str]) -> Vec<u8> {
    let mut html =
        String::from("<html><body><h1>Index of /iso</h1><pre>\n<a href=\"../\">../</a>\n");
    for dir in dirs {
        html.push_str(&format!("<a href=\"{dir}/\">{dir}/</a>\n"));
    }
    html.push_str("<a href=\"latest/\">latest/</a>\n</pre></body></html>\n");
    html.into_bytes()
}

/// 00:00:00Z of the UTC day before the one `at` falls in: the newest day the Arch Linux Archive
/// has published at `at` (LD-381).
fn published(at: i64) -> i64 {
    at - at.rem_euclid(86400) - 86400
}

/// The synthetic Arch archive as it stands at `at`: the bootstrap directory and the repository
/// day of the previous UTC day are present, and nothing of `at`'s own day, which the real
/// archive has not published yet.
fn upstream(at: i64) -> Server {
    let (iso_day, repo_day) = day(published(at));
    let rootfs = fixture("bootstrap-rootfs.synthetic");
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    files.insert(ISO.to_string(), listing(&["2024.05.01", &iso_day]));
    for dir in ["2024.05.01", &iso_day] {
        let layer = format!("archlinux-bootstrap-{dir}-x86_64.tar.zst");
        files.insert(
            format!("{ISO}{dir}/sha256sums.txt"),
            format!("{}  {layer}\n", sha256_hex(&rootfs)).into_bytes(),
        );
        files.insert(format!("{ISO}{dir}/{layer}"), rootfs.clone());
    }
    for (repository, desc, version) in [
        ("core", "core.desc", "2026.09.01-1"),
        ("extra", "extra.desc", "1.8.1-1"),
    ] {
        files.insert(
            format!("{REPOS}/{repo_day}/{repository}/os/x86_64/{repository}.db"),
            database(desc, version),
        );
    }
    Server::start(files)
}

/// A project directory of its own, with its own `HOME` and `LODI_HOME` below it.
struct Project {
    dir: PathBuf,
    home: PathBuf,
}

impl Project {
    fn new(name: &str) -> Project {
        // A fresh directory of this process per case (#350); a later run's sweep removes it.
        let base = support::scratch(&format!("arch-e2e-{name}"));
        let (dir, home) = (base.join("project"), base.join("home"));
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        Project { dir, home }
    }

    /// Run the real binary in this project. `rewrite` is empty for a command that must make no
    /// request; `path` replaces `PATH`, which is how the missing-runtime case is made.
    fn run_with(&self, args: &[&str], rewrite: &str, path: Option<&str>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lodi"));
        command
            .args(args)
            .current_dir(&self.dir)
            .env("HOME", &self.home)
            .env("LODI_HOME", self.home.join("lodi"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("LODI_FETCH_REWRITE", rewrite)
            // The task text of these fixtures is this test's own; trusting it is not what is
            // under test here (`tests/cli.rs` owns the trust surface).
            .env("LODI_TRUST", "1");
        match path {
            Some(path) => command.env("PATH", path),
            None => &mut command,
        };
        command.output().expect("the lodi binary runs")
    }

    fn run(&self, args: &[&str], server: &Server) -> Output {
        self.run_with(args, &server.rewrite(), None)
    }

    fn offline(&self, args: &[&str]) -> Output {
        self.run_with(args, "", None)
    }

    fn manifest(&self) -> String {
        fs::read_to_string(self.dir.join("lodi.toml")).unwrap()
    }

    fn lock(&self) -> lodi::lock::LockFile {
        lodi::lock::parse_lock(&fs::read(self.dir.join("lodi.lock")).unwrap()).unwrap()
    }

    /// Every entry of the project directory, sorted.
    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

fn both(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// A `PATH` with no `podman` in it, so that image realization is reached and reports that it
/// has no runtime rather than building anything.
fn path_without_podman(project: &Project) -> String {
    project.home.join("empty-bin").display().to_string()
}

// ---------------------------------------------------------------------------------------------
// The whole path, once.
// ---------------------------------------------------------------------------------------------

/// Acceptance (a) up to the image build: an empty directory, `lodi init --base arch`, and then a
/// lock, with no edit to the manifest in between.
#[test]
fn init_then_lock_pins_arch_with_no_edit_to_the_manifest() {
    let project = Project::new("init-lock");
    let at = now_utc();
    let server = upstream(at);

    let init = project.offline(&["init", "--base", "arch"]);
    assert_eq!(init.status.code(), Some(0), "{}", both(&init));
    let manifest = project.manifest();
    assert!(manifest.contains("distro = \"arch\""), "{manifest}");
    assert!(manifest.contains("release = \"rolling\""), "{manifest}");
    assert!(manifest.contains("common = []"), "{manifest}");
    // `lodi init` makes no request and downloads nothing.
    assert!(server.requests().is_empty());

    // The one edit this test makes is the request itself, which is the point of a manifest; the
    // base `lodi init` chose is untouched. The template's `[tools]` entry is dropped because
    // resolving an upstream tool is not this family's path and has its own tests and its own
    // fixtures; acceptance (a) with the template exactly as written is
    // `sh scripts/march-local.sh --case arch`.
    let with_package = manifest
        .replace("common = []", "common = [\"jq\"]")
        .split("[tools]")
        .next()
        .unwrap()
        .to_string();
    assert!(with_package.contains("[packages]"), "{with_package}");
    fs::write(project.dir.join("lodi.toml"), &with_package).unwrap();

    let locked = project.run(&["lock"], &server);
    assert_eq!(locked.status.code(), Some(0), "{}", both(&locked));
    let lock = project.lock();
    lodi::lock::validate(&lock).expect("the lock this build wrote is valid");
    let base = lock.base.as_ref().unwrap();
    assert_eq!(base.distro, "arch");
    assert_eq!(base.release, "rolling");
    assert_eq!(base.arch, "x86_64");
    assert_eq!(base.deb_arch, "x86_64");
    assert!(base.pinned);
    // No snapshot was written by hand: the default is the newest day the archive has
    // published, the previous UTC day at 00:00:00Z (LD-381), not an hour of today.
    assert!(!with_package.contains("snapshot"), "{with_package}");
    assert_eq!(base.snapshot, format_utc(published(at)));
    assert_eq!(base.requested["default"], ["jq".to_string()]);
    // Pinned from metadata alone: the listing, the checksum file and the two databases, and no
    // artifact of any kind.
    let requests = server.requests();
    assert_eq!(requests.len(), 4, "{requests:?}");
    assert!(
        !requests
            .iter()
            .any(|url| url.ends_with(".tar.zst") || url.ends_with(".sig") || url.contains("key"))
    );
    // The closure is what `extra` carries, with the file the image build will verify.
    assert_eq!(base.closure.packages.len(), 1);
    let jq = &base.closure.packages[0];
    assert_eq!((jq.name.as_str(), jq.version.as_str()), ("jq", "1.8.1-1"));
    assert_eq!(jq.origin, "install");
    assert_eq!(jq.repository.as_deref(), Some("extra"));
    assert!(jq.sha256.is_some() && jq.filename.is_some());
    // Both repositories are pinned by the digest of their one database.
    assert_eq!(base.repositories.len(), 2);
    for repository in &base.repositories {
        assert!(
            repository
                .url
                .starts_with("https://archive.archlinux.org/repos/")
        );
        assert_eq!(repository.indexes.len(), 1);
    }
    assert_eq!(base.rootfs.subdir.as_deref(), Some("root.x86_64"));
    assert!(base.rootfs.package_list.is_none());

    // A second lock of the same manifest resolves nothing and asks for nothing.
    server.clear();
    let again = project.run(&["lock"], &server);
    assert_eq!(again.status.code(), Some(0), "{}", both(&again));
    assert!(String::from_utf8_lossy(&again.stdout).contains("up to date"));
    assert!(server.requests().is_empty(), "{:?}", server.requests());
    assert_eq!(project.lock().to_canonical_json(), lock.to_canonical_json());

    // `lock --check` behaves as it does for the other two bases, offline.
    let check = project.offline(&["lock", "--check"]);
    assert_eq!(check.status.code(), Some(0), "{}", both(&check));

    // One more request and the lock is stale, not silently re-resolved.
    fs::write(
        project.dir.join("lodi.toml"),
        with_package.replace("common = [\"jq\"]", "common = [\"jq\", \"tree\"]"),
    )
    .unwrap();
    let stale = project.offline(&["lock", "--check"]);
    assert_eq!(stale.status.code(), Some(10), "{}", both(&stale));
    assert!(both(&stale).contains("E_LOCK_STALE"), "{}", both(&stale));

    // A lock that pins Arch under a manifest that asks for Debian is the same mismatch the
    // other bases give, not a rebuild.
    fs::write(
        project.dir.join("lodi.toml"),
        with_package
            .replace("distro = \"arch\"", "distro = \"debian\"")
            .replace("release = \"rolling\"", "release = \"bookworm\""),
    )
    .unwrap();
    let mismatch = project.offline(&["lock", "--check"]);
    assert_eq!(mismatch.status.code(), Some(10), "{}", both(&mismatch));
    let shown = both(&mismatch);
    assert!(shown.contains("E_LOCK_STALE"), "{shown}");
    assert!(
        shown.contains("debian") && shown.contains("arch"),
        "{shown}"
    );
}

/// A container manifest reaches image realization: with no `podman` on `PATH` the three entry
/// verbs each report that there is no runtime, which is the last thing before a build.
#[test]
fn every_entry_verb_reaches_image_realization() {
    let project = Project::new("enter");
    let server = upstream(now_utc());
    fs::create_dir_all(project.home.join("empty-bin")).unwrap();
    fs::write(
        project.dir.join("lodi.toml"),
        "[project]\nname = \"arch-enter\"\n\n\
         [container]\ndistro = \"arch\"\nrelease = \"rolling\"\n\n\
         [packages]\ncommon = [\"jq\"]\n\n\
         [tasks.show]\nrun = \"jq --version\"\n",
    )
    .unwrap();
    assert_eq!(
        project.run(&["lock"], &server).status.code(),
        Some(0),
        "the lock is written first"
    );

    let path = path_without_podman(&project);
    for args in [
        vec!["develop", "--", "true"],
        vec!["develop"],
        vec!["run", "show"],
    ] {
        let o = project.run_with(&args, "", Some(&path));
        let shown = both(&o);
        assert_eq!(o.status.code(), Some(7), "{args:?}: {shown}");
        assert!(shown.contains("E_NO_RUNTIME"), "{args:?}: {shown}");
        // The hint tells the user how to install Podman, and never offers to install it.
        assert!(shown.contains("podman"), "{args:?}: {shown}");
    }
    // Nothing was written into the project beyond the manifest and the lock.
    assert_eq!(project.entries(), ["lodi.lock", "lodi.toml"]);
}

/// Acceptance (d), minus the image: `lodi shell --base arch:rolling PACKAGE` resolves its own
/// lock from the arguments alone, writes nothing into the current directory, and goes down the
/// same container path.
#[test]
fn shell_with_an_arch_base_resolves_its_own_lock_and_writes_nothing_here() {
    let project = Project::new("shell");
    let server = upstream(now_utc());
    fs::create_dir_all(project.home.join("empty-bin")).unwrap();
    // A manifest that would be wrong for this request is present and must be ignored (LD-49).
    fs::write(
        project.dir.join("lodi.toml"),
        "[project]\nname = \"ignored\"\n",
    )
    .unwrap();

    let path = path_without_podman(&project);
    let mut command = Command::new(env!("CARGO_BIN_EXE_lodi"));
    let o = command
        .args(["shell", "--no-nest", "--base", "arch:rolling", "jq"])
        .current_dir(&project.dir)
        .env("HOME", &project.home)
        .env("LODI_HOME", project.home.join("lodi"))
        .env("XDG_CONFIG_HOME", project.home.join(".config"))
        .env("LODI_FETCH_REWRITE", server.rewrite())
        .env("PATH", &path)
        .output()
        .expect("the lodi binary runs");
    let shown = both(&o);
    assert_eq!(o.status.code(), Some(7), "{shown}");
    assert!(shown.contains("E_NO_RUNTIME"), "{shown}");
    assert!(shown.contains("container"), "{shown}");
    // The request was recognized as a container request — the runtime is checked before
    // anything is downloaded or written, which is why no lock exists yet — and the library's
    // own check that `--base arch:rolling` becomes an Arch container request is in
    // `tests/shell.rs`.
    assert!(!project.home.join("lodi/cache/shell").exists());
    assert!(server.requests().is_empty(), "{:?}", server.requests());
    // Nothing was written in the current directory.
    assert_eq!(project.entries(), ["lodi.toml"]);
    assert_eq!(
        fs::read_to_string(project.dir.join("lodi.toml")).unwrap(),
        "[project]\nname = \"ignored\"\n"
    );
}

// ---------------------------------------------------------------------------------------------
// The documented refusals (acceptance (e), the half that needs no image).
// ---------------------------------------------------------------------------------------------

#[test]
fn an_unsupported_arch_release_is_refused_before_any_request() {
    let project = Project::new("release");
    let server = upstream(now_utc());
    for release in ["2026.09.01", "latest", "bookworm"] {
        fs::write(
            project.dir.join("lodi.toml"),
            format!(
                "[project]\nname = \"x\"\n\n[container]\ndistro = \"arch\"\n\
                 release = \"{release}\"\n\n[packages]\ncommon = []\n"
            ),
        )
        .unwrap();
        let o = project.run(&["lock"], &server);
        let shown = both(&o);
        assert_eq!(o.status.code(), Some(3), "{release}: {shown}");
        assert!(shown.contains("E_UNSUPPORTED"), "{release}: {shown}");
        assert!(shown.contains("arch \"rolling\""), "{release}: {shown}");
        assert!(
            server.requests().is_empty(),
            "{release} asked for something"
        );
        assert!(!project.dir.join("lodi.lock").exists(), "{release}");
    }
}

#[test]
fn a_snapshot_before_the_archive_and_one_in_the_future_are_each_refused() {
    let project = Project::new("snapshot");
    let server = upstream(now_utc());
    for (snapshot, code) in [
        ("2020-01-01T00:00:00Z".to_string(), "E_SNAPSHOT_TOO_OLD"),
        ("2099-01-01T00:00:00Z".to_string(), "E_SNAPSHOT_FUTURE"),
    ] {
        fs::write(
            project.dir.join("lodi.toml"),
            format!(
                "[project]\nname = \"x\"\n\n[container]\ndistro = \"arch\"\n\
                 release = \"rolling\"\nsnapshot = \"{snapshot}\"\n\n[packages]\ncommon = []\n"
            ),
        )
        .unwrap();
        let o = project.run(&["lock"], &server);
        let shown = both(&o);
        assert_eq!(o.status.code(), Some(4), "{snapshot}: {shown}");
        assert!(shown.contains(code), "{snapshot}: {shown}");
        assert!(
            server.requests().is_empty(),
            "{snapshot}: a bound was checked late"
        );
        assert!(!project.dir.join("lodi.lock").exists(), "{snapshot}");
    }
}

#[test]
fn an_unknown_package_is_refused_and_no_lock_is_written() {
    let project = Project::new("unknown");
    let server = upstream(now_utc());
    fs::write(
        project.dir.join("lodi.toml"),
        "[project]\nname = \"x\"\n\n[container]\ndistro = \"arch\"\n\
         release = \"rolling\"\n\n[packages]\ncommon = [\"not-a-package\"]\n",
    )
    .unwrap();
    let o = project.run(&["lock"], &server);
    let shown = both(&o);
    assert_eq!(o.status.code(), Some(4), "{shown}");
    assert!(shown.contains("E_NO_MATCH"), "{shown}");
    assert!(shown.contains("not-a-package"), "{shown}");
    assert!(!project.dir.join("lodi.lock").exists());
}

/// A snapshot written by hand inside today's UTC day names a day the archive has not published:
/// it is refused before any request, and the refusal says why and names the latest valid value
/// (LD-381).
#[test]
fn a_snapshot_inside_today_is_refused_naming_the_latest_published_day() {
    let project = Project::new("today");
    let at = now_utc();
    let server = upstream(at);
    let today = format!("{}T00:00:00Z", &format_utc(at)[..10]);
    fs::write(
        project.dir.join("lodi.toml"),
        format!(
            "[project]\nname = \"x\"\n\n[container]\ndistro = \"arch\"\n\
             release = \"rolling\"\nsnapshot = \"{today}\"\n\n[packages]\ncommon = []\n"
        ),
    )
    .unwrap();
    let o = project.run(&["lock"], &server);
    let shown = both(&o);
    assert_eq!(o.status.code(), Some(4), "{shown}");
    assert!(shown.contains("E_SNAPSHOT_FUTURE"), "{shown}");
    assert!(shown.contains("not published"), "{shown}");
    assert!(shown.contains(&format_utc(published(at))), "{shown}");
    assert!(server.requests().is_empty(), "{:?}", server.requests());
    assert!(!project.dir.join("lodi.lock").exists());
}

/// The digest a lock records is the digest of the bytes that were read. A mirror that serves
/// different bytes under the same URL therefore produces a different pin, never a lock that
/// keeps the old digest beside a closure resolved from the new bytes.
#[test]
fn the_recorded_digest_is_the_digest_of_the_bytes_that_were_read() {
    let project = Project::new("digest");
    // One reading of the clock for both ends. `upstream` publishes the archive day of the
    // instant it is given, and the snapshot below names a day; reading `now_utc()` twice let a
    // UTC midnight crossed during the first `lodi lock` put them on different days, and so ask
    // for URLs this server never published (M-Arch-Base R-3, reviewing LD-266).
    let at = now_utc();
    let server = upstream(at);
    let manifest = "[project]\nname = \"x\"\n\n[container]\ndistro = \"arch\"\n\
         release = \"rolling\"\n\n[packages]\ncommon = [\"jq\"]\n";
    fs::write(project.dir.join("lodi.toml"), manifest).unwrap();
    assert_eq!(project.run(&["lock"], &server).status.code(), Some(0));
    let pinned = project.lock();
    let base = pinned.base.as_ref().unwrap();
    let extra = base
        .repositories
        .iter()
        .find(|r| r.name == "extra")
        .expect("extra is pinned");
    let url = format!("{}{}", extra.url, extra.indexes[0].packages.path);
    let recorded = extra.indexes[0].packages.sha256.clone();

    // The same URL, different bytes: the same package, described differently, as a remirrored
    // database legitimately can be.
    let remirrored = String::from_utf8(fixture("extra.desc"))
        .unwrap()
        .replace("synthetic jq package", "synthetic jq package, remirrored");
    server
        .files
        .lock()
        .unwrap()
        .insert(url.clone(), database_of(&remirrored, "1.8.1-1"));

    // Re-resolving records the new digest. The snapshot is what changes, so the same day — and
    // so the same URLs — are asked for again.
    // Not midnight itself: the previous day's midnight IS the default snapshot (LD-381), so the
    // manifest would then ask for what the lock already holds, and nothing would be re-resolved.
    // One second past it is the same published day and never the default.
    let midnight = format_utc(published(at) + 1);
    fs::write(
        project.dir.join("lodi.toml"),
        manifest.replace(
            "release = \"rolling\"",
            &format!("release = \"rolling\"\nsnapshot = \"{midnight}\""),
        ),
    )
    .unwrap();
    let o = project.run(&["lock"], &server);
    assert_eq!(o.status.code(), Some(0), "{}", both(&o));
    let second = project.lock();
    let second_base = second.base.as_ref().unwrap();
    let second_extra = second_base
        .repositories
        .iter()
        .find(|r| r.name == "extra")
        .unwrap();
    assert_ne!(
        second_extra.indexes[0].packages.sha256, recorded,
        "the pin must follow the bytes"
    );
    assert_eq!(second_base.closure.packages[0].name, "jq");
    lodi::lock::validate(&second).unwrap();
}
