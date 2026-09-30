//! The pacman backend: Arch (`spec/09` §5.2, design calls D10 and D11).
//!
//! Everything a host apply does to packages on Arch is here, and it is one program:
//!
//! | what | the command |
//! | --- | --- |
//! | the installed set | `pacman -Q` |
//! | the explicit marks | `pacman -Qe` |
//! | the candidates | `pacman -Si <names…>` |
//! | the index age | the newest file below `/var/lib/pacman/sync` |
//! | the transaction | `pacman -Syu --noconfirm <names…>` — a **full upgrade** |
//! | the removals | `pacman -Rns --noconfirm <names…>`, a second action |
//! | the marks again | `pacman -D --asdeps` / `--asexplicit` `<names…>` |
//! | the state of the database | `/var/lib/pacman/db.lck` |
//!
//! Every argv here that carries package names puts `--` before them (LD-358), so that no name
//! can be read as an option, whatever let it through.
//!
//! # Arch's own rule: a partial upgrade is not a supported state
//!
//! Arch is a rolling distribution whose packages are built against the current versions of their
//! dependencies. Installing a new package without upgrading the rest of the machine — `pacman -Sy
//! <name>`, or `-S` after someone else refreshed the databases — links that new package against
//! libraries the machine does not have yet, and is the classic way to break an Arch system. So:
//!
//! - the ordinary transaction is `pacman -Syu --noconfirm <names…>`, refreshing the databases
//!   and upgrading the machine together. A dated host pin alone pre-syncs private databases with
//!   `-Syy --config pin.conf` to replace even newer live databases before checking digests;
//!   its transaction still uses `-Syu --config pin.conf` (design call D10);
//! - `--unsupported-partial-upgrade` is the only way to the other mode. It builds
//!   `pacman -S --needed --noconfirm <names…>` and the plan prints `W_ARCH_PARTIAL`. The flag's
//!   own name is the warning ADR-013 asked for, and `README.md` says in one sentence that the
//!   mode is unsupported.
//!
//! # A `hold` binds lodi's transactions, and nothing else (D11)
//!
//! `spec/09` §5.2 has Lodi writing `IgnorePkg` into `/etc/pacman.d/lodi.conf`. A file there is
//! inert unless `/etc/pacman.conf` includes it, which the stock configuration does not, so that
//! declaration would appear to work and do nothing. Instead a held name becomes `--ignore <name>`
//! on lodi's **own** transactions: it is honest, it mutates no file the manifest never declared,
//! and the difference from `apt-mark hold` — which binds the whole system — is said in the plan,
//! in `README.md` and in `docs/ERRORS.md`. This backend never opens `/etc/pacman.conf`, never
//! writes a file anywhere, and reads no configuration at all.
//!
//! # Three more things that are not here, and their absence is the design
//!
//! - no `pacman -Rsn` orphan sweep and no `-Qdtq | pacman -Rns -` pipeline. `auto_remove` is
//!   bounded by the lock's record of what Lodi itself installed (OD-15); what else the machine no
//!   longer needs is not Lodi's business.
//! - no version-qualified package *name*: a pin belongs in `[packages.pin]` (LD-395, D9).
//!   `src/arch/base.rs` supplies the dated URLs for both container and host resolution.
//! - no `pacman-key` or PGP import here. The private dated `pin.conf` uses stock
//!   `SigLevel = Required DatabaseOptional`: packages require signatures; dated databases do not.
//!   The machine's configuration and keyring are untouched; the one opt-in keyring of #170 is
//!   private to `pin.conf` and lives in `hostscope::pin::keyring`.
//!
//! Under `--root DIR` every invocation carries the root options of design call D2 (LD-114):
//! pacman's own `--root DIR` and `--dbpath DIR/var/lib/pacman`. On the root `/` they add nothing,
//! so the argv a machine sees is the one `spec/09` §5.2 and T-5 fix. As LD-114 already said, those
//! options are an argv-level promise and nothing here claims that a real pacman then changes that
//! tree's database rather than the running machine's.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::diag::Diagnostic;

use super::super::safety::{Distro, Operation, resolve_program, searched, untrusted_runtime};
use super::{
    Backend, Conffile, ConffileStatus, Invocation, Observed, Origin, Simulation, Survey, capture,
    noninteractive_env,
};

/// Every program this backend can reach for. The safety gate already requires `pacman` on
/// `PATH` before it opens anything; `pacman-conf` ships beside it and is read by the **import**
/// alone, for the machine's own list of enabled repositories. Its absence is not an error —
/// [`classify`] falls back to the family's own repository names.
pub const PROGRAMS: &[&str] = &["pacman", "pacman-conf"];

/// The end of the options. Every argv that carries package names puts it before them, so that
/// no name can ever be read as an option; a manifest or host-lock name that begins with `-` is
/// also refused long before it reaches here.
const END_OF_OPTIONS: &str = "--";

/// Append `names` to `args` after [`END_OF_OPTIONS`].
fn names_after_options(args: &mut Vec<String>, names: &[String]) {
    args.push(END_OF_OPTIONS.to_string());
    args.extend(names.iter().cloned());
}

/// The program that prints a pacman machine's own configuration, and the one option this
/// repository ever gives it.
pub const CONF: &str = "pacman-conf";

/// Where pacman keeps the databases it synchronised, inside the root.
pub const SYNC: &str = "var/lib/pacman/sync";

/// The transaction lock pacman takes, inside the root. Lodi reads it and **never removes it**:
/// a lock left behind is the residue of a pacman that did not finish, and only the operator can
/// say what that pacman was in the middle of.
pub const DB_LOCK: &str = "var/lib/pacman/db.lck";

/// The package every Arch installation is built around, and the one this distribution's import
/// baseline subtracts the dependency closure of. `base` itself is kept: it is the declaration
/// that makes an Arch machine an Arch machine.
pub const BASE: &str = "base";

/// The kernels and the firmware `base` does **not** carry and a restored machine does not boot
/// without. They are kept whatever else the baseline subtracts. It is a committed list of exact
/// names, not a pattern: there are no regular expressions anywhere in this tree (LD-80).
pub const KERNELS: &[&str] = &[
    "linux",
    "linux-firmware",
    "linux-hardened",
    "linux-lts",
    "linux-zen",
];

pub struct Pacman {
    root: PathBuf,
    /// `--unsupported-partial-upgrade` (design call D10). It changes one thing: which transaction
    /// [`Pacman::transaction`] builds.
    partial: bool,
    programs: BTreeMap<String, PathBuf>,
    /// Programs that were found and are not trusted, with the reason (LD-357).
    refused: BTreeMap<String, String>,
    /// The command being run, which a missing program's hint names.
    operation: Operation,
}

