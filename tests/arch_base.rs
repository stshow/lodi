//! M-Arch-Base T-4: offline pinning of the Arch bootstrap and repository day.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

use lodi::arch::base::{verify_index, verify_rootfs};
use lodi::catalogue::{builtin_base, parse_base};
use lodi::debian::base::{BaseRequest, lock_base};
use lodi::diag::exit_status;
use lodi::fetch::{FetchError, Fetcher};
use lodi::lock::{LOCK_FORMAT, LOCK_VERSION, LockFile, Profile, base_entry, validate};
use lodi::util::{parse_utc, sha256_hex, sha256_tagged};
use support::{file, gzip, tar};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/arch");
const ISO: &str = "https://archive.archlinux.org/iso/";
const SNAPSHOT: &str = "2026-09-18T00:00:00Z";
const PINNED: &str = "2026.09.01";
const LAYER: &str = "archlinux-bootstrap-2026.09.01-x86_64.tar.zst";

fn fixture(name: &str) -> Vec<u8> {
    fs::read(Path::new(FIXTURE).join(name)).unwrap()
}

fn database(name: &str, desc: &str) -> Vec<u8> {
    let text = String::from_utf8(fixture(desc)).unwrap();
    let version = if name == "filesystem" {
        "2026.09.01-1"
    } else {
        "1.8.1-1"
    };
    gzip(&tar(&[file(
        &format!("{name}-{version}/desc"),
        &text,
        0o644,
    )]))
}

struct Upstream {
    files: BTreeMap<String, Vec<u8>>,
    requests: Mutex<Vec<String>>,
}

