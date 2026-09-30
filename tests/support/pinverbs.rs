//! The pin verbs' CLI harness (LD-397): one machine called `box` under a scratch `--root`, a
//! user-owned directory of hosts at `<root>/hosts`, and the recorded dated archives of
//! `tests/fixtures/host/pin/` served on loopback through `LODI_FETCH_REWRITE`. Shared by
//! `tests/host_pin_verbs_cli.rs` and `tests/host_pin_verbs.rs`.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Output;

use crate::fakehost::{Case, Machine, Pkg, err, story};
use crate::support;
use lodi::diag::exit_status;

pub const OLDER: &str = "2025-03-01T00:00:00Z";
pub const RECENT: &str = "2026-09-01T00:00:00Z";
/// What the recorded Debian archives serve: `bc` on both days, `tzdata` from the older day and
/// from the newer day's main suite.
pub const BC: &str = "1.07.1-3+b1";
pub const TZ_OLDER: &str = "2024b-0+deb12u1";
pub const TZ_RECENT: &str = "2026b-0+deb12u1";

pub const DEBIAN_SOURCES: &str = "Types: deb\nURIs: http://deb.debian.org/debian\n\
    Suites: bookworm bookworm-updates\nComponents: main\n\
    Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg\n\n\
    Types: deb\nURIs: http://deb.debian.org/debian-security\nSuites: bookworm-security\n\
    Components: main\nSigned-By: /usr/share/keyrings/debian-archive-keyring.gpg\n";

pub fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every recorded archive file by the public URL it was trimmed from, and every recorded answer
/// of Ubuntu's per-package interface.
pub fn archive_files() -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(dir).expect("a fixture directory") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if path.file_name().is_some_and(|n| n != "PROVENANCE") {
                let rel = path.strip_prefix(base).unwrap().display().to_string();
                out.insert(format!("https://{rel}"), fs::read(&path).unwrap());
            }
        }
    }
    let fixtures = repo().join("tests/fixtures/host/pin");
    let mut out = BTreeMap::new();
    let base = fixtures.join("archive");
    walk(&base, &base, &mut out);
    let interface = fixtures.join("interface");
    let index: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(interface.join("index.json")).unwrap()).unwrap();
    for (url, file) in index {
        out.insert(url, fs::read(interface.join(file)).unwrap());
    }
    out
}

pub fn pkg(name: &str, version: &str) -> Pkg {
    let mut pkg = Pkg::new(name);
    pkg.version = version.to_string();
    pkg
}

pub fn with(mut machine: Machine, installed: &[(&str, &str)]) -> Machine {
    for (name, version) in installed {
        machine = machine.with(pkg(name, version));
    }
    machine
}

/// A host manifest: `distro`, managed packages, an optional file-level snapshot and pins.
pub fn host(
    distro: &str,
    snapshot: Option<&str>,
    common: &[&str],
    pins: &[(&str, &str)],
) -> String {
    let mut text =
        format!("[host]\nversion = \"1\"\ndistro = \"{distro}\"\npackages = \"managed\"\n");
    if let Some(snapshot) = snapshot {
        text.push_str(&format!("snapshot = \"{snapshot}\"\n"));
    }
    let names: Vec<String> = common.iter().map(|n| format!("\"{n}\"")).collect();
    text.push_str(&format!("\n[packages]\ncommon = [{}]\n", names.join(", ")));
    if !pins.is_empty() {
        text.push_str("\n[packages.pin]\n");
        for (name, value) in pins {
            text.push_str(&format!("{name} = \"{value}\"\n"));
        }
    }
    text
}

/// The lines only `before` has and the lines only `after` has, each as a multiset.
pub fn changed_lines(before: &str, after: &str) -> (Vec<String>, Vec<String>) {
    let mut removed: Vec<String> = before.lines().map(str::to_string).collect();
    let mut added = Vec::new();
    for line in after.lines() {
        match removed.iter().position(|l| l == line) {
            Some(at) => {
                removed.remove(at);
            }
            None => added.push(line.to_string()),
        }
    }
    (removed, added)
}

/// Kind, mode, size, content digest and nanosecond mtime of everything below `dir`, by path.
pub fn census(dir: &Path) -> BTreeMap<PathBuf, (u32, u64, String, i128)> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, (u32, u64, String, i128)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let digest = if meta.is_file() {
                lodi::util::sha256_hex(&fs::read(&path).unwrap())
            } else {
                String::new()
            };
            out.insert(
                path.clone(),
                (
                    meta.mode(),
                    if meta.is_dir() { 0 } else { meta.len() },
                    digest,
                    i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
                ),
            );
            if meta.is_dir() {
                walk(&path, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, &mut out);
    out
}

/// Copy a host directory as `git clone` would bring it: every file, every mode.
pub fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap().flatten() {
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        if source.is_dir() {
            copy_tree(&source, &target);
        } else {
            fs::copy(&source, &target).unwrap();
        }
        let mode = fs::metadata(&source).unwrap().permissions();
        fs::set_permissions(&target, mode).unwrap();
    }
}

pub fn code(output: &Output) -> Option<i32> {
    output.status.code()
}

#[track_caller]
pub fn ok(output: &Output) {
    assert_eq!(code(output), Some(0), "{}", story(output));
}

#[track_caller]
pub fn refused(output: &Output, wanted: &str) {
    assert_eq!(
        code(output),
        Some(i32::from(exit_status(wanted))),
        "{}",
        story(output)
    );
    assert!(err(output).contains(wanted), "{}", story(output));
}

