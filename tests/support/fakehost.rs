//! A scratch root with a **fake machine** behind it: `tests/support/fakepm.py` answers as pacman
//! or apt would, from a package graph this file writes, and changes that graph when the product
//! installs, removes or marks something (LD-375).
//!
//! The product is the real binary, unchanged, run as a child with `--root` and with the fake's
//! shims first on its `PATH` — the same seam the recorded suites use (design call D3, LD-115).
//! No real package manager runs, and no host command here reaches `/` (`AGENTS.md` §8): the one
//! place a host verb is assembled appends this case's scratch root as `--root`.

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use serde_json::{Value, json};

use crate::hostroot::Root;

/// Held for writing while a case writes its shims (`Case::new`) and for reading around every
/// spawn that might exec one, in every test binary this file is compiled into (#456). `fs::write`
/// opens a shim for writing, writes it and closes it; a child forked by another thread in that
/// window — for any spawn at all, not only one of this case's own — inherits the open write
/// descriptor until it execs, and running any shim in that instant fails with `Os { code: 26,
/// kind: ExecutableFileBusy }` ("Text file busy"). Writing to a temporary name and renaming into
/// place narrows the window but does not close it: a fork in the middle of that write can still
/// inherit an fd to the temporary file, which is the same inode once renamed, so the lock stays
/// the fix (`tests/host_import_cli.rs` proved the same pattern first). One static per test binary
/// is enough: `fork()` only duplicates its own process's descriptor table.
static SHIMS: RwLock<()> = RwLock::new(());

/// Take before spawning anything that might exec a fakehost shim. [`Case::verb`], [`Case::verb_env`]
/// and [`Case::verb_by`] already take it; a test that builds its own [`Command`] against
/// [`Case::base`] or [`Case::fake_path`] must hold the returned guard across that call's
/// `output()`/`spawn()`.
pub fn spawning() -> RwLockReadGuard<'static, ()> {
    SHIMS.read().unwrap_or_else(|e| e.into_inner())
}

/// Take before writing an executable a spawned child might run: [`Case::new`] already takes it
/// for the shims it writes itself; a test that writes another one of its own (an extra shim
/// beside a case's, say) must hold the returned guard across that write.
pub fn writing() -> RwLockWriteGuard<'static, ()> {
    SHIMS.write().unwrap_or_else(|e| e.into_inner())
}

/// The programs the fake answers for. `dpkg` answers `--audit` with a clean database.
const PROGRAMS: &[&str] = &[
    "pacman",
    "pacman-conf",
    "pacman-key",
    "bsdtar",
    "apt-get",
    "apt-cache",
    "apt-mark",
    "dpkg-query",
    "dpkg",
    "dnf5",
    "rpm",
    "systemctl",
    // The shadow tools of `[users]` and `[groups]` (su-1), keeping the root's own account files.
    "useradd",
    "groupadd",
    "usermod",
    "gpasswd",
    // The OS basics (sd-1): each needs the `--root=` lodi passes under a scratch root.
    "hostnamectl",
    "timedatectl",
    "localectl",
    "nft",
    "netplan",
    "networkctl",
    "nmcli",
    "ping",
    "systemd-run",
    // The kernel's settings (bc-1): each needs the `--root=` lodi passes under a scratch root.
    "sysctl",
    "modprobe",
    "grub-mkconfig",
    "grub2-mkconfig",
    "grubby",
    // The bootloader's tools (bl-1), keeping the root's firmware variables and ESP.
    "efibootmgr",
    "bootctl",
    "kernel-install",
    "grub-install",
    // The trial boot (bv-1): the one-shot entry, under the same `--root=`.
    "grub-reboot",
    "grub2-reboot",
    // Never run by lodi (LD-432): a shim that logs its call is how a test proves it.
    "setenforce",
    "restorecon",
    "chcon",
];

/// Which family the fake machine is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Pacman,
    Apt,
}

/// One package record, as the fake keeps it.
#[derive(Debug, Clone)]
pub struct Pkg {
    pub name: String,
    pub version: String,
    pub explicit: bool,
    pub depends: Vec<String>,
    pub recommends: Vec<String>,
    /// What installing it takes off the machine: apt's `Conflicts`, which the fake apt resolves
    /// by removing the conflicting package in the same transaction.
    pub conflicts: Vec<String>,
    pub provides: Vec<String>,
    pub priority: String,
    pub section: String,
    /// The repository that offers it, or `None` for a package no repository has (local-only,
    /// foreign, AUR).
    pub repo: Option<String>,
    /// The configuration files it tracks, each reported as changed from what it shipped.
    pub conffiles: Vec<String>,
}

