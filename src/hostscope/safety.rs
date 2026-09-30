//! The safety gate: the first thing either host entry point calls, before the manifest is read.
//!
//! `AGENTS.md` §8 (LD-45) forbids running host-scope code against the machine an agent works on,
//! "not even a dry run". This module is what makes that structural rather than remembered
//! (M-0.6 design call D1, LD-113): a root is looked at only when it carries the marker file
//! `etc/lodi/host-allowed`, which only `lodi host arm`, run as root, writes (LD-320, which
//! supersedes forward LD-113's "no subcommand writes it").
//!
//! The order is fixed, and each step is refused before the next is attempted:
//!
//! 1. resolve the root — [`Options::root`] when given, else `/` (design call D2, LD-114);
//!    a root that does not exist is `E_STORE_IO` (exit 6), which says so, rather than an
//!    arming refusal for a directory that is not there ([`missing_root`]);
//! 2. **arming**: the root, `<root>/etc` and `<root>/etc/lodi` must each be a directory, not a
//!    symbolic link, that nobody but their owner can write — owned by root on `/`, by the
//!    invoking user under a `--root` — or the root is refused with `E_PATH_ESCAPE` (exit 3,
//!    LD-357); then `<root>/etc/lodi/host-allowed`, a regular file reached without following a
//!    symlink, owned by uid 0 and neither group- nor world-writable when the root is `/`, owned
//!    by the invoking user under a `--root`; anything else, absence included, is
//!    `E_HOST_NOT_ARMED` (exit 9). The manifest and the host lock in that directory are later
//!    read under the same rule ([`read_config`]);
//! 3. **supported distribution**: `<root>/etc/os-release`'s `ID` (or `ID_LIKE`) must be `debian`,
//!    `ubuntu` or `arch`, or its `ID` alone `fedora` (LD-432: a distribution that is only *like*
//!    Fedora is not Fedora); anything else is `E_UNSUPPORTED` (exit 3) naming the four. A root
//!    booted from an ostree deployment (`<root>/run/ostree-booted`: Fedora Silverblue and every
//!    other rpm-ostree system) is `E_HOST_OSTREE` (exit 3) before anything else is read. The
//!    `[host] distro` assertion is checked against this once the manifest is parsed
//!    ([`OsRelease::assert_distro`], `E_HOST_MISMATCH`, exit 3). Knowing the distribution is
//!    also what lets this step decide whether a flag means anything here:
//!    `--unsupported-partial-upgrade` is Arch's (design call D10), and a distribution whose
//!    package manager has no such mode refuses it with `E_UNSUPPORTED` (exit 3) rather than
//!    accepting a flag it would ignore;
//!    3b. **an import's destination**: an `--out` inside a directory the host scope itself owns
//!    is refused before a byte is read (`E_STORE_IO`, exit 6); the default destination,
//!    `<root>/etc/lodi`, needs euid 0 on `/` (`E_NEED_ROOT`, exit 9) and no symbolic link on the
//!    way (LD-325); and an existing `host.toml` without `--force` is `E_EXISTS` (exit 3). None
//!    is decided in this module — [`super::run_import`] and [`super::landing`] do it — but all
//!    happen before the machine is read, so a refused import reads nothing;
//! 4. **privilege**: an apply on the root `/` needs euid 0 (`E_NEED_ROOT`, exit 9). Lodi invokes
//!    no `sudo` and no `doas`: the operator runs `sudo lodi host apply`;
//! 5. **package manager**: the distribution's tools are resolved by bare name (design call D3,
//!    LD-115) — on the root `/` against the fixed [`FIXED_PATH`] and never the caller's `PATH`,
//!    under a `--root` through the caller's `PATH` (LD-357) — and a program found is run only if
//!    it is owned by root (or, under a `--root`, by the invoking user) and neither group- nor
//!    world-writable ([`untrusted_program`]). Absent or refused is `E_NO_RUNTIME` (exit 7) naming
//!    what it looked for. The resolved absolute paths go into the journal;
//! 6. **one apply at a time**: `<root>/var/lib/lodi/host/.lock` is taken exclusively and is not
//!    waited on — held is `E_SYSTEM_BUSY` (exit 9).

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::diag::Diagnostic;

