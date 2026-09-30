//! The dnf backend: Fedora 44 (LD-432).
//!
//! Everything a host apply does to packages on Fedora is here, through two programs:
//!
//! | what | the command |
//! | --- | --- |
//! | the installed set | `rpm -qa --qf '%{NAME} %{EPOCHNUM}:%{VERSION}-%{RELEASE} %{ARCH}\n'` |
//! | the marks | `dnf5 -C repoquery --installed --qf '%{name} %{reason} %{from_repo}\n'` |
//! | the candidates | `dnf5 -C repoquery --available --latest-limit=1 --arch=… <names…>` |
//! | the index | `dnf5 --refresh makecache`, six hours after the cache or lodi's last refresh |
//! | the transaction | `dnf5 -y --setopt=install_weak_deps=False install [--exclude=…] <names…>` |
//! | the removals | `dnf5 -y remove <names…>`, a second action |
//! | the marks again | `dnf5 -y mark dependency` / `mark user` `<names…>` |
//! | what a change would do | the same `install` or `remove` with `-C --assumeno`, which stops |
//! | requirement | `rpm -qa` of every requirement, provision and file, resolved here |
//! | the state of the database | `dnf5 -C check` |
//! | the conffiles (import only) | `rpm -Va --configfiles` |
//!
//! A version is written as dnf5 prints and takes one: `EPOCH:VERSION-RELEASE.ARCH`, the epoch
//! always spelled, so a plan line names exactly the build the transaction installs or removes.
//!
//! Weak dependencies are off for every transaction lodi builds (`install_weak_deps=False`), in
//! both modes: the owner's call, and what keeps an exact machine to what its file declares. A
//! `hold` is `--exclude=NAME` on lodi's own transactions and on nothing else (measured on a
//! Fedora 44 guest: an excluded installed package stays at its version and still satisfies what
//! needs it), so it is not machine state and nothing is written below `/etc/dnf`.
//!
//! # No `--` before the names
//!
//! dnf5 5.4 refuses `--` anywhere in its argv (`Unknown argument "--" for command "install"`,
//! measured), so it cannot carry the boundary the other backends put before package names. The
//! boundary is kept by refusing, here and before any argv is built, every name that is not a
//! package name of the manifest's own grammar ([`is_package_name`]: it starts with a letter or a
//! digit), so no name reaches dnf5 that it could read as an option. rpm takes `--` and gets it.
//!
//! Under `--root DIR` dnf5 gets `--installroot=DIR` (whose configuration, repositories and cache
//! are that tree's own) and rpm `--root DIR`; on `/` neither adds anything.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::diag::Diagnostic;

use super::super::manifest::is_package_name;
use super::super::safety::{Distro, Operation, resolve_program, searched, untrusted_runtime};
use super::{
    Backend, Conffile, ConffileStatus, Invocation, Observed, Origin, Simulation, Survey, capture,
    noninteractive_env,
};

/// Every program this backend runs; the gate requires both before it opens anything.
pub const PROGRAMS: &[&str] = &["dnf5", "rpm"];

/// Where dnf5 keeps each repository's metadata, inside the root.
pub const CACHE: &str = "var/cache/libdnf5";

/// lodi's record of its last `dnf5 --refresh makecache` that succeeded, inside the root: its
/// mtime is when that refresh ended (#421).
const REFRESHED: &str = "var/lib/lodi/host/dnf-refreshed";

/// Fedora's own repositories that a fresh install enables, by the id their `.repo` files give
/// them. A package whose installed build one of them offers is declared.
pub const BASE_REPOSITORIES: &[&str] = &["fedora", "updates"];

/// Fedora's own repositories a fresh install does not enable: a name installed from one is
/// declared, and the emitted file says its version is not what a fresh machine gets.
pub const OTHER_SUITES: &[&str] = &["updates-testing"];

/// The kernels and firmware a restored machine does not boot without. They survive the import's
/// subtraction and an exact apply never removes them, beside what `/etc/dnf/protected.d` lists.
pub const KERNELS: &[&str] = &[
    "kernel",
    "kernel-core",
    "kernel-modules",
    "kernel-modules-core",
    "linux-firmware",
];

/// The files naming the packages dnf itself refuses to remove, inside the root.
pub const PROTECTED_D: &str = "etc/dnf/protected.d";

/// Fedora's signing keys, repositories, dnf variables and SELinux policy settings (LD-432). On a
/// Fedora root a `[files]` entry below one is refused and the import never captures one, as the
/// apt and pacman keyrings and credentials are never lodi's to write or carry.
pub const SETTINGS: &[&str] = &[
    "/etc/dnf/vars/",
    "/etc/pki/",
    "/etc/selinux/",
    "/etc/yum.repos.d/",
];

/// Whether `path` (absolute, normalized) is one of [`SETTINGS`] or below one.
pub fn is_setting(path: &str) -> bool {
    SETTINGS
        .iter()
        .any(|dir| path.starts_with(dir) || Some(path) == dir.strip_suffix('/'))
}

/// The option every transaction lodi builds carries: no weak dependency is installed (LD-432).
pub const NO_WEAK_DEPENDENCIES: &str = "--setopt=install_weak_deps=False";

/// The option a transaction installing a pinned build's file carries: dnf5 verifies the file's
/// signature with the machine's own keys, as it does a repository's package (fk-1).
pub const LOCAL_SIGNATURES: &str = "--setopt=localpkg_gpgcheck=True";

/// What dnf5 prints when `--assumeno` stops a transaction it has resolved.
const ABORTED: &str = "Operation aborted by the user.";

/// rpm's pseudo-package for an imported signing key: never a package anybody installs.
const GPG_PUBKEY: &str = "gpg-pubkey";

pub struct Dnf {
    root: PathBuf,
    programs: BTreeMap<String, PathBuf>,
    /// Programs that were found and are not trusted, with the reason (LD-357).
    refused: BTreeMap<String, String>,
    operation: Operation,
    /// A private home for dnf5's own cache and state (fk-1): `sudo` keeps root's `HOME`, which
    /// the pin verb's dnf5, run as the repository's owner, cannot write.
    home: Option<PathBuf>,
}