impl Pacman {
    /// Resolve every program this backend might use, by bare name (D3, LD-115), from a trusted
    /// place ([`resolve_program`], LD-357). A program that is there and is refused is kept with
    /// its refusal, which is what a use of it reports. Nothing is run here: a backend is built by
    /// `plan`, which mutates nothing.
    pub fn new(root: &Path, partial: bool, operation: Operation) -> Pacman {
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
        Pacman {
            root: root.to_path_buf(),
            partial,
            programs,
            refused,
            operation,
        }
    }

    /// Whether this backend is in the unsupported partial-upgrade mode.
    pub fn is_partial(&self) -> bool {
        self.partial
    }

    /// What points pacman at `--root DIR` instead of the running machine (design call D2,
    /// LD-114): pacman's own root, and the database below it. The root `/` adds nothing.
    fn root_options(&self) -> Vec<String> {
        if self.root == Path::new("/") {
            return Vec::new();
        }
        vec![
            "--root".to_string(),
            self.root.display().to_string(),
            "--dbpath".to_string(),
            self.root.join("var/lib/pacman").display().to_string(),
        ]
    }

    /// One invocation, with the root options after the operation — where an option belongs for
    /// pacman, and where it leaves the operation itself readable.
    fn invocation(&self, args: &[String]) -> Result<Invocation, Diagnostic> {
        if let Some(why) = self.refused.get("pacman") {
            return Err(untrusted_runtime("pacman", why));
        }
        let path = self
            .programs
            .get("pacman")
            .ok_or_else(|| missing(&self.root, self.operation))?;
        let mut full: Vec<String> = Vec::with_capacity(args.len() + 4);
        if let Some((operation, rest)) = args.split_first() {
            full.push(operation.clone());
            full.extend(self.root_options());
            full.extend(rest.iter().cloned());
        } else {
            full.extend(self.root_options());
        }
        Ok(Invocation::resolved(
            "pacman",
            path,
            &full,
            noninteractive_env(),
        ))
    }

    fn read(&self, args: &[&str]) -> Result<String, Diagnostic> {
        let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        capture(&self.invocation(&args)?)
    }

    /// One `pacman -Q…` query whose empty answer is the machine's answer and not a failure.
    ///
    /// See [`super::capture_query`]: pacman reports "nothing matched" with exit status 1, which
    /// is what a machine with no foreign package says when it is asked for its foreign packages.
    fn read_query(&self, args: &[&str]) -> Result<String, Diagnostic> {
        let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        super::capture_query(&self.invocation(&args)?)
    }