/// The marker that arms a root, relative to it. Only `lodi host arm` writes it (LD-320).
pub const MARKER: &str = "etc/lodi/host-allowed";
/// The host manifest, relative to the root.
pub const MANIFEST: &str = "etc/lodi/host.toml";
/// The host lock, relative to the root: a record of what was applied, not a pin (LD-117).
pub const LOCK: &str = "etc/lodi/host.lock";
/// The host scope's state directory, relative to the root: journals and the apply lock.
pub const STATE: &str = "var/lib/lodi/host";
/// The distributions this build manages.
pub const SUPPORTED: &[&str] = &["debian", "ubuntu", "arch", "fedora"];
/// The file an ostree deployment's boot leaves, relative to the root (LD-432).
pub const OSTREE_BOOTED: &str = "run/ostree-booted";

/// What the entry point is doing. `plan` and `import` need no privilege and take no lock; only
/// `apply` changes the machine, so only `apply` reaches steps 4 and 6 below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Plan,
    Apply,
    /// `lodi host import` (M-Import T-3): a read of the machine, through the whole of the arming
    /// and supported-distribution gate and none of the rest of it. The read needs no privilege
    /// and takes no apply lock, so an import never blocks or is blocked by an apply; landing
    /// the result in `/etc/lodi` does need root, and [`super::landing`] decides that (LD-325).
    Import,
    /// `lodi host versions NAME` (M-Pin, LD-397): a read of the machine and of the archive,
    /// through the import's gate.
    Versions,
    /// `lodi host pin` and `lodi host unpin` (M-Pin, LD-397): they read the machine as the
    /// import does and write only the host directory, under its own advisory lock; they take no
    /// apply lock and need root only where the host directory is root's.
    Pin,
    Unpin,
    /// `lodi update` (LD-448, LD-416): the pin verbs' gate, writing the repository's root lock.
    Update,
}

impl Operation {
    pub fn command(self) -> &'static str {
        match self {
            Operation::Plan => "lodi host plan",
            Operation::Apply => "lodi host apply",
            Operation::Import => "lodi host import",
            Operation::Versions => "lodi host versions",
            Operation::Pin => "lodi host pin",
            Operation::Unpin => "lodi host unpin",
            Operation::Update => "lodi update",
        }
    }
}

/// The options both entry points take. `root` is the `--root DIR` flag of design call D2: a
/// real, documented flag meaning "operate on this filesystem tree instead of `/`", with the same
/// arming rule applied to it. There is no hidden test-only seam: the one thing a scratch root
/// takes from the environment is the `PATH` its package-manager programs are found on, and the
/// root `/` takes nothing from it ([`search_path`], LD-357).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub root: Option<PathBuf>,
    /// The positional `SOURCE` of `plan`, `apply` and `import` (LD-379): a host directory, or a
    /// directory of hosts one of which is chosen by hostname. `None` is `<root>/etc/lodi`.
    pub source: Option<PathBuf>,
    /// `--host NAME`: the host of a directory of hosts to use instead of the hostname (LD-379).
    pub host: Option<String>,
    /// `--ref NAME` of a URL `SOURCE`: the branch or tag to resolve instead of `HEAD` (LD-401).
    pub git_ref: Option<String>,
    /// `--rev COMMIT` of a URL `SOURCE`: the commit to apply, by its 40 hexadecimal digits.
    pub rev: Option<String>,
    /// `--refresh` of a URL `SOURCE`: resolve the ref again instead of using the locked commit.
    pub refresh: bool,
    /// `--resolved JOURNAL-ID`: the operator declares an ambiguous journal resolved (LD-118).
    pub resolved: Option<String>,
    /// `--overwrite-drift`: the confirmation ADR-013 requires before drift is overwritten
    /// (LD-120).
    pub overwrite_drift: bool,
    /// `--no-update`: do not refresh the distribution's index as part of this apply.
    pub no_update: bool,
    /// `--no-home`: act on the host alone without inspecting its home directory.
    pub no_home: bool,
    /// `--out DIR`: where `lodi host import` writes the manifest and the bundle beside it.
    /// `None` is the import that lands in `<root>/etc/lodi` itself (LD-325).
    pub out: Option<PathBuf>,
    /// `--stdout`: `lodi host import` prints the manifest and captures no configuration file.
    /// It and `--out` are a usage error together.
    pub stdout: bool,
    /// `--force`: replace an existing `host.toml` and the `files/` beside it, as `lodi init
    /// --force` replaces a project manifest. Without it an existing file is reconciled (LD-378).
    pub force: bool,
    /// `--dry-run`: `lodi host import` prints what it would change in an existing manifest — the
    /// unified diff and the captured paths with their digests — and writes nothing (LD-378).
    pub dry_run: bool,
    /// `--unsupported-partial-upgrade`: install on Arch **without** upgrading the machine
    /// (design call D10). Arch does not support that state, which is why the flag says so in its
    /// own name, why it is the only way to reach the mode, and why a distribution with no
    /// partial-upgrade mode refuses it at step 3a below rather than ignoring it.
    pub unsupported_partial_upgrade: bool,
    /// The package `NAME` of `versions`, `pin` and `unpin` (M-Pin, LD-397).
    pub name: Option<String>,
    /// `--to VERSION|DATE` of `pin`: what to pin to; `None` is the installed version, or now.
    pub to: Option<String>,
    /// `--all` of `pin` and `unpin`: the file-level snapshot instead of one name.
    pub all: bool,
}