impl Dnf {
    /// Resolve both programs by bare name from a trusted place (D3, LD-115, LD-357). Nothing is
    /// run here.
    pub fn new(root: &Path, operation: Operation) -> Dnf {
        let system_root = root == Path::new("/");
        let euid = super::super::safety::current_euid();
        let mut programs = BTreeMap::new();
        let mut refused = BTreeMap::new();
        for name in PROGRAMS {
            match resolve_program(name, system_root, euid) {
                Ok(Some(found)) => {
                    programs.insert((*name).to_string(), found);
                }
                Ok(None) => {}
                Err(why) => {
                    refused.insert((*name).to_string(), why);
                }
            }
        }
        Dnf {
            root: root.to_path_buf(),
            programs,
            refused,
            operation,
            home: None,
        }
    }

    /// The same backend, with dnf5's cache and state directories below `dir`.
    pub fn with_home(mut self, dir: &Path) -> Dnf {
        self.home = Some(dir.to_path_buf());
        self
    }

    /// One invocation of `program`, with its root option first.
    fn invocation(&self, program: &str, args: &[String]) -> Result<Invocation, Diagnostic> {
        if let Some(why) = self.refused.get(program) {
            return Err(untrusted_runtime(program, why));
        }
        let path = self
            .programs
            .get(program)
            .ok_or_else(|| missing(program, &self.root, self.operation))?;
        let mut full = Vec::with_capacity(args.len() + 2);
        if self.root != Path::new("/") {
            if program == "rpm" {
                full.push("--root".to_string());
                full.push(self.root.display().to_string());
            } else {
                full.push(format!("--installroot={}", self.root.display()));
            }
        }
        full.extend(args.iter().cloned());
        let mut env = noninteractive_env();
        if let Some(home) = &self.home {
            // dnf5 writes nowhere else below a home (measured: `/root/.local/state`).
            for (key, below) in [("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state")] {
                env.push((key.to_string(), home.join(below).display().to_string()));
            }
        }
        Ok(Invocation::resolved(program, path, &full, env))
    }

    /// A dnf5 invocation carrying package names after `leading`. Every name is checked first:
    /// dnf5 has no `--`, so a name that could read as an option never reaches it.
    fn dnf(&self, leading: &[&str], names: &[String]) -> Result<Invocation, Diagnostic> {
        if let Some(bad) = names.iter().find(|name| !is_package_name(name)) {
            return Err(Diagnostic::new(
                "E_TYPE",
                format!("`{bad}` is not a package name, and dnf5 is never given one"),
            )
            .hint("a package name starts with a letter or a digit; nothing was changed"));
        }
        let mut args: Vec<String> = leading.iter().map(|a| (*a).to_string()).collect();
        args.extend(names.iter().cloned());
        self.invocation("dnf5", &args)
    }

    /// Every build of `name` the configured repositories offer (fk-1), newest first.
    pub fn offers(
        &self,
        name: &str,
    ) -> Result<Vec<crate::hostscope::pin::fedora::Offer>, Diagnostic> {
        let text = capture(&self.dnf(
            &[
                "repoquery",
                "--available",
                "--qf",
                "%{name} %{epoch} %{version} %{release} %{arch} %{repoid} %{sourcerpm}\\n",
            ],
            &[name.to_string()],
        )?)?;
        Ok(crate::hostscope::pin::fedora::offers(name, &text))
    }

    /// `dnf5 download --destdir DIR NEVRA` from the configured repositories (fk-1): the bytes of
    /// `DIR/FILE`, or `None` when no configured repository serves that build. dnf5 checks what it
    /// downloads against its repository's metadata; the caller checks the lock's SHA-256.
    pub fn download(
        &self,
        nevra: &str,
        dir: &Path,
        file: &str,
    ) -> Result<Option<Vec<u8>>, Diagnostic> {
        if !nevra.starts_with(|c: char| c.is_ascii_alphanumeric())
            || !crate::hostscope::pin::arch::safe_file_name(&file.replace(".rpm", ".pkg.tar.zst"))
        {
            return Err(Diagnostic::new(
                "E_TYPE",
                format!("`{nevra}` is not a build dnf5 is ever given"),
            ));
        }
        let args = vec![
            "download".to_string(),
            "--destdir".to_string(),
            dir.display().to_string(),
            nevra.to_string(),
        ];
        let invocation = self.invocation("dnf5", &args)?;
        let done = output(&invocation)?;
        let path = dir.join(file);
        if !done.status.success() {
            let _ = fs::remove_file(&path);
            return Ok(None);
        }
        Ok(fs::read(&path).ok())
    }

    /// The pinned transaction's argv after `leading`: each held name excluded, the unpinned
    /// names, then the staged files, whose absolute paths dnf5 cannot read as options.
    fn with_files(
        &self,
        leading: &[&str],
        install: &[String],
        files: &[String],
        ignore: &[String],
    ) -> Result<Invocation, Diagnostic> {
        if let Some(bad) = install.iter().find(|name| !is_package_name(name)) {
            return Err(Diagnostic::new(
                "E_TYPE",
                format!("`{bad}` is not a package name, and dnf5 is never given one"),
            ));
        }
        if let Some(bad) = files
            .iter()
            .find(|f| !f.starts_with('/') || !f.ends_with(".rpm"))
        {
            return Err(Diagnostic::new(
                "E_TYPE",
                format!("`{bad}` is not a staged package file"),
            ));
        }
        let mut args: Vec<String> = leading.iter().map(|a| (*a).to_string()).collect();
        args.extend(excludes(ignore)?);
        args.extend(install.iter().cloned());
        args.extend(files.iter().cloned());
        self.invocation("dnf5", &args)
    }

    fn rpm(&self, args: &[&str]) -> Result<String, Diagnostic> {
        let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        capture(&self.invocation("rpm", &args)?)
    }

    /// The `dnf5 -C repoquery --installed` records: name, reason and the repository dnf says
    /// each came from.
    fn installed_records(&self) -> Result<Vec<Vec<String>>, Diagnostic> {
        let text = capture(&self.dnf(
            &[
                "-C",
                "repoquery",
                "--installed",
                "--qf",
                "%{name} %{reason} %{from_repo}\\n",
            ],
            &[],
        )?)?;
        Ok(words(&text))
    }

