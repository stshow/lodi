//! A scratch root with a **fake machine** behind it: `tests/support/fakepm.py` answers as pacman
//! or apt would, from a package graph this file writes, and changes that graph when the product
//! installs, removes or marks something (LD-375).
//!
//! The product is the real binary, unchanged, run as a child with `--root` and with the fake's
//! shims first on its `PATH` — the same seam the recorded suites use (design call D3, LD-115).
//! No real package manager runs, and no host command here reaches `/` (`AGENTS.md` §8): the one
//! place a host verb is assembled appends this case's scratch root as `--root`.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use serde_json::{Value, json};

use crate::hostroot::Root;

/// The real binary on a pseudo-terminal, as every host command here runs.
#[allow(clippy::duplicate_mod)]
#[path = "terminal.rs"]
mod fhterminal;

/// The elevators lodi knows, in its order: each a stub here (#709).
const ELEVATORS: [&str; 3] = ["sudo", "doas", "run0"];

/// The 2.0 command that keeps a 1.x host verb's behaviour (#699, story 23): `plan` is
/// `switch --host --dry-run`, `apply` is `switch --host`, `import` is `import --yes`, and
/// `versions`, `pin` and `unpin` are `pin` and `unpin`, and `switch` is both parts. `extra`
/// follows, but for a switch's `--no-home`.
pub fn command_line(verb: &str, extra: &[&str]) -> Vec<String> {
    let head: &[&str] = match verb {
        "plan" => &["switch", "--host", "--dry-run"],
        "apply" => &["switch", "--host"],
        "import" => &["import", "--yes"],
        "versions" | "pin" => &["pin"],
        "unpin" => &["unpin"],
        // Both parts, host then home: what 1.x's combined `apply` of a host with a home did.
        "switch" => &["switch"],
        other => panic!("no 2.0 command keeps the host verb {other}"),
    };
    // `switch --host` runs no home part, which is what 1.x's `--no-home` asked for.
    let switch = head[0] == "switch";
    head.iter()
        .copied()
        .chain(
            extra
                .iter()
                .copied()
                .filter(|arg| !(switch && *arg == "--no-home")),
        )
        .map(str::to_string)
        .collect()
}

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
    // Never run by lodi (LD-491): a shim that logs its call is how a test proves it.
    "grub-reboot",
    "grub2-reboot",
    // Never run by lodi (LD-432): a shim that logs its call is how a test proves it.
    "setenforce",
    "restorecon",
    "chcon",
];

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
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host/pin");
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

/// Serve the newer recorded day as the dated archive of the last ended UTC day and of the next
/// one on `server`: the instants a command given no date takes, whichever day it runs in
/// (LD-444).
pub fn alias_now(server: &crate::support::Server) -> Vec<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let day = lodi::arch::base::latest_published_day(now);
    let instants: Vec<String> = [day, day + 86_400]
        .iter()
        .map(|at| lodi::util::format_utc(*at))
        .collect();
    let mut files = server.files.lock().unwrap();
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
    /// The offline dated archive every fetch goes to: the recorded days of
    /// `tests/fixtures/host/pin/`, the newer one served as today's too ([`alias_now`]).
    pub archive_server: crate::support::Server,
}

impl Drop for Case {
    fn drop(&mut self) {
        crate::hostroot::remove(&self.base);
    }
}

