//! M-0.3 T-4: Ubuntu 24.04 (noble) as a second container base, and since M-Arch-Base T-6 the
//! three-distribution comparison the third base extended it into.
//!
//! Offline: every upstream document is served from the synthetic fixtures in
//! `tests/fixtures/ubuntu/` (see its README) through an in-memory fetcher. The Debian half of
//! the cross-distro comparison is served the same way from `tests/fixtures/spike/lock/`, and the
//! Arch half from `tests/fixtures/arch/`; this test only reads all three. Live discovery against
//! `snapshot.ubuntu.com` and the image server is the evidence of
//! `sh scripts/m03-local.sh --case distros`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

mod support;

use lodi::fetch::{FetchError, Fetcher};
use lodi::lock::{Failure, LockFile, Outcome, lock_project};
use lodi::util::{parse_utc, sha256_hex};

const UBUNTU: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ubuntu");
const DEBIAN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/spike/lock");

const SNAPSHOT: &str = "2026-09-18T00:00:00Z";
const SNAPSHOT_ID: &str = "20260918T000000Z";
/// The dated image directories the synthetic server lists. `20260917` is the newest that is not
/// after the snapshot; `20260919` is published but later, and must not be the one pinned.
const DATED: &[&str] = &["20260829", "20260905", "20260917", "20260919"];
const PINNED_DIR: &str = "20260917";
const IMAGES: &str = "https://partner-images.canonical.com/oci/noble/";
const LAYER: &str = "ubuntu-noble-oci-amd64-root.tar.gz";
const PACKAGE_LIST: &str = "ubuntu-noble-oci-amd64-root.manifest";

// The Debian world of `tests/spike_lock.rs`, rebuilt here for the comparison only.
const DEB_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const DEB_REPO: &str = "debuerreotype/docker-debian-artifacts";
const DEB_EPOCH: i64 = 1_789_603_200; // 2026-09-17T00:00:00Z
const DEB_LAYER_SIZE: u64 = 48_503_440;

// The Arch world of `tests/arch_base.rs`, rebuilt here for the comparison only.
const ARCH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/arch");
const ISO: &str = "https://archive.archlinux.org/iso/";
/// The dated bootstrap directories the synthetic ISO server lists. `2026.09.01` is the newest
/// that is not after the snapshot.
const ARCH_DATED: &[&str] = &["2024.05.01", "2026.09.01", "2026.10.01"];

fn now() -> i64 {
    parse_utc("2026-09-19T03:30:00Z").unwrap()
}

fn fixture(dir: &str, name: &str) -> String {
    fs::read_to_string(Path::new(dir).join(name)).expect("fixture exists")
}

fn synthetic_hash(name: &str) -> String {
    sha256_hex(format!("synthetic:{name}").as_bytes())
}

/// `Packages.xz` the way Ubuntu's archive publishes it: two concatenated streams, the second
/// empty, with stream padding between them. An `.xz` file is a sequence of streams, and a
/// decompressor that stopped after the first would read this index short (LD-73).
fn xz(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    lzma_rs::xz_compress(&mut BufReader::new(bytes), &mut out).unwrap();
    while out.len() % 4 != 0 {
        out.push(0);
    }
    lzma_rs::xz_compress(&mut BufReader::new(&b""[..]), &mut out).unwrap();
    out
}

/// The single-stream form, which Debian publishes.
fn xz_one_stream(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    lzma_rs::xz_compress(&mut BufReader::new(bytes), &mut out).unwrap();
    out
}

/// The upstream world: URL -> body, with every derived document consistent by construction.
#[derive(Clone)]
struct Upstream {
    files: BTreeMap<String, Vec<u8>>,
}

impl Upstream {
    fn ubuntu() -> Upstream {
        let mut u = Upstream {
            files: BTreeMap::new(),
        };
        u.set(IMAGES, dated_listing(DATED));
        for dir in DATED {
            let list = fixture(UBUNTU, PACKAGE_LIST);
            // Every dated directory publishes a checksum file over the two files that matter.
            u.set(
                &format!("{IMAGES}{dir}/SHA256SUMS"),
                format!(
                    "{} *{LAYER}\n{} *{PACKAGE_LIST}\n",
                    synthetic_hash(&format!("{dir}/{LAYER}")),
                    sha256_hex(list.as_bytes())
                ),
            );
            u.set(&format!("{IMAGES}{dir}/{PACKAGE_LIST}"), list);
        }
        for (suite, component, file) in [
            ("noble", "main", "noble.Packages"),
            ("noble", "universe", "noble-universe.Packages"),
            ("noble-updates", "main", "noble-updates.Packages"),
            ("noble-updates", "universe", "empty.Packages"),
            ("noble-security", "main", "noble-security.Packages"),
            ("noble-security", "universe", "empty.Packages"),
        ] {
            let packages = if file == "empty.Packages" {
                String::new()
            } else {
                fixture(UBUNTU, file)
            };
            u.set_ubuntu_index(suite, component, &packages);
        }
        u
    }

