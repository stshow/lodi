//! fd-1 (LD-435): locking and `lodi develop` on the Fedora 44 base, offline.
//!
//! The Fedora world is `tests/fixtures/fedora/` (see its `PROVENANCE`): the release's real
//! container `CHECKSUM` file, and the release's real `Everything` metadata trimmed to the builds
//! these tests resolve — every `<package>` element verbatim, with `repomd.xml` stating the
//! trimmed file's digests. The binary reaches it over a loopback server through
//! `LODI_FETCH_REWRITE`; the image half runs against a stand-in for `podman` that records what
//! it was asked, so that "before the session" is observable. The real image build and session
//! are the guest row `fedora-develop-uses-locked-rpm-closure`.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;

use lodi::catalogue::builtin_base;
use lodi::debian::base::{BaseRequest, lock_base};
use lodi::diag::exit_status;
use lodi::fetch::{FetchError, Fetcher};
use lodi::lock::base_entry;
use lodi::util::{parse_utc, sha256_hex, sha256_tagged};
use serde_json::Value;
use support::Server;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fedora");
const IMAGES: &str =
    "https://dl.fedoraproject.org/pub/fedora/linux/releases/44/Container/x86_64/images/";
const ARCHIVE: &str = "Fedora-Container-Base-Generic-44-1.7.x86_64.oci.tar.xz";
const CHECKSUM: &str = "Fedora-Container-44-1.7-x86_64-CHECKSUM";
const REPO: &str =
    "https://dl.fedoraproject.org/pub/fedora/linux/releases/44/Everything/x86_64/os/";
const PRIMARY: &str =
    "bb09509d54a5d1e07ad37d1abe8869bb69ab363310273e5e394bcab0b9ab7eab-primary.xml.zst";

/// What dnf5 5.4.1 installs for `dnf install --setopt=install_weak_deps=False --repo=fedora
/// ntpstat nss-mdns` in the Fedora 44 container image (measured 2026-09-28, the record): the
/// `(ntpsec or chrony)` of ntpstat takes chrony, chrony pulls systemd, and systemd switches on
/// nss-mdns's `(avahi if systemd)` and dbus's `(dbus-broker >= 16-4 if systemd)`.
const DNF_NTPSTAT_NSS_MDNS: &[&str] = &[
    "avahi-0:0.9~rc2-8.fc44.x86_64",
    "avahi-libs-0:0.9~rc2-8.fc44.x86_64",
    "chrony-0:4.8-5.fc44.x86_64",
    "dbus-1:1.16.2-1.fc44.x86_64",
    "dbus-broker-0:37-8.fc44.x86_64",
    "dbus-common-1:1.16.2-1.fc44.noarch",
    "dbus-libs-1:1.16.2-1.fc44.x86_64",
    "expat-0:2.7.3-2.fc44.x86_64",
    "libdaemon-0:0.14-33.fc44.x86_64",
    "libedit-0:3.1-58.20251016cvs.fc44.x86_64",
    "libfdisk-0:2.41.3-12.fc44.x86_64",
    "libseccomp-0:2.6.0-3.fc44.x86_64",
    "nss-mdns-0:0.15.1-28.fc44.x86_64",
    "ntpstat-0:0.6-15.fc44.noarch",
    "systemd-0:259.5-1.fc44.x86_64",
    "systemd-shared-0:259.5-1.fc44.x86_64",
];

fn fixture(name: &str) -> Vec<u8> {
    fs::read(Path::new(FIXTURE).join(name)).unwrap()
}

/// The release as the fixtures state it: URL -> bytes.
fn world() -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        (format!("{IMAGES}{CHECKSUM}"), fixture(CHECKSUM)),
        (format!("{REPO}repodata/repomd.xml"), fixture("repomd.xml")),
        (format!("{REPO}repodata/{PRIMARY}"), fixture(PRIMARY)),
    ])
}

struct Upstream {
    files: BTreeMap<String, Vec<u8>>,
    requests: Mutex<Vec<String>>,
}

impl Upstream {
    fn new(files: BTreeMap<String, Vec<u8>>) -> Self {
        Upstream {
            files,
            requests: Mutex::new(Vec::new()),
        }
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
        self.requests.lock().unwrap().clone()
    }
}