/// The distributions this build manages, as `spec/01` §5 names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distro {
    Debian,
    Ubuntu,
    Arch,
    Fedora,
}

impl Distro {
    pub fn name(self) -> &'static str {
        match self {
            Distro::Debian => "debian",
            Distro::Ubuntu => "ubuntu",
            Distro::Arch => "arch",
            Distro::Fedora => "fedora",
        }
    }

    pub fn from_id(id: &str) -> Option<Distro> {
        match id {
            "debian" => Some(Distro::Debian),
            "ubuntu" => Some(Distro::Ubuntu),
            "arch" => Some(Distro::Arch),
            "fedora" => Some(Distro::Fedora),
            _ => None,
        }
    }

    /// The programs an apply needs on `PATH`, in the order they are reported.
    pub fn programs(self) -> &'static [&'static str] {
        match self {
            Distro::Debian | Distro::Ubuntu => &["apt-get", "dpkg-query"],
            Distro::Arch => &["pacman"],
            Distro::Fedora => &["dnf5", "rpm"],
        }
    }
}

/// The fields of `/etc/os-release` the host scope uses (`spec/01` §2.2 `host.*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsRelease {
    pub id: String,
    pub id_like: Vec<String>,
    pub version_id: String,
    pub codename: String,
}

impl OsRelease {
    /// Parse the `KEY=value` lines of an `os-release` file; unquoted, single- and double-quoted
    /// values are all accepted, and unknown keys are ignored.
    pub fn parse(text: &str) -> OsRelease {
        let mut os = OsRelease {
            id: String::new(),
            id_like: Vec::new(),
            version_id: String::new(),
            codename: String::new(),
        };
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            match key.trim() {
                "ID" => os.id = value.to_string(),
                "ID_LIKE" => {
                    os.id_like = value.split_whitespace().map(str::to_string).collect();
                }
                "VERSION_ID" => os.version_id = value.to_string(),
                "VERSION_CODENAME" => os.codename = value.to_string(),
                _ => {}
            }
        }
        os
    }

    /// The distribution this build manages, from `ID` and then `ID_LIKE`. Fedora is read from
    /// `ID` alone (LD-432): a distribution that says it is like Fedora is not one lodi manages.
    pub fn distro(&self) -> Option<Distro> {
        Distro::from_id(&self.id).or_else(|| {
            self.id_like
                .iter()
                .find_map(|l| Distro::from_id(l).filter(|d| *d != Distro::Fedora))
        })
    }

    /// `spec/01` §5.1: `[host] distro` is an assertion about the machine, not a selector for
    /// something to install.
    pub fn assert_distro(&self, declared: Option<&str>, file: &str) -> Result<(), Diagnostic> {
        let Some(declared) = declared else {
            return Ok(());
        };
        let actual = self.distro().map_or(self.id.as_str(), |d| d.name());
        if declared == actual || declared == self.id {
            return Ok(());
        }
        Err(Diagnostic::new(
            "E_HOST_MISMATCH",
            format!("{file} asserts `[host] distro = \"{declared}\"`, but this root is `{actual}`"),
        )
        .hint(format!(
            "apply this manifest on a {declared} machine, or change the assertion to `{actual}`"
        )))
    }
}

/// Where the host scope looks for a package-manager program on the root `/`, and the whole of
/// the `PATH` every package-manager process it starts gets (LD-357).
pub const FIXED_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// The directories a program is resolved in. On the running system's `/` it is [`FIXED_PATH`],
/// whatever the environment says, so no variable of the caller's can choose the program root
/// runs. Under a `--root` it is the caller's `PATH` — or [`FIXED_PATH`] when that is unset —
/// which is how a scratch root is driven by test-owned programs; that seam exists only where
/// the root is not the machine (LD-357).
pub fn search_path(system_root: bool, caller: Option<&std::ffi::OsStr>) -> std::ffi::OsString {
    match caller {
        Some(path) if !system_root => path.to_os_string(),
        _ => FIXED_PATH.into(),
    }
}