    /// The Arch world: the dated ISO listing with its checksum file, and one `<repo>.db` per
    /// repository built as the real gzip-tar of `desc` stanzas.
    fn arch() -> Upstream {
        let mut u = Upstream {
            files: BTreeMap::new(),
        };
        u.set(ISO, arch_listing(ARCH_DATED));
        let rootfs = fs::read(Path::new(ARCH).join("bootstrap-rootfs.synthetic")).unwrap();
        for date in ARCH_DATED {
            let layer = format!("archlinux-bootstrap-{date}-x86_64.tar.zst");
            u.set(
                &format!("{ISO}{date}/sha256sums.txt"),
                format!("{}  {layer}\n", sha256_hex(&rootfs)),
            );
            u.set(&format!("{ISO}{date}/{layer}"), rootfs.clone());
        }
        for (repository, desc, version) in [
            ("core", "core.desc", "2026.09.01-1"),
            ("extra", "extra.desc", "1.8.1-1"),
        ] {
            let text = fixture(ARCH, desc);
            let name = text
                .split("%NAME%\n")
                .nth(1)
                .and_then(|rest| rest.lines().next())
                .expect("the stanza names its package")
                .to_string();
            let database = support::gzip(&support::tar(&[support::file(
                &format!("{name}-{version}/desc"),
                &text,
                0o644,
            )]));
            u.set(
                &format!(
                    "https://archive.archlinux.org/repos/2026/09/18/{repository}/os/x86_64/{repository}.db"
                ),
                database,
            );
        }
        u
    }

    fn debian() -> Upstream {
        let mut u = Upstream {
            files: BTreeMap::new(),
        };
        u.set(
            &format!("https://api.github.com/repos/{DEB_REPO}/git/ref/heads/dist-amd64"),
            format!(r#"{{"object":{{"sha":"{DEB_COMMIT}","type":"commit"}}}}"#),
        );
        let raw = format!("https://raw.githubusercontent.com/{DEB_REPO}/{DEB_COMMIT}/bookworm/");
        u.set(&format!("{raw}rootfs.dpkg-arch"), "amd64\n");
        u.set(
            &format!("{raw}rootfs.debuerreotype-epoch"),
            format!("{DEB_EPOCH}\n"),
        );
        u.set(
            &format!("{raw}rootfs.manifest"),
            fixture(DEBIAN, "rootfs.manifest"),
        );
        // The commit's directory listing names the package list by its git blob id.
        u.set(
            &format!("https://api.github.com/repos/{DEB_REPO}/contents/bookworm?ref={DEB_COMMIT}"),
            format!(
                r#"[{{"name":"rootfs.manifest","type":"file","sha":"{}"}}]"#,
                lodi::debian::gitblob::blob_id(fixture(DEBIAN, "rootfs.manifest").as_bytes())
            ),
        );
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:{}","size":453}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"sha256:{}","size":{DEB_LAYER_SIZE}}}]}}"#,
            synthetic_hash("image-config.json"),
            synthetic_hash("rootfs.tar.gz")
        );
        u.set(
            &format!("{raw}oci/blobs/image-manifest.json"),
            &manifest[..],
        );
        u.set(
            &format!("{raw}oci/index.json"),
            format!(
                r#"{{"schemaVersion":2,"manifests":[{{"digest":"sha256:{}","size":{},"platform":{{"os":"linux","architecture":"amd64"}}}}]}}"#,
                sha256_hex(manifest.as_bytes()),
                manifest.len()
            ),
        );
        for (archive, suite) in [
            ("debian", "bookworm"),
            ("debian", "bookworm-updates"),
            ("debian-security", "bookworm-security"),
        ] {
            u.set_debian_index(
                archive,
                suite,
                &fixture(DEBIAN, &format!("{suite}.Packages")),
            );
        }
        u
    }

    fn set(&mut self, url: &str, body: impl Into<Vec<u8>>) {
        self.files.insert(url.to_string(), body.into());
    }

    fn get(&self, url: &str) -> String {
        String::from_utf8(self.files[url].clone()).unwrap()
    }