    /// Every build the local index offers of `names`, as `name evr arch repoid` rows.
    fn available(&self, names: &[String], latest: bool) -> Result<Vec<Vec<String>>, Diagnostic> {
        if names.is_empty() || self.index_age().is_none() {
            return Ok(Vec::new());
        }
        let arches = format!("--arch={},noarch", std::env::consts::ARCH);
        let mut leading = vec!["-C", "repoquery", "--available", arches.as_str()];
        if latest {
            leading.push("--latest-limit=1");
        }
        leading.extend([
            "--qf",
            "%{name} %{epoch}:%{version}-%{release} %{arch} %{repoid}\\n",
        ]);
        Ok(words(&capture(&self.dnf(&leading, names)?)?))
    }

    /// Run a transaction with `-C --assumeno`: dnf5 resolves it, prints its table and stops.
    fn table(&self, leading: &[&str], names: &[String]) -> Result<Transaction, Diagnostic> {
        let invocation = self.dnf(leading, names)?;
        let output = output(&invocation)?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stopped =
            String::from_utf8_lossy(&output.stderr).contains(ABORTED) || stdout.contains(ABORTED);
        if !output.status.success() && !stopped {
            return Err(failed(&invocation, &output));
        }
        Ok(parse_transaction(&stdout))
    }

    /// Every installed package's requirements, resolved to the installed packages that satisfy
    /// them: by what each provides, and a file requirement by the package that owns the file.
    fn installed_depends(&self) -> Result<BTreeMap<String, BTreeSet<String>>, Diagnostic> {
        let requires = pairs(&self.rpm(&["-qa", "--qf", "[%{=NAME}\\t%{REQUIRENAME}\\n]"])?);
        let provides = pairs(&self.rpm(&["-qa", "--qf", "[%{=NAME}\\t%{PROVIDENAME}\\n]"])?);
        let files = if requires.iter().any(|(_, what)| what.starts_with('/')) {
            pairs(&self.rpm(&["-qa", "--qf", "[%{=NAME}\\t%{FILENAMES}\\n]"])?)
        } else {
            Vec::new()
        };
        Ok(resolve(&requires, &provides, &files))
    }

    /// The names dnf itself refuses to remove, from `/etc/dnf/protected.d/*.conf`.
    fn dnf_protected(&self) -> BTreeSet<String> {
        let Ok(entries) = fs::read_dir(self.root.join(PROTECTED_D)) else {
            return BTreeSet::new();
        };
        let mut out = BTreeSet::new();
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "conf") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            out.extend(
                text.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty() && !line.starts_with('#'))
                    .filter(|line| is_package_name(line))
                    .map(ToString::to_string),
            );
        }
        out
    }
}

fn missing(program: &str, root: &Path, operation: Operation) -> Diagnostic {
    Diagnostic::new(
        "E_NO_RUNTIME",
        format!(
            "`{program}` is not on {}, and the dnf backend needs it",
            searched(root == Path::new("/"))
        ),
    )
    .hint(format!(
        "run `{}` on a Fedora machine that has dnf5 and rpm",
        operation.command()
    ))
}

fn output(invocation: &Invocation) -> Result<Output, Diagnostic> {
    invocation.command().output().map_err(|e| {
        Diagnostic::new(
            "E_APPLY",
            format!("cannot run {}: {e}", invocation.path.display()),
        )
        .hint("the package manager was resolved on PATH when the command started")
    })
}

fn failed(invocation: &Invocation, output: &Output) -> Diagnostic {
    let text = String::from_utf8_lossy(&output.stderr);
    let text = if text.trim().is_empty() {
        String::from_utf8_lossy(&output.stdout)
    } else {
        text
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(5)..].join("; ");
    Diagnostic::new(
        "E_APPLY",
        format!(
            "`{}` exited {}: {}",
            invocation.command_line(),
            output
                .status
                .code()
                .map_or_else(|| "on a signal".to_string(), |c| c.to_string()),
            if tail.is_empty() {
                "it printed nothing"
            } else {
                &tail
            }
        ),
    )
}

/// The whitespace-separated words of each non-empty line.
fn words(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .map(|line| line.split_whitespace().map(ToString::to_string).collect())
        .filter(|fields: &Vec<String>| !fields.is_empty())
        .collect()
}

/// `NAME<TAB>VALUE` lines, as rpm's `[%{=NAME}\t%{TAG}\n]` prints one per array element.
pub fn pairs(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(name, value)| !name.is_empty() && !value.trim().is_empty())
        .map(|(name, value)| (name.to_string(), value.trim().to_string()))
        .collect()
}

/// The installed set from `rpm -qa --qf '%{NAME} %{EPOCHNUM}:%{VERSION}-%{RELEASE} %{ARCH}\n'`,
/// each name at `EPOCH:VERSION-RELEASE.ARCH`. rpm's key pseudo-packages are not packages. Where
/// one name is installed more than once — a second architecture, or the kernels dnf keeps
/// several of — the build of this machine's own architecture (or `noarch`) wins, then the
/// newest.
pub fn parse_installed(text: &str, native: &str) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, (bool, String, String)> = BTreeMap::new();
    for fields in words(text) {
        let [name, evr, arch] = fields.as_slice() else {
            continue;
        };
        if name == GPG_PUBKEY {
            continue;
        }
        let ours = arch == native || arch == "noarch";
        let better = match out.get(name) {
            None => true,
            Some((was_ours, was, _)) => {
                (ours && !was_ours)
                    || (ours == *was_ours && compare_evr(evr, was) == std::cmp::Ordering::Greater)
            }
        };
        if better {
            out.insert(name.clone(), (ours, evr.clone(), arch.clone()));
        }
    }
    out.into_iter()
        .map(|(name, (_, evr, arch))| (name, format!("{evr}.{arch}")))
        .collect()
}

/// What a dnf5 transaction table says it would install and remove.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transaction {
    /// `Installing:`, `Installing dependencies:` and `Installing weak dependencies:`.
    pub installing: BTreeSet<String>,
    /// `Removing:`, `Removing dependent packages:` and `Removing unused dependencies:`.
    pub removing: BTreeSet<String>,
}