/// Why a program with this owner and mode is not one the host scope runs, or `None` when it
/// is: owned by root — or, under a `--root`, by root or the invoking user — and neither group-
/// nor world-writable, the rule the arming marker is judged by.
pub fn untrusted_program(owner: u32, mode: u32, system_root: bool, euid: u32) -> Option<String> {
    if owner != 0 && (system_root || owner != euid) {
        let who = if system_root {
            "root".to_string()
        } else {
            format!("root or the invoking user (uid {euid})")
        };
        return Some(format!("it is owned by uid {owner}, not by {who}"));
    }
    if mode & 0o022 != 0 {
        return Some(format!(
            "it is mode {:04o}: group- or world-writable",
            mode & 0o7777
        ));
    }
    None
}

/// Resolve one package-manager program by bare name: the first executable of that name in
/// [`search_path`], judged by [`untrusted_program`]. `Ok(None)` is a program that is not there;
/// `Err` is one that is there and is refused — the search does not go on past it to a later
/// directory.
pub fn resolve_program(
    name: &str,
    system_root: bool,
    euid: u32,
) -> Result<Option<PathBuf>, String> {
    let path = search_path(system_root, std::env::var_os("PATH").as_deref());
    let Some(found) = which(name, &path) else {
        return Ok(None);
    };
    let meta = fs::metadata(&found).map_err(|e| format!("{}: {e}", found.display()))?;
    match untrusted_program(meta.uid(), meta.mode(), system_root, euid) {
        None => Ok(Some(found)),
        Some(why) => Err(format!("{} is refused: {why}", found.display())),
    }
}

/// The package-manager programs of the distribution, resolved to absolute paths by
/// [`resolve_program`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageManager {
    pub distro: Distro,
    /// Program name -> the absolute path `PATH` resolved it to, in [`Distro::programs`] order.
    pub programs: Vec<(String, PathBuf)>,
}

impl PackageManager {
    pub fn path_of(&self, program: &str) -> Option<&Path> {
        self.programs
            .iter()
            .find(|(name, _)| name == program)
            .map(|(_, path)| path.as_path())
    }
}

/// An exclusive `flock` held until drop.
#[derive(Debug)]
pub struct ApplyLock {
    file: fs::File,
}