    /// One Ubuntu suite and component: `Packages.xz` and a `Release` that lists its size and
    /// SHA-256. As on the real archive, `Codename` is the bare release and `Suite` carries the
    /// pocket.
    fn set_ubuntu_index(&mut self, suite: &str, component: &str, packages: &str) {
        let base = format!("https://snapshot.ubuntu.com/ubuntu/{SNAPSHOT_ID}/");
        let compressed = xz(packages.as_bytes());
        let release_url = format!("{base}dists/{suite}/Release");
        let mut release = match self.files.get(&release_url) {
            Some(text) => String::from_utf8(text.clone()).unwrap(),
            None => format!(
                "Origin: Ubuntu\nLabel: Ubuntu\nSuite: {suite}\nVersion: 24.04\nCodename: noble\n\
                 Date: Thu, 17 Sep 2026 10:00:00 UTC\n\
                 Architectures: amd64 arm64 armhf i386 ppc64el riscv64 s390x\n\
                 Components: main restricted universe multiverse\n\
                 Description: synthetic fixture\nSHA256:\n"
            ),
        };
        release.push_str(&format!(
            " {} {} {component}/binary-amd64/Packages\n {} {} {component}/binary-amd64/Packages.xz\n",
            sha256_hex(packages.as_bytes()),
            packages.len(),
            sha256_hex(&compressed),
            compressed.len()
        ));
        self.set(&release_url, release);
        self.set(
            &format!("{base}dists/{suite}/{component}/binary-amd64/Packages.xz"),
            compressed,
        );
    }

    fn set_debian_index(&mut self, archive: &str, suite: &str, packages: &str) {
        let base = format!("https://snapshot.debian.org/archive/{archive}/{SNAPSHOT_ID}/");
        let compressed = xz_one_stream(packages.as_bytes());
        let components = if archive == "debian-security" {
            "updates/main updates/contrib"
        } else {
            "main contrib non-free-firmware"
        };
        let release = format!(
            "Origin: Debian\nLabel: Debian\nSuite: oldstable\nCodename: {suite}\n\
             Date: Thu, 17 Sep 2026 10:00:00 UTC\n\
             Architectures: all amd64 arm64\nComponents: {components}\n\
             Description: synthetic fixture\nSHA256:\n {} {} main/binary-amd64/Packages\n {} {} main/binary-amd64/Packages.xz\n",
            sha256_hex(packages.as_bytes()),
            packages.len(),
            sha256_hex(&compressed),
            compressed.len()
        );
        self.set(&format!("{base}dists/{suite}/Release"), release);
        self.set(
            &format!("{base}dists/{suite}/main/binary-amd64/Packages.xz"),
            compressed,
        );
    }

    fn fetcher(&self) -> FixtureFetcher {
        FixtureFetcher {
            files: self.files.clone(),
            log: Mutex::new(Vec::new()),
        }
    }
}

/// The HTML directory index the image server serves, in the shape Apache writes it.
fn dated_listing(dirs: &[&str]) -> String {
    let mut html = String::from(
        "<!DOCTYPE HTML PUBLIC \"-//W3C//DTD HTML 3.2 Final//EN\">\n<html>\n<body>\n\
         <h1>Index of /oci/noble</h1>\n<table>\n\
         <tr><td><a href=\"/oci/\">Parent Directory</a></td></tr>\n",
    );
    for dir in dirs {
        html.push_str(&format!(
            "<tr><td><a href=\"{dir}/\">{dir}/</a></td></tr>\n"
        ));
    }
    // The moving pointer the server also publishes; it is never what a lock pins.
    html.push_str(
        "<tr><td><a href=\"current/\">current/</a></td></tr>\n</table>\n</body></html>\n",
    );
    html
}

/// The HTML directory index `archive.archlinux.org/iso/` serves.
fn arch_listing(dirs: &[&str]) -> String {
    let mut html = String::from("<html><body><h1>Index of /iso</h1><pre>\n");
    html.push_str("<a href=\"../\">../</a>\n");
    for dir in dirs {
        html.push_str(&format!("<a href=\"{dir}/\">{dir}/</a>\n"));
    }
    html.push_str("<a href=\"latest/\">latest/</a>\n</pre></body></html>\n");
    html
}

struct FixtureFetcher {
    files: BTreeMap<String, Vec<u8>>,
    log: Mutex<Vec<String>>,
}

impl Fetcher for FixtureFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        self.log.lock().unwrap().push(url.to_string());
        self.files
            .get(url)
            .cloned()
            .ok_or_else(|| FetchError::Status(url.to_string(), 404))
    }

    fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

fn project(manifest: &str) -> PathBuf {
    let dir = support::scratch("ubuntu-base");
    fs::write(dir.join("lodi.toml"), manifest).unwrap();
    dir
}

const PACKAGES: &str = r#"["build-essential", "openssl", "jq"]"#;

fn manifest(distro: &str, release: &str, packages: &str) -> String {
    format!(
        "[project]\nname = \"ubuntu-base\"\n\n\
         [container]\ndistro   = \"{distro}\"\nrelease  = \"{release}\"\n\
         snapshot = \"{SNAPSHOT}\"\n\n[packages]\ncommon = {packages}\n"
    )
}

fn ubuntu_manifest() -> String {
    manifest("ubuntu", "noble", PACKAGES)
}

fn written(outcome: Outcome) -> LockFile {
    match outcome {
        Outcome::Written { lock, .. } => lock,
        Outcome::UpToDate(_) => panic!("expected a new lock"),
    }
}

