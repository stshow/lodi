//! The pin verbs' CLI harness (LD-397): one machine called `box` under a scratch `--root`, its
//! config, and the recorded dated archives of `tests/fixtures/host/pin/` served on loopback
//! through `LODI_FETCH_REWRITE`. The config is the scratch `HOME`'s `~/.config/lodi`, which
//! `lodi pin` finds with no path (#696).

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
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

pub use crate::fakehost::archive_files;

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

    /// The folder holding the host's `host.toml`: the config.
    pub fn dir(&self) -> PathBuf {
        self.case.config()
    }

    pub fn host_toml(&self) -> String {
        fs::read_to_string(self.dir().join("host.toml")).unwrap()
    }

    pub fn set_host(&self, text: &str) {
        self.case.set_manifest(text)
    }

    /// The host's pins: the section of its `host.toml` in the config's own `lodi.lock`, shaped
    /// as the cases read it.
    pub fn lock_at(&self, dir: &Path) -> Option<serde_json::Value> {
        let (key, bytes) = ("host.toml", fs::read(dir.join("lodi.lock")).ok()?);
        let root: serde_json::Value = serde_json::from_slice(&bytes).expect("lodi.lock is JSON");
        let mut section = root["hosts"][key].as_object()?.clone();
        section.remove("keys");
        section
            .entry("pins")
            .or_insert_with(|| serde_json::json!({}));
        Some(serde_json::Value::Object(section))
    }

    /// The config's lock this harness's verbs write, as bytes.
    pub fn root_lock(&self) -> Option<Vec<u8>> {
        fs::read(self.case.config().join("lodi.lock")).ok()
    }

    pub fn lock(&self) -> Option<serde_json::Value> {
        self.lock_at(&self.dir())
    }

    /// Serve the newer recorded day as the dated archive of the last ended UTC day and of the
    /// next one: the instants a verb given no `--to` takes, whichever day it runs in (LD-444).
    pub fn alias_now(&self) -> Vec<String> {
        crate::fakehost::alias_now(&self.server)
    }
}