/// One machine called `box`, a directory of hosts the user owns at `<root>/hosts` holding its
/// host `box/`, and the loopback archive the product fetches through.
pub struct Verbs {
    pub case: Case,
    pub server: support::Server,
}

impl Verbs {
    pub fn debian(name: &str, installed: &[(&str, &str)]) -> Verbs {
        Verbs::on(name, with(Machine::debian(), installed), false)
    }

    pub fn ubuntu(name: &str, installed: &[(&str, &str)]) -> Verbs {
        Verbs::on(name, with(Machine::debian(), installed), true)
    }

    pub fn on(name: &str, machine: Machine, ubuntu: bool) -> Verbs {
        Verbs::named(name, machine, ubuntu, "box")
    }

    pub fn named(name: &str, machine: Machine, ubuntu: bool, hostname: &str) -> Verbs {
        let case = Case::new(name, machine);
        case.root.write("etc/hostname", &format!("{hostname}\n"));
        let fixtures = repo().join("tests/fixtures/host/pin/archive");
        if ubuntu {
            case.root.write(
                "etc/os-release",
                "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\n\
                 VERSION_CODENAME=noble\n",
            );
        } else {
            case.root
                .write("etc/apt/sources.list.d/debian.sources", DEBIAN_SOURCES);
            for repository in ["debian", "debian-security"] {
                let dir = fixtures
                    .join("snapshot.debian.org/archive")
                    .join(repository);
                for id in fs::read_dir(&dir).unwrap().flatten() {
                    let id = id.file_name().to_string_lossy().into_owned();
                    case.archive(
                        &format!("https://snapshot.debian.org/archive/{repository}/{id}/"),
                        &dir.join(&id),
                    );
                }
            }
        }
        for dir in ["hosts", "hosts/box"] {
            fs::create_dir_all(case.root.path(dir)).unwrap();
            fs::set_permissions(case.root.path(dir), fs::Permissions::from_mode(0o755)).unwrap();
        }
        Verbs {
            case,
            server: support::Server::start(archive_files()),
        }
    }

    pub fn run(&self, verb: &str, extra: &[&str]) -> Output {
        let rewrite = self.server.rewrite();
        self.case
            .verb_env(verb, extra, &[("LODI_FETCH_REWRITE", rewrite.as_str())])
    }

    pub fn root(&self) -> String {
        self.case.root.dir.display().to_string()
    }

    pub fn hosts(&self) -> String {
        self.case.root.path("hosts").display().to_string()
    }

    pub fn dir(&self) -> PathBuf {
        self.case.root.path("hosts/box")
    }

    pub fn host_toml(&self) -> String {
        fs::read_to_string(self.dir().join("host.toml")).unwrap()
    }

    pub fn set_host(&self, text: &str) {
        fs::write(self.dir().join("host.toml"), text).unwrap();
        fs::set_permissions(
            self.dir().join("host.toml"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
    }

    /// The host's pins as a 1.4 `pins.lock` reads: that file when the host has one, else its
    /// section of the repository's root lock — the directory above it, which the verbs of this
    /// harness name as `SOURCE` (LD-416) — with the version and format the section leaves out.
    pub fn lock_at(&self, dir: &Path) -> Option<serde_json::Value> {
        if let Ok(bytes) = fs::read(dir.join("pins.lock")) {
            return Some(serde_json::from_slice(&bytes).expect("pins.lock is JSON"));
        }
        let key = dir.file_name()?.to_str()?;
        let bytes = fs::read(dir.parent()?.join("lodi.lock")).ok()?;
        let root: serde_json::Value = serde_json::from_slice(&bytes).expect("lodi.lock is JSON");
        let mut section = root["hosts"][key].as_object()?.clone();
        section.remove("keys");
        section
            .entry("pins")
            .or_insert_with(|| serde_json::json!({}));
        section.insert("version".into(), serde_json::json!(1));
        section.insert("format".into(), serde_json::json!("lodi-host-pins/1"));
        Some(serde_json::Value::Object(section))
    }

    /// The repository's root lock this harness's verbs write, as bytes (LD-416).
    pub fn root_lock(&self) -> Option<Vec<u8>> {
        fs::read(self.case.root.path("hosts/lodi.lock")).ok()
    }

    pub fn lock(&self) -> Option<serde_json::Value> {
        self.lock_at(&self.dir())
    }

    /// Serve the newer recorded day as the dated archive of the last ended UTC day and of the
    /// next one: the instants a verb given no `--to` takes, whichever day it runs in (LD-444).
    pub fn alias_now(&self) -> Vec<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let day = lodi::arch::base::latest_published_day(now);
        let instants: Vec<String> = [day, day + 86_400]
            .iter()
            .map(|at| lodi::util::format_utc(*at))
            .collect();
        let mut files = self.server.files.lock().unwrap();
        let recorded: Vec<(String, Vec<u8>)> = files
            .iter()
            .filter(|(url, _)| url.contains("/20260901T000000Z/"))
            .map(|(url, bytes)| (url.clone(), bytes.clone()))
            .collect();
        for instant in &instants {
            let id = instant.replace(['-', ':'], "");
            for (url, bytes) in &recorded {
                files.insert(url.replace("20260901T000000Z", &id), bytes.clone());
            }
        }
        instants
    }
}