fn lock_ok(dir: &Path, upstream: &Upstream) -> (LockFile, Vec<String>) {
    let fetcher = upstream.fetcher();
    let lock = written(lock_project(dir, &fetcher, now()).expect("lock succeeds"));
    (lock, fetcher.requests())
}

fn lock_err(dir: &Path, upstream: &Upstream) -> Failure {
    lock_project(dir, &upstream.fetcher(), now()).expect_err("lock fails")
}

fn code(failure: &Failure) -> String {
    failure.diagnostics[0].code.to_string()
}

fn closure<'a>(lock: &'a LockFile, name: &str) -> &'a lodi::lock::ClosureEntry {
    let base = lock.base.as_ref().unwrap();
    base.closure
        .packages
        .iter()
        .find(|p| p.name == name)
        .unwrap_or_else(|| panic!("{name} is in the closure"))
}

// ---------------------------------------------------------------------------------------------
// (a) the lock an Ubuntu manifest produces

#[test]
fn locks_ubuntu_noble_pinned_and_chained_to_its_release_files() {
    let dir = project(&ubuntu_manifest());
    let (lock, requests) = lock_ok(&dir, &Upstream::ubuntu());
    let base = lock.base.as_ref().expect("the lock pins a base");

    assert_eq!(
        (base.distro.as_str(), base.release.as_str(), base.pinned),
        ("ubuntu", "noble", true)
    );
    assert_eq!(
        (base.arch.as_str(), base.deb_arch.as_str()),
        ("x86_64", "amd64")
    );
    assert_eq!(base.snapshot, SNAPSHOT);

    // The rootfs is pinned by the checksum file of the newest dated directory that is not
    // after the snapshot — never by `current/`, and never by a later directory.
    let rootfs = &base.rootfs;
    assert_eq!(rootfs.url, format!("{IMAGES}{PINNED_DIR}/{LAYER}"));
    assert_eq!(
        rootfs.sha256,
        format!(
            "sha256:{}",
            synthetic_hash(&format!("{PINNED_DIR}/{LAYER}"))
        )
    );
    assert_eq!(rootfs.epoch, "2026-09-17T00:00:00Z");
    assert_eq!(rootfs.source, "builtin:ubuntu.toml");
    assert_eq!(rootfs.format, "tar.gz");
    let checksums = rootfs.checksums.as_ref().expect("a checksum-file pin");
    assert_eq!(checksums.path, format!("{IMAGES}{PINNED_DIR}/SHA256SUMS"));
    assert_eq!(
        checksums.sha256,
        format!(
            "sha256:{}",
            sha256_hex(
                Upstream::ubuntu()
                    .get(&format!("{IMAGES}{PINNED_DIR}/SHA256SUMS"))
                    .as_bytes()
            )
        )
    );
    // No Debian-shaped pin is invented for it, and no size is claimed that nothing measured.
    assert!(rootfs.commit.is_none() && rootfs.repo.is_none() && rootfs.oci_manifest.is_none());
    assert!(rootfs.size.is_none());
    assert_eq!(
        rootfs.package_list.as_ref().unwrap().path,
        format!("{IMAGES}{PINNED_DIR}/{PACKAGE_LIST}")
    );

    // One repository, three suites, two components: every index hash is the one its own
    // `Release` lists, and the `Release` is recorded with its own hash.
    let upstream = Upstream::ubuntu();
    let [repo] = base.repositories.as_slice() else {
        panic!("one repository: {:?}", base.repositories)
    };
    assert_eq!(repo.name, "ubuntu");
    assert_eq!(
        repo.url,
        format!("https://snapshot.ubuntu.com/ubuntu/{SNAPSHOT_ID}/")
    );
    assert_eq!(repo.suites, ["noble", "noble-updates", "noble-security"]);
    assert_eq!(repo.components, ["main", "universe"]);
    assert_eq!(repo.indexes.len(), 6);
    for index in &repo.indexes {
        let release_url = format!("{}{}", repo.url, index.release.path);
        assert_eq!(
            index.release.sha256,
            format!(
                "sha256:{}",
                sha256_hex(upstream.get(&release_url).as_bytes())
            ),
            "{release_url}"
        );
        let listed = format!(
            " {} {} {}/binary-amd64/Packages.xz\n",
            index.packages.sha256.strip_prefix("sha256:").unwrap(),
            index.packages.size,
            index.component
        );
        assert!(
            upstream.get(&release_url).contains(&listed),
            "{release_url} does not list {listed:?}"
        );
        assert_eq!(
            index.packages.path,
            format!(
                "dists/{}/{}/binary-amd64/Packages.xz",
                index.suite, index.component
            )
        );
    }

    // The closure: the base at its rootfs versions, the requested packages resolved across the
    // suites and components, and nothing downloaded to find out.
    assert_eq!(closure(&lock, "base-files").origin, "base");
    assert_eq!(closure(&lock, "tzdata").origin, "base");
    assert!(closure(&lock, "tzdata").sha256.is_none());
    // `libc6` is upgraded out of `noble-updates` because `make` needs that version.
    let libc6 = closure(&lock, "libc6");
    assert_eq!(
        (libc6.version.as_str(), libc6.origin.as_str()),
        ("2.39-0ubuntu8.4", "install")
    );
    assert_eq!(libc6.suite.as_deref(), Some("noble-updates"));
    // `openssl` comes from the same archive's security suite, `jq` from `universe`.
    assert_eq!(
        closure(&lock, "openssl").suite.as_deref(),
        Some("noble-security")
    );
    assert_eq!(closure(&lock, "libjq1").suite.as_deref(), Some("noble"));
    assert_eq!(closure(&lock, "jq").repository.as_deref(), Some("ubuntu"));
    for name in ["gcc", "gcc-13", "make", "libc6-dev", "mawk"] {
        assert!(!closure(&lock, name).version.is_empty(), "{name}");
    }

    assert!(
        !requests
            .iter()
            .any(|u| u.ends_with(".deb") || u.ends_with(".tar.gz")),
        "locking downloaded an artifact: {requests:?}"
    );
}

