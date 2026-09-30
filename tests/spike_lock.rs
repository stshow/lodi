//! M-Spike S-2: `lodi lock` — snapshot, base and closure pinning and upstream tool resolution.
//!
//! Offline: every upstream document is served from the synthetic fixtures in
//! `tests/fixtures/spike/lock/` (see its README), either by an in-memory fetcher (library
//! tests) or by a loopback HTTP server that the real `lodi` binary reaches through
//! `LODI_FETCH_REWRITE` (CLI tests). Live discovery is S-5's evidence.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lodi::debian::index::parse_packages;
use lodi::debian::resolve::{Origin, ResolveError, resolve};
use lodi::debian::version::DebVersion;
use lodi::fetch::{FetchError, Fetcher};
use lodi::lock::{
    self, Failure, LOCK_FILE, LockFile, LockProblem, Outcome, frozen, lock_project, parse_lock,
    staleness, write_atomic_with,
};
use lodi::util::{parse_utc, sha256_hex};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/spike/lock");
const SNAPSHOT: &str = "2026-09-18T00:00:00Z";
const SNAPSHOT_ID: &str = "20260918T000000Z";
const EPOCH: i64 = 1_789_603_200; // 2026-09-17T00:00:00Z
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const REPO: &str = "debuerreotype/docker-debian-artifacts";
const LAYER_SIZE: u64 = 48_503_440;

fn now() -> i64 {
    parse_utc("2026-09-19T03:30:00Z").unwrap()
}

fn fixture(name: &str) -> String {
    fs::read_to_string(Path::new(FIXTURES).join(name)).expect("fixture exists")
}

fn synthetic_hash(name: &str) -> String {
    sha256_hex(format!("synthetic:{name}").as_bytes())
}

fn xz(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    lzma_rs::xz_compress(&mut BufReader::new(bytes), &mut out).unwrap();
    out
}

fn raw_base() -> String {
    format!("https://raw.githubusercontent.com/{REPO}/{COMMIT}/bookworm/")
}

fn tree_url() -> String {
    format!("https://api.github.com/repos/{REPO}/contents/bookworm?ref={COMMIT}")
}

fn snapshot_url(archive: &str) -> String {
    format!("https://snapshot.debian.org/archive/{archive}/{SNAPSHOT_ID}/")
}

/// The upstream world: URL -> body. Derived documents are consistent with the fixtures.
#[derive(Clone)]
struct Upstream {
    files: BTreeMap<String, Vec<u8>>,
}

impl Upstream {
    fn standard() -> Upstream {
        let mut u = Upstream {
            files: BTreeMap::new(),
        };
        u.set(
            &format!("https://api.github.com/repos/{REPO}/git/ref/heads/dist-amd64"),
            format!(
                r#"{{"ref":"refs/heads/dist-amd64","object":{{"sha":"{COMMIT}","type":"commit"}}}}"#
            ),
        );
        let raw = raw_base();
        u.set(&format!("{raw}rootfs.dpkg-arch"), "amd64\n");
        u.set(
            &format!("{raw}rootfs.debuerreotype-epoch"),
            format!("{EPOCH}\n"),
        );
        u.set(&format!("{raw}rootfs.manifest"), fixture("rootfs.manifest"));
        u.set_tree(&fixture("rootfs.manifest"));
        u.set_image_manifest(&format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:{}","size":453}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"sha256:{}","size":{LAYER_SIZE}}}]}}"#,
            synthetic_hash("image-config.json"),
            synthetic_hash("rootfs.tar.gz")
        ));
        for (archive, suite) in [
            ("debian", "bookworm"),
            ("debian", "bookworm-updates"),
            ("debian-security", "bookworm-security"),
        ] {
            u.set_index(
                archive,
                suite,
                &fixture(&format!("{suite}.Packages")),
                "all amd64 arm64",
            );
        }

        let index = fixture("nodejs-index.json");
        u.set("https://nodejs.org/dist/index.json", index.clone());
        let entries: serde_json::Value = serde_json::from_str(&index).unwrap();
        for entry in entries.as_array().unwrap() {
            let v = entry["version"].as_str().unwrap();
            let names = [
                format!("node-{v}-linux-x64.tar.xz"),
                format!("node-{v}-linux-x64.tar.gz"),
                format!("node-{v}-linux-arm64.tar.xz"),
            ];
            let sums: String = names
                .iter()
                .map(|n| format!("{}  {n}\n", synthetic_hash(n)))
                .collect();
            u.set(&format!("https://nodejs.org/dist/{v}/SHASUMS256.txt"), sums);
        }