    /// What `pacman -Si <names…>` prints on standard output for a set of names.
    ///
    /// pacman exits non-zero when **any** name it was given is not in a database, having
    /// printed the records of the ones that are. That is the ordinary shape of this question
    /// and not a failure, so the records are read whatever the status — but every line pacman
    /// wrote to standard error has to be one of its own "was not found" lines. Anything else
    /// (no databases at all, an unreadable one) is a real failure and is reported as one,
    /// rather than being turned into "the index offers none of these names".
    fn sync_records(&self, names: &[String]) -> Result<String, Diagnostic> {
        let mut args = vec!["-Si".to_string()];
        names_after_options(&mut args, names);
        let invocation = self.invocation(&args)?;
        let output = invocation.command().output().map_err(|e| {
            Diagnostic::new(
                "E_APPLY",
                format!("cannot run {}: {e}", invocation.path.display()),
            )
            .hint("the package manager was resolved on PATH when the command started")
        })?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success()
            && let Some(line) = stderr
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !is_not_found(line))
        {
            return Err(Diagnostic::new(
                "E_APPLY",
                format!("`{}` failed: {line}", invocation.command_line()),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// The repositories this machine has enabled, as `pacman-conf --repo-list` states them.
    ///
    /// It is the **import's** reading and no apply path reaches it. It never fails the import:
    /// a machine whose `pacman-conf` is not on `PATH`, or whose configuration the program will
    /// not read, answers with the empty set, and [`classify`] then decides on the family's own
    /// repository names alone. A root that is not the machine is read through `--sysroot`, so
    /// the configuration that is read is that root's and never this machine's.
    fn enabled_repositories(&self) -> BTreeSet<String> {
        let Some(path) = self.programs.get(CONF) else {
            return BTreeSet::new();
        };
        let mut args: Vec<String> = Vec::new();
        if self.root != Path::new("/") {
            args.push("--sysroot".to_string());
            args.push(self.root.display().to_string());
        }
        args.push("--repo-list".to_string());
        let invocation = Invocation::resolved(CONF, path, &args, noninteractive_env());
        match capture(&invocation) {
            Ok(text) => parse_repo_list(&text),
            Err(_) => BTreeSet::new(),
        }
    }

    /// The package names one `pacman -Q…` listing prints, with their versions.
    ///
    /// Each line is `name version`, separated by one space; neither field can hold one.
    fn listing(&self, operation: &str) -> Result<BTreeMap<String, String>, Diagnostic> {
        let text = self.read(&[operation])?;
        let mut out = BTreeMap::new();
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let (Some(name), Some(version)) = (fields.next(), fields.next()) else {
                continue;
            };
            out.insert(name.to_string(), version.to_string());
        }
        Ok(out)
    }
}

/// The `Backup Files` rows of `pacman -Qii`, from every package's record at once.
///
/// pacman lays a multi-valued field out as its label, a colon, the first value, and one indented
/// line per value after it. A backup row is a path and the verdict pacman puts beside it, as
/// `/etc/nanorc [modified]` — the layout a pacman 7 guest printed under M-Import T-5 (LD-302).
/// Older pacman wrote the verdict first and the path after a tab, so a row is read either way
/// round; `UNMODIFIED` ends in the same letters as `MODIFIED`, so a verdict is compared as a
/// whole word and never as a suffix. A row pacman spells any other way — `None`, or a shape this
/// build does not understand — is skipped, and a path is reported once however many packages
/// claim it.
pub fn parse_qii_backups(text: &str) -> Vec<Conffile> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let mut inside = false;
    for line in text.lines() {
        let value = match field_label(line) {
            Some((label, rest)) => {
                inside = label == "Backup Files";
                rest
            }
            None => line,
        };
        if !inside {
            continue;
        }
        let Some((path, status)) = backup_row(value) else {
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

/// The label of a `Name            : value` line, and what follows its colon.
///
/// A label starts at the first column and is made of the letters and spaces pacman spells a
/// field name with, so neither an indented continuation line nor a row carrying a path (which
/// holds a `/`) is mistaken for one.
fn field_label(line: &str) -> Option<(&str, &str)> {
    if line.starts_with(char::is_whitespace) {
        return None;
    }
    let colon = line.find(':')?;
    let (label, rest) = line.split_at(colon);
    let label = label.trim_end();
    if label.is_empty() || !label.chars().all(|c| c.is_ascii_alphabetic() || c == ' ') {
        return None;
    }
    Some((label, &rest[1..]))
}

/// One backup row, in either of the two layouts pacman has used for it.
fn backup_row(value: &str) -> Option<(&str, ConffileStatus)> {
    let mut fields = value.split_whitespace();
    let first = fields.next()?;
    let second = fields.next()?;
    if fields.next().is_some() {
        return None;
    }
    // `[unreadable]` is what pacman prints for a tracked file it could not open — a 0600 file
    // read by an unprivileged import, say. That is exactly `Undetermined`: the family could not
    // say, nothing was opened to find out, and the file is a candidate so that the capture names
    // the refusal rather than dropping the file in silence (LD-303; a guest run of M-Import T-5
    // found it dropped without a word).
    let status = |word: &str| match word {
        "MODIFIED" | "[modified]" => Some(ConffileStatus::Modified),
        "UNMODIFIED" | "[unmodified]" => Some(ConffileStatus::Unmodified),
        "[unreadable]" => Some(ConffileStatus::Undetermined),
        _ => None,
    };
    if first.starts_with('/') {
        return Some((first, status(second)?));
    }
    if second.starts_with('/') {
        return Some((second, status(first)?));
    }
    None
}

fn missing(root: &Path, operation: Operation) -> Diagnostic {
    Diagnostic::new(
        "E_NO_RUNTIME",
        format!(
            "`pacman` is not on {}, and the pacman backend needs it",
            searched(root == Path::new("/"))
        ),
    )
    .hint(format!(
        "run `{}` on a machine whose package manager is pacman",
        operation.command()
    ))
}

impl Backend for Pacman {
    fn distro(&self) -> Distro {
        Distro::Arch
    }

    /// `pacman -Q` for the installed set and `pacman -Qe` for the explicit marks.
    ///
    /// pacman's two marks are `explicit` and `dependency` and every installed package is exactly
    /// one of them, so what `-Qe` does not list is what `pacman -Qd` would: the automatically
    /// installed set. `held` is always empty, because an Arch hold is not machine state (D11) —
    /// which is also why the plan re-passes `--ignore` on every transaction rather than checking
    /// whether a hold is already "set".
    fn observe(&self) -> Result<Observed, Diagnostic> {
        let installed = self.listing("-Q")?;
        let explicit: BTreeSet<String> = self.listing("-Qe")?.into_keys().collect();
        let auto = installed
            .keys()
            .filter(|name| !explicit.contains(*name))
            .cloned()
            .collect();
        Ok(Observed {
            installed,
            auto,
            held: BTreeSet::new(),
        })
    }

    /// `pacman -Si <names…>`: one call, one record per name the synchronised databases carry.
    ///
    /// pacman exits non-zero when **any** name it was given is not in a database, having printed
    /// the records of the ones that are. That is the ordinary shape of this question and not a
    /// failure, so the records are read whatever the status — but every line pacman wrote to
    /// standard error has to be one of its own "was not found" lines. Anything else (no
    /// databases at all, an unreadable one) is a real failure and is reported as one, rather
    /// than being turned into "the index offers none of these names".
    fn candidates(&self, names: &[String]) -> Result<BTreeMap<String, String>, Diagnostic> {
        if names.is_empty() {
            return Ok(BTreeMap::new());
        }
        Ok(parse_si(&self.sync_records(names)?))
    }

    /// Where the installed version of each named package came from: the repositories
    /// `pacman -Si <names…>` says offer it, against the enabled set `pacman-conf --repo-list`
    /// states.
    ///
    /// A name a third-party repository such as `chaotic-aur` offers had a `-Si` version and was
    /// declared before R-2, because [`parse_si`] kept `Version` and dropped `Repository`. A
    /// name **both** a third-party repository and one of Arch's own offer is still declared:
    /// a fresh machine can install it from Arch's own.
    ///
    /// An AUR or hand-built package is not answered here at all — it is in no database, so
    /// `-Si` has no record of it — and is answered by `pacman -Qqem` in [`Backend::survey`],
    /// which the import reads first.
    fn origins(&self, names: &[String]) -> Result<BTreeMap<String, Origin>, Diagnostic> {
        if names.is_empty() {
            return Ok(BTreeMap::new());
        }
        if !has_index(&self.root) {
            return Ok(names
                .iter()
                .map(|name| (name.clone(), Origin::Unchecked))
                .collect());
        }
        let offered = parse_si_repositories(&self.sync_records(names)?);
        let enabled = self.enabled_repositories();
        let empty = BTreeSet::new();
        Ok(names
            .iter()
            .map(|name| {
                let repositories = offered.get(name).unwrap_or(&empty);
                (name.clone(), classify(repositories, &enabled))
            })
            .collect())
    }

    /// `pacman -Qi` for what every installed package depends on, and `pacman -Qqem` for the
    /// names that came from no repository pacman knows (an AUR build, a package file installed
    /// by hand). Both are reads; neither changes anything.
    ///
    /// `priority` is empty: pacman has no such field, and this family's baseline is the
    /// dependency closure of `base` instead (design call D3, LD-271). `metapackages` is
    /// therefore `base` itself when it is installed, and `keep` is the kernels and firmware of
    /// [`KERNELS`] that are installed — a machine restored without them does not boot, and
    /// `base` does not carry them.
    fn survey(&self) -> Result<Survey, Diagnostic> {
        let mut survey = Survey {
            depends: parse_qi_depends(&self.read(&["-Qi"])?),
            ..Survey::default()
        };
        if survey.depends.contains_key(BASE) {
            survey.metapackages.insert(BASE.to_string());
        }
        for name in KERNELS {
            if survey.depends.contains_key(*name) {
                survey.keep.insert((*name).to_string());
            }
        }
        survey.foreign = self
            .read_query(&["-Qqem"])?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect();
        Ok(survey)
    }

    /// pacman's own verdict on every backup file it tracks, from one `pacman -Qii`.
    ///
    /// pacman answers the question directly — it prints `MODIFIED` or `UNMODIFIED` beside each
    /// backup path under `Backup Files` — so this family needs no digest of its own and opens
    /// no file at all. `LC_ALL=C` is already on every invocation of this seam, which is what
    /// makes those two words the two words.
    fn conffiles(&self) -> Result<Vec<Conffile>, Diagnostic> {
        Ok(parse_qii_backups(&self.read(&["-Qii"])?))
    }

    /// The nearest names among every name the synchronised databases carry.
    ///
    /// `pacman -Slq` is the whole list rather than a search, because pacman's own `-Ss` takes a
    /// **regular expression** and a misspelling is not one: handing a name with a `+` or a `.`
    /// in it to `-Ss` would ask a different question than the one the operator asked. There are
    /// no regular expressions anywhere in this tree (LD-80).
    fn nearest(&self, name: &str) -> Vec<String> {
        let Ok(text) = self.read(&["-Slq"]) else {
            return Vec::new();
        };
        let known: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect();
        crate::tools::nearest(name, &known)
    }

    /// How long ago pacman last wrote a synchronised database. A directory that is not there, or
    /// holds nothing, is `None`: no databases at all.
    ///
    /// Nothing acts on this on Arch — the transaction refreshes by itself — but the plan reports
    /// it and `--no-update` reads it, so it is answered honestly rather than left out.
    fn index_age(&self) -> Option<Duration> {
        let newest = fs::read_dir(self.root.join(SYNC))
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| entry.metadata().ok())
            .filter(|meta| meta.is_file())
            .filter_map(|meta| meta.modified().ok())
            .max()?;
        SystemTime::now().duration_since(newest).ok()
    }

    /// Nothing. Arch refreshes inside the transaction, and the separate refresh that would go
    /// here is `pacman -Sy` — precisely the partial state this backend exists to avoid (D10).
    fn refresh(&self) -> Result<Vec<Invocation>, Diagnostic> {
        Ok(Vec::new())
    }

    /// `pacman -Syu --noconfirm <names…>`, the full upgrade that is Arch's only supported way to
    /// install. Holding nothing, it is [`Pacman::transaction_ignoring`] with an empty set.
    fn transaction(
        &self,
        install: &[String],
        remove: &[String],
    ) -> Result<Vec<Invocation>, Diagnostic> {
        self.transaction_ignoring(install, remove, &[])
    }

    /// The transaction, with each held name as one `--ignore <name>` — which is the whole of an
    /// Arch hold (D11). Nothing is written to `/etc/pacman.conf`, and the option binds this
    /// invocation and no other command on the machine.
    ///
    /// `remove` is always empty: [`Pacman::removal`] is the second action that carries removals.
    fn transaction_ignoring(
        &self,
        install: &[String],
        remove: &[String],
        ignore: &[String],
    ) -> Result<Vec<Invocation>, Diagnostic> {
        debug_assert!(remove.is_empty(), "pacman removes in its own second action");
        if install.is_empty() {
            return Ok(Vec::new());
        }
        // The one branch `--unsupported-partial-upgrade` makes anywhere in this build. `-S
        // --needed` installs against the databases the machine already has and upgrades
        // nothing; `-Syu` refreshes and upgrades the machine in the same transaction, which is
        // the only state Arch supports and therefore the default (D10).
        let leading: &[&str] = if self.partial {
            &["-S", "--needed", "--noconfirm"]
        } else {
            &["-Syu", "--noconfirm"]
        };
        let mut args: Vec<String> = leading.iter().map(|a| (*a).to_string()).collect();
        for name in ignore {
            args.push("--ignore".to_string());
            args.push(name.clone());
        }
        names_after_options(&mut args, install);
        Ok(vec![self.invocation(&args)?])
    }

    fn arch_pinned(
        &self,
        install: &[String],
        files: &[String],
        ignore: &[String],
        dated: bool,
    ) -> Result<Vec<Invocation>, Diagnostic> {
        let mut out = Vec::new();
        if !install.is_empty() || !files.is_empty() {
            let mut args = vec!["-Syu".to_string()];
            if dated {
                args.extend([
                    "--config".to_string(),
                    self.root
                        .join("var/lib/lodi/host/pin/pin.conf")
                        .display()
                        .to_string(),
                ]);
            }
            args.push("--noconfirm".to_string());
            for name in ignore {
                args.extend(["--ignore".to_string(), name.clone()]);
            }
            names_after_options(&mut args, install);
            out.push(self.invocation(&args)?);
        }
        if !files.is_empty() {
            let mut args = vec![
                "-U".to_string(),
                "--config".to_string(),
                self.root
                    .join("var/lib/lodi/host/pin/pin.conf")
                    .display()
                    .to_string(),
                "--noconfirm".to_string(),
            ];
            names_after_options(&mut args, files);
            out.push(self.invocation(&args)?);
        }
        Ok(out)
    }

    fn arch_simulate(&self, files: &[String]) -> Result<(), Diagnostic> {
        if files.is_empty() {
            return Ok(());
        }
        let mut args = vec![
            "-Up".to_string(),
            "--config".to_string(),
            self.root
                .join("var/lib/lodi/host/pin/pin.conf")
                .display()
                .to_string(),
            "--print-format".to_string(),
            "%n".to_string(),
        ];
        names_after_options(&mut args, files);
        capture(&self.invocation(&args)?).map(|_| ())
    }

    fn private_refresh(&self) -> Result<Vec<Invocation>, Diagnostic> {
        Ok(vec![
            self.invocation(&[
                "-Syy".into(),
                "--config".into(),
                self.root
                    .join("var/lib/lodi/host/pin/pin.conf")
                    .display()
                    .to_string(),
                "--noconfirm".into(),
            ])?,
        ])
    }

    /// `pacman -Rns --noconfirm <names…>`: the package, the dependencies nothing else needs, and
    /// no backup files left behind. It is a **second action** after the install, which is the
    /// order `spec/09` §6.2 gives for pacman, and it has its own journal brackets.
    fn removal(&self, remove: &[String]) -> Result<Vec<Invocation>, Diagnostic> {
        if remove.is_empty() {
            return Ok(Vec::new());
        }
        let mut args: Vec<String> = ["-Rns", "--noconfirm"]
            .iter()
            .map(|a| (*a).to_string())
            .collect();
        names_after_options(&mut args, remove);
        Ok(vec![self.invocation(&args)?])
    }

    /// `pacman -D --asdeps` and `--asexplicit`: the two marks pacman keeps, and the only thing
    /// this backend changes about a package it is not installing or removing.
    fn marks(&self, auto: &[String], manual: &[String]) -> Result<Vec<Invocation>, Diagnostic> {
        let mut out = Vec::new();
        for (flag, names) in [("--asdeps", auto), ("--asexplicit", manual)] {
            if names.is_empty() {
                continue;
            }
            let mut args = vec!["-D".to_string(), flag.to_string()];
            names_after_options(&mut args, names);
            out.push(self.invocation(&args)?);
        }
        Ok(out)
    }

    /// Nothing. An Arch hold is `--ignore` on lodi's own transaction (D11), not a command that
    /// changes the machine — which is what `hold_is_machine_state: false` says, and why nothing
    /// here could ever edit `/etc/pacman.conf`.
    fn holds(&self, _hold: &[String], _unhold: &[String]) -> Result<Vec<Invocation>, Diagnostic> {
        Ok(Vec::new())
    }

    /// `Required By` of `pacman -Qi`: pacman resolves it through what each package provides,
    /// which the `Depends On` names alone do not (LD-375).
    fn required_by(&self) -> Result<BTreeMap<String, BTreeSet<String>>, Diagnostic> {
        Ok(parse_qi_required_by(&self.read(&["-Qi"])?))
    }

    /// `pacman -Rsp --print-format %n -- <names…>`: what `-Rs` of those names would take — the
    /// names, and every dependency they leave with no reason to stay — printed and not done.
    /// pacman refuses, and so does this, when anything outside the set still requires one of
    /// them. `install` plays no part: on Arch the removal is its own action after the upgrade.
    fn simulate(&self, install: &[String], remove: &[String]) -> Result<Simulation, Diagnostic> {
        let _ = install;
        if remove.is_empty() {
            return Ok(Simulation::default());
        }
        let mut args: Vec<String> = ["-Rsp", "--print-format", "%n"]
            .iter()
            .map(|a| (*a).to_string())
            .collect();
        names_after_options(&mut args, remove);
        let text = capture(&self.invocation(&args)?)?;
        Ok(Simulation {
            removed: text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToString::to_string)
                .collect(),
            ..Simulation::default()
        })
    }

    /// `pacman -Sp --print-format %n --needed --noconfirm -- <names…>`: every package the sync
    /// would install, what it was asked for and the dependencies that pulls in, printed and not
    /// done; then `pacman -Si` of those for what each depends on (LD-412). pacman resolves a
    /// requirement through what packages provide, so each name a record depends on is also
    /// answered by the installed (`pacman -Qi`) or arriving package that provides it. All three
    /// are reads.
    fn arriving(&self, install: &[String]) -> Result<Survey, Diagnostic> {
        if install.is_empty() {
            return Ok(Survey::default());
        }
        let mut args: Vec<String> = ["-Sp", "--print-format", "%n", "--needed", "--noconfirm"]
            .iter()
            .map(|a| (*a).to_string())
            .collect();
        names_after_options(&mut args, install);
        let names: Vec<String> = capture(&self.invocation(&args)?)?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect();
        if names.is_empty() {
            return Ok(Survey::default());
        }
        let records = self.sync_records(&names)?;
        let mut depends = parse_qi_depends(&records);
        depends.retain(|name, _| names.contains(name));
        let mut providers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for text in [self.read(&["-Qi"])?, records] {
            for (package, provided) in parse_qi_provides(&text) {
                for name in provided {
                    providers.entry(name).or_default().insert(package.clone());
                }
            }
        }
        for reached in depends.values_mut() {
            let by: Vec<String> = reached
                .iter()
                .filter_map(|name| providers.get(name))
                .flatten()
                .cloned()
                .collect();
            reached.extend(by);
        }
        let mut survey = Survey {
            depends,
            ..Survey::default()
        };
        if survey.depends.contains_key(BASE) {
            survey.metapackages.insert(BASE.to_string());
        }
        for name in KERNELS {
            if survey.depends.contains_key(*name) {
                survey.keep.insert((*name).to_string());
            }
        }
        Ok(survey)
    }

    /// `pacman -Rs --noconfirm -- <names…>`: the exact apply's removal (LD-375). Not `-Rns`:
    /// a configuration file a person changed is kept as a `.pacsave` beside where it was.
    fn exact_removal(&self, remove: &[String]) -> Result<Vec<Invocation>, Diagnostic> {
        if remove.is_empty() {
            return Ok(Vec::new());
        }
        let mut args: Vec<String> = ["-Rs", "--noconfirm"]
            .iter()
            .map(|a| (*a).to_string())
            .collect();
        names_after_options(&mut args, remove);
        Ok(vec![self.invocation(&args)?])
    }

    /// `pacman -Sg -- <names…>`: one `group member` line per member of each name that is a
    /// group. It only ever sharpens a refusal, so a failure to answer is no group at all.
    fn groups(&self, names: &[String]) -> Result<BTreeMap<String, Vec<String>>, Diagnostic> {
        if names.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut args = vec!["-Sg".to_string()];
        names_after_options(&mut args, names);
        let invocation = self.invocation(&args)?;
        let Ok(output) = invocation.command().output() else {
            return Ok(BTreeMap::new());
        };
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut words = line.split_whitespace();
            if let (Some(group), Some(member)) = (words.next(), words.next())
                && names.iter().any(|name| name == group)
            {
                out.entry(group.to_string())
                    .or_default()
                    .push(member.to_string());
            }
        }
        Ok(out)
    }

    /// pacman has no `dpkg --audit`. What it leaves behind when it does not finish is its
    /// transaction lock, `/var/lib/pacman/db.lck`, and this is the one question this backend
    /// answers by looking at the filesystem rather than by running a command.
    ///
    /// The lock is reported whenever it is there. pacman writes nothing into it — not a pid, not
    /// a start time — so "is a pacman running?" cannot be answered from the lock itself, and the
    /// alternative would be walking the process table and guessing from a name. Lodi does not
    /// guess about a machine it cannot see: it says the lock is there, names it, and leaves both
    /// the judgement and the removal to the operator. Lodi itself holds the apply lock while it
    /// asks, so the one pacman it could have started is not running.
    fn unclean(&self) -> Result<Vec<String>, Diagnostic> {
        let lock = self.root.join(DB_LOCK);
        if lock.symlink_metadata().is_ok() {
            return Ok(vec![format!(
                "{} is present, so a pacman transaction did not finish",
                lock.display()
            )]);
        }
        Ok(Vec::new())
    }
}

/// `pacman -Qi` output: one record per installed package, `Name` and `Depends On` inside it.
///
/// ```text
/// Name            : bash
/// Version         : 5.2.037-1
/// Depends On      : readline  libreadline.so=8-64  glibc  ncurses
/// ```
///
/// Records are separated by a blank line. A dependency may carry a version relation
/// (`glibc>=2.39`) or be a literal soname ABI tag (`libreadline.so=8-64`); both are reduced to
/// the bare name here, and `None` — pacman's word for an empty field — yields nothing. A record
/// with no `Depends On` at all still appears, with an empty set: this map's keys are the
/// installed set as `-Qi` reports it, which is what the closure walk needs.
pub fn parse_qi_depends(text: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut out = BTreeMap::new();
    let mut name: Option<String> = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            name = None;
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "Name" if !value.is_empty() => {
                name = Some(value.to_string());
                out.entry(value.to_string()).or_insert_with(BTreeSet::new);
            }
            "Depends On" => {
                if let Some(name) = &name {
                    let reached = dependency_names(value);
                    out.entry(name.clone())
                        .or_insert_with(BTreeSet::new)
                        .extend(reached);
                }
            }
            _ => {}
        }
    }
    out
}