/// The package rows of a dnf5 transaction table, as `--assumeno` prints it (measured on a Fedora
/// 44 guest): a heading line ending in `:` at the first column, then one row per package
/// indented by one space, whose first word is the name. A `replacing` line is indented further
/// and is not a package of the heading above it.
pub fn parse_transaction(text: &str) -> Transaction {
    let mut out = Transaction::default();
    let mut section: Option<bool> = None;
    for line in text.lines() {
        if !line.starts_with(' ') {
            section = match line.trim_end() {
                "Installing:" | "Installing dependencies:" | "Installing weak dependencies:" => {
                    Some(true)
                }
                "Removing:" | "Removing dependent packages:" | "Removing unused dependencies:" => {
                    Some(false)
                }
                _ => None,
            };
            continue;
        }
        if line.starts_with("  ") {
            continue;
        }
        let (Some(installing), Some(name)) = (section, line.split_whitespace().next()) else {
            continue;
        };
        if installing {
            out.installing.insert(name.to_string());
        } else {
            out.removing.insert(name.to_string());
        }
    }
    out
}

/// `rpm -Va --configfiles`: a line per configuration file that differs from what its package
/// shipped. The nine result characters come first; a size (`S`) or digest (`5`) difference is a
/// changed file, and `?` where the digest belongs is a file rpm could not read, which is
/// `Undetermined`. `missing` with `(Permission denied)` is a file whose directory rpm could not
/// read, also `Undetermined`; a file that is really missing has nothing to capture.
pub fn parse_verify(text: &str) -> Vec<Conffile> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for line in text.lines() {
        let Some(path_at) = line.find(" /") else {
            continue;
        };
        let path = line[path_at + 1..].trim_end();
        let (path, denied) = match path.strip_suffix(" (Permission denied)") {
            Some(path) => (path, true),
            None => (path, false),
        };
        let flags = line.split_whitespace().next().unwrap_or("");
        let status = if flags == "missing" {
            if !denied {
                continue;
            }
            ConffileStatus::Undetermined
        } else if flags.len() != 9 {
            continue;
        } else if flags.as_bytes()[2] == b'?' {
            ConffileStatus::Undetermined
        } else if flags.contains('S') || flags.contains('5') {
            ConffileStatus::Modified
        } else {
            continue;
        };
        if seen.insert(path.to_string()) {
            out.push(Conffile {
                path: path.to_string(),
                status,
            });
        }
    }
    out
}

/// The capabilities one requirement names. A plain one is itself; a rich dependency — `(a if
/// b)`, `(a or b)` — names every operand, which can only keep more than it needs to, never less.
/// A version relation (`= 1.2-3`) and the word after it are not capabilities.
pub fn capabilities(requirement: &str) -> Vec<String> {
    const WORDS: &[&str] = &["and", "or", "if", "else", "with", "without", "unless"];
    const RELATIONS: &[&str] = &["=", "<", ">", "<=", ">=", "<>"];
    let text = requirement.replace(['(', ')'], " ");
    let mut out = Vec::new();
    let mut after_relation = false;
    for word in text.split_whitespace() {
        if after_relation {
            after_relation = false;
            continue;
        }
        if RELATIONS.contains(&word) {
            after_relation = true;
            continue;
        }
        if WORDS.contains(&word) {
            continue;
        }
        out.push(word.to_string());
    }
    // `foo(x86-64)` and `libc.so.6()(64bit)` are single capabilities: a plain requirement is
    // never split on its parentheses.
    if !requirement.starts_with('(') {
        return requirement
            .split_whitespace()
            .next()
            .map(|word| vec![word.to_string()])
            .unwrap_or_default();
    }
    out
}

/// Resolve each package's requirements to the packages that satisfy them: by provision, and a
/// file requirement also by the package that owns the file. A package never requires itself
/// here, and a requirement nothing installed satisfies (`rpmlib(…)`) reaches nothing. Every name
/// that provides anything is a key, so the keys are the installed set.
pub fn resolve(
    requires: &[(String, String)],
    provides: &[(String, String)],
    files: &[(String, String)],
) -> BTreeMap<String, BTreeSet<String>> {
    let mut providers: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (name, what) in provides.iter().chain(files.iter()) {
        providers
            .entry(what.as_str())
            .or_default()
            .insert(name.as_str());
    }
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (name, _) in provides {
        out.entry(name.clone()).or_default();
    }
    for (name, requirement) in requires {
        let reached = out.entry(name.clone()).or_default();
        for capability in capabilities(requirement) {
            for by in providers.get(capability.as_str()).into_iter().flatten() {
                if *by != name {
                    reached.insert((*by).to_string());
                }
            }
        }
    }
    out
}

/// The records `dnf5 repoquery --qf '@@%{name}\n%{requires}\n@@\n%{provides}\n'` prints: for
/// each package, its requirements and its provisions (a list tag prints one per line).
pub fn parse_records(text: &str) -> Vec<(String, Vec<String>, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
    let mut in_provides = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "@@" {
            in_provides = true;
        } else if let Some(name) = line.strip_prefix("@@") {
            out.push((name.to_string(), Vec::new(), Vec::new()));
            in_provides = false;
        } else if let Some((_, requires, provides)) = out.last_mut() {
            if in_provides {
                provides.push(line.to_string());
            } else {
                requires.push(line.to_string());
            }
        }
    }
    out
}

/// rpm's own version comparison (`rpmvercmp`) of two `EPOCH:VERSION-RELEASE` strings, with an
/// architecture suffix ignored when both carry one. A missing epoch is 0.
pub fn compare_evr(left: &str, right: &str) -> std::cmp::Ordering {
    let split = |evr: &str| -> (u64, String, String) {
        let (epoch, rest) = match evr.split_once(':') {
            Some((epoch, rest)) => (epoch.parse().unwrap_or(0), rest),
            None => (0, evr),
        };
        match rest.rsplit_once('-') {
            Some((version, release)) => (epoch, version.to_string(), release.to_string()),
            None => (epoch, rest.to_string(), String::new()),
        }
    };
    let (le, lv, lr) = split(left);
    let (re, rv, rr) = split(right);
    le.cmp(&re)
        .then_with(|| vercmp(&lv, &rv))
        .then_with(|| vercmp(&lr, &rr))
}