impl Case {
    pub fn new(name: &str, machine: Machine) -> Case {
        let root = Root::new(name);
        root.may_manage();
        let (uid, gid) = crate::hostroot::ids(&root);
        root.write("etc/hostname", "box\n");
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
            // Recording stub elevators that run the rest unprivileged with only `PATH`, as an
            // elevator with a secure path does (#709), and the offline archive's rewrite, so
            // that the root run fetches nothing from the network either.
            let elevators = base.join("elevators");
            fs::create_dir_all(&elevators).expect("the elevator directory");
            let env = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|dir| dir.join("env"))
                .find(|file| file.is_file())
                .expect("env on PATH");
            for name in ELEVATORS {
                let body = format!(
                    "#!/bin/sh\nprintf '%s %s\\n' {name} \"$*\" >> {calls}\n\
                     [ \"$1\" = -- ] && shift\nexec {env} -i PATH=\"$PATH\" \
                     LODI_FETCH_REWRITE=\"$LODI_FETCH_REWRITE\" \
                     LODI_FETCH_ATTEMPTS=\"$LODI_FETCH_ATTEMPTS\" \"$@\"\n",
                    calls = quote(&base.join("elevator-calls")),
                    env = quote(&env),
                );
                let file = elevators.join(name);
                fs::write(&file, body).expect("a stub elevator");
                fs::set_permissions(&file, fs::Permissions::from_mode(0o755))
                    .expect("a stub elevator's mode");
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
            archive_server: {
                let server = crate::support::Server::start(archive_files());
                alias_now(&server);
                server
            },
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

    /// Run one host command against this root, with the log emptied first: a 1.x host verb's
    /// name, run as the 2.0 command that keeps its behaviour ([`command_line`]).
    // check-host-safety: refusal — one place, and it appends this scratch root as --root.
    pub fn verb(&self, verb: &str, extra: &[&str]) -> Output {
        self.verb_env(verb, extra, &[])
    }

    /// [`Case::verb`] with more of the child's environment: `LODI_FETCH_REWRITE` pointing at a
    /// loopback archive (M-Pin). Without one, every fetch goes to a closed loopback port.
    // check-host-safety: refusal — one place, and it appends this scratch root as --root.
    pub fn verb_env(&self, verb: &str, extra: &[&str], envs: &[(&str, &str)]) -> Output {
        let _ = fs::remove_file(self.state().join("log"));
        let _ = fs::remove_file(self.state().join("update-saw"));
        let _ = fs::remove_file(self.state().join("private-saw"));
        let command = self.command(verb, extra, envs);
        let _spawning = spawning();
        // On a terminal, as a person runs it: a host part with changes asks the first stub
        // elevator, which runs lodi's root entry unprivileged, as the fake machine allows.
        // The command is dropped inside, so that no copy of the terminal outlives the child.
        let (mut output, shown) = fhterminal::on_terminal("", move |stdin, stderr| {
            let mut command = command;
            command
                .stdin(stdin)
                .stderr(stderr)
                .output()
                .expect("lodi runs")
        });
        output.stderr = String::from_utf8_lossy(&shown)
            .replace("\r\n", "\n")
            .into_bytes();
        output
    }

    /// The 2.0 command [`Case::verb_env`] runs, with this case's environment, the may-manage
    /// marker written and standard output piped; standard input and error are the caller's, so a
    /// test that holds a run part way (a kill inside the transaction) spawns it on its own
    /// terminal. Hold [`spawning`] across the spawn.
    // check-host-safety: refusal — one place, and it appends this scratch root as --root.
    pub fn command(&self, verb: &str, extra: &[&str], envs: &[(&str, &str)]) -> Command {
        // The owner agreed once: the "may manage" marker `lodi import` leaves (#705).
        if !self.root.exists("etc/lodi/may-manage") {
            self.root
                .write("etc/lodi/may-manage", "")
                .chmod("etc/lodi/may-manage", 0o644);
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_lodi"));
        command
            .args(command_line(verb, extra))
            .arg("--root")
            .arg(&self.root.dir)
            .current_dir(&self.base)
            .env("PATH", self.elevating_path())
            .env("HOME", self.root.path("home"))
            .env("TERM", "dumb")
            .env("XDG_CONFIG_HOME", self.root.path("home/config"))
            .env("XDG_DATA_HOME", self.root.path("home/data"))
            .env_remove("XDG_STATE_HOME")
            .env_remove("LODI_REPO")
            .env_remove("SUDO_UID")
            .env_remove("SUDO_GID")
            .env_remove("DOAS_USER")
            .env("LODI_HOME", self.root.path("lodi-home"))
            .env("LODI_HOST_REQUIRE_ROOT", "1")
            .env("LODI_FETCH_REWRITE", self.archive_server.rewrite())
            .env("LODI_FETCH_ATTEMPTS", "1")
            .env("GIT_CEILING_DIRECTORIES", &self.root.dir)
            .envs(envs.iter().copied())
            .stdout(Stdio::piped());
        command
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

    /// `lodi switch --host --dry-run` with more arguments before the `--root` [`Case::verb`]
    /// adds.
    pub fn plan_with(&self, extra: &[&str]) -> Output {
        self.verb("plan", extra)
    }

    /// The `PATH` a host command of this case runs with: the stub elevators, then the fake's
    /// shims, then the test's own.
    pub fn elevating_path(&self) -> String {
        format!(
            "{}:{}",
            self.base.join("elevators").display(),
            self.fake_path()
        )
    }

    /// Every stub elevator call, one line each: the elevator's name, then its arguments.
    pub fn elevator_calls(&self) -> Vec<String> {
        fs::read_to_string(self.base.join("elevator-calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The fake's shims, then the test's own `PATH`.
    pub fn fake_path(&self) -> String {
        format!(
            "{}:{}",
            self.shims().display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    /// `lodi import`, then its `[host] snapshot` line taken out ([`without_snapshot`]): the
    /// fake machine offers its own packages, not the recorded dated archive's, so the cases of
    /// this harness switch the unpinned file; a dated archive is `tests/host_pin*.rs`'s.
    pub fn import(&self) -> Output {
        let output = self.verb("import", &[]);
        if output.status.success() {
            self.set_manifest(&without_snapshot(&self.manifest()));
        }
        output
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

    /// The config `lodi switch` finds: `~/.config/lodi` of the scratch `HOME`.
    pub fn config(&self) -> PathBuf {
        self.root.path("home/.config/lodi")
    }

    /// A file beside the manifest, where its relative paths (`files/...`) point: in the config
    /// folder.
    pub fn beside(&self, rel: &str) -> PathBuf {
        self.config().join(rel)
    }

    /// Write `bytes` to [`Case::beside`]`(rel)`, its folders made `0755` and the file `0644`.
    pub fn write_beside(&self, rel: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let file = self.beside(rel);
        let top = self.config();
        let mut dir = file.parent().expect("a parent").to_path_buf();
        fs::create_dir_all(&dir).expect("the folders beside the manifest");
        while dir.starts_with(&top) {
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("a folder mode");
            if !dir.pop() {
                break;
            }
        }
        fs::write(&file, bytes).expect("a file beside the manifest");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("its mode");
        file
    }

    pub fn manifest(&self) -> String {
        fs::read_to_string(self.config().join("host.toml")).unwrap_or_default()
    }

    pub fn set_manifest(&self, text: &str) {
        let config = self.config();
        fs::create_dir_all(&config).expect("the config folder");
        fs::set_permissions(&config, fs::Permissions::from_mode(0o755)).expect("its mode");
        let file = config.join("host.toml");
        fs::write(&file, text).expect("host.toml");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("its mode");
    }

    /// Record `pins` as `lodi pin` does (#696): the section of the config's `lodi.lock` for its
    /// `host.toml`. The bytes written are returned.
    pub fn set_pins(&self, pins: &lodi::hostscope::pin::PinsLock) -> String {
        let mut lock = lodi::config::lock::Lock::default();
        lock.hosts.insert(
            "host.toml".to_string(),
            lodi::config::lock::HostSection {
                distro: pins.distro.clone(),
                release: pins.release.clone(),
                arch: pins.arch.clone(),
                snapshot: pins.snapshot.clone(),
                pins: pins.pins.clone(),
                keys: BTreeMap::new(),
            },
        );
        let text = lock.render();
        let config = self.config();
        fs::create_dir_all(&config).expect("the config folder");
        fs::write(config.join("lodi.lock"), &text).expect("lodi.lock");
        text
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

/// Whether a `lodi switch` had nothing to do: what it says first on standard error, after the
/// `source` line of a config fetched from a URL (LD-528).
pub fn nothing(output: &Output) -> bool {
    let text = err(output);
    let mut lines = text.lines().skip_while(|line| line.starts_with("source "));
    output.status.success()
        && lines
            .next()
            .is_some_and(|l| l.starts_with("nothing to switch"))
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