/// `pacman -Qi` output: for each installed package, the names its `Required By` field lists.
///
/// pacman fills that field itself, and it resolves a requirement through what a package
/// **provides** (`libfoo.so`, `sh`), which is why it is read here rather than derived from the
/// `Depends On` names. A package nothing requires has `None` there and an empty set here.
pub fn parse_qi_required_by(text: &str) -> BTreeMap<String, BTreeSet<String>> {
    parse_qi_names(text, "Required By")
}

/// `pacman -Qi` or `-Si` output: for each record, the names its `Provides` field lists, with any
/// version relation or soname ABI tag dropped the way [`dependency_names`] drops them from a
/// `Depends On` name, so that the two meet (LD-412).
pub fn parse_qi_provides(text: &str) -> BTreeMap<String, BTreeSet<String>> {
    parse_qi_names(text, "Provides")
}

/// For each record's `Name`, the names one list field holds, over as many lines as pacman lays
/// it out on.
fn parse_qi_names(text: &str, label: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut name: Option<String> = None;
    let mut in_field = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            name = None;
            in_field = false;
            continue;
        }
        // A long field continues on lines that begin with whitespace and carry no label.
        if line.starts_with(char::is_whitespace) && in_field {
            if let Some(name) = &name {
                out.entry(name.clone())
                    .or_default()
                    .extend(dependency_names(line.trim()));
            }
            continue;
        }
        in_field = false;
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "Name" if !value.is_empty() => {
                name = Some(value.to_string());
                out.entry(value.to_string()).or_default();
            }
            key if key == label => {
                in_field = true;
                if let Some(name) = &name {
                    out.entry(name.clone())
                        .or_default()
                        .extend(dependency_names(value));
                }
            }
            _ => {}
        }
    }
    out
}