/// `rpmvercmp` of rpm 4.x and 6.0: alternating runs of digits and letters, separators skipped,
/// `~` sorting before anything and `^` after the end but before anything else.
pub fn vercmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if a == b {
        return Ordering::Equal;
    }
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    let separator = |c: u8| !c.is_ascii_alphanumeric() && c != b'~' && c != b'^';
    loop {
        while let Some((&c, rest)) = a.split_first()
            && separator(c)
        {
            a = rest;
        }
        while let Some((&c, rest)) = b.split_first()
            && separator(c)
        {
            b = rest;
        }
        match (a.first(), b.first()) {
            (Some(b'~'), Some(b'~')) => {
                a = &a[1..];
                b = &b[1..];
                continue;
            }
            (Some(b'~'), _) => return Ordering::Less,
            (_, Some(b'~')) => return Ordering::Greater,
            (Some(b'^'), Some(b'^')) => {
                a = &a[1..];
                b = &b[1..];
                continue;
            }
            (Some(b'^'), None) => return Ordering::Greater,
            (None, Some(b'^')) => return Ordering::Less,
            (Some(b'^'), _) => return Ordering::Less,
            (_, Some(b'^')) => return Ordering::Greater,
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            _ => {}
        }
        let numeric = a[0].is_ascii_digit();
        let run = |s: &[u8]| {
            s.iter()
                .take_while(|c| {
                    if numeric {
                        c.is_ascii_digit()
                    } else {
                        c.is_ascii_alphabetic()
                    }
                })
                .count()
        };
        let (na, nb) = (run(a), run(b));
        if nb == 0 {
            // Different kinds of run: the numeric one is newer.
            return if numeric {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        let (sa, sb) = (&a[..na], &b[..nb]);
        let order = if numeric {
            let trim = |s: &[u8]| {
                let zeros = s.iter().take_while(|c| **c == b'0').count();
                s[zeros..].to_vec()
            };
            let (ta, tb) = (trim(sa), trim(sb));
            ta.len().cmp(&tb.len()).then_with(|| ta.cmp(&tb))
        } else {
            sa.cmp(sb)
        };
        if order != Ordering::Equal {
            return order;
        }
        a = &a[na..];
        b = &b[nb..];
    }
}

/// How long ago the index below `root` was last refreshed: the later of the change and the
/// modification time of each `repodata/repomd.xml` below the cache, and of lodi's record of its
/// last refresh ([`REFRESHED`]). dnf5 leaves an unchanged repository's cached `repomd.xml` as it
/// was, so the file alone can stay older than the window however often it is refreshed (#421).
/// `None` when the cache holds no `repomd.xml`, whatever lodi recorded: there is no index.
fn age_at(root: &Path, now: SystemTime) -> Option<Duration> {
    let cached = fs::read_dir(root.join(CACHE))
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| fs::symlink_metadata(entry.path().join("repodata/repomd.xml")).ok())
        .filter(|meta| meta.is_file())
        .filter_map(|meta| {
            let changed = u64::try_from(meta.ctime())
                .ok()
                .zip(u32::try_from(meta.ctime_nsec()).ok())
                .map(|(secs, nanos)| UNIX_EPOCH + Duration::new(secs, nanos));
            meta.modified().ok().max(changed)
        })
        .max()?;
    let recorded = fs::symlink_metadata(root.join(REFRESHED))
        .ok()
        .filter(|meta| meta.is_file())
        .and_then(|meta| meta.modified().ok());
    let newest = recorded.map_or(cached, |recorded| recorded.max(cached));
    Some(now.duration_since(newest).unwrap_or_default())
}

impl Backend for Dnf {
    fn distro(&self) -> Distro {
        Distro::Fedora
    }

    /// rpm for the installed set, and dnf's own reason for the marks: `Dependency` and `Weak
    /// Dependency` are the automatically installed. `held` is empty: a hold is not machine
    /// state here.
    fn observe(&self) -> Result<Observed, Diagnostic> {
        let installed = parse_installed(
            &self.rpm(&[
                "-qa",
                "--qf",
                "%{NAME} %{EPOCHNUM}:%{VERSION}-%{RELEASE} %{ARCH}\\n",
            ])?,
            std::env::consts::ARCH,
        );
        let auto = self
            .installed_records()?
            .into_iter()
            .filter(|fields| {
                matches!(fields.get(1).map(String::as_str), Some("Dependency"))
                    || (fields.get(1).map(String::as_str) == Some("Weak")
                        && fields.get(2).map(String::as_str) == Some("Dependency"))
            })
            .map(|fields| fields[0].clone())
            .filter(|name| installed.contains_key(name))
            .collect();
        Ok(Observed {
            installed,
            auto,
            held: BTreeSet::new(),
        })
    }

    /// The newest build of each name the local index offers for this architecture, in the
    /// version spelling of [`parse_installed`]. No index at all is no candidate.
    fn candidates(&self, names: &[String]) -> Result<BTreeMap<String, String>, Diagnostic> {
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        for fields in self.available(names, true)? {
            let [name, evr, arch, _] = fields.as_slice() else {
                continue;
            };
            if !names.contains(name) {
                continue;
            }
            let version = format!("{evr}.{arch}");
            let newer = out.get(name).is_none_or(|was| {
                compare_evr(&version, was) == std::cmp::Ordering::Greater
                    || (arch != "noarch" && was.ends_with(".noarch"))
            });
            if newer {
                out.insert(name.clone(), version);
            }
        }
        Ok(out)
    }

    fn nearest(&self, name: &str) -> Vec<String> {
        if self.index_age().is_none() {
            return Vec::new();
        }
        let Ok(invocation) = self.dnf(
            &["-C", "repoquery", "--available", "--qf", "%{name}\\n"],
            &[],
        ) else {
            return Vec::new();
        };
        let Ok(text) = capture(&invocation) else {
            return Vec::new();
        };
        let known: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        crate::tools::nearest(name, &known)
    }