        let releases: serde_json::Value =
            serde_json::from_str(&fixture("python-releases.json")).unwrap();
        let all = releases.as_array().unwrap();
        u.set(
            "https://api.github.com/repos/astral-sh/python-build-standalone/releases?per_page=2",
            serde_json::to_string(&all[..2]).unwrap(),
        );
        for release in all {
            let tag = release["tag_name"].as_str().unwrap();
            let sums: String = release["assets"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|a| {
                    let digest = a["digest"].as_str()?.strip_prefix("sha256:")?;
                    Some(format!("{digest}  {}\n", a["name"].as_str()?))
                })
                .collect();
            u.set(
                &format!(
                    "https://github.com/astral-sh/python-build-standalone/releases/download/{tag}/SHA256SUMS"
                ),
                sums,
            );
        }
        u
    }

    fn set(&mut self, url: &str, body: impl Into<Vec<u8>>) {
        self.files.insert(url.to_string(), body.into());
    }

    fn get(&self, url: &str) -> Vec<u8> {
        self.files[url].clone()
    }

    /// The base commit's directory listing, naming `rootfs.manifest` by the git blob id of
    /// `manifest` (the shape of the GitHub contents API).
    fn set_tree(&mut self, manifest: &str) {
        self.set(
            &tree_url(),
            format!(
                r#"[{{"name":"oci","path":"bookworm/oci","type":"dir","sha":"{}"}},{{"name":"rootfs.manifest","path":"bookworm/rootfs.manifest","type":"file","size":{},"sha":"{}"}}]"#,
                "1".repeat(40),
                manifest.len(),
                lodi::debian::gitblob::blob_id(manifest.as_bytes())
            ),
        );
    }

    fn set_image_manifest(&mut self, manifest: &str) {
        let raw = raw_base();
        self.set(&format!("{raw}oci/blobs/image-manifest.json"), manifest);
        self.set(
            &format!("{raw}oci/index.json"),
            format!(
                r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:{}","size":{},"platform":{{"os":"linux","architecture":"amd64"}}}}]}}"#,
                sha256_hex(manifest.as_bytes()),
                manifest.len()
            ),
        );
    }

    /// Serve `packages` as `dists/<suite>/main/binary-amd64/Packages.xz` with a `Release` that
    /// lists its size and SHA-256 (and those of the uncompressed index).
    fn set_index(&mut self, archive: &str, suite: &str, packages: &str, architectures: &str) {
        let base = snapshot_url(archive);
        let compressed = xz(packages.as_bytes());
        // As observed on the real archives: the security suite names its components
        // `updates/<component>` while its index paths start at `<component>/`.
        let components = if archive == "debian-security" {
            "updates/main updates/contrib updates/non-free-firmware updates/non-free"
        } else {
            "main contrib non-free-firmware non-free"
        };
        let release = format!(
            "Origin: Debian\nLabel: Debian\nSuite: {suite}\nCodename: {suite}\n\
             Date: Thu, 17 Sep 2026 10:00:00 UTC\nAcquire-By-Hash: yes\n\
             Architectures: {architectures}\nComponents: {components}\n\
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

/// A fresh project directory below the cargo target directory.
fn project(manifest: &str) -> PathBuf {
    let dir = support::scratch("spike-lock");
    fs::write(dir.join("lodi.toml"), manifest).unwrap();
    dir
}

const MANIFEST: &str = r#"
[project]
name = "spike-lock"

[container]
distro   = "debian"
release  = "bookworm"
snapshot = "2026-09-18T00:00:00Z"

[packages]
common = ["pkg-config", "libssl-dev", "build-essential"]

[tools]
python = "3.12"
nodejs = "22"
"#;

fn with_packages(names: &str) -> String {
    MANIFEST.replace(
        r#"common = ["pkg-config", "libssl-dev", "build-essential"]"#,
        &format!("common = [{names}]"),
    )
}

fn written(outcome: Outcome) -> (LockFile, Vec<String>) {
    match outcome {
        Outcome::Written { lock, resolved } => (lock, resolved),
        Outcome::UpToDate(_) => panic!("expected a new lock"),
    }
}

fn lock_ok(dir: &Path, upstream: &Upstream) -> (LockFile, Vec<String>, Vec<String>) {
    let fetcher = upstream.fetcher();
    let (lock, resolved) = written(lock_project(dir, &fetcher, now()).expect("lock succeeds"));
    (lock, resolved, fetcher.requests())
}

fn lock_err(dir: &Path, upstream: &Upstream) -> Failure {
    lock_project(dir, &upstream.fetcher(), now()).expect_err("lock fails")
}

fn closure_entry<'a>(lock: &'a LockFile, name: &str) -> &'a lock::ClosureEntry {
    let base = lock.base.as_ref().unwrap();
    base.closure
        .packages
        .iter()
        .find(|p| p.name == name)
        .unwrap_or_else(|| panic!("{name} is in the closure"))
}

fn is_artifact(url: &str) -> bool {
    url.ends_with(".deb")
        || url.ends_with("rootfs.tar.gz")
        || url.ends_with(".tar.xz")
        || url.ends_with(".tar.gz")
        || url.contains("%2B")
}

// ---------------------------------------------------------------------------------------------
// The complete lock