impl Pkg {
    pub fn new(name: &str) -> Pkg {
        Pkg {
            name: name.to_string(),
            version: "1.0-1".to_string(),
            explicit: true,
            depends: Vec::new(),
            recommends: Vec::new(),
            conflicts: Vec::new(),
            provides: Vec::new(),
            priority: "optional".to_string(),
            section: "utils".to_string(),
            repo: Some("main".to_string()),
            conffiles: Vec::new(),
        }
    }

    /// A configuration file this package tracks, which the machine has changed since the
    /// package shipped it: what an import captures (LD-377).
    pub fn conffile(mut self, path: &str) -> Pkg {
        self.conffiles.push(path.to_string());
        self
    }

    pub fn dep(mut self) -> Pkg {
        self.explicit = false;
        self
    }

    pub fn depends(mut self, names: &[&str]) -> Pkg {
        self.depends = names.iter().map(ToString::to_string).collect();
        self
    }

    pub fn recommends(mut self, names: &[&str]) -> Pkg {
        self.recommends = names.iter().map(ToString::to_string).collect();
        self
    }

    pub fn conflicts(mut self, names: &[&str]) -> Pkg {
        self.conflicts = names.iter().map(ToString::to_string).collect();
        self
    }

    pub fn provides(mut self, names: &[&str]) -> Pkg {
        self.provides = names.iter().map(ToString::to_string).collect();
        self
    }

    pub fn priority(mut self, priority: &str) -> Pkg {
        self.priority = priority.to_string();
        self
    }

    pub fn section(mut self, section: &str) -> Pkg {
        self.section = section.to_string();
        self
    }

    pub fn repo(mut self, repo: Option<&str>) -> Pkg {
        self.repo = repo.map(ToString::to_string);
        self
    }

    fn installed(&self) -> Value {
        json!({
            "version": self.version,
            "explicit": self.explicit,
            "depends": self.depends,
            "recommends": self.recommends,
            "conflicts": self.conflicts,
            "provides": self.provides,
            "priority": self.priority,
            "section": self.section,
            // dpkg's digest of the shipped bytes: never the digest of a file a test writes, so
            // every conffile reads as changed.
            "conffiles": self.conffiles.iter()
                .map(|path| (path.clone(), json!("0".repeat(32))))
                .collect::<serde_json::Map<_, _>>(),
        })
    }

    fn offered(&self, family: Family, dnf: bool) -> Value {
        let repo = self.repo.clone().unwrap_or_default();
        let repo = match (family, repo.as_str()) {
            // dnf calls Fedora's own repository `fedora`.
            (_, "main") if dnf => "fedora".to_string(),
            // pacman calls the distribution's own repository by its own name.
            (Family::Pacman, "main") => "extra".to_string(),
            _ => repo,
        };
        json!({
            "version": self.version,
            "depends": self.depends,
            "recommends": self.recommends,
            "conflicts": self.conflicts,
            "provides": self.provides,
            "priority": self.priority,
            "section": self.section,
            "repo": repo,
        })
    }
}

/// A machine: what is installed, and what the repositories offer beyond it.
pub struct Machine {
    pub family: Family,
    pub installed: Vec<Pkg>,
    pub offered: Vec<Pkg>,
    /// What the repositories offer that the machine's index does not know until `apt-get update`
    /// fetches it.
    pub unfetched: Vec<Pkg>,
    /// A Fedora machine: dnf5 and rpm answer, whose removals are a second action like pacman's,
    /// which is the family it is filed under here.
    pub dnf: bool,
    /// The systemd units `systemctl` answers for, by name (sc-1).
    pub units: serde_json::Map<String, Value>,
    /// What the OS basics' tools answer from (sd-1): `locale`, `keymap`, `locales`, `nft`, `cut`.
    pub os: serde_json::Map<String, Value>,
}