impl ApplyLock {
    /// Take the lock without waiting. `Ok(None)` means another apply holds it.
    fn try_take(path: &Path) -> io::Result<Option<ApplyLock>> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        loop {
            // SAFETY: flock on a file descriptor this struct owns.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                return Ok(Some(ApplyLock { file }));
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EWOULDBLOCK) || e.kind() == io::ErrorKind::WouldBlock
            {
                return Ok(None);
            }
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

impl Drop for ApplyLock {
    fn drop(&mut self) {
        // SAFETY: unlocking the descriptor this struct owns; closing would unlock as well.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// A root that passed the gate.
#[derive(Debug)]
pub struct Gate {
    pub root: PathBuf,
    /// Whether the root is the running system's own `/`.
    pub system_root: bool,
    pub os: OsRelease,
    pub distro: Distro,
    pub pm: PackageManager,
    pub euid: u32,
    pub egid: u32,
    pub operation: Operation,
    /// `--unsupported-partial-upgrade`, carried to the package backend and nowhere else.
    pub partial_upgrade: bool,
    /// Held for the whole apply; `None` for a plan, which takes no lock.
    pub lock: Option<ApplyLock>,
}

impl Gate {
    pub fn manifest_path(&self) -> PathBuf {
        self.root.join(MANIFEST)
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join(LOCK)
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join(STATE)
    }

    /// `host.arch` (`spec/01` §2.2): the architecture this binary was built for.
    pub fn arch(&self) -> &'static str {
        std::env::consts::ARCH
    }

    /// Take the apply lock only after the complete read-only pre-flight has succeeded.
    pub fn lock_for_apply(&mut self) -> Result<(), Diagnostic> {
        if self.operation != Operation::Apply || self.lock.is_some() {
            return Ok(());
        }
        self.lock = Some(take_apply_lock(&self.root)?);
        Ok(())
    }

    /// Run the gate in order. Nothing below the root is opened until arming has passed.
    pub fn open(options: &Options, operation: Operation) -> Result<Gate, Diagnostic> {
        let requested = options
            .root
            .clone()
            .unwrap_or_else(|| PathBuf::from("/"))
            .clone();
        // Resolve aliases such as `/tmp/..` before deciding whether this is the running system.
        // A missing or inaccessible root is left spelled as requested so the refusal names the
        // path the operator asked about.
        let root = fs::canonicalize(&requested).unwrap_or(requested);
        let system_root = root == Path::new("/");
        let euid = current_euid();
        let egid = current_egid();

        // 1b. A `--root` that does not exist is said to be missing, not to be unarmed: there is
        //     nothing there to arm, and the arming hint would send the operator the wrong way.
        if let Some(missing) = missing_root(&root) {
            return Err(missing);
        }

        // 2. Arming, before anything else below the root is opened.
        check_marker(&root, system_root, euid)?;

        // 2b. An ostree deployment is not a machine a package manager changes in place: its
        //    packages are layered with rpm-ostree, which lodi does not drive (LD-432).
        let ostree = root.join(OSTREE_BOOTED);
        if fs::symlink_metadata(&ostree).is_ok() {
            let mut refusal = Diagnostic::new(
                "E_HOST_OSTREE",
                format!("{} runs from rpm-ostree", root.display()),
            );
            refusal.notes.push(format!("found: {}", ostree.display()));
            refusal
                .notes
                .push("lodi does not manage an rpm-ostree system, such as Silverblue".into());
            return Err(refusal
                .hint("rpm-ostree layers its packages, not dnf; nothing was read or changed"));
        }

        // 3. A distribution this build manages.
        let os_path = root.join("etc/os-release");
        let text = fs::read_to_string(&os_path).map_err(|e| {
            Diagnostic::new(
                "E_UNSUPPORTED",
                format!("cannot read {}: {e}", os_path.display()),
            )
            .hint(format!(
                "the host scope manages {} and identifies a root by its os-release",
                SUPPORTED.join(", ")
            ))
        })?;
        let os = OsRelease::parse(&text);
        let distro = os.distro().ok_or_else(|| {
            Diagnostic::new(
                "E_UNSUPPORTED",
                format!(
                    "{} says ID={}, which this build does not manage",
                    os_path.display(),
                    if os.id.is_empty() { "(unset)" } else { &os.id }
                ),
            )
            .hint(format!(
                "the host scope manages {}; nothing was read or changed",
                SUPPORTED.join(", ")
            ))
        })?;

        // 3a. A flag that has no meaning for this distribution is refused, never ignored. It is
        //     checked here, before the manifest is opened, so that a root with no `[packages]`
        //     table at all still says that the flag means nothing to it.
        if options.unsupported_partial_upgrade && !super::pm::Shape::of(distro).has_partial_upgrade
        {
            return Err(Diagnostic::new(
                "E_UNSUPPORTED",
                format!(
                    "--unsupported-partial-upgrade is an arch option, and this root is `{}`",
                    distro.name()
                ),
            )
            .hint(
                "drop the flag: on this distribution installing without upgrading the machine \
                 is the ordinary, supported thing, and lodi already does it",
            ));
        }

        // 4. Privilege. An apply on `/` is a root action; under a `--root` the invoking user's
        //    own privilege is what bounds it, action by action.
        if operation == Operation::Apply && system_root && euid != 0 {
            return Err(Diagnostic::new(
                "E_NEED_ROOT",
                "applying to / changes root-owned files and distribution packages".to_string(),
            )
            .hint("run it as root: sudo lodi host apply"));
        }

        // 5. The package manager, by bare name, from a trusted place.
        let pm = find_package_manager(distro, system_root, euid, operation)?;

        Ok(Gate {
            root,
            system_root,
            os,
            distro,
            pm,
            euid,
            egid,
            operation,
            partial_upgrade: options.unsupported_partial_upgrade,
            lock: None,
        })
    }
}

/// Step 1b: the refusal for a root that is not there at all, or `None` when something is at
/// that name — whatever it is, the arming step judges it. `lodi host arm --root` refuses a
/// root that is not a directory with the same code (`E_STORE_IO`).
pub fn missing_root(root: &Path) -> Option<Diagnostic> {
    match fs::symlink_metadata(root) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Some(
            Diagnostic::new("E_STORE_IO", format!("{} does not exist", root.display()))
                .hint("--root names an existing directory; nothing was read or changed"),
        ),
        _ => None,
    }
}

/// The euid of this process.
pub fn current_euid() -> u32 {
    // SAFETY: geteuid cannot fail and takes no arguments.
    unsafe { libc::geteuid() }
}

pub fn current_egid() -> u32 {
    // SAFETY: getegid cannot fail and takes no arguments.
    unsafe { libc::getegid() }
}

/// The hint every `E_HOST_NOT_ARMED` carries: the one command that arms the root in question,
/// run once and deliberately (LD-320). Nothing else — no install, plan, apply or import — arms it.
pub fn arm_command(root: &Path, system_root: bool) -> String {
    let marker = root.join(MARKER);
    if system_root {
        format!(
            "arm this machine once, as root: sudo lodi host arm (it writes {}, an empty file)",
            marker.display()
        )
    } else {
        format!(
            "arm this root once: lodi host arm --root {} (it writes {}, an empty file)",
            root.display(),
            marker.display()
        )
    }
}

/// The refusal for a directory holding the host scope's configuration — the root, `etc` or
/// `etc/lodi` — that someone other than its owner could write, or that is not a directory.
fn untrusted_config_dir(dir: &Path, why: &str) -> Diagnostic {
    Diagnostic::new(
        "E_PATH_ESCAPE",
        format!("{} is not a trusted directory: {why}", dir.display()),
    )
    .hint(
        "lodi trusts its arming marker, manifest and lock only in directories nobody but their \
         owner can write; give it the owner the message names and remove its group and world \
         write bits (chmod go-w). Nothing was read or changed",
    )
}

/// Step 2, first half: the root, `<root>/etc` and `<root>/etc/lodi` — every directory the marker,
/// the manifest and the lock are reached through — are directories, not symbolic links, owned
/// by the trusted owner and neither group- nor world-writable ([`super::files::untrusted_directory`]).
/// A component that is not there ends the walk: the marker check then says the root is not
/// armed.
fn check_config_dirs(root: &Path, system_root: bool, euid: u32) -> Result<(), Diagnostic> {
    let trusted = super::files::trusted_owner(system_root, euid);
    let mut dir = root.to_path_buf();
    for part in ["", "etc", "lodi"] {
        if !part.is_empty() {
            dir.push(part);
        }
        let meta = match fs::symlink_metadata(&dir) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(untrusted_config_dir(
                    &dir,
                    &format!("it could not be inspected ({e})"),
                ));
            }
        };
        if meta.file_type().is_symlink() {
            return Err(untrusted_config_dir(&dir, "it is a symbolic link"));
        }
        if !meta.is_dir() {
            return Err(untrusted_config_dir(&dir, "it is not a directory"));
        }
        if let Some(why) = super::files::untrusted_directory(meta.uid(), meta.mode(), trusted) {
            return Err(untrusted_config_dir(&dir, &why));
        }
    }
    Ok(())
}