#[test]
fn locks_base_snapshot_closure_and_tools_without_realization() {
    let marker = support::scratch("s2-marker").join("must-not-exist");
    let dir = project(&MANIFEST.replace(
        "[tools]",
        &format!(
            "[tasks.mark]\nrun = \"touch {}\"\n\n[tools]",
            marker.display()
        ),
    ));
    let upstream = Upstream::standard();
    let (lock, resolved, requests) = lock_ok(&dir, &upstream);
    assert_eq!(resolved, ["base", "nodejs", "python"]);

    // The base: rootfs pinned to a commit and the OCI layer digest, never downloaded.
    let base = lock.base.as_ref().unwrap();
    assert_eq!(
        (base.distro.as_str(), base.release.as_str()),
        ("debian", "bookworm")
    );
    assert_eq!(
        (base.arch.as_str(), base.deb_arch.as_str()),
        ("x86_64", "amd64")
    );
    assert_eq!(base.snapshot, SNAPSHOT);
    assert!(base.pinned);
    assert_eq!(base.rootfs.commit.as_deref(), Some(COMMIT));
    assert_eq!(base.rootfs.repo.as_deref(), Some(REPO));
    assert!(base.rootfs.oci_manifest.is_some() && base.rootfs.checksums.is_none());
    assert_eq!(
        base.rootfs.url,
        format!("{}oci/blobs/rootfs.tar.gz", raw_base())
    );
    assert_eq!(
        base.rootfs.sha256,
        format!("sha256:{}", synthetic_hash("rootfs.tar.gz"))
    );
    assert_eq!(base.rootfs.size, Some(LAYER_SIZE));
    assert_eq!(base.rootfs.epoch, "2026-09-17T00:00:00Z");
    assert_eq!(
        base.rootfs.package_list.as_ref().unwrap().sha256,
        format!(
            "sha256:{}",
            sha256_hex(fixture("rootfs.manifest").as_bytes())
        )
    );

    // Repositories: the snapshot URLs and the exact index bytes that fed the closure.
    let urls: Vec<&str> = base.repositories.iter().map(|r| r.url.as_str()).collect();
    assert_eq!(
        urls,
        [snapshot_url("debian"), snapshot_url("debian-security")]
    );
    assert_eq!(
        base.repositories[0].suites,
        ["bookworm", "bookworm-updates"]
    );
    let security = &base.repositories[1].indexes[0];
    assert_eq!(
        security.packages.path,
        "dists/bookworm-security/main/binary-amd64/Packages.xz"
    );
    let served = upstream.get(&format!(
        "{}{}",
        snapshot_url("debian-security"),
        security.packages.path
    ));
    assert_eq!(
        security.packages.sha256,
        format!("sha256:{}", sha256_hex(&served))
    );
    assert_eq!(security.packages.size, served.len() as u64);
    let release = upstream.get(&format!(
        "{}dists/bookworm-security/Release",
        snapshot_url("debian-security")
    ));
    assert_eq!(
        security.release.sha256,
        format!("sha256:{}", sha256_hex(&release))
    );

    // The closure: requested names sorted, the full transitive set, base packages kept.
    assert_eq!(
        base.requested["default"],
        ["build-essential", "libssl-dev", "pkg-config"]
    );
    let names: Vec<&str> = base
        .closure
        .packages
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    let expected = [
        "base-files",
        "binutils",
        "binutils-common",
        "build-essential",
        "cpp",
        "cpp-12",
        "dpkg",
        "dpkg-dev",
        "g++",
        "g++-12",
        "gcc",
        "gcc-12",
        "gcc-12-base",
        "libc-dev-bin",
        "libc6",
        "libc6-dev",
        "libdpkg-perl",
        "libgcc-s1",
        "libpkgconf3",
        "libssl-dev",
        "libssl3",
        "linux-libc-dev",
        "make",
        "mawk",
        "perl",
        "perl-base",
        "pkg-config",
        "pkgconf",
        "pkgconf-bin",
        "tzdata",
    ];
    assert_eq!(names, expected);
    assert!(
        !names.contains(&"clang-14"),
        "an alternative that was not needed"
    );

    // Highest version across suites; exact artifact identity from the index.
    let ssl = closure_entry(&lock, "libssl-dev");
    assert_eq!(ssl.version, "3.0.17-1~deb12u2");
    assert_eq!(ssl.origin, "install");
    assert_eq!(ssl.repository.as_deref(), Some("debian-security"));
    assert_eq!(ssl.suite.as_deref(), Some("bookworm-security"));
    let filename = "pool/main/l/libssl-dev/libssl-dev_3.0.17-1~deb12u2_amd64.deb";
    assert_eq!(ssl.filename.as_deref(), Some(filename));
    assert_eq!(
        ssl.sha256,
        Some(format!("sha256:{}", synthetic_hash(filename)))
    );
    assert_eq!(closure_entry(&lock, "linux-libc-dev").version, "6.1.153-1");
    assert_eq!(
        closure_entry(&lock, "dpkg-dev").arch.as_deref(),
        Some("all")
    );

    // Base packages: kept at their base version, upgraded only when a dependency needs it,
    // recorded without artifact data when no pinned index has that version.
    let dpkg = closure_entry(&lock, "dpkg");
    assert_eq!(
        (dpkg.origin.as_str(), dpkg.version.as_str()),
        ("base", "1.21.22")
    );
    assert!(dpkg.sha256.is_some());
    let libc = closure_entry(&lock, "libc6");
    assert_eq!(
        (libc.origin.as_str(), libc.version.as_str()),
        ("install", "2.36-9+deb12u10")
    );
    let tzdata = closure_entry(&lock, "tzdata");
    assert_eq!(tzdata.origin, "base");
    assert_eq!((tzdata.sha256.clone(), tzdata.arch.clone()), (None, None));
    assert_eq!(closure_entry(&lock, "pkgconf").origin, "install");

    // Tools: exact version, URL and SHA-256 from upstream metadata.
    let python = &lock.packages["python"];
    assert_eq!(python.version, "3.12.14");
    assert_eq!(python.tag.as_deref(), Some("20260901"));
    let asset = "cpython-3.12.14+20260901-x86_64-unknown-linux-gnu-install_only.tar.gz";
    assert_eq!(
        python.artifacts[0].url,
        format!(
            "https://github.com/astral-sh/python-build-standalone/releases/download/20260901/{}",
            asset.replace('+', "%2B")
        )
    );
    assert_eq!(
        python.artifacts[0].sha256,
        format!("sha256:{}", synthetic_hash(asset))
    );
    assert_eq!(python.artifacts[0].strip_components, 1);
    assert_eq!(python.recipe.path, "python.toml");
    let node = &lock.packages["nodejs"];
    assert_eq!(node.version, "22.23.2");
    assert_eq!(
        node.artifacts[0].url,
        "https://nodejs.org/dist/v22.23.2/node-v22.23.2-linux-x64.tar.xz"
    );
    assert_eq!(
        node.artifacts[0].sha256,
        format!(
            "sha256:{}",
            synthetic_hash("node-v22.23.2-linux-x64.tar.xz")
        )
    );
    assert_eq!(node.spec.bin, ["bin/node", "bin/npm", "bin/npx"]);
    assert_eq!(lock.profiles["default"].packages, ["nodejs", "python"]);

    // No realization: only metadata was fetched, nothing was created but lodi.lock, and the
    // manifest's task text was never executed.
    assert!(!requests.iter().any(|u| is_artifact(u)), "{requests:#?}");
    let mut created: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    created.sort();
    assert_eq!(created, ["lodi.lock", "lodi.toml"]);
    assert!(!marker.exists(), "lock time executed task text");

    // The written file is the canonical encoding of the returned lock and reads back equal.
    let bytes = fs::read(dir.join(LOCK_FILE)).unwrap();
    assert_eq!(
        String::from_utf8(bytes.clone()).unwrap(),
        lock.to_canonical_json()
    );
    assert_eq!(parse_lock(&bytes).unwrap(), lock);
}

#[test]
fn the_lock_is_canonical_and_deterministic() {
    let upstream = Upstream::standard();
    let a = project(MANIFEST);
    let b = project(MANIFEST);
    lock_ok(&a, &upstream);
    lock_ok(&b, &upstream);
    let text = fs::read_to_string(a.join(LOCK_FILE)).unwrap();
    assert_eq!(text, fs::read_to_string(b.join(LOCK_FILE)).unwrap());
    assert!(text.ends_with("}\n") && !text.contains('\r') && !text.contains('\t'));
    assert!(
        text.starts_with("{\n  \"base\": {\n"),
        "2-space indent, sorted keys"
    );
    // Re-encoding any parse of the file yields the same bytes: keys are sorted everywhere.
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(lock::canonical_json(&value), text);
    assert!(
        !text.contains(env!("CARGO_TARGET_TMPDIR")),
        "no local paths in the lock"
    );
}

#[test]
fn a_fresh_lock_is_kept_without_any_fetch_and_versions_are_never_substituted() {
    let dir = project(MANIFEST);
    let mut upstream = Upstream::standard();
    lock_ok(&dir, &upstream);
    let before = fs::read(dir.join(LOCK_FILE)).unwrap();

    // Upstream publishes a newer matching Node 22; the fresh lock still wins.
    let mut index: serde_json::Value = serde_json::from_str(&fixture("nodejs-index.json")).unwrap();
    index.as_array_mut().unwrap().insert(
        0,
        serde_json::json!({"version": "v22.99.0", "files": ["linux-x64"], "lts": "Jod"}),
    );
    upstream.set("https://nodejs.org/dist/index.json", index.to_string());
    let fetcher = upstream.fetcher();
    match lock_project(&dir, &fetcher, now()).unwrap() {
        Outcome::UpToDate(lock) => assert_eq!(lock.packages["nodejs"].version, "22.23.2"),
        Outcome::Written { .. } => panic!("a fresh lock was rewritten"),
    }
    assert!(fetcher.requests().is_empty(), "{:?}", fetcher.requests());
    assert_eq!(fs::read(dir.join(LOCK_FILE)).unwrap(), before);
    assert_eq!(frozen(&dir).unwrap().packages["nodejs"].version, "22.23.2");
}

