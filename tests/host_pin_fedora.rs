//! fk-1 (LD-434): exact Fedora builds pinned in the repository's one `lodi.lock`, and `git
//! revert` puts the older build back — from a configured repository when it still serves those
//! bytes, else from Fedora's signed Koji copy, else not at all.
//!
//! Every case is the real binary against a scratch `--root` with the fake machine of
//! `tests/support/fakepm.py` behind it: its dnf5 answers `repoquery` with the rows a Fedora 44
//! guest printed, and `download` with the real package files of `tests/fixtures/host/dnf/pin/`
//! (see its `PROVENANCE.json`). Koji is those same files on loopback. Nothing reaches `/` or
//! the network, and no real package manager runs (`AGENTS.md` §8).

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
mod support;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use fakehost::{Case, Machine, err, out, story};
use serde_json::{Value, json};

const OLDER: &str = "0:1.10.4-1.fc44.x86_64";
const NEWER: &str = "0:1.10.5-1.fc44.x86_64";
const HTOP: &str = "0:3.4.1-3.fc44.x86_64";
const KOJI: &str = "https://kojipkgs.fedoraproject.org/packages";

fn fixture(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/host/dnf/pin")
        .join(file)
}

fn digest(file: &str) -> String {
    format!(
        "sha256:{}",
        lodi::util::sha256_hex(&fs::read(fixture(file)).unwrap())
    )
}

/// Fedora's signed Koji copy of a build, as the lock records it.
fn koji(source: &str, name: &str, version: &str, release: &str) -> String {
    format!(
        "{KOJI}/{source}/{version}/{release}/data/signed/6d9f90a6/x86_64/\
         {name}-{version}-{release}.x86_64.rpm"
    )
}

fn pv_koji(version: &str) -> String {
    koji("pv", "pv", version, "1.fc44")
}

/// What Koji serves: the signed copy of each build these cases use.
fn koji_files() -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        (
            pv_koji("1.10.4"),
            fs::read(fixture("pv-1.10.4-1.fc44.x86_64.rpm")).unwrap(),
        ),
        (
            pv_koji("1.10.5"),
            fs::read(fixture("pv-1.10.5-1.fc44.x86_64.rpm")).unwrap(),
        ),
        (
            koji("htop", "htop", "3.4.1", "3.fc44"),
            fs::read(fixture("htop-3.4.1-3.fc44.x86_64.rpm")).unwrap(),
        ),
    ])
}

/// A Fedora 44 machine named `fk1-machine` with `htop` installed, `pv` at `pv` (or not
/// installed), both builds of `pv` offered as the guest's repositories offered them, and a
/// repository at `hosts/` whose host `box` declares both.
struct Fedora {
    case: Case,
    server: support::Server,
}