impl Machine {
    /// An Arch machine with `base` and what `base` needs, and a kernel.
    pub fn arch() -> Machine {
        Machine {
            family: Family::Pacman,
            installed: vec![
                Pkg::new("base").depends(&["glibc", "bash"]),
                Pkg::new("glibc").dep(),
                Pkg::new("bash").dep().depends(&["glibc"]),
                Pkg::new("linux"),
            ],
            offered: Vec::new(),
            unfetched: Vec::new(),
            dnf: false,
            units: serde_json::Map::new(),
            os: serde_json::Map::new(),
        }
    }

    /// A Debian 12 machine: the installer's required packages, and a kernel metapackage.
    pub fn debian() -> Machine {
        Machine {
            family: Family::Apt,
            installed: vec![
                Pkg::new("libc6").priority("required"),
                Pkg::new("bash").priority("required").depends(&["libc6"]),
                Pkg::new("linux-image-amd64")
                    .section("kernel")
                    .depends(&["linux-image-6.1.0-25-amd64"]),
                Pkg::new("linux-image-6.1.0-25-amd64")
                    .dep()
                    .section("kernel"),
            ],
            offered: Vec::new(),
            unfetched: Vec::new(),
            dnf: false,
            units: serde_json::Map::new(),
            os: serde_json::Map::new(),
        }
    }

    /// A Fedora 44 machine: glibc and bash as the image installs them, a kernel, and dnf5,
    /// which `/etc/dnf/protected.d` names.
    pub fn fedora() -> Machine {
        let pkg = |name: &str| {
            let mut pkg = Pkg::new(name);
            pkg.version = "0:1.0-1.fc44".to_string();
            pkg
        };
        Machine {
            family: Family::Pacman,
            installed: vec![
                pkg("glibc").dep(),
                pkg("bash").depends(&["glibc"]),
                pkg("kernel").depends(&["kernel-core"]),
                pkg("kernel-core").dep(),
                pkg("dnf5").depends(&["glibc"]),
            ],
            offered: Vec::new(),
            unfetched: Vec::new(),
            dnf: true,
            units: serde_json::Map::new(),
            os: serde_json::Map::new(),
        }
    }

    pub fn with(mut self, pkg: Pkg) -> Machine {
        self.installed.retain(|p| p.name != pkg.name);
        self.installed.push(pkg);
        self
    }

    /// A package the repositories offer and the machine does not have.
    pub fn offering(mut self, pkg: Pkg) -> Machine {
        self.offered.push(pkg);
        self
    }

    /// A package the repositories offer that this machine's index does not know yet: the fake
    /// apt learns it when `apt-get update` runs, as a machine whose index was never fetched does.
    pub fn offering_once_fetched(mut self, pkg: Pkg) -> Machine {
        self.unfetched.push(pkg);
        self
    }

    /// A systemd unit: `enabled` or `disabled` (or `static`, `masked`), running or not, and where
    /// `systemctl preset` puts it.
    pub fn unit(mut self, name: &str, state: &str, active: bool, preset: &str) -> Machine {
        self.units.insert(
            name.to_string(),
            json!({ "state": state, "active": active, "preset": preset }),
        );
        self
    }

    /// One value of what the OS basics' tools answer from (sd-1).
    pub fn os(mut self, key: &str, value: Value) -> Machine {
        self.os.insert(key.to_string(), value);
        self
    }

    /// A unit that exists only while `package` is installed.
    pub fn unit_of(mut self, name: &str, package: &str) -> Machine {
        self.units.insert(
            name.to_string(),
            json!({ "state": "disabled", "active": false, "preset": "enabled", "package": package }),
        );
        self
    }