#[test]
fn a_second_lock_of_a_fresh_manifest_asks_for_nothing() {
    let dir = project(&ubuntu_manifest());
    let upstream = Upstream::ubuntu();
    lock_ok(&dir, &upstream);
    let fetcher = upstream.fetcher();
    match lock_project(&dir, &fetcher, now()).expect("the second lock succeeds") {
        Outcome::UpToDate(_) => {}
        Outcome::Written { .. } => panic!("the lock was rewritten although it is fresh"),
    }
    assert!(fetcher.requests().is_empty(), "{:?}", fetcher.requests());
}

// ---------------------------------------------------------------------------------------------
// (e) and the rest of the failure surface: it fails closed

#[test]
fn a_tampered_index_fails_with_e_hash_mismatch() {
    let dir = project(&ubuntu_manifest());
    let mut upstream = Upstream::ubuntu();
    let url = format!(
        "https://snapshot.ubuntu.com/ubuntu/{SNAPSHOT_ID}/dists/noble/main/binary-amd64/Packages.xz"
    );
    let tampered = xz(fixture(UBUNTU, "noble.Packages")
        .replace(
            "Description: synthetic fixture entry for make",
            "Description: tampered",
        )
        .as_bytes());
    upstream.set(&url, tampered);
    let failure = lock_err(&dir, &upstream);
    assert_eq!(code(&failure), "E_HASH_MISMATCH");
    let text = failure.diagnostics[0].to_string();
    assert!(text.contains("Release"), "{text}");
    assert!(text.contains(&url), "{text}");
}

#[test]
fn a_tampered_package_list_fails_against_its_checksum_file() {
    let dir = project(&ubuntu_manifest());
    let mut upstream = Upstream::ubuntu();
    upstream.set(
        &format!("{IMAGES}{PINNED_DIR}/{PACKAGE_LIST}"),
        fixture(UBUNTU, PACKAGE_LIST).replace("tzdata\t2024a-2ubuntu1\n", ""),
    );
    let failure = lock_err(&dir, &upstream);
    assert_eq!(code(&failure), "E_HASH_MISMATCH");
    assert!(
        failure.diagnostics[0].to_string().contains("SHA256SUMS"),
        "{}",
        failure.diagnostics[0]
    );
}

#[test]
fn a_checksum_file_that_does_not_list_the_rootfs_is_refused() {
    let dir = project(&ubuntu_manifest());
    let mut upstream = Upstream::ubuntu();
    let sums = upstream.get(&format!("{IMAGES}{PINNED_DIR}/SHA256SUMS"));
    let without = sums
        .lines()
        .filter(|l| !l.ends_with(LAYER))
        .collect::<Vec<_>>()
        .join("\n");
    upstream.set(
        &format!("{IMAGES}{PINNED_DIR}/SHA256SUMS"),
        format!("{without}\n"),
    );
    let failure = lock_err(&dir, &upstream);
    assert_eq!(code(&failure), "E_REPO_UNREACHABLE");
    assert!(
        failure.diagnostics[0].to_string().contains(LAYER),
        "{}",
        failure.diagnostics[0]
    );
}

#[test]
fn a_release_file_for_another_suite_is_refused() {
    let dir = project(&ubuntu_manifest());
    let mut upstream = Upstream::ubuntu();
    let url =
        format!("https://snapshot.ubuntu.com/ubuntu/{SNAPSHOT_ID}/dists/noble-updates/Release");
    let wrong = upstream
        .get(&url)
        .replace("Suite: noble-updates", "Suite: jammy-updates")
        .replace("Codename: noble", "Codename: jammy");
    upstream.set(&url, wrong);
    let failure = lock_err(&dir, &upstream);
    assert_eq!(code(&failure), "E_REPO_UNREACHABLE");
    assert!(
        failure.diagnostics[0].to_string().contains("noble-updates"),
        "{}",
        failure.diagnostics[0]
    );
}