/// `NAME-EPOCH:VERSION-RELEASE.ARCH` of one closure entry, as the lock's JSON states it.
fn nevra(p: &Value) -> String {
    let field = |key: &str| {
        p[key]
            .as_str()
            .unwrap_or_else(|| panic!("{} states no {key}: {p}", p["name"]))
            .to_string()
    };
    let epoch = p["epoch"]
        .as_u64()
        .unwrap_or_else(|| panic!("{} states no epoch: {p}", p["name"]));
    format!(
        "{}-{epoch}:{}-{}.{}",
        field("name"),
        field("version"),
        field("release"),
        field("arch")
    )
}

/// The closure of a base, as the lock's JSON states it.
fn closure(base: &lodi::lock::BaseEntry) -> Vec<Value> {
    serde_json::to_value(&base.closure.packages)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

/// The SHA-256 `primary.xml` states for each of the four builds `jq zip` adds (the fixture's
/// `<checksum type="sha256" pkgid="YES">`, the same as the release's full index).
const INSTALL_SHA256: &[(&str, &str)] = &[
    (
        "jq",
        "2a7d772f7f3f514f4a6404402c7acc282af700ae2208736ef2946e22ba52a17a",
    ),
    (
        "oniguruma",
        "3b49398d54913199677395c4336eb25e186a9235539f0e78e4dfdafe64b142cc",
    ),
    (
        "unzip",
        "2c8d388de55bb06cbdab00702a03d1e14f442d380509039bc524ed2c7cd41f68",
    ),
    (
        "zip",
        "78ecac054bb765596e0d02f5d5a36ef332402d40551309f5913294201c9ba130",
    ),
];

fn lock_library(requested: &[&str], upstream: &Upstream) -> lodi::lock::BaseEntry {
    let definition = builtin_base("fedora").unwrap().unwrap();
    let requested: Vec<String> = requested.iter().map(|s| s.to_string()).collect();
    let release = &definition.releases["44"];
    let locked = lock_base(
        upstream,
        &definition,
        &BaseRequest {
            release: "44",
            arch: "x86_64",
            snapshot: release.snapshot_min,
            now: parse_utc("2026-09-28T00:00:00Z").unwrap(),
            requested: &requested,
        },
    )
    .unwrap();
    base_entry(locked)
}

/// A project of its own, with its own `HOME` and `LODI_HOME` below it.
struct Project {
    dir: PathBuf,
    home: PathBuf,
}

impl Project {
    fn new(name: &str, packages: &[&str]) -> Project {
        let base = support::scratch(&format!("fedora-base-{name}"));
        let (dir, home) = (base.join("project"), base.join("home"));
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let list: Vec<String> = packages.iter().map(|p| format!("\"{p}\"")).collect();
        fs::write(
            dir.join("lodi.toml"),
            format!(
                "[container]\ndistro = \"fedora\"\nrelease = \"44\"\n\n[packages]\ncommon = [{}]\n",
                list.join(", ")
            ),
        )
        .unwrap();
        Project { dir, home }
    }

    /// `lodi develop` with no Podman on `PATH`: it locks by itself (LD-496) and then stops with
    /// `E_NO_RUNTIME`, so only the lock is observed.
    fn lock(&self, server: &Server) -> Output {
        Command::new(env!("CARGO_BIN_EXE_lodi"))
            .args(["develop", "--", "true"])
            .current_dir(&self.dir)
            .env_clear()
            .env("PATH", "")
            .env("HOME", &self.home)
            .env("LODI_HOME", self.home.join("lodi"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("LODI_FETCH_REWRITE", server.rewrite())
            .env("LODI_TRUST", "1")
            .output()
            .expect("the lodi binary runs")
    }

    fn lock_bytes(&self) -> Option<Vec<u8>> {
        fs::read(self.dir.join("lodi.lock")).ok()
    }
}

/// A locking run that got past the lock to the missing Podman.
fn locked(o: &Output) {
    assert_eq!(o.status.code(), Some(7), "{}", both(o));
    assert!(both(o).contains("E_NO_RUNTIME"), "{}", both(o));
}

fn both(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// Criterion 1: the base's digest and every build of the closure with its name, epoch,
/// version, release, arch, SHA-256 and repository, byte-identical when locked again.
#[test]
fn lock_records_the_exact_rpm_closure() {
    let project = Project::new("closure", &["jq", "zip"]);
    let server = Server::start(world());
    let first = project.lock(&server);
    locked(&first);
    assert!(
        both(&first).contains("lodi: wrote lodi.lock (resolved base)"),
        "{}",
        both(&first)
    );
    let bytes = project.lock_bytes().expect("develop wrote lodi.lock");
    let lock = lodi::lock::parse_lock(&bytes).unwrap();
    lodi::lock::validate(&lock).unwrap();
    let base = lock.base.as_ref().unwrap();
    assert_eq!(
        (base.distro.as_str(), base.release.as_str()),
        ("fedora", "44")
    );
    assert_eq!(base.snapshot, "2026-04-22T13:34:32Z");
    let json: Value = serde_json::from_slice(&bytes).unwrap();

    // The base image: the archive's SHA-256 and size as the release's CHECKSUM states them,
    // the CHECKSUM file's own digest, and the image manifest the archive must name.
    let checksum = String::from_utf8(fixture(CHECKSUM)).unwrap();
    assert!(checksum.contains(&format!(
        "SHA256 ({ARCHIVE}) = {}",
        lodi::util::untag_sha256(&base.rootfs.sha256).unwrap()
    )));
    assert_eq!(base.rootfs.url, format!("{IMAGES}{ARCHIVE}"));
    assert_eq!(base.rootfs.size, Some(70_170_200));
    assert_eq!(
        base.rootfs.oci_manifest.as_deref(),
        Some("sha256:f1e66cdd6eff2c9ccad192f8865af9be6d69b46b3f13329d2975a2d61a1296c5")
    );
    let pin = base.rootfs.checksums.as_ref().unwrap();
    assert_eq!(pin.sha256, sha256_tagged(checksum.as_bytes()));

    // The repository is pinned by repomd.xml and the primary index it names.
    let index = &base.repositories[0].indexes[0];
    assert_eq!(index.release.sha256, sha256_tagged(&fixture("repomd.xml")));
    assert_eq!(index.packages.path, format!("repodata/{PRIMARY}"));
    assert_eq!(index.packages.sha256, sha256_tagged(&fixture(PRIMARY)));

    // Every build: the 146 of the base image, and exactly what dnf5 installs for `jq zip`.
    let packages = json["base"]["closure"]["packages"].as_array().unwrap();
    assert_eq!(packages.len(), 150);
    let installs: Vec<String> = packages
        .iter()
        .filter(|p| p["origin"] == "install")
        .map(nevra)
        .collect();
    assert_eq!(
        installs,
        [
            "jq-0:1.8.1-2.fc44.x86_64",
            "oniguruma-0:6.9.10-4.fc44.x86_64",
            "unzip-0:6.0-69.fc44.x86_64",
            "zip-0:3.0-45.fc44.x86_64",
        ]
    );
    for p in packages {
        // Every build names its file, its SHA-256 and its repository, the image's own too.
        let sha = p["sha256"].as_str().unwrap_or_default();
        assert!(
            sha.len() == 71 && sha.starts_with("sha256:"),
            "{}: {sha}",
            nevra(p)
        );
        assert!(p["filename"].as_str().is_some_and(|f| f.ends_with(".rpm")));
        assert_eq!(p["repository"], "fedora");
    }
    for (name, sha) in INSTALL_SHA256 {
        let p = packages.iter().find(|p| p["name"] == *name).unwrap();
        assert_eq!(p["sha256"], format!("sha256:{sha}"), "{name}");
    }
    // Epochs are recorded as the builds state them.
    let tar = packages.iter().find(|p| p["name"] == "tar").unwrap();
    assert_eq!(nevra(tar), "tar-2:1.35-8.fc44.x86_64");
    assert_eq!(tar["origin"], "base");
    // Metadata only: no archive and no package was downloaded.
    let requests = server.requests();
    assert!(
        requests
            .iter()
            .all(|u| !u.ends_with(".rpm") && !u.ends_with(".tar.xz")),
        "{requests:?}"
    );

    // A second run changes nothing, and a lock written again from nothing is the same bytes.
    let second = project.lock(&server);
    locked(&second);
    assert!(
        !both(&second).contains("wrote lodi.lock"),
        "{}",
        both(&second)
    );
    assert_eq!(project.lock_bytes().unwrap(), bytes);
    fs::remove_file(project.dir.join("lodi.lock")).unwrap();
    let again = project.lock(&server);
    locked(&again);
    assert_eq!(project.lock_bytes().unwrap(), bytes);
}

/// A zstd frame of RLE blocks that expands to `blocks` × 128 KiB from four bytes per block.
fn zstd_bomb(blocks: usize) -> Vec<u8> {
    let mut out = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x58];
    for i in 0..blocks {
        let last = usize::from(i + 1 == blocks);
        let header = last | 1 << 1 | (128 * 1024) << 3;
        out.extend_from_slice(&header.to_le_bytes()[..3]);
        out.push(b'<');
    }
    out
}

/// Criterion 2: primary metadata whose digest is not repomd's, or that decompresses past the
/// cap, stops locking with a named error and no lock.
#[test]
fn tampered_or_oversized_metadata_fails_closed() {
    let repomd_url = format!("{REPO}repodata/repomd.xml");
    let primary_url = format!("{REPO}repodata/{PRIMARY}");

    // One flipped byte of the compressed index.
    let mut tampered = world();
    tampered.get_mut(&primary_url).unwrap()[100] ^= 1;
    let project = Project::new("tampered", &["jq"]);
    let out = project.lock(&Server::start(tampered));
    assert_eq!(out.status.code(), Some(5), "{}", both(&out));
    assert!(both(&out).contains("E_HASH_MISMATCH"), "{}", both(&out));
    assert!(both(&out).contains("repomd.xml states"), "{}", both(&out));
    assert!(project.lock_bytes().is_none());

    // An index that expands past the 512 MiB cap: repomd.xml describes the bomb truthfully as
    // far as the compressed bytes go, and understates what it expands to.
    let bomb = zstd_bomb(4097);
    let repomd = String::from_utf8(fixture("repomd.xml")).unwrap();
    let bomb_sha = sha256_hex(&bomb);
    let lying = repomd
        .replace(
            "bb09509d54a5d1e07ad37d1abe8869bb69ab363310273e5e394bcab0b9ab7eab",
            &bomb_sha,
        )
        .replace(
            "<size>63323</size>",
            &format!("<size>{}</size>", bomb.len()),
        );
    let mut oversized = world();
    oversized.insert(repomd_url.clone(), lying.clone().into_bytes());
    oversized.insert(format!("{REPO}repodata/{bomb_sha}-primary.xml.zst"), bomb);
    let project = Project::new("oversized", &["jq"]);
    let out = project.lock(&Server::start(oversized));
    assert_eq!(out.status.code(), Some(4), "{}", both(&out));
    assert!(both(&out).contains("E_REPO_UNREACHABLE"), "{}", both(&out));
    assert!(both(&out).contains("byte limit"), "{}", both(&out));
    assert!(project.lock_bytes().is_none());

    // A repomd.xml that states more than the cap is refused before anything is decompressed.
    let mut stated = world();
    stated.insert(
        repomd_url,
        repomd
            .replace(
                "<open-size>545929</open-size>",
                "<open-size>9999999999</open-size>",
            )
            .into_bytes(),
    );
    let project = Project::new("stated", &["jq"]);
    let out = project.lock(&Server::start(stated));
    assert_eq!(out.status.code(), Some(4), "{}", both(&out));
    assert!(both(&out).contains("past the"), "{}", both(&out));
    assert!(project.lock_bytes().is_none());
}

/// Criterion 3: a rich dependency and a file provide resolve as dnf5 resolves them, with rpm's
/// version comparison.
#[test]
fn rich_dependencies_and_file_provides_resolve_like_dnf() {
    let upstream = Upstream::new(world());
    let base = lock_library(&["ntpstat", "nss-mdns"], &upstream);
    let packages = closure(&base);
    let installs: Vec<String> = packages
        .iter()
        .filter(|p| p["origin"] == "install")
        .map(nevra)
        .collect();
    // `(ntpsec or chrony)`: both are served (PROVENANCE), and dnf takes the first name in byte
    // order. `/usr/bin/bash`, a file ntpstat requires, is met by the image's own bash.
    assert_eq!(installs, DNF_NTPSTAT_NSS_MDNS);
    assert!(
        packages
            .iter()
            .any(|p| p["name"] == "bash" && p["origin"] == "base")
    );
    assert!(!packages.iter().any(|p| p["name"] == "ntpsec"));

    // Without systemd in the closure the condition is off: `jq zip nss-mdns` takes no avahi.
    let base = lock_library(&["nss-mdns"], &upstream);
    let installs: Vec<String> = closure(&base)
        .iter()
        .filter(|p| p["origin"] == "install")
        .map(nevra)
        .collect();
    assert_eq!(installs, ["nss-mdns-0:0.15.1-28.fc44.x86_64"]);
    // rpm's version rules decided the versioned requirements above: avahi's `avahi-libs(x86-64)
    // = 0.9~rc2` (no release: every release of that version) and dbus's `(dbus-broker >= 16-4
    // if systemd)` against 37-8.fc44; `src/distro/dnf/metadata.rs` holds the comparison itself.

    // A request no build satisfies is named, and nothing else is chosen for it.
    let definition = builtin_base("fedora").unwrap().unwrap();
    let wanted = vec!["no-such-package".to_string()];
    let error = lock_base(
        &upstream,
        &definition,
        &BaseRequest {
            release: "44",
            arch: "x86_64",
            snapshot: definition.releases["44"].snapshot_min,
            now: parse_utc("2026-09-28T00:00:00Z").unwrap(),
            requested: &wanted,
        },
    )
    .unwrap_err();
    assert_eq!(error.code, "E_NO_MATCH");
}

/// A stand-in for `podman` that records every invocation and has no image: realization reaches
/// its downloads, and the log shows whether anything was built or run.
fn podman_shim(dir: &Path) -> (PathBuf, PathBuf) {
    fs::create_dir_all(dir).unwrap();
    let log = dir.join("podman.log");
    let path = dir.join("podman");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             echo \"$*\" >> '{}'\n\
             for a in \"$@\"; do case \"$a\" in exists) exit 1 ;; esac; done\n\
             exit 0\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&path, PermissionsExt::from_mode(0o755)).unwrap();
    (dir.to_path_buf(), log)
}

/// Criterion 4: a locked build that is no longer served, or whose bytes differ, stops before
/// anything is built or entered, with a named error, and no other build is fetched.
#[test]
fn a_missing_or_changed_build_stops_before_the_session() {
    let upstream = Upstream::new(world());
    let mut base = lock_library(&["jq", "zip"], &upstream);
    // The base archive is a stand-in this test owns; the builds are the lock's own.
    let rootfs = b"a stand-in for the Fedora 44 base archive, never imported".to_vec();
    base.rootfs.sha256 = sha256_tagged(&rootfs);
    base.rootfs.size = Some(rootfs.len() as u64);
    let jq = base
        .closure
        .packages
        .iter()
        .find(|p| p.name == "jq")
        .unwrap()
        .clone();
    let jq_url = format!("{REPO}{}", jq.filename.as_deref().unwrap());
    let others: Vec<(String, Vec<u8>)> = base
        .closure
        .packages
        .iter()
        .filter(|p| p.origin == "install" && p.name != "jq")
        .map(|p| {
            (
                format!("{REPO}{}", p.filename.as_deref().unwrap()),
                Vec::new(),
            )
        })
        .collect();

    for (case, served) in [
        ("missing", None),
        ("changed", Some(b"not the locked jq".to_vec())),
    ] {
        let root = support::scratch(&format!("fedora-base-stop-{case}"));
        let store = lodi::store::Store::open(&root.join("lodi-home")).unwrap();
        let (bin, log) = podman_shim(&root.join("bin"));
        let podman = lodi::container::Podman::find(Some(bin.as_os_str())).unwrap();
        let mut files = BTreeMap::from([(base.rootfs.url.clone(), rootfs.clone())]);
        if let Some(bytes) = served {
            files.insert(jq_url.clone(), bytes);
        }
        let fetcher = Upstream::new(files);
        let error = lodi::container::realize_image(
            &store,
            &podman,
            &fetcher,
            &base,
            &mut lodi::store::Report::default(),
        )
        .expect_err("the environment is refused");
        let (code, words) = match case {
            "missing" => (
                "E_BUILD_UNAVAILABLE",
                "jq-0:1.8.1-2.fc44.x86_64 is not served any more",
            ),
            _ => ("E_HASH_MISMATCH", "does not match the lock"),
        };
        assert_eq!(error.code, code, "{error:?}");
        assert!(error.message.contains(words), "{}", error.message);
        assert_eq!(exit_status(error.code), 5);
        // Nothing was built, imported or run, and the only build asked for is the locked file:
        // no other version and no metadata was requested in its place.
        let asked = fs::read_to_string(&log).unwrap_or_default();
        assert!(
            !asked.contains("build") && !asked.contains("import") && !asked.contains("run"),
            "{asked}"
        );
        let requests = fetcher.requests();
        assert!(requests.contains(&jq_url), "{requests:?}");
        assert!(
            requests.iter().all(|u| *u == base.rootfs.url
                || *u == jq_url
                || others.iter().any(|(o, _)| o == u)),
            "{requests:?}"
        );
        support::make_writable(&root);
        fs::remove_dir_all(&root).unwrap();
    }
}