impl Fedora {
    fn new(name: &str, pv: Option<&str>) -> Fedora {
        let case = Case::new(name, Machine::fedora());
        case.root.write("etc/hostname", "fk1-machine\n");
        let serves: BTreeMap<String, String> = [
            ("pv", OLDER, "pv-1.10.4-1.fc44.x86_64.rpm"),
            ("pv", NEWER, "pv-1.10.5-1.fc44.x86_64.rpm"),
            ("htop", HTOP, "htop-3.4.1-3.fc44.x86_64.rpm"),
        ]
        .into_iter()
        .map(|(n, build, file)| (format!("{n}-{build}"), fixture(file).display().to_string()))
        .collect();
        case.edit(|m| {
            let build = |b: &str| b.trim_end_matches(".x86_64").to_string();
            m["installed"]["htop"] =
                json!({"version": build(HTOP), "explicit": true, "depends": ["glibc"]});
            if let Some(pv) = pv {
                m["installed"]["pv"] =
                    json!({"version": build(pv), "explicit": true, "depends": ["glibc"]});
            }
            m["available"]["pv"] =
                json!({"version": build(NEWER), "repo": "updates", "depends": ["glibc"]});
            m["also"]["pv"] =
                json!([{"version": build(OLDER), "repo": "fedora", "depends": ["glibc"]}]);
            m["available"]["htop"] =
                json!({"version": build(HTOP), "repo": "fedora", "depends": ["glibc"]});
            m["serves"] = json!(serves);
        });
        for dir in ["hosts", "hosts/box"] {
            fs::create_dir_all(case.root.path(dir)).unwrap();
            fs::set_permissions(case.root.path(dir), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let fedora = Fedora {
            case,
            server: support::Server::start(koji_files()),
        };
        fedora.set_host(
            "[host]\nversion = \"1\"\ndistro = \"fedora\"\n\n[packages.fedora]\n\
             add = [\"htop\", \"pv\"]\n",
        );
        fedora
    }

    fn hosts(&self) -> PathBuf {
        self.case.root.path("hosts")
    }

    fn set_host(&self, text: &str) {
        let path = self.hosts().join("box/host.toml");
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    fn host_toml(&self) -> String {
        fs::read_to_string(self.hosts().join("box/host.toml")).unwrap()
    }

    fn lock_bytes(&self) -> Vec<u8> {
        fs::read(self.hosts().join("lodi.lock")).unwrap_or_default()
    }

    fn lock(&self) -> Value {
        serde_json::from_slice(&self.lock_bytes()).expect("lodi.lock is JSON")
    }

    /// `lodi host VERB [ARGS] hosts --host box --root ROOT`, Koji on loopback.
    fn run(&self, verb: &str, args: &[&str]) -> Output {
        let hosts = self.hosts().display().to_string();
        let mut extra: Vec<&str> = args.to_vec();
        extra.extend([hosts.as_str(), "--host", "box"]);
        let rewrite = self.server.rewrite();
        self.case
            .verb_env(verb, &extra, &[("LODI_FETCH_REWRITE", rewrite.as_str())])
    }

    fn pv(&self) -> String {
        let m = self.case.machine();
        format!(
            "{}.{}",
            m["installed"]["pv"]["version"].as_str().unwrap_or("none"),
            m["installed"]["pv"]["arch"].as_str().unwrap_or("x86_64")
        )
    }

    fn koji_requests(&self) -> Vec<String> {
        self.server
            .requests()
            .into_iter()
            .filter(|url| url.contains("kojipkgs"))
            .collect()
    }

    /// `git` in the repository, as its owner, under an identity with no address.
    fn git(&self, args: &[&str]) {
        let done = Command::new("git")
            .args(["-c", "user.name=fk1", "-c", "user.email=fk1"])
            .args(args)
            .current_dir(self.hosts())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", self.case.root.path("home"))
            .output()
            .expect("git runs");
        assert!(done.status.success(), "git {args:?}: {}", story(&done));
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", message]);
    }

    /// Pin `pv` at the installed older build and commit, pin it to the newer build, commit and
    /// apply: the update a `git revert` then takes back.
    fn updated(&self) {
        self.git(&["init", "-q"]);
        let pinned = self.run("pin", &["pv"]);
        assert_eq!(pinned.status.code(), Some(0), "{}", story(&pinned));
        self.commit("pin pv");
        let moved = self.run("pin", &["pv", "--to", NEWER]);
        assert_eq!(moved.status.code(), Some(0), "{}", story(&moved));
        self.commit("pv newer");
        let applied = self.run("apply", &[]);
        assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
        assert_eq!(self.pv(), NEWER, "{}", story(&applied));
        self.git(&["revert", "--no-edit", "HEAD"]);
    }

    /// The configured repositories no longer serve the older build.
    fn evict_older(&self) {
        self.case.edit(|m| {
            m["serves"]
                .as_object_mut()
                .unwrap()
                .remove(&format!("pv-{OLDER}"));
        });
    }

    fn mutated(&self) -> Vec<String> {
        self.case
            .log()
            .into_iter()
            .filter(|line| line.starts_with("dnf5 ") && line.contains(" -y "))
            .collect()
    }
}

#[test]
fn pin_records_the_exact_build_in_the_root_lock() {
    let f = Fedora::new("fk1-pin", Some(OLDER));
    let pinned = f.run("pin", &["htop"]);
    assert_eq!(pinned.status.code(), Some(0), "{}", story(&pinned));
    assert!(
        f.host_toml().contains(&format!("htop = \"{HTOP}\"")),
        "{}",
        f.host_toml()
    );
    let lock = f.lock();
    assert_eq!(
        (lock["version"].clone(), lock["format"].clone()),
        (json!(2), json!("lodi-repository-lock/2"))
    );
    let section = &lock["hosts"]["box"];
    assert_eq!(
        (&section["distro"], &section["release"], &section["arch"]),
        (&json!("fedora"), &json!("44"), &json!("x86_64"))
    );
    assert_eq!(
        section["pins"]["htop"],
        json!({
            "policy": "version",
            "requested": HTOP,
            "epoch": "0",
            "version": "3.4.1",
            "release": "3.fc44",
            "arch": "x86_64",
            "repository": "fedora",
            "filename": "htop-3.4.1-3.fc44.x86_64.rpm",
            "sha256": digest("htop-3.4.1-3.fc44.x86_64.rpm"),
            "source": koji("htop", "htop", "3.4.1", "3.fc44"),
        })
    );
    let text = String::from_utf8(f.lock_bytes()).unwrap();
    let root = f.case.root.dir.display().to_string();
    for private in ["fk1-machine", root.as_str(), "/home/", "/people/"] {
        assert!(!text.contains(private), "{private} in the lock:\n{text}");
    }
    assert!(
        f.koji_requests().is_empty(),
        "the mirror served it: {:?}",
        f.koji_requests()
    );

    // Pinning the same build again changes nothing and asks nothing.
    let log = f.case.log();
    let again = f.run("pin", &["htop"]);
    assert_eq!(again.status.code(), Some(0), "{}", story(&again));
    assert!(out(&again).contains("nothing written"), "{}", story(&again));
    assert!(
        f.case.log().iter().all(|line| !line.contains("download")),
        "{log:?}"
    );
    assert_eq!(
        f.lock()["hosts"]["box"]["pins"]["htop"],
        section["pins"]["htop"]
    );
}

#[test]
fn revert_reinstalls_the_locked_build_from_the_mirror() {
    let f = Fedora::new("fk1-revert-mirror", Some(OLDER));
    f.updated();
    assert_eq!(
        f.lock()["hosts"]["box"]["pins"]["pv"]["release"],
        json!("1.fc44")
    );
    assert_eq!(
        f.lock()["hosts"]["box"]["pins"]["pv"]["version"],
        json!("1.10.4")
    );
    let applied = f.run("apply", &[]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    assert_eq!(f.pv(), OLDER, "{}", story(&applied));
    let log = f.case.log();
    assert!(
        log.iter().any(|line| line.contains("dnf5 ")
            && line.contains("download")
            && line.contains(&format!("pv-{OLDER}"))),
        "{log:?}"
    );
    assert!(
        log.iter().any(|line| line.contains(" -y ")
            && line.contains("--setopt=localpkg_gpgcheck=True")
            && line.contains("/var/lib/lodi/host/pin/stage/pv-1.10.4-1.fc44.x86_64.rpm")),
        "{log:?}"
    );
    assert!(f.koji_requests().is_empty(), "{:?}", f.koji_requests());
    assert!(
        !f.case.root.path("var/lib/lodi/host/pin/stage").exists(),
        "the stage is removed"
    );

    // A warm re-apply asks nothing of any archive and changes nothing.
    let requests = f.server.requests().len();
    let again = f.run("apply", &[]);
    assert_eq!(again.status.code(), Some(0), "{}", story(&again));
    assert!(out(&again).contains("nothing to do"), "{}", story(&again));
    assert_eq!(f.server.requests().len(), requests);
    assert!(
        f.case.log().iter().all(|line| !line.contains("download")),
        "{:?}",
        f.case.log()
    );
}

#[test]
fn a_dropped_build_comes_from_the_signed_koji_copy_or_not_at_all() {
    // The mirror dropped the older build: Koji's signed copy, held to the locked SHA-256.
    let f = Fedora::new("fk1-revert-koji", Some(OLDER));
    f.updated();
    f.evict_older();
    let applied = f.run("apply", &[]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    assert_eq!(f.pv(), OLDER, "{}", story(&applied));
    assert_eq!(f.koji_requests(), vec![pv_koji("1.10.4")]);

    // Koji serves other bytes than the lock's: refused, and nothing changes.
    let f = Fedora::new("fk1-koji-digest", Some(OLDER));
    f.updated();
    f.evict_older();
    let unsigned = fs::read(fixture("unsigned-pv-1.10.4-1.fc44.x86_64.rpm")).unwrap();
    let f = Fedora {
        server: support::Server::start(BTreeMap::from([(pv_koji("1.10.4"), unsigned.clone())])),
        ..f
    };
    let refused = f.run("apply", &[]);
    assert_eq!(refused.status.code(), Some(5), "{}", story(&refused));
    assert!(
        err(&refused).contains("E_HASH_MISMATCH"),
        "{}",
        story(&refused)
    );
    assert_eq!(f.pv(), NEWER);
    assert!(f.mutated().is_empty(), "{:?}", f.mutated());

    // A lock whose digest is of a copy nobody signed: the digest matches, the key does not.
    let mut lock = f.lock();
    lock["hosts"]["box"]["pins"]["pv"]["sha256"] =
        json!(digest("unsigned-pv-1.10.4-1.fc44.x86_64.rpm"));
    fs::write(
        f.hosts().join("lodi.lock"),
        lodi::lock::canonical_json(&lock),
    )
    .unwrap();
    let refused = f.run("apply", &[]);
    assert_eq!(refused.status.code(), Some(5), "{}", story(&refused));
    assert!(
        err(&refused).contains("E_PIN_UNTRUSTED"),
        "{}",
        story(&refused)
    );
    assert!(
        err(&refused).contains("is not signed"),
        "{}",
        story(&refused)
    );
    assert_eq!(f.pv(), NEWER);
    assert!(f.mutated().is_empty(), "{:?}", f.mutated());
}

#[test]
fn a_pruned_build_stops_before_any_change() {
    let f = Fedora::new("fk1-pruned", Some(OLDER));
    f.updated();
    f.evict_older();
    let f = Fedora {
        server: support::Server::start(BTreeMap::new()),
        ..f
    };
    let refused = f.run("apply", &[]);
    assert_eq!(refused.status.code(), Some(4), "{}", story(&refused));
    let said = err(&refused);
    assert!(said.contains("E_PIN_UNAVAILABLE"), "{}", story(&refused));
    assert!(said.contains(&format!("pv {OLDER}")), "{}", story(&refused));
    assert!(said.contains("signed Koji copy"), "{}", story(&refused));
    assert_eq!(f.pv(), NEWER);
    assert!(f.mutated().is_empty(), "{:?}", f.mutated());
    assert!(!f.case.root.path("var/lib/lodi/host/pin/stage").exists());
}

#[test]
fn versions_and_unpin_work_on_fedora() {
    let f = Fedora::new("fk1-versions", None);
    let pinned = f.run("pin", &["pv", "--to", OLDER]);
    assert_eq!(pinned.status.code(), Some(0), "{}", story(&pinned));

    let versions = f.run("versions", &["pv"]);
    assert_eq!(versions.status.code(), Some(0), "{}", story(&versions));
    let listed = out(&versions);
    let rows: Vec<&str> = listed.lines().collect();
    assert!(rows[0].starts_with("build"), "{listed}");
    assert!(
        rows[1].starts_with(NEWER) && rows[1].contains("updates") && rows[1].ends_with("latest"),
        "{listed}"
    );
    assert!(
        rows[2].starts_with(OLDER) && rows[2].contains("fedora") && rows[2].ends_with("pinned"),
        "{listed}"
    );
    assert!(
        listed.contains(&format!("pin pv --to {NEWER} ")),
        "{listed}"
    );

    let plan = f.run("plan", &[]);
    assert_eq!(plan.status.code(), Some(0), "{}", story(&plan));
    assert!(
        out(&plan).contains(&format!("+ package pv {OLDER}")),
        "{}",
        story(&plan)
    );

    let unpinned = f.run("unpin", &["pv"]);
    assert_eq!(unpinned.status.code(), Some(0), "{}", story(&unpinned));
    assert!(!f.host_toml().contains("pv = "), "{}", f.host_toml());
    assert!(
        f.lock_bytes().is_empty(),
        "the only pin is gone, and the lock with it"
    );
    let applied = f.run("apply", &[]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    assert_eq!(f.pv(), NEWER, "{}", story(&applied));
    assert!(f.koji_requests().is_empty());
}