/// Why a configuration file with this owner and mode is not one the host scope reads, or `None`
/// when it is: the arming marker's rule — owned by root on `/`, by the invoking user under a
/// `--root`, and neither group- nor world-writable.
pub fn untrusted_config(owner: u32, mode: u32, system_root: bool, euid: u32) -> Option<String> {
    let trusted = super::files::trusted_owner(system_root, euid);
    if owner != trusted {
        let who = if system_root {
            "root".to_string()
        } else {
            format!("the invoking user (uid {euid})")
        };
        return Some(format!("it is owned by uid {owner}, not by {who}"));
    }
    if mode & 0o022 != 0 {
        return Some(format!(
            "it is mode {:04o}: group- or world-writable",
            mode & 0o7777
        ));
    }
    None
}

/// Read one of the host scope's configuration files — `etc/lodi/host.toml` or
/// `etc/lodi/host.lock` — the way the arming marker is judged: opened without following a
/// symbolic link and without blocking, and read only when the descriptor is a regular file that
/// passes [`untrusted_config`]. The directories above it were judged at step 2. `Ok(None)` is a
/// file that is not there; at most `limit` bytes and one more are read, so the caller can tell a
/// file over its limit.
pub fn read_config(
    root: &Path,
    rel: &str,
    system_root: bool,
    euid: u32,
    limit: usize,
) -> Result<Option<Vec<u8>>, Diagnostic> {
    use std::io::Read;
    let path = root.join(rel);
    let refuse = |why: String| {
        Diagnostic::new(
            "E_PATH_ESCAPE",
            format!(
                "{} is not a file the host scope reads: {why}",
                path.display()
            ),
        )
        .hint(
            "lodi reads its manifest and lock only as regular files, never through a symbolic \
             link, owned by root (by the invoking user under --root) and not writable by group \
             or others. Nothing was read or changed",
        )
    };
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(refuse("it is a symbolic link".to_string()));
        }
        Err(e) => {
            return Err(Diagnostic::new(
                "E_STORE_IO",
                format!("cannot read {}: {e}", path.display()),
            ));
        }
    };
    let meta = file.metadata().map_err(|e| {
        Diagnostic::new("E_STORE_IO", format!("cannot read {}: {e}", path.display()))
    })?;
    if !meta.is_file() {
        return Err(refuse("it is not a regular file".to_string()));
    }
    if let Some(why) = untrusted_config(meta.uid(), meta.mode(), system_root, euid) {
        return Err(refuse(why));
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| {
            Diagnostic::new("E_STORE_IO", format!("cannot read {}: {e}", path.display()))
        })?;
    Ok(Some(bytes))
}