/// The bare names a `Depends On` field lists: whitespace separated, with any version relation
/// or soname ABI tag dropped, and pacman's `None` yielding nothing.
pub fn dependency_names(field: &str) -> Vec<String> {
    if field == "None" {
        return Vec::new();
    }
    let mut out = Vec::new();
    for word in field.split_whitespace() {
        let name = word
            .split(['>', '<', '=', ':'])
            .next()
            .unwrap_or("")
            .trim_end_matches(".so")
            .trim();
        if !name.is_empty() {
            out.push(name.to_string());
        }
    }
    out
}

/// One of pacman's own "not in the databases" lines, under `LC_ALL=C`.
///
/// It is matched by its ending, which is the part pacman does not vary: the middle carries the
/// name and, for a name that looks like a group or a repository, the wording of the front of the
/// line changes.
fn is_not_found(line: &str) -> bool {
    line.starts_with("error:") && line.ends_with("was not found")
}

/// libalpm's line for a download that failed for a reason that passes (LD-386) — a timeout, a
/// connection that failed or was cut off, an HTTP 500, 502, 503 or 504 — without its `error:`.
/// `None` for anything else: a 404 is an answer. `text` is pacman's output, its lines joined by
/// newlines or by `; ` as a failed invocation reports them after its command line.
pub fn passing_download_failure(text: &str) -> Option<String> {
    const PASSES: [&str; 13] = [
        "timed out",
        "Operation too slow",
        "Failed to connect",
        "Could not connect",
        "Couldn't connect",
        "Connection reset",
        "Recv failure",
        "Send failure",
        "Empty reply from server",
        "returned error: 500",
        "returned error: 502",
        "returned error: 503",
        "returned error: 504",
    ];
    text.split(['\n', ';'])
        .filter_map(|line| {
            line.find("failed retrieving file ")
                .map(|at| line[at..].trim())
        })
        .find(|line| PASSES.iter().any(|p| line.contains(p)))
        .map(str::to_string)
}