    fn to_json(&self) -> Value {
        let mut installed = serde_json::Map::new();
        let mut available = serde_json::Map::new();
        for pkg in &self.installed {
            installed.insert(pkg.name.clone(), pkg.installed());
            if pkg.repo.is_some() {
                available.insert(pkg.name.clone(), pkg.offered(self.family, self.dnf));
            }
        }
        for pkg in &self.offered {
            available.insert(pkg.name.clone(), pkg.offered(self.family, self.dnf));
        }
        let mut machine = json!({
            "installed": installed,
            "available": available,
            "repositories": ["core", "extra"],
            "sources": {
                "main": {
                    "key": "http://deb.debian.org/debian bookworm/main amd64 Packages",
                    "release": "v=12,o=Debian,a=stable,n=bookworm,l=Debian,c=main,b=amd64",
                },
                "vendor": {
                    "key": "http://vendor.example bookworm/stable amd64 Packages",
                    "release": "o=Vendor,a=bookworm,n=bookworm,l=Vendor,c=stable,b=amd64",
                },
            },
        });
        if !self.units.is_empty() {
            machine["units"] = Value::Object(self.units.clone());
        }
        if !self.os.is_empty() {
            machine["os"] = Value::Object(self.os.clone());
        }
        if !self.unfetched.is_empty() {
            let fetch: serde_json::Map<String, Value> = self
                .unfetched
                .iter()
                .map(|pkg| (pkg.name.clone(), pkg.offered(self.family, self.dnf)))
                .collect();
            machine["on"] = json!({ "fetch": fetch });
        }
        machine
    }
}

/// One scratch root, one fake machine, one shim directory.
pub struct Case {
    pub root: Root,
    pub family: Family,
    dnf: bool,
    base: PathBuf,
}

impl Drop for Case {
    fn drop(&mut self) {
        crate::hostroot::remove(&self.base);
    }
}