    /// Which enabled repositories offer the **installed** build of each name: one of Fedora's
    /// own that a fresh install enables is [`Origin::Base`], one of its own it does not enable is
    /// [`Origin::Suite`], any other is [`Origin::ThirdParty`] by its id, and none is
    /// [`Origin::LocalOnly`]. With no index at all every name is [`Origin::Unchecked`].
    fn origins(&self, names: &[String]) -> Result<BTreeMap<String, Origin>, Diagnostic> {
        if names.is_empty() {
            return Ok(BTreeMap::new());
        }
        if self.index_age().is_none() {
            return Ok(names
                .iter()
                .map(|name| (name.clone(), Origin::Unchecked))
                .collect());
        }
        let installed = self.observe()?.installed;
        let mut offering: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for fields in self.available(names, false)? {
            let [name, evr, arch, repo] = fields.as_slice() else {
                continue;
            };
            if installed.get(name) == Some(&format!("{evr}.{arch}")) {
                offering
                    .entry(name.clone())
                    .or_default()
                    .insert(repo.clone());
            }
        }
        let empty = BTreeSet::new();
        Ok(names
            .iter()
            .map(|name| {
                let repos = offering.get(name).unwrap_or(&empty);
                let origin = if repos
                    .iter()
                    .any(|r| BASE_REPOSITORIES.contains(&r.as_str()))
                {
                    Origin::Base
                } else if let Some(suite) =
                    repos.iter().find(|r| OTHER_SUITES.contains(&r.as_str()))
                {
                    Origin::Suite(suite.clone())
                } else if let Some(repo) = repos.iter().next() {
                    Origin::ThirdParty(repo.clone())
                } else {
                    Origin::LocalOnly
                };
                (name.clone(), origin)
            })
            .collect())
    }

    /// rpm's requirements resolved to installed packages for `depends`; `foreign` is what dnf
    /// says came from a package file on its command line; `keep` the kernels and what
    /// `/etc/dnf/protected.d` names. Fedora states no priority and has no metapackage section.
    fn survey(&self) -> Result<Survey, Diagnostic> {
        let depends = self.installed_depends()?;
        let foreign = self
            .installed_records()?
            .into_iter()
            .filter(|fields| fields.last().map(String::as_str) == Some("@commandline"))
            .map(|fields| fields[0].clone())
            .collect();
        let keep = KERNELS
            .iter()
            .map(|name| (*name).to_string())
            .chain(self.dnf_protected())
            .filter(|name| depends.contains_key(name))
            .collect();
        Ok(Survey {
            depends,
            foreign,
            keep,
            ..Survey::default()
        })
    }

    /// `rpm -Va --configfiles`, read whatever its exit status: rpm exits non-zero when any file
    /// differs, which is the answer and not a failure.
    fn conffiles(&self) -> Result<Vec<Conffile>, Diagnostic> {
        let invocation =
            self.invocation("rpm", &["-Va".to_string(), "--configfiles".to_string()])?;
        let output = output(&invocation)?;
        Ok(parse_verify(&String::from_utf8_lossy(&output.stdout)))
    }

    fn index_age(&self) -> Option<Duration> {
        age_at(&self.root, SystemTime::now())
    }

    /// `dnf5 --refresh makecache`: the metadata of every enabled repository, fetched again.
    fn refresh(&self) -> Result<Vec<Invocation>, Diagnostic> {
        Ok(vec![self.dnf(&["--refresh", "makecache"], &[])?])
    }

    /// [`REFRESHED`], written now. A record that cannot be written costs only a refresh that the
    /// next apply makes again.
    fn refreshed(&self) {
        let _ = crate::lock::write_atomic(&self.root.join(REFRESHED), b"");
    }

    fn transaction(
        &self,
        install: &[String],
        remove: &[String],
    ) -> Result<Vec<Invocation>, Diagnostic> {
        self.transaction_ignoring(install, remove, &[])
    }

    /// `dnf5 -y --setopt=install_weak_deps=False install`, each held name one `--exclude=NAME`.
    /// `remove` is always empty: [`Dnf::removal`] is the second action.
    fn transaction_ignoring(
        &self,
        install: &[String],
        remove: &[String],
        ignore: &[String],
    ) -> Result<Vec<Invocation>, Diagnostic> {
        debug_assert!(remove.is_empty(), "dnf removes in its own second action");
        if install.is_empty() {
            return Ok(Vec::new());
        }
        let excludes = excludes(ignore)?;
        let mut leading = vec!["-y", NO_WEAK_DEPENDENCIES, "install"];
        leading.extend(excludes.iter().map(String::as_str));
        Ok(vec![self.dnf(&leading, install)?])
    }