/// `pacman -Si` output: one record per name, `Name` and `Version` inside it.
///
/// ```text
/// Repository      : extra
/// Name            : tree
/// Version         : 2.1.1-1
/// Description     : A directory listing program displaying a depth indented list of files
/// ```
///
/// A record is ended by the blank line pacman prints between them, and a `Name` with no
/// `Version` after it contributes nothing: a candidate is a name **and** the version that name
/// would install, and half of that is not an answer.
pub fn parse_si(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut name: Option<String> = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            name = None;
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "Name" if !value.is_empty() => name = Some(value.to_string()),
            "Version" if !value.is_empty() => {
                if let Some(name) = name.take() {
                    out.insert(name, value.to_string());
                }
            }
            _ => {}
        }
    }
    out
}

/// The repositories Arch itself publishes, by the names pacman knows them under.
///
/// It is a committed list of exact names, not a pattern (LD-80), and it is only half the test:
/// the other half is `pacman-conf --repo-list`, the machine's own statement of which
/// repositories are enabled, so a name is called base only when this machine really has that
/// repository configured. `catalogue/bases/arch.toml` names the same set for the container
/// bases.
pub const BASE_REPOSITORIES: &[&str] = &[
    "core",
    "core-testing",
    "extra",
    "extra-testing",
    "multilib",
    "multilib-testing",
];

/// Whether this root has a sync database at all: a `.db` under `var/lib/pacman/sync`.
///
/// It is read from the directory and not inferred from an empty `-Si` answer, because a machine
/// every one of whose chosen names is an AUR build answers `-Si` with nothing too, and that
/// machine's index is perfectly good. A machine with no database has never been told what any
/// repository offers, so no origin it could state would mean anything: see [`Origin::Unchecked`].
///
/// This is the import's reading and the mirror of [`super::apt::has_index`]. No apply path
/// reaches it; `refresh` is what an apply uses to make the answer true.
pub fn has_index(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root.join("var/lib/pacman/sync")) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "db")
    })
}

/// `pacman -Si <names…>` output: for each name, every repository that offers it.
///
/// A name several repositories offer has one record per repository, so the answer is a set and
/// not one value: a package `chaotic-aur` and `extra` both carry can be installed on a fresh
/// machine from `extra`, and is declared.
pub fn parse_si_repositories(text: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut repository: Option<String> = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            repository = None;
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "Repository" if !value.is_empty() => repository = Some(value.to_string()),
            "Name" if !value.is_empty() => {
                let entry = out.entry(value.to_string()).or_default();
                if let Some(repository) = &repository {
                    entry.insert(repository.clone());
                }
            }
            _ => {}
        }
    }
    out
}