// ---------------------------------------------------------------------------------------------
// Staleness and frozen use

#[test]
fn frozen_refuses_a_missing_lock() {
    let dir = project(MANIFEST);
    let f = frozen(&dir).unwrap_err();
    assert_eq!((f.codes(), f.exit_status), (vec!["E_LOCK_STALE"], 10));
    assert!(f.to_string().contains("lodi.lock does not exist"));
}

#[test]
fn a_changed_manifest_makes_the_lock_stale_and_relocks_only_what_changed() {
    let dir = project(MANIFEST);
    let upstream = Upstream::standard();
    let (first, _, _) = lock_ok(&dir, &upstream);

    // A different Node line: only nodejs is stale; frozen refuses; lock re-resolves nodejs only.
    fs::write(
        dir.join("lodi.toml"),
        MANIFEST.replace("nodejs = \"22\"", "nodejs = \"24\""),
    )
    .unwrap();
    let f = frozen(&dir).unwrap_err();
    assert_eq!((f.codes(), f.exit_status), (vec!["E_LOCK_STALE"], 10));
    let text = f.to_string();
    assert!(
        text.contains("nodejs: locked for `22`, the manifest asks for `24`"),
        "{text}"
    );
    assert!(!text.contains("base:"), "{text}");
    let (second, resolved, requests) = lock_ok(&dir, &upstream);
    assert_eq!(resolved, ["nodejs"]);
    assert!(
        requests
            .iter()
            .all(|u| u.starts_with("https://nodejs.org/")),
        "{requests:?}"
    );
    assert_eq!(second.packages["nodejs"].version, "24.21.0");
    assert_eq!(second.base, first.base);
    assert_eq!(second.packages["python"], first.packages["python"]);
    frozen(&dir).unwrap();

    // A different package list re-resolves the base.
    fs::write(
        dir.join("lodi.toml"),
        with_packages(r#""pkg-config", "libssl-dev", "build-essential", "make""#)
            .replace("nodejs = \"22\"", "nodejs = \"24\""),
    )
    .unwrap();
    let stale = frozen(&dir).unwrap_err().to_string();
    assert!(
        stale.contains("base: requested packages changed"),
        "{stale}"
    );
    let (_, resolved, _) = lock_ok(&dir, &upstream);
    assert_eq!(resolved, ["base"]);

    // A different snapshot, a removed tool and host-tool mode are stale too.
    let lock = parse_lock(&fs::read(dir.join(LOCK_FILE)).unwrap()).unwrap();
    let other_snapshot = lodi::manifest::parse_project_manifest(
        &MANIFEST.replace(SNAPSHOT, "2026-09-18T01:00:00Z"),
        "lodi.toml",
    )
    .unwrap();
    assert!(
        staleness(&other_snapshot, &lock)
            .iter()
            .any(|s| s.contains("snapshot"))
    );
    let host = lodi::manifest::parse_project_manifest("[tools]\npython = \"3.12\"\n", "lodi.toml")
        .unwrap();
    let host_stale = staleness(&host, &lock);
    assert!(
        host_stale.iter().any(|s| s.contains("no [container]")),
        "{host_stale:?}"
    );
    assert!(
        host_stale
            .iter()
            .any(|s| s.contains("nodejs: no longer in the manifest"))
    );
}

#[test]
fn an_invalid_or_tampered_lock_is_refused_and_never_overwritten() {
    let dir = project(MANIFEST);
    let upstream = Upstream::standard();
    lock_ok(&dir, &upstream);
    let good = fs::read_to_string(dir.join(LOCK_FILE)).unwrap();

    let tampered = [
        // A closure version edited by hand: the closure hash no longer matches.
        good.replacen("\"3.0.17-1~deb12u2\"", "\"3.0.16-1~deb12u1\"", 1),
        // A tool hash that is not a SHA-256.
        good.replacen(&synthetic_hash("node-v22.23.2-linux-x64.tar.xz"), "0000", 1),
        // An unknown key (the spike schema is strict).
        good.replacen("{\n  \"base\"", "{\n  \"extra\": 1,\n  \"base\"", 1),
        // Another architecture.
        good.replace("\"arch\": \"x86_64\"", "\"arch\": \"aarch64\""),
        // Not JSON at all.
        "{ truncated".to_string(),
    ];
    for text in tampered {
        assert_ne!(text, good);
        fs::write(dir.join(LOCK_FILE), &text).unwrap();
        let f = frozen(&dir).unwrap_err();
        assert_eq!(
            (f.codes(), f.exit_status),
            (vec!["E_LOCK_STALE"], 10),
            "{f}"
        );
        assert!(f.to_string().contains("lodi.lock is invalid"), "{f}");
        let f = lock_err(&dir, &upstream);
        assert_eq!(f.exit_status, 10);
        assert_eq!(
            fs::read_to_string(dir.join(LOCK_FILE)).unwrap(),
            text,
            "overwritten"
        );
    }

    fs::write(
        dir.join(LOCK_FILE),
        good.replacen("\"version\": 1", "\"version\": 2", 1),
    )
    .unwrap();
    let f = frozen(&dir).unwrap_err();
    assert_eq!((f.codes(), f.exit_status), (vec!["E_LOCK_VERSION"], 4));
    assert!(matches!(
        parse_lock(b"{\"version\": 2}"),
        Err(LockProblem::Version { found: 2, .. })
    ));
}

// ---------------------------------------------------------------------------------------------
// Resolution failures

#[test]
fn a_missing_package_fails_with_suggestions() {
    let dir = project(&with_packages(r#""libssl-devel""#));
    let f = lock_err(&dir, &Upstream::standard());
    assert_eq!((f.codes(), f.exit_status), (vec!["E_NO_MATCH"], 4));
    let text = f.to_string();
    assert!(
        text.contains("package `libssl-devel` is not in the pinned Debian indexes"),
        "{text}"
    );
    assert!(text.contains("did you mean libssl-dev"), "{text}");
    assert!(!dir.join(LOCK_FILE).exists());
}

#[test]
fn an_unavailable_version_fails() {
    let dir = project(&with_packages(r#""spike-needs-future-ssl""#));
    let f = lock_err(&dir, &Upstream::standard());
    assert_eq!((f.codes(), f.exit_status), (vec!["E_NO_MATCH"], 4));
    let text = f.to_string();
    assert!(
        text.contains("no package satisfies `libssl3 (>= 4.0)`"),
        "{text}"
    );
    assert!(
        text.contains("available: libssl3 3.0.17-1~deb12u2, libssl3 3.0.16-1~deb12u1"),
        "{text}"
    );

    for (constraint, code, needle) in [
        (
            "19",
            "E_NO_MATCH",
            "available: 26.9.0, 24.21.0, 24.20.0, 22.23.2, 22.23.1",
        ),
        ("23", "E_UNSUPPORTED_ARCH", "without a x86_64 build"),
        (
            "21",
            "E_NO_MATCH",
            "no nodejs version upstream matches `21`",
        ),
    ] {
        let dir =
            project(&MANIFEST.replace("nodejs = \"22\"", &format!("nodejs = \"{constraint}\"")));
        let f = lock_err(&dir, &Upstream::standard());
        assert_eq!(f.codes(), vec![code], "{constraint}: {f}");
        assert!(f.to_string().contains(needle), "{constraint}: {f}");
    }
    let dir = project(&MANIFEST.replace("python = \"3.12\"", "python = \"3.15\""));
    let f = lock_err(&dir, &Upstream::standard());
    assert_eq!(
        f.codes(),
        vec!["E_NO_MATCH"],
        "prereleases are not matched: {f}"
    );
}

#[test]
fn unsupported_architectures_fail() {
    let mut upstream = Upstream::standard();
    upstream.set(&format!("{}rootfs.dpkg-arch", raw_base()), "arm64\n");
    let f = lock_err(&project(MANIFEST), &upstream);
    assert_eq!((f.codes(), f.exit_status), (vec!["E_UNSUPPORTED_ARCH"], 4));

    let mut upstream = Upstream::standard();
    upstream.set_index(
        "debian",
        "bookworm-updates",
        &fixture("bookworm-updates.Packages"),
        "all arm64",
    );
    let f = lock_err(&project(MANIFEST), &upstream);
    assert_eq!(f.codes(), vec!["E_UNSUPPORTED_ARCH"]);
    assert!(
        f.to_string().contains("does not list architecture amd64"),
        "{f}"
    );

    let f = lock_err(
        &project(&with_packages(r#""spike-needs-i386""#)),
        &Upstream::standard(),
    );
    assert_eq!(f.codes(), vec!["E_UNSUPPORTED_ARCH"]);
    assert!(f.to_string().contains("libc6:i386"), "{f}");

    // The manifest subset itself refuses another architecture (exit 3).
    let f = lock_err(
        &project(&MANIFEST.replace(
            "release  = \"bookworm\"",
            "release  = \"bookworm\"\narch = \"aarch64\"",
        )),
        &Upstream::standard(),
    );
    assert_eq!((f.codes(), f.exit_status), (vec!["E_UNSUPPORTED"], 3));
}

#[test]
fn a_tampered_index_fails_and_preserves_the_previous_lock() {
    let dir = project(MANIFEST);
    lock_ok(&dir, &Upstream::standard());
    let before = fs::read(dir.join(LOCK_FILE)).unwrap();
    fs::write(dir.join("lodi.toml"), with_packages(r#""make""#)).unwrap();

    // Packages.xz bytes that do not match the Release file.
    let mut upstream = Upstream::standard();
    let url = format!(
        "{}dists/bookworm-security/main/binary-amd64/Packages.xz",
        snapshot_url("debian-security")
    );
    let evil =
        fixture("bookworm-security.Packages").replace("3.0.17-1~deb12u2", "3.0.17-1~deb12u9");
    upstream.set(&url, xz(evil.as_bytes()));
    let f = lock_err(&dir, &upstream);
    assert_eq!((f.codes(), f.exit_status), (vec!["E_HASH_MISMATCH"], 5));
    assert!(
        f.to_string().contains("does not match its Release file"),
        "{f}"
    );
    assert_eq!(fs::read(dir.join(LOCK_FILE)).unwrap(), before);

    // An image manifest that does not match the digest in the OCI index.
    let mut upstream = Upstream::standard();
    let manifest_url = format!("{}oci/blobs/image-manifest.json", raw_base());
    let mut manifest = upstream.get(&manifest_url);
    manifest.push(b' ');
    upstream.set(&manifest_url, manifest);
    assert_eq!(lock_err(&dir, &upstream).codes(), vec!["E_HASH_MISMATCH"]);

    // An upstream checksum file that disagrees with the release's own asset digest.
    let mut upstream = Upstream::standard();
    let sums = "https://github.com/astral-sh/python-build-standalone/releases/download/20260901/SHA256SUMS";
    let asset = "cpython-3.12.14+20260901-x86_64-unknown-linux-gnu-install_only.tar.gz";
    let text = String::from_utf8(upstream.get(sums)).unwrap();
    upstream.set(sums, text.replace(&synthetic_hash(asset), &"e".repeat(64)));
    fs::write(
        dir.join("lodi.toml"),
        MANIFEST.replace("python = \"3.12\"", "python = \"3.12.14\""),
    )
    .unwrap();
    assert_eq!(lock_err(&dir, &upstream).codes(), vec!["E_HASH_MISMATCH"]);

    // A checksum file without the asset.
    let mut upstream = Upstream::standard();
    upstream.set(sums, text.replace(asset, "something-else.tar.gz"));
    assert_eq!(lock_err(&dir, &upstream).codes(), vec!["E_NO_CHECKSUM"]);

    // A checksum file that names the asset only under another directory: an entry is matched
    // on the exact name it lists, never on its basename (M-1.0 u-4, LD-363).
    let mut upstream = Upstream::standard();
    upstream.set(sums, text.replace(asset, &format!("elsewhere/{asset}")));
    assert_eq!(lock_err(&dir, &upstream).codes(), vec!["E_NO_CHECKSUM"]);
    assert_eq!(fs::read(dir.join(LOCK_FILE)).unwrap(), before);
}

/// The OCI-layout base's package list is held to the git blob id its pinned commit's tree
/// lists for it, as the dated bases hold theirs to a checksum file (M-1.0 u-4, LD-363).
#[test]
fn the_debian_package_list_must_match_its_commits_tree() {
    let dir = project(MANIFEST);
    lock_ok(&dir, &Upstream::standard());
    let before = fs::read(dir.join(LOCK_FILE)).unwrap();
    fs::write(dir.join("lodi.toml"), with_packages(r#""make""#)).unwrap();
    let list_url = format!("{}rootfs.manifest", raw_base());

    // A package list that differs from the one the commit's tree names.
    let mut upstream = Upstream::standard();
    let tampered = format!("{}spike-planted\t9.9\n", fixture("rootfs.manifest"));
    upstream.set(&list_url, tampered);
    let f = lock_err(&dir, &upstream);
    assert_eq!((f.codes(), f.exit_status), (vec!["E_HASH_MISMATCH"], 5));
    assert!(f.to_string().contains(&list_url), "{f}");
    assert_eq!(fs::read(dir.join(LOCK_FILE)).unwrap(), before);

    // A tree that names no package list at all.
    let mut upstream = Upstream::standard();
    upstream.set(&tree_url(), "[]");
    let f = lock_err(&dir, &upstream);
    assert_eq!((f.codes(), f.exit_status), (vec!["E_REPO_UNREACHABLE"], 4));
    assert_eq!(fs::read(dir.join(LOCK_FILE)).unwrap(), before);

    // The untampered list still locks, and the lock records its SHA-256 as before.
    lock_ok(&dir, &Upstream::standard());
}

#[test]
fn unreachable_repositories_and_snapshot_bounds() {
    let mut upstream = Upstream::standard();
    upstream.files.remove(&format!(
        "{}dists/bookworm-updates/Release",
        snapshot_url("debian")
    ));
    let f = lock_err(&project(MANIFEST), &upstream);
    assert_eq!((f.codes(), f.exit_status), (vec!["E_REPO_UNREACHABLE"], 4));

    // Older than the base build, older than the release, in the future.
    for (snapshot, code) in [
        ("2026-09-16T23:00:00Z", "E_SNAPSHOT_TOO_OLD"),
        ("2020-01-01T00:00:00Z", "E_SNAPSHOT_TOO_OLD"),
        ("2026-09-20T00:00:00Z", "E_SNAPSHOT_FUTURE"),
    ] {
        let f = lock_err(
            &project(&MANIFEST.replace(SNAPSHOT, snapshot)),
            &Upstream::standard(),
        );
        assert_eq!(f.codes(), vec![code], "{snapshot}: {f}");
    }
}

#[test]
fn an_absent_snapshot_pins_the_current_hour() {
    let manifest = MANIFEST.replace("snapshot = \"2026-09-18T00:00:00Z\"\n", "");
    let dir = project(&manifest);
    let mut upstream = Upstream::standard();
    for (archive, suite) in [
        ("debian", "bookworm"),
        ("debian", "bookworm-updates"),
        ("debian-security", "bookworm-security"),
    ] {
        let from = format!("{}dists/{suite}/", snapshot_url(archive));
        let to = from.replace(SNAPSHOT_ID, "20260919T030000Z");
        for name in ["Release", "main/binary-amd64/Packages.xz"] {
            let body = upstream.get(&format!("{from}{name}"));
            upstream.set(&format!("{to}{name}"), body);
        }
    }
    let (lock, _, _) = lock_ok(&dir, &upstream);
    assert_eq!(lock.base.as_ref().unwrap().snapshot, "2026-09-19T03:00:00Z");
    // A manifest without a snapshot accepts whatever the lock pinned.
    frozen(&dir).unwrap();
}

#[test]
fn host_tool_mode_locks_only_tools_and_a_second_node_version() {
    let upstream = Upstream::standard();
    let a = project("[tools]\nnodejs = \"22\"\n");
    let b = project("[tools]\nnodejs = \"24\"\npython = \"3.12\"\n");
    let (lock_a, _, requests) = lock_ok(&a, &upstream);
    let (lock_b, _, _) = lock_ok(&b, &upstream);
    assert!(lock_a.base.is_none());
    assert!(
        requests
            .iter()
            .all(|u| u.starts_with("https://nodejs.org/")),
        "{requests:?}"
    );
    assert_eq!(lock_a.packages["nodejs"].version, "22.23.2");
    assert_eq!(lock_b.packages["nodejs"].version, "24.21.0");
    assert_ne!(
        lock_a.packages["nodejs"].artifacts,
        lock_b.packages["nodejs"].artifacts
    );
    assert_eq!(lock_b.packages["python"].version, "3.12.14");
}

// ---------------------------------------------------------------------------------------------
// Atomic replacement

#[test]
fn a_failed_write_preserves_the_previous_lock() {
    let dir = project(MANIFEST);
    let upstream = Upstream::standard();
    lock_ok(&dir, &upstream);
    let path = dir.join(LOCK_FILE);
    let before = fs::read(&path).unwrap();

    // A fault after the new content is durable but before the rename.
    let e = write_atomic_with(&path, b"{\"new\": true}\n", |tmp| {
        assert!(tmp.exists());
        Err(std::io::Error::other("injected fault"))
    })
    .unwrap_err();
    assert_eq!(e.to_string(), "injected fault");
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(
        !lock::temporary_path(&path).exists(),
        "temporary file left behind"
    );

    // A real failure inside `lodi lock`: the temporary path is occupied by a directory.
    fs::write(
        dir.join("lodi.toml"),
        MANIFEST.replace("nodejs = \"22\"", "nodejs = \"24\""),
    )
    .unwrap();
    let blocker = lock::temporary_path(&path);
    fs::create_dir(&blocker).unwrap();
    let f = lock_err(&dir, &upstream);
    assert_eq!((f.codes(), f.exit_status), (vec!["E_STORE_IO"], 6));
    assert!(
        f.to_string().contains("the previous lock is unchanged"),
        "{f}"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(
        blocker.is_dir(),
        "the failure path must not delete what it did not create"
    );
    fs::remove_dir(&blocker).unwrap();

    // And the successful write replaces the file whole.
    let (lock, _, _) = lock_ok(&dir, &upstream);
    assert_eq!(fs::read_to_string(&path).unwrap(), lock.to_canonical_json());
}

// ---------------------------------------------------------------------------------------------
// The resolver on its own

fn index(text: &str) -> Vec<lodi::debian::index::BinaryPackage> {
    let mut t = String::new();
    for (i, stanza) in text.trim().split("\n\n").enumerate() {
        t.push_str(stanza.trim());
        t.push_str(&format!(
            "\nArchitecture: amd64\nFilename: pool/f{i}.deb\nSize: 1\nSHA256: {}\n\n",
            "a".repeat(64)
        ));
    }
    parse_packages(&t, "amd64", "debian", "bookworm").unwrap()
}

fn names(closure: &[lodi::debian::resolve::ClosurePackage]) -> Vec<String> {
    closure
        .iter()
        .map(|p| format!("{}={}", p.name, p.version))
        .collect()
}

#[test]
fn resolver_semantics() {
    let idx = index(
        "Package: app\nVersion: 1\nDepends: liba (>= 2) | libb, virt\n\n\
         Package: liba\nVersion: 1\n\n\
         Package: liba\nVersion: 3\n\n\
         Package: libb\nVersion: 1\n\n\
         Package: impl\nVersion: 1\nProvides: virt\n\n\
         Package: two1\nVersion: 1\nProvides: many\n\n\
         Package: two2\nVersion: 1\nProvides: many\n\n\
         Package: needs-many\nVersion: 1\nDepends: many\n\n\
         Package: breaker\nVersion: 1\nBreaks: liba (<< 4)\n\n\
         Package: loop1\nVersion: 1\nDepends: loop2\n\n\
         Package: loop2\nVersion: 1\nDepends: loop1",
    );
    // First satisfiable alternative (the highest liba), a single-provider virtual name.
    let c = resolve(&idx, &[], &["app".into()]).unwrap();
    assert_eq!(names(&c), ["app=1", "impl=1", "liba=3"]);
    assert!(c.iter().all(|p| p.origin == Origin::Install));

    // A base package that already satisfies a dependency is kept at its version.
    let base = [("liba".to_string(), DebVersion::parse("3").unwrap())];
    let c = resolve(&idx, &base, &["app".into()]).unwrap();
    assert_eq!(
        c.iter().find(|p| p.name == "liba").unwrap().origin,
        Origin::Base
    );

    // Dependency cycles terminate.
    assert_eq!(
        names(&resolve(&idx, &[], &["loop1".into()]).unwrap()),
        ["loop1=1", "loop2=1"]
    );

    // Several providers and nothing chosen: refused with the providers named.
    match resolve(&idx, &[], &["needs-many".into()]) {
        Err(ResolveError::AmbiguousVirtual { providers, .. }) => {
            assert_eq!(providers, ["two1", "two2"])
        }
        other => panic!("{other:?}"),
    }
    // Choosing one explicitly resolves it.
    let c = resolve(&idx, &[], &["needs-many".into(), "two2".into()]).unwrap();
    assert_eq!(names(&c), ["needs-many=1", "two2=1"]);

    // Breaks between members of the result are an error, not solved around.
    assert!(matches!(
        resolve(&idx, &[], &["app".into(), "breaker".into()]),
        Err(ResolveError::Conflict { .. })
    ));
    // Conflicts, through the synthetic Debian fixture as well.
    let dir = project(&with_packages(r#""make", "spike-conflicts-make""#));
    let f = lock_err(&dir, &Upstream::standard());
    assert_eq!(f.codes(), vec!["E_NO_MATCH"]);
    assert!(f.to_string().contains("conflicts with make"), "{f}");
}

/// The Debian resolver is bounded like the Arch resolver: each cap, once passed, is an error
/// naming it rather than unbounded work (M-1.0 u-4, LD-363).
#[test]
fn the_debian_resolver_is_bounded() {
    use lodi::debian::resolve::{LIMITS, Limits, resolve_with_limits};
    let limit = |what: &'static str, limit: usize| Err(ResolveError::Limit { what, limit });
    let small = |max_requests, max_closure_size, max_iterations| Limits {
        max_requests,
        max_closure_size,
        max_iterations,
    };

    // Two packages whose versions each need the other's other version: every dependency group
    // replaces a version the previous one chose, so the queue never drains on its own.
    let flip = index(
        "Package: pb\nVersion: 2\nDepends: pc (>= 2)\n\n\
         Package: pb\nVersion: 1\nDepends: pc (<< 2)\n\n\
         Package: pc\nVersion: 2\nDepends: pb (<< 2)\n\n\
         Package: pc\nVersion: 1\nDepends: pb (>= 2)",
    );
    // With the production bounds, in bounded time: the cap ends it, not the test's patience.
    let (tx, rx) = std::sync::mpsc::channel();
    let production = flip.clone();
    std::thread::spawn(move || {
        let _ = tx.send(resolve(&production, &[], &["pb".into()]));
    });
    let ended = rx
        .recv_timeout(std::time::Duration::from_secs(120))
        .expect("the resolution ended");
    assert_eq!(ended, limit("iteration count", LIMITS.max_iterations));
    assert_eq!(
        resolve_with_limits(&flip, &[], &["pb".into()], small(10, 10, 50)),
        limit("iteration count", 50)
    );

    // The request count and the closure size, base packages included.
    let idx = index(
        "Package: pa\nVersion: 1\nDepends: pb\n\n\
         Package: pb\nVersion: 1\nDepends: pc\n\n\
         Package: pc\nVersion: 1",
    );
    let requested: Vec<String> = vec!["pa".into(), "pb".into()];
    assert_eq!(
        resolve_with_limits(&idx, &[], &requested, small(1, 10, 100)),
        limit("request count", 1)
    );
    assert_eq!(
        resolve_with_limits(&idx, &[], &["pa".into()], small(10, 2, 100)),
        limit("closure size", 2)
    );
    let base: Vec<(String, DebVersion)> = ["bx", "by", "bz"]
        .iter()
        .map(|n| (n.to_string(), DebVersion::parse("1").unwrap()))
        .collect();
    assert_eq!(
        resolve_with_limits(&idx, &base, &["pc".into()], small(10, 2, 100)),
        limit("closure size", 2)
    );
    // Within the bounds the same resolution succeeds.
    assert_eq!(
        names(&resolve_with_limits(&idx, &[], &["pa".into()], small(1, 3, 3)).unwrap()),
        ["pa=1", "pb=1", "pc=1"]
    );
    // And the lock names the cap as E_NO_MATCH, the way the Arch resolver's are named.
    let d = lodi::debian::base::resolve_diagnostic(
        ResolveError::Limit {
            what: "iteration count",
            limit: 50,
        },
        "debian",
    );
    assert_eq!(d.code, "E_NO_MATCH");
    assert_eq!(
        d.message,
        "Debian resolution exceeds the iteration count limit of 50"
    );
}

// ---------------------------------------------------------------------------------------------
// The `lodi` binary against a loopback mirror of the fixtures

struct Mirror {
    port: u16,
    log: Arc<Mutex<Vec<String>>>,
}

impl Mirror {
    /// Serve `upstream` at `http://127.0.0.1:<port>/<host>/<path>`.
    fn start(upstream: &Upstream) -> Mirror {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let files = upstream.files.clone();
        let thread_log = Arc::clone(&log);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                        break;
                    }
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("/");
                let url = format!("https://{}", &path[1..]);
                thread_log.lock().unwrap().push(url.clone());
                let response = match files.get(&url) {
                    Some(body) => {
                        let mut r = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(body);
                        r
                    }
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = stream.write_all(&response);
            }
        });
        Mirror { port, log }
    }

    fn rewrite(&self) -> String {
        [
            "snapshot.debian.org",
            "api.github.com",
            "raw.githubusercontent.com",
            "github.com",
            "nodejs.org",
        ]
        .iter()
        .map(|host| format!("https://{host}/=http://127.0.0.1:{}/{host}/", self.port))
        .collect::<Vec<_>>()
        .join(";")
    }

    fn take_log(&self) -> Vec<String> {
        std::mem::take(&mut *self.log.lock().unwrap())
    }
}

fn lodi_in(dir: &Path, args: &[&str], rewrite: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(args)
        .current_dir(dir)
        .env("LODI_FETCH_REWRITE", rewrite)
        .output()
        .expect("the lodi binary runs")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn the_binary_locks_checks_and_detects_staleness() {
    let upstream = Upstream::standard();
    let mirror = Mirror::start(&upstream);
    let rewrite = mirror.rewrite();
    let fixture_manifest = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/spike/lodi.toml"
    ))
    .unwrap();
    let dir = project(&fixture_manifest);

    let check = lodi_in(&dir, &["lock", "--check"], &rewrite);
    assert_eq!(check.status.code(), Some(10), "{}", text(&check.stderr));
    assert!(text(&check.stderr).contains("lodi: error E_LOCK_STALE: lodi.lock does not exist"));

    let out = lodi_in(&dir, &["lock"], &rewrite);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(
        stdout.starts_with("wrote lodi.lock (resolved base, nodejs, python)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("debian bookworm snapshot 2026-09-18T00:00:00Z"),
        "{stdout}"
    );
    let requests = mirror.take_log();
    assert!(
        !requests.is_empty() && !requests.iter().any(|u| is_artifact(u)),
        "{requests:#?}"
    );
    let lock = parse_lock(&fs::read(dir.join(LOCK_FILE)).unwrap()).unwrap();
    assert_eq!(lock.packages["python"].version, "3.12.14");
    // The lock records the canonical upstream URLs, not the mirror.
    let written = fs::read_to_string(dir.join(LOCK_FILE)).unwrap();
    assert!(!written.contains("127.0.0.1") && written.contains("https://snapshot.debian.org/"));

    let again = lodi_in(&dir, &["lock"], &rewrite);
    assert_eq!(again.status.code(), Some(0));
    assert!(text(&again.stdout).starts_with("lodi.lock is up to date"));
    assert!(
        mirror.take_log().is_empty(),
        "a fresh lock fetched something"
    );
    let check = lodi_in(&dir, &["lock", "--check"], "");
    assert_eq!(check.status.code(), Some(0), "{}", text(&check.stderr));

    fs::write(
        dir.join("lodi.toml"),
        fixture_manifest.replace("nodejs = \"22\"", "nodejs = \"24\""),
    )
    .unwrap();
    let check = lodi_in(&dir, &["lock", "--check"], "");
    assert_eq!(check.status.code(), Some(10));
    let stderr = text(&check.stderr);
    assert!(
        stderr.contains("E_LOCK_STALE") && stderr.contains("nodejs: locked for `22`"),
        "{stderr}"
    );
    assert_eq!(
        fs::read_to_string(dir.join(LOCK_FILE)).unwrap(),
        written,
        "--check wrote"
    );

    // Manifest errors exit 3; insecure rewrites are refused before any request.
    fs::write(dir.join("lodi.toml"), "[project]\nnmae = \"x\"\n").unwrap();
    let bad = lodi_in(&dir, &["lock"], &rewrite);
    assert_eq!(bad.status.code(), Some(3));
    assert!(text(&bad.stderr).contains("E_UNKNOWN_ATTR"));
    let insecure = lodi_in(&dir, &["lock"], "https://nodejs.org/=http://example.com/");
    assert_eq!(insecure.status.code(), Some(3));
    assert!(text(&insecure.stderr).contains("E_CONFIG: LODI_FETCH_REWRITE"));
}

/// LD-386: `LODI_FETCH_ATTEMPTS` is how many attempts a fetch whose failure passes gets. At 1 a
/// 503 is asked for once and reported as it came; a value that is not a whole number from 1 to
/// 10 is refused before any request.
#[test]
fn the_fetch_attempts_setting_asks_once_at_one_and_refuses_anything_but_one_to_ten() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let asked = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&asked);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            counter.fetch_add(1, Ordering::SeqCst);
            {
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                    line.clear();
                }
            }
            let _ = stream.write_all(
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    let dir = project(MANIFEST);
    let rewrite = format!("https://=http://127.0.0.1:{port}/");
    let lock = |attempts: &str| {
        Command::new(env!("CARGO_BIN_EXE_lodi"))
            .arg("lock")
            .current_dir(&dir)
            .env("LODI_FETCH_REWRITE", &rewrite)
            .env("LODI_FETCH_ATTEMPTS", attempts)
            .output()
            .expect("the lodi binary runs")
    };

    let once = lock("1");
    let stderr = text(&once.stderr);
    assert_eq!(once.status.code(), Some(4), "{stderr}");
    assert!(
        stderr.contains("E_REPO_UNREACHABLE") && stderr.contains("HTTP status 503"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("retrying") && !stderr.contains("gave up"),
        "{stderr}"
    );
    assert_eq!(asked.load(Ordering::SeqCst), 1, "one attempt, one request");

    for bad in ["0", "11", "", "two", "+2", " 2", "-1"] {
        let refused = lock(bad);
        let stderr = text(&refused.stderr);
        assert_eq!(refused.status.code(), Some(3), "{bad:?}: {stderr}");
        assert!(
            stderr.contains("E_CONFIG: LODI_FETCH_ATTEMPTS"),
            "{bad:?}: {stderr}"
        );
    }
    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "a refused setting made a request"
    );
}