impl Case {
    pub fn new(name: &str, machine: Machine) -> Case {
        let root = Root::new(name);
        root.arm();
        let (uid, gid) = crate::hostroot::ids(&root);
        root.write(
            "etc/passwd",
            &format!("root:x:0:0::/root:/bin/sh\nsample:x:{uid}:{gid}::/people/sample:/bin/sh\n"),
        );
        match machine.family {
            _ if machine.dnf => {
                root.write(
                    "etc/os-release",
                    "NAME=\"Fedora Linux\"\nID=fedora\nVERSION_ID=44\nVERSION_CODENAME=\"\"\n",
                );
                root.write("etc/dnf/protected.d/dnf.conf", "dnf5\n");
                root.write(
                    "var/cache/libdnf5/fedora-0/repodata/repomd.xml",
                    "metadata\n",
                );
            }
            Family::Pacman => {
                root.write(
                    "etc/os-release",
                    "NAME=\"Arch Linux\"\nID=arch\nBUILD_ID=rolling\n",
                );
                root.write("var/lib/pacman/sync/extra.db", "a database\n");
            }
            Family::Apt => {
                root.debian();
                root.write("var/lib/apt/lists/fake_Packages", "an index\n");
            }
        }
        let base = crate::support::scratch(&format!("fakepm-{name}"));
        let shims = base.join("bin");
        let state = base.join("state");
        fs::create_dir_all(&shims).expect("the shim directory");
        fs::create_dir_all(&state).expect("the state directory");
        let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
        let path = std::env::var("PATH")
            .unwrap_or_default()
            .replace('\'', "'\\''");
        {
            let _writing = SHIMS.write().unwrap_or_else(|e| e.into_inner());
            fs::write(
                shims.join("shim-env"),
                format!(
                    "PATH='{path}'\nexport PATH\nLODI_FAKEPM_STATE={}\nexport LODI_FAKEPM_STATE\n",
                    quote(&state)
                ),
            )
            .expect("the shim environment");
            let fake = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/fakepm.py");
            for program in PROGRAMS {
                let script = format!(
                    "#!/bin/sh\n# A test-owned shim: the fake machine answers (tests/support/fakepm.py).\n\
                     . \"${{0%/*}}/shim-env\"\nexec python3 -B {} {program} \"$@\"\n",
                    quote(&fake)
                );
                let file = shims.join(program);
                fs::write(&file, script).expect("a shim");
                fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).expect("a shim mode");
            }
        }
        for directory in ["home", "home/config", "home/data", "lodi-home"] {
            fs::create_dir_all(root.path(directory)).expect("scratch environment roots");
        }
        let case = Case {
            root,
            family: machine.family,
            dnf: machine.dnf,
            base,
        };
        case.set_machine(&machine.to_json());
        case
    }

    fn shims(&self) -> PathBuf {
        self.base.join("bin")
    }

    /// The directory the fake's shims and state live in, which a transcript names `<fake>`.
    pub fn base(&self) -> &Path {
        &self.base
    }

    fn state(&self) -> PathBuf {
        self.base.join("state")
    }

    pub fn machine(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.state().join("machine.json")).unwrap())
            .expect("the machine parses")
    }

    pub fn set_machine(&self, machine: &Value) {
        fs::write(
            self.state().join("machine.json"),
            serde_json::to_string_pretty(machine).unwrap(),
        )
        .expect("the machine");
    }

    /// Change the fake machine by hand, as its administrator would.
    pub fn edit(&self, change: impl FnOnce(&mut Value)) {
        let mut machine = self.machine();
        change(&mut machine);
        self.set_machine(&machine);
    }

    /// Install a package by hand, outside lodi: explicitly wanted, from the repositories.
    pub fn install_by_hand(&self, pkg: Pkg) {
        let (family, dnf) = (self.family, self.dnf);
        self.edit(|m| {
            m["installed"][&pkg.name] = pkg.installed();
            if pkg.repo.is_some() {
                m["available"][&pkg.name] = pkg.offered(family, dnf);
            }
        });
    }

    /// The installed set, by name.
    pub fn installed(&self) -> BTreeSet<String> {
        self.machine()["installed"]
            .as_object()
            .expect("an installed set")
            .keys()
            .cloned()
            .collect()
    }

    /// A unit's `(state, active)`, as the fake `systemctl` holds it.
    pub fn unit(&self, name: &str) -> (String, bool) {
        let unit = &self.machine()["units"][name];
        (
            unit["state"].as_str().expect("a unit state").to_string(),
            unit["active"] == Value::Bool(true),
        )
    }

    pub fn is_explicit(&self, name: &str) -> bool {
        self.machine()["installed"][name]["explicit"] == Value::Bool(true)
    }

    /// Run one host command against this root, with the log emptied first.
    // check-host-safety: refusal — one place, and it appends this scratch root as --root.
    pub fn verb(&self, verb: &str, extra: &[&str]) -> Output {
        self.verb_by(
            std::ffi::OsStr::new(env!("CARGO_BIN_EXE_lodi")),
            verb,
            extra,
        )
    }

    /// [`Case::verb`] with more of the child's environment: `LODI_FETCH_REWRITE` pointing at a
    /// loopback archive (M-Pin).
    // check-host-safety: refusal — one place, and it appends this scratch root as --root.
    pub fn verb_env(&self, verb: &str, extra: &[&str], envs: &[(&str, &str)]) -> Output {
        let _ = fs::remove_file(self.state().join("log"));
        let _ = fs::remove_file(self.state().join("update-saw"));
        let _ = fs::remove_file(self.state().join("private-saw"));
        let _spawning = spawning();
        Command::new(env!("CARGO_BIN_EXE_lodi"))
            .arg("host")
            .arg(verb)
            .args(extra)
            .arg("--root")
            .arg(&self.root.dir)
            .env("PATH", self.fake_path())
            .env("HOME", self.root.path("home"))
            .env("XDG_CONFIG_HOME", self.root.path("home/config"))
            .env("XDG_DATA_HOME", self.root.path("home/data"))
            .env("LODI_HOME", self.root.path("lodi-home"))
            .env("LODI_HOST_REQUIRE_ROOT", "1")
            .envs(envs.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("lodi runs")
    }

    /// Every private source set the last command's `apt-get update` read, in order (M-Pin).
    pub fn private_saw(&self) -> Vec<String> {
        fs::read_to_string(self.state().join("private-saw"))
            .unwrap_or_default()
            .split('\u{1e}')
            .filter(|set| !set.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Serve the dated archive at `uri` from the recorded fixture directory `dir` to the fake's
    /// private update (M-Pin).
    pub fn archive(&self, uri: &str, dir: &Path) {
        let dir = dir.display().to_string();
        self.edit(|m| m["archives"][uri] = json!(dir));
    }

    /// [`Case::verb`] with another `lodi` binary: the build a compatibility golden was recorded
    /// from (`tests/host_sources.rs`, LD-365).
    // check-host-safety: refusal — one place, and it appends this scratch root as --root.
    pub fn verb_by(&self, binary: &std::ffi::OsStr, verb: &str, extra: &[&str]) -> Output {
        let _ = fs::remove_file(self.state().join("log"));
        let _ = fs::remove_file(self.state().join("update-saw"));
        let path = self.fake_path();
        let _spawning = spawning();
        Command::new(binary)
            .arg("host")
            .arg(verb)
            .args(extra)
            .arg("--root")
            .arg(&self.root.dir)
            .env("PATH", path)
            .env("HOME", self.root.path("home"))
            .env("XDG_CONFIG_HOME", self.root.path("home/config"))
            .env("XDG_DATA_HOME", self.root.path("home/data"))
            .env("LODI_HOME", self.root.path("lodi-home"))
            .env("LODI_HOST_REQUIRE_ROOT", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("lodi runs")
    }

    /// `lodi boot confirm` against this root (bv-1), with the log emptied first.
    // check-host-safety: refusal — one place, and it appends this scratch root as --root.
    pub fn boot_confirm(&self) -> Output {
        let _ = fs::remove_file(self.state().join("log"));
        let _spawning = spawning();
        Command::new(env!("CARGO_BIN_EXE_lodi"))
            .args(["boot", "confirm", "--root"])
            .arg(&self.root.dir)
            .env("PATH", self.fake_path())
            .env("HOME", self.root.path("home"))
            .env("LODI_HOST_REQUIRE_ROOT", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("lodi runs")
    }

    /// `lodi host plan` with more arguments before the `--root` [`Case::verb`] adds.
    pub fn plan_with(&self, extra: &[&str]) -> Output {
        self.verb("plan", extra)
    }

    /// The `PATH` a host command of this case runs with: the fake's shims first.
    pub fn fake_path(&self) -> String {
        format!(
            "{}:{}",
            self.shims().display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    pub fn import(&self) -> Output {
        self.verb("import", &[])
    }

    pub fn plan(&self) -> Output {
        self.verb("plan", &[])
    }

    pub fn apply(&self, extra: &[&str]) -> Output {
        self.verb("apply", extra)
    }

    /// Every argv the fake saw in the last command, one line each.
    pub fn log(&self) -> Vec<String> {
        let bytes = fs::read(self.state().join("log")).unwrap_or_default();
        String::from_utf8_lossy(&bytes)
            .split('\u{1e}')
            .filter(|record| !record.is_empty())
            .map(|record| record.replace('\u{1f}', " "))
            .collect()
    }

    pub fn manifest(&self) -> String {
        self.root.read("etc/lodi/host.toml")
    }

    pub fn set_manifest(&self, text: &str) {
        self.root.write("etc/lodi/host.toml", text);
    }

    /// Take one package's line out of the manifest, as the owner did: the name as a quoted
    /// list element, wherever it stands.
    pub fn delete_line(&self, name: &str) {
        let before = self.manifest();
        let quoted = format!("\"{name}\"");
        let after: String = before
            .lines()
            .filter(|line| {
                let trimmed = line.trim().trim_end_matches(',');
                trimmed != quoted
            })
            .map(|line| format!("{line}\n"))
            .collect();
        assert_ne!(
            before, after,
            "the manifest has no line for {name}:\n{before}"
        );
        self.set_manifest(&after);
    }

    /// What every `apt-get update` of the last command found armed, one line per stanza
    /// (`tests/support/fakepm.py`, `saw`).
    pub fn update_saw(&self) -> Vec<String> {
        fs::read_to_string(self.state().join("update-saw"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub fn lock(&self) -> Value {
        serde_json::from_str(&self.root.read("etc/lodi/host.lock")).expect("the lock parses")
    }

    pub fn has_lock(&self) -> bool {
        self.root.exists("etc/lodi/host.lock")
    }

    pub fn recorded(&self) -> BTreeSet<String> {
        self.lock()["packages"]
            .as_object()
            .map(|packages| packages.keys().cloned().collect())
            .unwrap_or_default()
    }
}

/// A host manifest with its `[host] snapshot` line taken out: the unpinned form of an imported
/// file, which plans and applies as 1.3 did whatever its snapshot said (M-Pin P3, LD-395). Since
/// M-Pin that line is live on apt (D-A), so a walk that holds a build to an earlier release's
/// transcripts walks the file without it.
pub fn without_snapshot(text: &str) -> String {
    text.lines()
        .filter(|line| !line.starts_with("snapshot = "))
        .map(|line| format!("{line}\n"))
        .collect()
}

pub fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn err(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A command's whole story, for an assertion message.
pub fn story(output: &Output) -> String {
    format!(
        "status {:?}\n--- stdout\n{}--- stderr\n{}",
        output.status.code(),
        out(output),
        err(output)
    )
}