/// `pacman-conf --repo-list`: one enabled repository name per line, in configuration order.
pub fn parse_repo_list(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// Where one installed version came from, given the repositories that offer its name and the
/// machine's own enabled set.
///
/// `enabled` empty means `pacman-conf` could not be asked at all, in which case the family's
/// own repository names decide alone — the honest fallback, and never a name declared that the
/// stricter test would have left out.
pub fn classify(repositories: &BTreeSet<String>, enabled: &BTreeSet<String>) -> Origin {
    let is_base = |name: &String| {
        BASE_REPOSITORIES.contains(&name.as_str()) && (enabled.is_empty() || enabled.contains(name))
    };
    if repositories.iter().any(is_base) {
        return Origin::Base;
    }
    match repositories.iter().next() {
        Some(repository) => Origin::ThirdParty(repository.clone()),
        None => Origin::LocalOnly,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pacman() -> Pacman {
        let mut programs = BTreeMap::new();
        programs.insert("pacman".to_string(), PathBuf::from("/usr/bin/pacman"));
        Pacman {
            root: PathBuf::from("/"),
            partial: false,
            programs,
            refused: BTreeMap::new(),
            operation: Operation::Apply,
        }
    }

    fn partial() -> Pacman {
        Pacman {
            partial: true,
            ..pacman()
        }
    }

    fn rooted() -> Pacman {
        Pacman {
            root: PathBuf::from("/srv/elsewhere"),
            ..pacman()
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|n| (*n).to_string()).collect()
    }

    /// The layout a pacman 7 guest actually prints: the field's first value sits on the label
    /// line, every further value is an indented line of its own, and the verdict follows the
    /// path in brackets. A guest run of M-Import T-5 found this build reading **nothing at all**
    /// from it, because the only layout it understood was the older verdict-first one; both are
    /// read now (LD-302), and neither an unmodified row nor a `None` field is a change.
    #[test]
    fn a_backup_row_is_read_in_the_layout_pacman_prints_it() {
        let recorded = "Name            : nano\n\
                        Version         : 9.2-1\n\
                        Backup Files    : /etc/nanorc [modified]\n\
                                          /etc/nanorc.d/x [unmodified]\n\
                        Extended Data   : pkgtype=pkg\n\
                        \n\
                        Name            : wget\n\
                        Backup Files    : /etc/wgetrc [unreadable]\n\
                        Description     : a downloader\n\
                        \n\
                        Name            : glibc\n\
                        Backup Files    : None\n\
                        \n\
                        Name            : older-pacman\n\
                        Backup Files    :\n\
                        MODIFIED\t/etc/makepkg.conf\n\
                        UNMODIFIED\t/etc/pacman.conf\n";
        let rows = parse_qii_backups(recorded);
        let changed: Vec<&str> = rows
            .iter()
            .filter(|c| c.status == ConffileStatus::Modified)
            .map(|c| c.path.as_str())
            .collect();
        assert_eq!(changed, ["/etc/nanorc", "/etc/makepkg.conf"], "{rows:?}");
        let undetermined: Vec<&str> = rows
            .iter()
            .filter(|c| c.status == ConffileStatus::Undetermined)
            .map(|c| c.path.as_str())
            .collect();
        assert_eq!(
            undetermined,
            ["/etc/wgetrc"],
            "a file pacman could not read is judged, not dropped: {rows:?}"
        );
        let paths: Vec<&str> = rows.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/etc/nanorc",
                "/etc/nanorc.d/x",
                "/etc/wgetrc",
                "/etc/makepkg.conf",
                "/etc/pacman.conf"
            ],
            "every row of either layout is read, and no other line is"
        );
    }

    #[test]
    fn the_default_transaction_is_a_full_upgrade() {
        let invocations = pacman().transaction(&names(&["tree", "jq"]), &[]).unwrap();
        assert_eq!(invocations.len(), 1, "one transaction, always");
        assert_eq!(
            invocations[0].command_line(),
            "pacman -Syu --noconfirm -- tree jq"
        );
        assert!(pacman().transaction(&[], &[]).unwrap().is_empty());
    }

    /// The partial mode exists, it is reached only by the flag whose name says it is
    /// unsupported, and it is the only place `-S` appears without `-Syu`.
    #[test]
    fn the_partial_mode_is_needed_and_never_syncs() {
        let invocations = partial().transaction(&names(&["tree"]), &[]).unwrap();
        assert_eq!(
            invocations[0].command_line(),
            "pacman -S --needed --noconfirm -- tree"
        );
    }

    /// A hold is `--ignore` on lodi's own transaction and nothing else: there is no command that
    /// sets it, and nothing this backend can build ever names `pacman.conf` (D11).
    #[test]
    fn a_hold_is_an_ignore_on_the_transaction_and_never_a_file() {
        let pacman = pacman();
        let invocations = pacman
            .transaction_ignoring(&names(&["tree", "jq"]), &[], &names(&["glibc", "linux"]))
            .unwrap();
        assert_eq!(
            invocations[0].command_line(),
            "pacman -Syu --noconfirm --ignore glibc --ignore linux -- tree jq"
        );
        assert!(
            pacman
                .holds(&names(&["glibc"]), &names(&["linux"]))
                .unwrap()
                .is_empty()
        );
        assert!(!super::super::Shape::of(Distro::Arch).hold_is_machine_state);
    }

    /// Every argv this backend can build, in **both** modes, and the words none of them may
    /// ever contain. It is the check M-0.5's A12/A14 source scan used to make textually, made
    /// against the backend itself now that Arch really has one.
    #[test]
    fn the_backend_never_syncs_alone_collects_orphans_or_touches_a_key() {
        for mode in [pacman(), partial()] {
            no_argv_of_this_backend_syncs_alone(&mode);
        }
        let pacman = pacman();
        let mut every: Vec<String> = Vec::new();
        every.extend(
            pacman
                .refresh()
                .unwrap()
                .iter()
                .map(Invocation::command_line),
        );
        every.extend(
            pacman
                .transaction_ignoring(&names(&["tree"]), &[], &names(&["glibc"]))
                .unwrap()
                .iter()
                .map(Invocation::command_line),
        );
        every.extend(
            pacman
                .removal(&names(&["zip"]))
                .unwrap()
                .iter()
                .map(Invocation::command_line),
        );
        every.extend(
            pacman
                .marks(&names(&["tree"]), &names(&["jq"]))
                .unwrap()
                .iter()
                .map(Invocation::command_line),
        );
        every.extend(
            pacman
                .holds(&names(&["glibc"]), &[])
                .unwrap()
                .iter()
                .map(Invocation::command_line),
        );
        assert_eq!(
            every,
            [
                "pacman -Syu --noconfirm --ignore glibc -- tree",
                "pacman -Rns --noconfirm -- zip",
                "pacman -D --asdeps -- tree",
                "pacman -D --asexplicit -- jq",
            ]
        );
    }

    /// The word list, applied to every argv one mode of the backend can build — including the
    /// read-only queries, which are argv the machine sees just as much as the mutations are.
    fn no_argv_of_this_backend_syncs_alone(pacman: &Pacman) {
        let some = names(&["tree"]);
        let mut every: Vec<String> = Vec::new();
        for built in [
            pacman.refresh(),
            pacman.transaction_ignoring(&some, &[], &some),
            pacman.transaction(&some, &[]),
            pacman.removal(&some),
            pacman.marks(&some, &some),
            pacman.holds(&some, &some),
        ] {
            every.extend(built.unwrap().iter().map(Invocation::command_line));
        }
        for query in [
            vec!["-Q".to_string()],
            vec!["-Qe".to_string()],
            vec!["-Slq".to_string()],
            vec!["-Si".to_string(), "tree".to_string()],
        ] {
            every.push(pacman.invocation(&query).unwrap().command_line());
        }
        assert!(every.len() >= 5, "the backend really built something");
        let dated = crate::hostscope::pin::arch::render_config("2026-09-01T00:00:00Z").unwrap();
        assert!(
            dated.contains("SigLevel = Required DatabaseOptional\n"),
            "private Arch config keeps required package signatures"
        );
        for line in &every {
            for forbidden in [
                "-Sy ",
                "-Syy",
                "-Sc",
                "-Qdt",
                "pacman-key",
                "SigLevel",
                "pacman.conf",
                "db.lck",
                "sudo",
                "doas",
            ] {
                assert!(!line.contains(forbidden), "{line} contains {forbidden}");
            }
            // `-Syu` is the only operation that may begin with `-Sy`. The partial mode's `-S`
            // is a plain install: it never carries a `y`, so it can never refresh anything.
            assert!(
                !line.starts_with("pacman -Sy") || line.starts_with("pacman -Syu"),
                "{line} refreshes without upgrading"
            );
        }
    }

    /// `--root DIR` puts pacman's own root options after the operation, and `/` puts none there
    /// — which is why the argv a guest sees is the one T-5 fixes.
    #[test]
    fn a_root_that_is_not_the_machine_carries_the_root_options() {
        let rooted = rooted();
        let db = "--root /srv/elsewhere --dbpath /srv/elsewhere/var/lib/pacman";
        assert_eq!(
            rooted.transaction(&names(&["tree"]), &[]).unwrap()[0].command_line(),
            format!("pacman -Syu {db} --noconfirm -- tree")
        );
        assert_eq!(
            rooted.removal(&names(&["zip"])).unwrap()[0].command_line(),
            format!("pacman -Rns {db} --noconfirm -- zip")
        );
        assert_eq!(
            rooted.marks(&names(&["tree"]), &[]).unwrap()[0].command_line(),
            format!("pacman -D {db} --asdeps -- tree")
        );
        assert_eq!(
            rooted
                .invocation(&["-Q".to_string()])
                .unwrap()
                .command_line(),
            format!("pacman -Q {db}")
        );
        assert_eq!(
            pacman().transaction(&names(&["tree"]), &[]).unwrap()[0].command_line(),
            "pacman -Syu --noconfirm -- tree"
        );
    }

    #[test]
    fn a_missing_program_is_a_runtime_error_that_names_it() {
        let pacman = Pacman {
            root: PathBuf::from("/"),
            partial: false,
            programs: BTreeMap::new(),
            refused: BTreeMap::new(),
            operation: Operation::Apply,
        };
        let error = pacman.removal(&names(&["zip"])).unwrap_err();
        assert_eq!(error.code, "E_NO_RUNTIME");
        assert!(error.to_string().contains("pacman"));
    }

    /// The hint of a missing program names the command that was run, whichever it is.
    #[test]
    fn a_missing_program_names_the_command_that_was_run() {
        for operation in [Operation::Plan, Operation::Apply, Operation::Import] {
            let pacman = Pacman {
                programs: BTreeMap::new(),
                operation,
                ..pacman()
            };
            let text = pacman.removal(&names(&["zip"])).unwrap_err().to_string();
            assert!(
                text.contains(&format!("run `{}`", operation.command())),
                "{text}"
            );
            for other in [Operation::Plan, Operation::Apply, Operation::Import] {
                if other != operation {
                    assert!(!text.contains(other.command()), "{text}");
                }
            }
        }
    }

    /// `Required By` is pacman's own resolution, through provides, and a long field wraps.
    #[test]
    fn required_by_is_read_as_pacman_states_it() {
        let text = "Name            : libfoo\nVersion         : 1.0-1\n\
                    Provides        : libfoo.so=1-64\nDepends On      : glibc\n\
                    Required By     : app  tool\n                  viewer\n\
                    Install Reason  : Explicitly installed\n\n\
                    Name            : app\nVersion         : 2.0-1\n\
                    Depends On      : libfoo.so=1-64\nRequired By     : None\n\n";
        let map = parse_qi_required_by(text);
        assert_eq!(
            map["libfoo"].iter().cloned().collect::<Vec<_>>(),
            ["app", "tool", "viewer"]
        );
        assert!(map["app"].is_empty());
    }

    /// `Provides` is read the way a `Depends On` name is (LD-412): a soname and its ABI tag come
    /// to the name `app`'s dependency on it comes to, so an arriving package's requirement meets
    /// the installed package that provides it.
    #[test]
    fn provides_is_read_so_that_it_meets_a_dependency() {
        let text = "Repository      : core\nName            : libfoo\nVersion         : 1.0-1\n\
                    Provides        : libfoo.so=1-64  foo-compat=1.0\n\
                    Depends On      : glibc\n\n\
                    Name            : app\nVersion         : 2.0-1\nProvides        : None\n\
                    Depends On      : libfoo.so=1-64\n\n";
        let provides = parse_qi_provides(text);
        assert_eq!(
            provides["libfoo"].iter().cloned().collect::<Vec<_>>(),
            ["foo-compat", "libfoo"]
        );
        assert!(provides["app"].is_empty());
        let depends = parse_qi_depends(text);
        assert!(provides["libfoo"].is_superset(&depends["app"]));
    }

    /// The exact removal is `-Rs`, never `-Rns`, and its simulation only prints.
    #[test]
    fn the_exact_removal_keeps_changed_configuration() {
        let pacman = pacman();
        let names = vec!["tree".to_string()];
        let removal = pacman.exact_removal(&names).unwrap();
        assert_eq!(removal[0].args, ["-Rs", "--noconfirm", "--", "tree"]);
        assert!(pacman.exact_removal(&[]).unwrap().is_empty());
    }

    #[test]
    fn si_output_gives_the_candidate_of_each_record() {
        let text = "Repository      : extra\nName            : tree\nVersion         : \
                    2.1.1-1\nDescription     : A directory listing program\n\nRepository      \
                    : core\nName            : jq\nVersion         : 1.7.1-1\n";
        let parsed = parse_si(text);
        assert_eq!(parsed.get("tree").map(String::as_str), Some("2.1.1-1"));
        assert_eq!(parsed.get("jq").map(String::as_str), Some("1.7.1-1"));
        assert_eq!(parsed.len(), 2);
        // A record with a name and no version is not a candidate.
        assert!(parse_si("Name            : half\n").is_empty());
    }

    #[test]
    fn only_pacmans_own_not_found_lines_are_tolerated() {
        assert!(is_not_found(
            "error: package 'lodi-absent-probe' was not found"
        ));
        assert!(!is_not_found(
            "error: failed to init transaction (unable to lock database)"
        ));
        assert!(!is_not_found(
            "warning: database file for 'core' was not found"
        ));
    }

    #[test]
    fn a_database_lock_left_behind_is_not_clean_and_is_never_removed() {
        let dir = std::env::temp_dir().join(format!("lodi-pacman-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("var/lib/pacman")).expect("the scratch database");
        let pacman = Pacman {
            root: dir.clone(),
            ..pacman()
        };
        assert!(pacman.unclean().unwrap().is_empty());
        let lock = dir.join(DB_LOCK);
        fs::write(&lock, "").expect("the lock");
        let lines = pacman.unclean().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("db.lck"), "{lines:?}");
        assert!(lock.exists(), "lodi never removes the lock file");
        assert!(
            super::super::Shape::of(Distro::Arch)
                .repair
                .contains("db.lck")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_index_that_is_not_there_has_no_age() {
        assert!(
            Pacman {
                root: PathBuf::from("/nonexistent-root"),
                ..pacman()
            }
            .index_age()
            .is_none()
        );
    }
}