impl Upstream {
    fn valid() -> Self {
        let rootfs = fixture("bootstrap-rootfs.synthetic");
        let mut files = BTreeMap::new();
        files.insert(ISO.to_string(), fixture("iso-listing.html"));
        for date in ["2024.05.01", PINNED, "2026.10.01"] {
            let layer = format!("archlinux-bootstrap-{date}-x86_64.tar.zst");
            files.insert(
                format!("{ISO}{date}/sha256sums.txt"),
                format!("{}  {layer}\n", sha256_hex(&rootfs)).into_bytes(),
            );
            files.insert(format!("{ISO}{date}/{layer}"), rootfs.clone());
        }
        files.insert(
            "https://archive.archlinux.org/repos/2026/09/18/core/os/x86_64/core.db".into(),
            database("filesystem", "core.desc"),
        );
        files.insert(
            "https://archive.archlinux.org/repos/2026/09/18/extra/os/x86_64/extra.db".into(),
            database("jq", "extra.desc"),
        );
        Self {
            files,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn request_log(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Fetcher for Upstream {
    fn get(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        self.requests.lock().unwrap().push(url.to_string());
        self.files
            .get(url)
            .cloned()
            .ok_or_else(|| FetchError::Status(url.to_string(), 404))
    }

    fn requests(&self) -> Vec<String> {
        self.request_log()
    }
}

/// One request for `jq`, which only `extra` carries.
static REQUESTED: &[String] = &[];

fn request<'a>(snapshot: &str, now: &str, requested: &'a [String]) -> BaseRequest<'a> {
    BaseRequest {
        release: "rolling",
        arch: "x86_64",
        snapshot: parse_utc(snapshot).unwrap(),
        now: parse_utc(now).unwrap(),
        requested,
    }
}

#[test]
fn arch_base_pins_rootfs_and_both_databases_without_artifact_downloads() {
    let definition = builtin_base("arch").unwrap().unwrap();
    let upstream = Upstream::valid();
    let wanted = vec!["jq".to_string()];
    let locked = lock_base(
        &upstream,
        &definition,
        &request(SNAPSHOT, "2026-09-21T00:00:00Z", &wanted),
    )
    .unwrap();

    assert_eq!(
        (locked.distro.as_str(), locked.release.as_str()),
        ("arch", "rolling")
    );
    assert_eq!(
        (locked.arch.as_str(), locked.deb_arch.as_str()),
        ("x86_64", "x86_64")
    );
    assert_eq!(locked.snapshot, SNAPSHOT);
    assert_eq!(locked.rootfs.url, format!("{ISO}{PINNED}/{LAYER}"));
    assert_eq!(
        locked.rootfs.sha256,
        sha256_hex(&fixture("bootstrap-rootfs.synthetic"))
    );
    assert_eq!(locked.rootfs.format, "tar.zst");
    assert_eq!(locked.rootfs.subdir.as_deref(), Some("root.x86_64"));
    assert!(locked.rootfs.package_list_url.is_none());
    let checksum = match &locked.rootfs.pin {
        lodi::debian::base::RootfsPin::ChecksumFile { url, sha256 } => (url, sha256),
        other => panic!("expected checksum pin, got {other:?}"),
    };
    assert_eq!(checksum.0, &format!("{ISO}{PINNED}/sha256sums.txt"));
    assert_eq!(locked.repositories.len(), 2);
    for repository in &locked.repositories {
        // One `<repo>.db` is both the index and the statement of its own digest: unlike an apt
        // repository there is no separate signed `Release` file to name it (LD-57).
        assert_eq!(repository.indexes.len(), 1);
        let index = &repository.indexes[0];
        assert_eq!(index.release_sha256, index.packages_sha256);
        assert_eq!(index.release_path, format!("{}.db", repository.name));
        assert_eq!(
            index.packages_size as usize,
            upstream.files[&format!("{}{}.db", repository.url, repository.name)].len()
        );
        assert!(repository.suites.is_empty() && repository.components.is_empty());
    }
    // The request resolved through the pacman resolver: `jq` is in `extra` and nothing else is
    // pulled in, and the closure carries the artifact data the image build needs.
    assert_eq!(locked.requested, vec!["jq".to_string()]);
    assert_eq!(locked.closure.len(), 1);
    let jq = &locked.closure[0];
    assert_eq!((jq.name.as_str(), jq.version.as_str()), ("jq", "1.8.1-1"));
    let artifact = jq
        .artifact
        .as_ref()
        .expect("a locked package names its file");
    assert_eq!(artifact.filename, "jq-1.8.1-1-x86_64.pkg.tar.zst");
    assert_eq!(artifact.repository, "extra");
    let requests = upstream.request_log();
    assert_eq!(
        requests.len(),
        4,
        "listing, checksums, core.db and extra.db only"
    );
    assert!(
        !requests
            .iter()
            .any(|url| url.ends_with(".tar.zst") || url.ends_with(".sig"))
    );

    let lock = LockFile {
        version: LOCK_VERSION,
        format: LOCK_FORMAT.into(),
        generated_by: "lodi test".into(),
        manifest_hash: sha256_tagged(b"synthetic Arch manifest"),
        base: Some(base_entry(locked)),
        packages: BTreeMap::new(),
        profiles: BTreeMap::from([(
            "default".into(),
            Profile {
                packages: Vec::new(),
            },
        )]),
    };
    validate(&lock).unwrap();
    let json = lock.to_canonical_json();
    assert!(json.contains("\"pinned\": true"));
    assert!(json.contains("\"subdir\": \"root.x86_64\""));
    assert!(!json.contains("packageList"));
}

#[test]
fn snapshot_bounds_fail_before_any_network_request() {
    let definition = builtin_base("arch").unwrap().unwrap();
    for (snapshot, now, expected) in [
        (
            "2024-04-30T23:59:59Z",
            "2026-09-21T00:00:00Z",
            "E_SNAPSHOT_TOO_OLD",
        ),
        (
            "2026-09-22T00:00:00Z",
            "2026-09-21T00:00:00Z",
            "E_SNAPSHOT_FUTURE",
        ),
    ] {
        let upstream = Upstream::valid();
        let error =
            lock_base(&upstream, &definition, &request(snapshot, now, REQUESTED)).unwrap_err();
        assert_eq!(error.code, expected);
        assert_eq!(exit_status(error.code), 4);
        assert!(upstream.request_log().is_empty());
    }
}

#[test]
fn tampered_database_and_rootfs_each_fail_closed() {
    let definition = builtin_base("arch").unwrap().unwrap();
    let upstream = Upstream::valid();
    let locked = lock_base(
        &upstream,
        &definition,
        &request(SNAPSHOT, "2026-09-21T00:00:00Z", REQUESTED),
    )
    .unwrap();

    let mut tampered = Upstream::valid();
    tampered.files.get_mut(&locked.rootfs.url).unwrap()[0] ^= 1;
    let error = verify_rootfs(&tampered, &locked.rootfs).unwrap_err();
    assert_eq!(error.code, "E_HASH_MISMATCH");
    assert_eq!(exit_status(error.code), 5);

    let core = &locked.repositories[0];
    let url = format!("{}core.db", core.url);
    tampered.files.get_mut(&url).unwrap()[0] ^= 1;
    let error =
        verify_index(&tampered, "core", &url, &core.indexes[0].packages_sha256).unwrap_err();
    assert_eq!(error.code, "E_HASH_MISMATCH");
    assert_eq!(exit_status(error.code), 5);
}

#[test]
fn checksum_without_the_tarball_is_repository_unreachable() {
    let definition = builtin_base("arch").unwrap().unwrap();
    let mut upstream = Upstream::valid();
    upstream.files.insert(
        format!("{ISO}{PINNED}/sha256sums.txt"),
        b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  another-file\n"
            .to_vec(),
    );
    let error = lock_base(
        &upstream,
        &definition,
        &request(SNAPSHOT, "2026-09-21T00:00:00Z", REQUESTED),
    )
    .unwrap_err();
    assert_eq!(error.code, "E_REPO_UNREACHABLE");
    assert_eq!(exit_status(error.code), 4);
}

#[test]
fn apt_empty_suites_and_unknown_template_variables_stay_invalid() {
    let apt = r#"
[base]
distro = "broken"
family = "apt"
[release.one]
aliases = []
snapshot_min = "2024-01-01T00:00:00Z"
repositories = ["main"]
[release.one.rootfs.x86_64]
strategy = "dated_checksum_dir"
index_url = "https://example.invalid/"
checksums = "SHA256SUMS"
layer = "root.tar.gz"
[repository.main]
snapshot_url = "https://example.invalid/${snapshot_id}/"
suites = []
components = ["main"]
"#;
    assert_eq!(
        parse_base(apt, "broken.toml").unwrap_err().code,
        "E_RECIPE_INVALID"
    );

    let unknown = apt
        .replace("family = \"apt\"", "family = \"pacman\"")
        .replace("${snapshot_id}", "${unknown}")
        .replace("components = [\"main\"]", "components = []");
    assert_eq!(
        parse_base(&unknown, "broken.toml").unwrap_err().code,
        "E_RECIPE_INVALID"
    );
}