    /// `dnf5 -y remove`: the names, and what dnf's `clean_requirements_on_remove` takes with
    /// them. A configuration file a person changed is kept by rpm as `.rpmsave`.
    fn removal(&self, remove: &[String]) -> Result<Vec<Invocation>, Diagnostic> {
        if remove.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![self.dnf(&["-y", "remove"], remove)?])
    }

    fn marks(&self, auto: &[String], manual: &[String]) -> Result<Vec<Invocation>, Diagnostic> {
        let mut out = Vec::new();
        for (reason, names) in [("dependency", auto), ("user", manual)] {
            if !names.is_empty() {
                out.push(self.dnf(&["-y", "mark", reason], names)?);
            }
        }
        Ok(out)
    }

    /// `dnf5 -y install` of the unpinned names and the staged files of the pinned builds, weak
    /// dependencies off and local signatures checked (fk-1).
    fn fedora_pinned(
        &self,
        install: &[String],
        files: &[String],
        ignore: &[String],
    ) -> Result<Vec<Invocation>, Diagnostic> {
        Ok(vec![self.with_files(
            &["-y", NO_WEAK_DEPENDENCIES, LOCAL_SIGNATURES, "install"],
            install,
            files,
            ignore,
        )?])
    }

    /// The same transaction with `--assumeno`: dnf5 resolves it and stops. A transaction it
    /// cannot resolve is its error, with what dnf5 said.
    fn fedora_simulate(
        &self,
        install: &[String],
        files: &[String],
        ignore: &[String],
    ) -> Result<(), Diagnostic> {
        let invocation = self.with_files(
            &[NO_WEAK_DEPENDENCIES, "install", "--assumeno"],
            install,
            files,
            ignore,
        )?;
        let done = output(&invocation)?;
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&done.stdout),
            String::from_utf8_lossy(&done.stderr)
        );
        if done.status.success() || said.contains(ABORTED) {
            return Ok(());
        }
        Err(failed(&invocation, &done))
    }

    /// Nothing: a hold is `--exclude` on lodi's own transaction.
    fn holds(&self, _hold: &[String], _unhold: &[String]) -> Result<Vec<Invocation>, Diagnostic> {
        Ok(Vec::new())
    }

    fn required_by(&self) -> Result<BTreeMap<String, BTreeSet<String>>, Diagnostic> {
        let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (name, depends) in self.installed_depends()? {
            for dependency in depends {
                out.entry(dependency).or_default().insert(name.clone());
            }
        }
        Ok(out)
    }

    /// `dnf5 -C remove --assumeno`: what removing `remove` takes, dependents and unused
    /// dependencies with it. `install` plays no part: the removal is its own action.
    fn simulate(&self, install: &[String], remove: &[String]) -> Result<Simulation, Diagnostic> {
        let _ = install;
        if remove.is_empty() {
            return Ok(Simulation::default());
        }
        let table = self.table(&["-C", "remove", "--assumeno"], remove)?;
        Ok(Simulation {
            removed: table.removing,
            ..Simulation::default()
        })
    }

    /// The transaction's own table for what it would install, then one repoquery of those
    /// packages' requirements and provisions, resolved against the installed packages (by
    /// provision and by file) and against each other.
    fn arriving(&self, install: &[String]) -> Result<Survey, Diagnostic> {
        if install.is_empty() {
            return Ok(Survey::default());
        }
        let table = self.table(
            &["-C", "install", "--assumeno", NO_WEAK_DEPENDENCIES],
            install,
        )?;
        let names: Vec<String> = table.installing.into_iter().collect();
        if names.is_empty() {
            return Ok(Survey::default());
        }
        let arches = format!("--arch={},noarch", std::env::consts::ARCH);
        let records = parse_records(&capture(&self.dnf(
            &[
                "-C",
                "repoquery",
                "--available",
                "--latest-limit=1",
                arches.as_str(),
                "--qf",
                "@@%{name}\\n%{requires}\\n@@\\n%{provides}\\n",
            ],
            &names,
        )?)?);
        let mut requires: Vec<(String, String)> = Vec::new();
        let mut provides = pairs(&self.rpm(&["-qa", "--qf", "[%{=NAME}\\t%{PROVIDENAME}\\n]"])?);
        for (name, needs, gives) in &records {
            requires.extend(needs.iter().map(|n| (name.clone(), n.clone())));
            provides.extend(gives.iter().map(|g| (name.clone(), plain(g))));
        }
        let files = if requires.iter().any(|(_, what)| what.starts_with('/')) {
            pairs(&self.rpm(&["-qa", "--qf", "[%{=NAME}\\t%{FILENAMES}\\n]"])?)
        } else {
            Vec::new()
        };
        let mut depends = resolve(&requires, &provides, &files);
        depends.retain(|name, _| names.contains(name));
        let keep = KERNELS
            .iter()
            .map(|name| (*name).to_string())
            .chain(self.dnf_protected())
            .filter(|name| depends.contains_key(name))
            .collect();
        Ok(Survey {
            depends,
            keep,
            ..Survey::default()
        })
    }

    /// The exact apply's removal is the managed one: rpm keeps a changed configuration file as
    /// `.rpmsave` either way.
    fn exact_removal(&self, remove: &[String]) -> Result<Vec<Invocation>, Diagnostic> {
        self.removal(remove)
    }

    /// `dnf5 -C check`: every problem it reports with the installed packages — a duplicate an
    /// interrupted rpm transaction left, a broken dependency — one line each.
    fn unclean(&self) -> Result<Vec<String>, Diagnostic> {
        let invocation = self.dnf(&["-C", "check"], &[])?;
        let output = output(&invocation)?;
        if output.status.success() {
            return Ok(Vec::new());
        }
        let mut lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .chain(String::from_utf8_lossy(&output.stderr).lines())
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect();
        if lines.is_empty() {
            lines.push(format!(
                "`{}` failed and said nothing",
                invocation.command_line()
            ));
        }
        Ok(lines)
    }
}

/// A provision as dnf5 prints it (`htop = 3.4.1-3.fc44`), without its version.
fn plain(provision: &str) -> String {
    provision
        .split_whitespace()
        .next()
        .unwrap_or(provision)
        .to_string()
}