#[test]
fn a_snapshot_older_than_every_published_base_is_refused() {
    // Older than the oldest dated directory, but not before the release itself: the base, not
    // the archive, is what cannot be served.
    let dir =
        project(&manifest("ubuntu", "noble", PACKAGES).replace(SNAPSHOT, "2026-08-01T00:00:00Z"));
    let mut upstream = Upstream::ubuntu();
    // Serve the indexes at the older snapshot id too, so the failure is about the base alone.
    for url in upstream.files.keys().cloned().collect::<Vec<_>>() {
        if let Some(rest) = url.strip_prefix(&format!(
            "https://snapshot.ubuntu.com/ubuntu/{SNAPSHOT_ID}/"
        )) {
            let bytes = upstream.files[&url].clone();
            upstream.set(
                &format!("https://snapshot.ubuntu.com/ubuntu/20260801T000000Z/{rest}"),
                bytes,
            );
        }
    }
    let failure = lock_err(&dir, &upstream);
    assert_eq!(code(&failure), "E_SNAPSHOT_TOO_OLD");
    assert!(
        failure.diagnostics[0].to_string().contains(DATED[0]),
        "{}",
        failure.diagnostics[0]
    );
}

#[test]
fn an_unknown_package_names_the_nearest_ubuntu_names() {
    let dir = project(&manifest("ubuntu", "noble", r#"["buildessential"]"#));
    let failure = lock_err(&dir, &Upstream::ubuntu());
    assert_eq!(code(&failure), "E_NO_MATCH");
    let text = failure.diagnostics[0].to_string();
    assert!(
        text.contains("package `buildessential` is not in the pinned Ubuntu indexes"),
        "{text}"
    );
    assert!(text.contains("build-essential"), "{text}");
}

#[test]
fn an_unsatisfiable_version_and_a_conflict_are_refused() {
    for (packages, code_expected, needle) in [
        (r#"["ubuntu-needs-future-ssl"]"#, "E_NO_MATCH", "libssl3t64"),
        (r#"["ubuntu-conflicts-make", "make"]"#, "E_NO_MATCH", "make"),
        (
            r#"["ubuntu-needs-i386"]"#,
            "E_UNSUPPORTED_ARCH",
            "libc6:i386",
        ),
    ] {
        let dir = project(&manifest("ubuntu", "noble", packages));
        let failure = lock_err(&dir, &Upstream::ubuntu());
        assert_eq!(code(&failure), code_expected, "{packages}");
        assert!(
            failure.diagnostics[0].to_string().contains(needle),
            "{packages}: {}",
            failure.diagnostics[0]
        );
    }
}

#[test]
fn a_virtual_name_with_several_providers_names_them() {
    let dir = project(&manifest(
        "ubuntu",
        "noble",
        r#"["ubuntu-wants-a-compiler"]"#,
    ));
    let mut upstream = Upstream::ubuntu();
    let mut main = fixture(UBUNTU, "noble.Packages");
    main.push_str(
        "\nPackage: ubuntu-wants-a-compiler\nPriority: optional\nSection: misc\n\
         Architecture: amd64\nVersion: 1.0-1\nDepends: c-compiler\n\
         Filename: pool/main/u/ubuntu-wants-a-compiler/ubuntu-wants-a-compiler_1.0-1_amd64.deb\n\
         Size: 4000\nSHA256: ",
    );
    main.push_str(&synthetic_hash("wants-a-compiler"));
    main.push_str("\nDescription: synthetic fixture entry\n");
    upstream.set_ubuntu_index("noble", "main", &main);
    let failure = lock_err(&dir, &upstream);
    assert_eq!(code(&failure), "E_NO_MATCH");
    let text = failure.diagnostics[0].to_string();
    assert!(text.contains("gcc") && text.contains("tcc"), "{text}");
}

// ---------------------------------------------------------------------------------------------
// (d) the Debian path is not regressed, and the two locks differ only where the definitions do

/// Every key path of a JSON document, with array indexes collapsed, so two locks can be
/// compared by shape rather than by content.
fn key_paths(value: &serde_json::Value, prefix: &str, out: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                out.insert(path.clone());
                key_paths(v, &path, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                key_paths(item, &format!("{prefix}[]"), out);
            }
        }
        _ => {}
    }
}

fn shape(lock: &LockFile) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    key_paths(&serde_json::to_value(lock).unwrap(), "", &mut out);
    out
}

#[test]
fn the_three_distros_differ_only_where_their_definitions_differ() {
    let debian_dir = project(&manifest("debian", "bookworm", r#"["build-essential"]"#));
    let ubuntu_dir = project(&manifest("ubuntu", "noble", r#"["build-essential"]"#));
    // `jq` is what the Arch fixtures' `extra` carries; the requested set differs because the
    // distributions do not name the same packages, which is a difference of their worlds and
    // not of this code.
    let arch_dir = project(&manifest("arch", "rolling", r#"["jq"]"#));
    let (debian, _) = lock_ok(&debian_dir, &Upstream::debian());
    let (ubuntu, _) = lock_ok(&ubuntu_dir, &Upstream::ubuntu());
    let (arch, _) = lock_ok(&arch_dir, &Upstream::arch());
    let (d, u, a) = (
        debian.base.as_ref().unwrap(),
        ubuntu.base.as_ref().unwrap(),
        arch.base.as_ref().unwrap(),
    );

    // The same schema, the same architecture, the same snapshot, all pinned, same options.
    for lock in [&debian, &ubuntu, &arch] {
        assert_eq!(lock.version, lodi::lock::LOCK_VERSION);
        assert_eq!(lock.format, debian.format);
        assert_eq!(lock.generated_by, debian.generated_by);
    }
    for base in [d, u, a] {
        assert_eq!(base.arch, "x86_64");
        assert_eq!(base.snapshot, SNAPSHOT);
        assert!(base.pinned);
        assert_eq!(base.options, d.options);
    }
    assert_eq!(d.requested, u.requested);
    // The package architecture is the distribution's own word for one machine architecture
    // (design call D11): `amd64` for the apt family, `x86_64` for the other.
    assert_eq!(
        (d.deb_arch.as_str(), u.deb_arch.as_str()),
        ("amd64", "amd64")
    );
    assert_eq!(a.deb_arch, "x86_64");

    // What the definitions say differs, differs — and nothing is shared that should not be.
    assert_eq!(
        [d.distro.as_str(), u.distro.as_str(), a.distro.as_str()],
        ["debian", "ubuntu", "arch"]
    );
    for (left, right) in [(d, u), (d, a), (u, a)] {
        assert_ne!(left.distro, right.distro);
        assert_ne!(left.release, right.release);
        assert_ne!(left.rootfs.url, right.rootfs.url);
        assert_ne!(left.rootfs.source, right.rootfs.source);
    }
    assert_eq!(d.repositories.len(), 2);
    assert_eq!(u.repositories.len(), 1);
    assert_eq!(a.repositories.len(), 2);
    assert!(u.repositories[0].url.contains("snapshot.ubuntu.com"));
    assert!(
        d.repositories
            .iter()
            .all(|r| r.url.contains("snapshot.debian.org"))
    );
    assert!(
        a.repositories
            .iter()
            .all(|r| r.url.contains("archive.archlinux.org"))
    );
    // An apt repository is addressed by suite and component; a pacman repository by neither,
    // and its one database is both the index and the statement of its own digest.
    for base in [d, u] {
        assert!(
            base.repositories
                .iter()
                .all(|r| !r.suites.is_empty() && !r.components.is_empty())
        );
    }
    for repository in &a.repositories {
        assert!(repository.suites.is_empty() && repository.components.is_empty());
        assert_eq!(repository.indexes.len(), 1);
        assert_eq!(
            repository.indexes[0].release.sha256,
            repository.indexes[0].packages.sha256
        );
    }

    // The structural differences are the rootfs pin and the base package list; every other key
    // path is the same.
    let (ds, us, as_) = (shape(&debian), shape(&ubuntu), shape(&arch));
    let only =
        |mine: &BTreeSet<String>, a: &BTreeSet<String>, b: &BTreeSet<String>| -> Vec<String> {
            mine.difference(a)
                .filter(|k| !b.contains(*k))
                .cloned()
                .collect()
        };
    assert_eq!(
        only(&ds, &us, &as_),
        [
            "base.rootfs.commit",
            "base.rootfs.ociManifest",
            "base.rootfs.repo",
            "base.rootfs.size",
        ],
        "Debian-only key paths"
    );
    assert_eq!(
        only(&us, &ds, &as_),
        Vec::<String>::new(),
        "Ubuntu shares every key path with one of the other two"
    );
    assert_eq!(
        only(&as_, &ds, &us),
        ["base.rootfs.subdir"],
        "Arch-only key paths"
    );
    // The checksum-file pin is shared by the two `dated_checksum_dir` bases and absent from the
    // one pinned by a commit and an OCI manifest.
    for key in [
        "base.rootfs.checksums",
        "base.rootfs.checksums.path",
        "base.rootfs.checksums.sha256",
    ] {
        assert!(
            us.contains(key) && as_.contains(key) && !ds.contains(key),
            "{key}"
        );
    }
    // The base package list is shared by the two apt bases and absent from the pacman one,
    // which is the difference the recorded base set exists for.
    for key in [
        "base.rootfs.packageList",
        "base.rootfs.packageList.path",
        "base.rootfs.packageList.sha256",
    ] {
        assert!(
            ds.contains(key) && us.contains(key) && !as_.contains(key),
            "{key}"
        );
    }

    // Every closure is complete and sorted, and carries the artifact data the image build needs.
    for lock in [&debian, &ubuntu, &arch] {
        let base = lock.base.as_ref().unwrap();
        assert!(base.closure.packages.iter().any(|p| p.origin == "install"));
        assert!(
            base.closure
                .packages
                .iter()
                .all(|p| p.origin != "install" || p.sha256.is_some())
        );
        assert!(
            base.closure
                .packages
                .windows(2)
                .all(|w| w[0].name <= w[1].name)
        );
    }
    // An apt rootfs publishes its own package list, so its closure names the base packages too;
    // an Arch bootstrap publishes none, so its closure is what the lock installs and the build
    // records what the bootstrap brought inside the image instead (M-Arch-Base T-6).
    for base in [d, u] {
        assert!(base.closure.packages.iter().any(|p| p.origin == "base"));
        assert!(base.rootfs.package_list.is_some());
    }
    assert!(a.closure.packages.iter().all(|p| p.origin == "install"));
    assert!(a.rootfs.package_list.is_none());
    assert_eq!(a.rootfs.subdir.as_deref(), Some("root.x86_64"));

    // The Debian lock this build writes is still what the Debian path wrote before T-4: the
    // pin fields it used are all there, and the closure resolves from the same fixtures.
    assert_eq!(d.rootfs.commit.as_deref(), Some(DEB_COMMIT));
    assert_eq!(d.rootfs.size, Some(DEB_LAYER_SIZE));
    assert!(d.rootfs.checksums.is_none());

    for dir in [debian_dir, ubuntu_dir, arch_dir] {
        fs::remove_dir_all(&dir).unwrap();
    }
}

// ---------------------------------------------------------------------------------------------
// The surface the binary refuses (D10/LD-55: one Ubuntu release, not two)

fn lodi_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("lodi")
}

#[test]
fn another_ubuntu_release_is_unsupported_and_asks_for_nothing() {
    for release in ["22.04", "jammy", "26.04", "oracular"] {
        let dir = project(&manifest("ubuntu", release, PACKAGES));
        // Through the library: the manifest is refused before any fetch happens.
        let fetcher = Upstream::ubuntu().fetcher();
        let failure = lock_project(&dir, &fetcher, now()).expect_err("lock fails");
        assert_eq!(code(&failure), "E_UNSUPPORTED", "{release}");
        let text = failure.diagnostics[0].to_string();
        assert!(
            text.contains("ubuntu") && text.contains("noble"),
            "{release}: {text}"
        );
        assert!(
            fetcher.requests().is_empty(),
            "{release}: {:?}",
            fetcher.requests()
        );

        // And through the binary, with no network configured at all: exit status 3.
        // `develop` locks first, so the refusal is its own (LD-496).
        let out = Command::new(lodi_binary())
            .args(["develop", "--trust", "--", "true"])
            .current_dir(&dir)
            .env_clear()
            .env("PATH", "")
            .env("HOME", dir.join("home"))
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("LODI_HOME", dir.join("lodi-home"))
            .output()
            .expect("the binary runs");
        assert_eq!(out.status.code(), Some(3), "{release}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("E_UNSUPPORTED"), "{release}: {stderr}");
        assert!(!dir.join("lodi.lock").exists(), "{release} wrote a lock");
        fs::remove_dir_all(&dir).unwrap();
    }
}

/// A distribution outside the supported ones is refused before anything is fetched, and the
/// diagnostic names them all (M-Arch-Base T-6 replaced the Arch case of this test, which was the
/// same check for `arch` while it was the refused one; fd-1 made it `gentoo` when Fedora joined).
#[test]
fn a_distro_outside_the_three_is_refused_and_asks_for_nothing() {
    let dir = project(&manifest("gentoo", "41", PACKAGES));
    let fetcher = Upstream::ubuntu().fetcher();
    let failure = lock_project(&dir, &fetcher, now()).expect_err("lock fails");
    assert_eq!(code(&failure), "E_TYPE");
    let text = failure.diagnostics[0].to_string();
    for distro in ["arch", "debian", "fedora", "ubuntu"] {
        assert!(text.contains(distro), "{distro} is not offered: {text}");
    }
    assert!(fetcher.requests().is_empty());
    fs::remove_dir_all(&dir).unwrap();
}

/// A release an Arch base does not have is refused the same way another Ubuntu release is, and
/// the hint names the one release there is rather than assuming a second.
#[test]
fn another_arch_release_is_unsupported_and_asks_for_nothing() {
    let dir = project(&manifest("arch", "2026.09.01", PACKAGES));
    let fetcher = Upstream::arch().fetcher();
    let failure = lock_project(&dir, &fetcher, now()).expect_err("lock fails");
    assert_eq!(code(&failure), "E_UNSUPPORTED");
    let text = failure.diagnostics[0].to_string();
    assert!(text.contains("arch \"rolling\""), "{text}");
    assert!(fetcher.requests().is_empty());
    fs::remove_dir_all(&dir).unwrap();
}