/// Step 2. Every failure of the marker itself carries the same code and the same hint, so that
/// the refusal never says more about a root than that it is not armed.
fn check_marker(root: &Path, system_root: bool, euid: u32) -> Result<(), Diagnostic> {
    check_config_dirs(root, system_root, euid)?;
    let marker = root.join(MARKER);
    let refuse = |why: String| {
        Err(Diagnostic::new(
            "E_HOST_NOT_ARMED",
            format!("{} is not armed for the host scope: {why}", root.display()),
        )
        .hint(arm_command(root, system_root)))
    };
    let meta = match fs::symlink_metadata(&marker) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return refuse(format!("{} does not exist", marker.display()));
        }
        Err(e) => return refuse(format!("{}: {e}", marker.display())),
    };
    if !meta.is_file() {
        return refuse(format!(
            "{} is not a regular file (a symlink is never followed here)",
            marker.display()
        ));
    }
    let mode = meta.permissions().mode() & 0o7777;
    if system_root {
        if meta.uid() != 0 {
            return refuse(format!(
                "{} is owned by uid {}, not by root",
                marker.display(),
                meta.uid()
            ));
        }
        if mode & 0o022 != 0 {
            return refuse(format!(
                "{} is mode {mode:04o}: group- or world-writable",
                marker.display()
            ));
        }
    } else if meta.uid() != euid {
        return refuse(format!(
            "{} is owned by uid {}, not by the invoking user (uid {euid})",
            marker.display(),
            meta.uid()
        ));
    }
    Ok(())
}

/// Step 5: resolve each program the distribution needs by bare name ([`resolve_program`]).
///
/// `operation` is the command being run: the hint of a missing program names it, as a
/// backend's own `E_NO_RUNTIME` does, so that a plan or an import on a machine without its
/// package manager is told to run that command, never the apply.
pub fn find_package_manager(
    distro: Distro,
    system_root: bool,
    euid: u32,
    operation: Operation,
) -> Result<PackageManager, Diagnostic> {
    let mut programs = Vec::new();
    for name in distro.programs() {
        match resolve_program(name, system_root, euid) {
            Ok(Some(found)) => programs.push(((*name).to_string(), found)),
            Ok(None) => {
                return Err(Diagnostic::new(
                    "E_NO_RUNTIME",
                    format!(
                        "`{name}` is not on {}, and the host scope on {} needs {}",
                        searched(system_root),
                        distro.name(),
                        distro.programs().join(" and ")
                    ),
                )
                .hint(format!(
                    "run `{}` on a {} machine where {name} is installed; lodi installs no \
                     package manager and changes no host setting",
                    operation.command(),
                    distro.name()
                )));
            }
            Err(why) => return Err(untrusted_runtime(name, &why)),
        }
    }
    Ok(PackageManager { distro, programs })
}

/// What a missing program was looked for on, as a message says it.
pub fn searched(system_root: bool) -> String {
    if system_root {
        format!("the fixed path {FIXED_PATH}")
    } else {
        "PATH".to_string()
    }
}

/// The refusal for a package-manager program that is there and is not trusted.
pub fn untrusted_runtime(name: &str, why: &str) -> Diagnostic {
    Diagnostic::new(
        "E_NO_RUNTIME",
        format!("`{name}` is not a program the host scope runs: {why}"),
    )
    .hint(
        "lodi runs a package manager only if it is owned by root and nobody else can write it; \
         restore the distribution's own program. Nothing was changed",
    )
}

/// The first executable named `name` in `path`, as a shell would resolve it.
pub fn which(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    for dir in std::env::split_paths(path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if let Ok(meta) = fs::metadata(&candidate)
            && meta.is_file()
            && meta.permissions().mode() & 0o111 != 0
        {
            return Some(candidate);
        }
    }
    None
}