/// One `--exclude=NAME` per held name, each checked like every name dnf5 is given.
fn excludes(names: &[String]) -> Result<Vec<String>, Diagnostic> {
    names
        .iter()
        .map(|name| {
            if is_package_name(name) {
                Ok(format!("--exclude={name}"))
            } else {
                Err(Diagnostic::new(
                    "E_TYPE",
                    format!("`{name}` is not a package name, and dnf5 is never given one"),
                ))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    /// Bytes a Fedora 44 guest printed (`tests/fixtures/host/dnf/PROVENANCE.json`).
    fn recorded(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/host/dnf")
            .join(name);
        fs::read_to_string(path).expect("a recorded fixture")
    }

    /// dnf5's refresh leaves an unchanged repository's cached `repomd.xml` as it was, so the file
    /// can be a day old a minute after lodi refreshed the index. Counted from the file, every
    /// apply would refresh again and none would be `nothing to do` (#421); counted from lodi's
    /// own record of its refresh, the index is as old as that refresh.
    #[test]
    fn an_index_lodi_refreshed_is_fresh_when_dnf5_left_repomd_as_it_was() {
        let root = Path::new(env!("OUT_DIR")).join(format!("dnf-refreshed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let repomd = root.join(CACHE).join("fedora-0/repodata/repomd.xml");
        fs::create_dir_all(repomd.parent().unwrap()).unwrap();
        fs::write(&repomd, "metadata\n").unwrap();
        let hours = |n: u64| SystemTime::now() + Duration::from_secs(n * 3600);
        let window = super::super::REFRESH_AFTER;
        let record = root.join("var/lib/lodi/host/dnf-refreshed");
        fs::create_dir_all(record.parent().unwrap()).unwrap();
        fs::File::create(&record)
            .unwrap()
            .set_modified(hours(24))
            .unwrap();
        let fresh = age_at(&root, hours(25));
        let stale = age_at(&root, hours(31));
        fs::remove_file(&repomd).unwrap();
        let no_cache = age_at(&root, hours(25));
        let _ = fs::remove_dir_all(&root);
        assert!(fresh.is_some_and(|age| age < window), "{fresh:?}");
        assert!(stale.is_some_and(|age| age >= window), "{stale:?}");
        assert_eq!(no_cache, None, "a record is no index without the cache");
    }

    #[test]
    fn the_recorded_transaction_tables_are_read_by_heading() {
        let install = parse_transaction(&recorded("install-assumeno.txt"));
        assert_eq!(
            install.installing.into_iter().collect::<Vec<_>>(),
            ["libutempter", "tmux"]
        );
        let remove = parse_transaction(&recorded("remove-assumeno.txt"));
        assert!(remove.installing.is_empty());
        assert_eq!(remove.removing.len(), 32);
        assert!(remove.removing.contains("cloud-init") && remove.removing.contains("openssl"));
    }

    #[test]
    fn the_recorded_verify_output_is_read_as_a_verdict() {
        let files = parse_verify(&recorded("verify-configfiles.txt"));
        let status = |path: &str| files.iter().find(|f| f.path == path).map(|f| f.status);
        assert_eq!(
            status("/etc/libaudit.conf"),
            Some(ConffileStatus::Undetermined)
        );
        assert_eq!(
            status("/etc/grub.d/00_header"),
            Some(ConffileStatus::Undetermined)
        );
        // A mode that differs is not a changed file.
        assert_eq!(status("/etc/machine-id"), None);
    }

    #[test]
    fn the_recorded_listings_give_each_name_its_build() {
        let installed = parse_installed(&recorded("installed.txt"), "x86_64");
        assert_eq!(installed.len(), 40);
        assert!(
            installed
                .values()
                .all(|v| v.starts_with("0:") || v.starts_with("1:"))
        );
        assert_eq!(installed["fedora-repos"], "0:44-1.noarch");
        let candidates = words(&recorded("candidates.txt"));
        assert_eq!(
            candidates[1],
            ["htop", "0:3.4.1-3.fc44", "x86_64", "fedora"]
        );
        let reasons = words(&recorded("reasons.txt"));
        assert!(reasons.iter().all(|fields| fields.len() >= 2));
    }

    #[test]
    fn a_second_architecture_or_an_older_kernel_does_not_hide_the_build_that_counts() {
        let text = "glibc 0:2.43-2.fc44 i686\nglibc 0:2.43-2.fc44 x86_64\n\
                    kernel-core 0:6.19.1-300.fc44 x86_64\nkernel-core 0:6.19.9-300.fc44 x86_64\n\
                    gpg-pubkey 0:6d9f90a6-6786af3b (none)\n";
        let installed = parse_installed(text, "x86_64");
        assert_eq!(installed["glibc"], "0:2.43-2.fc44.x86_64");
        assert_eq!(installed["kernel-core"], "0:6.19.9-300.fc44.x86_64");
        assert!(!installed.contains_key("gpg-pubkey"));
    }

    #[test]
    fn rpm_versions_compare_as_rpmvercmp_does() {
        assert_eq!(vercmp("1.0", "1.0"), Ordering::Equal);
        assert_eq!(vercmp("1.0~rc1", "1.0"), Ordering::Less);
        assert_eq!(vercmp("1.0^git1", "1.0"), Ordering::Greater);
        assert_eq!(vercmp("1.10", "1.9"), Ordering::Greater);
        assert_eq!(vercmp("1.0a", "1.0"), Ordering::Greater);
        assert_eq!(vercmp("2.43", "2.043"), Ordering::Equal);
        assert_eq!(compare_evr("1:1.0-1", "2.0-1"), Ordering::Greater);
    }

    #[test]
    fn a_rich_dependency_names_every_operand() {
        assert_eq!(
            capabilities("(pam >= 1.1.3-7 if systemd)"),
            ["pam", "systemd"]
        );
        assert_eq!(capabilities("libc.so.6()(64bit)"), ["libc.so.6()(64bit)"]);
    }

    #[test]
    fn requirements_resolve_by_provision_and_by_file() {
        let pairs = |rows: &[(&str, &str)]| -> Vec<(String, String)> {
            rows.iter()
                .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
                .collect()
        };
        let depends = resolve(
            &pairs(&[
                ("htop", "libhwloc.so.15()(64bit)"),
                ("htop", "rpmlib(PayloadIsZstd)"),
                ("cloud-init", "/usr/bin/python3"),
            ]),
            &pairs(&[
                ("htop", "htop"),
                ("hwloc-libs", "libhwloc.so.15()(64bit)"),
                ("python3", "python3"),
                ("cloud-init", "cloud-init"),
            ]),
            &pairs(&[("python3", "/usr/bin/python3")]),
        );
        assert_eq!(depends["htop"].iter().collect::<Vec<_>>(), ["hwloc-libs"]);
        assert_eq!(
            depends["cloud-init"].iter().collect::<Vec<_>>(),
            ["python3"]
        );
        assert!(depends["hwloc-libs"].is_empty());
    }

    #[test]
    fn a_name_that_could_read_as_an_option_never_reaches_dnf5() {
        let dnf = Dnf::new(Path::new("/nonexistent-root"), Operation::Plan);
        let refused = dnf
            .dnf(&["install"], &["--noplugins".to_string()])
            .unwrap_err();
        assert_eq!(refused.code, "E_TYPE");
        assert!(is_setting("/etc/selinux") && is_setting("/etc/pki/rpm-gpg/KEY"));
        assert!(!is_setting("/etc/selinuxish"));
    }
}