/// Step 6: the apply lock, taken exclusively and never waited on.
fn take_apply_lock(root: &Path) -> Result<ApplyLock, Diagnostic> {
    let dir =
        super::files::ensure_dir_trusted(root, &format!("/{STATE}"), 0o700).map_err(|error| {
            Diagnostic::new("E_SYSTEM_BUSY", error.message)
                .hint("the host scope keeps its journals and its apply lock there")
        })?;
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    let path = dir.join(".lock");
    super::files::inspect_trusted(root, &format!("/{STATE}/.lock"), false).map_err(|error| {
        Diagnostic::new("E_SYSTEM_BUSY", error.message)
            .hint("the host apply lock must be a regular file below the selected root")
    })?;
    match ApplyLock::try_take(&path) {
        Ok(Some(lock)) => Ok(lock),
        Ok(None) => Err(Diagnostic::new(
            "E_SYSTEM_BUSY",
            format!("another lodi host apply holds {}", path.display()),
        )
        .hint("wait for it to finish, then run `lodi host apply` again")),
        Err(e) => Err(Diagnostic::new(
            "E_SYSTEM_BUSY",
            format!("cannot take {}: {e}", path.display()),
        )
        .hint("check the permissions of that path")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_reads_id_and_id_like() {
        let os = OsRelease::parse(
            "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\nVERSION_CODENAME=noble\n",
        );
        assert_eq!(os.id, "ubuntu");
        assert_eq!(os.id_like, vec!["debian".to_string()]);
        assert_eq!(os.version_id, "24.04");
        assert_eq!(os.codename, "noble");
        assert_eq!(os.distro(), Some(Distro::Ubuntu));
    }

    #[test]
    fn nixos_is_not_a_distribution_this_build_manages() {
        let os = OsRelease::parse("ID=nixos\nVERSION_ID=\"25.11\"\n");
        assert_eq!(os.distro(), None);
    }

    #[test]
    fn a_declared_distro_that_disagrees_is_a_mismatch() {
        let os = OsRelease::parse("ID=arch\n");
        assert!(os.assert_distro(Some("arch"), "host.toml").is_ok());
        assert!(os.assert_distro(None, "host.toml").is_ok());
        let d = os.assert_distro(Some("debian"), "host.toml").unwrap_err();
        assert_eq!(d.code, "E_HOST_MISMATCH");
        assert_eq!(crate::diag::exit_status(d.code), 3);
    }

    #[test]
    fn a_package_manager_is_trusted_only_if_root_owns_it_and_nobody_else_can_write_it() {
        // On `/`: root only, and never group- or world-writable.
        assert_eq!(untrusted_program(0, 0o100755, true, 1000), None);
        assert_eq!(untrusted_program(0, 0o104755, true, 1000), None);
        assert!(
            untrusted_program(1000, 0o100755, true, 1000)
                .unwrap()
                .contains("not by root")
        );
        assert!(
            untrusted_program(0, 0o100775, true, 0)
                .unwrap()
                .contains("group- or world")
        );
        assert!(
            untrusted_program(0, 0o100757, true, 0)
                .unwrap()
                .contains("group- or world")
        );
        // Under a `--root`: root or the invoking user, and the same mode rule.
        assert_eq!(untrusted_program(0, 0o100755, false, 1000), None);
        assert_eq!(untrusted_program(1000, 0o100755, false, 1000), None);
        assert!(untrusted_program(1001, 0o100755, false, 1000).is_some());
        assert!(untrusted_program(1000, 0o100775, false, 1000).is_some());
    }

    #[test]
    fn on_the_system_root_the_search_path_is_the_fixed_one_whatever_the_caller_says() {
        let planted = std::ffi::OsStr::new("/planted/bin:/usr/bin");
        assert_eq!(search_path(true, Some(planted)), FIXED_PATH);
        assert_eq!(search_path(true, None), FIXED_PATH);
        assert_eq!(search_path(false, Some(planted)), planted);
        assert_eq!(search_path(false, None), FIXED_PATH);
    }

    #[test]
    fn a_configuration_file_is_judged_by_the_markers_rule() {
        assert_eq!(untrusted_config(0, 0o100644, true, 1000), None);
        assert!(
            untrusted_config(1000, 0o100644, true, 1000)
                .unwrap()
                .contains("not by root")
        );
        assert!(untrusted_config(0, 0o100664, true, 0).is_some());
        assert!(untrusted_config(0, 0o100646, true, 0).is_some());
        assert_eq!(untrusted_config(1000, 0o100600, false, 1000), None);
        assert!(untrusted_config(0, 0o100644, false, 1000).is_some());
        assert!(untrusted_config(1000, 0o100660, false, 1000).is_some());
    }

    #[test]
    fn the_arm_hint_names_the_marker_of_the_root_it_refused() {
        let hint = arm_command(Path::new("/srv/scratch"), false);
        assert!(
            hint.contains("/srv/scratch/etc/lodi/host-allowed"),
            "{hint}"
        );
        assert!(
            hint.contains("lodi host arm --root /srv/scratch "),
            "{hint}"
        );
        assert!(hint.contains("an empty file"), "{hint}");
        assert!(!hint.contains("sudo"), "{hint}");
        assert!(arm_command(Path::new("/"), true).contains("sudo lodi host arm "));
    }
}
