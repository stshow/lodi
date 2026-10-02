//! The package-manager seam: how an apply reaches `apt-get` or `pacman`, and nothing else.
//!
//! 0.6 lands the trait, the invocation model and the `match` on the distribution. T-4 filled in
//! the `apt` arm for Debian and Ubuntu, and T-5 the `pacman` arm for Arch, so every distribution
//! the safety gate accepts now has a backend and a `[packages]` declaration is behaviour on all
//! three. The seam itself is the part that decides what a root shell would run:
//!
//! - every invocation is an **argv vector**, never a shell string;
//! - the program is resolved **by bare name** (design call D3, LD-115) — on the root `/` against
//!   the fixed path and never the caller's `PATH`, and only if root owns it and nobody else can
//!   write it ([`super::safety::resolve_program`], LD-357) — and the absolute path it resolved
//!   to goes into the journal beside the argv;
//! - every invocation runs with the environment cleared and exactly the fixed non-interactive
//!   set of [`noninteractive_env`] plus `PATH=`[`super::safety::FIXED_PATH`] (LD-357): no
//!   variable of the caller's reaches a package manager, an apply never stops on a prompt, and
//!   no locale changes a tool's output.
//!
//! A package manager runs in a process that is already root: [`crate::elevate`] gives a lodi
//! started as the user one (`spec/10` §6's `system { sudo = … }` is not read, design call D12).
//!
//! # Where the two families differ, and how the plan learns it
//!
//! apt and pacman do not arrange one apply the same way, and the differences are not the plan's
//! to guess. Each backend states them as data in its own [`Shape`]:
//!
//! | | apt | pacman | dnf |
//! | --- | --- | --- | --- |
//! | index refresh | its own action, `apt-get update` | inside `pacman -Syu` | `dnf5 makecache` |
//! | removals | inside the transaction (`name-`) | `pacman -Rns`, a second action | `dnf5 remove` |
//! | a `hold` | machine state, `apt-mark hold` | `--ignore` on lodi's actions (D11) | `--exclude` |
//!
//! A backend that refreshes inside its transaction has no separate index action to build, and
//! there is no `pacman -Sy`: fetching the databases without upgrading is exactly the partial
//! state Arch does not support, which is the whole subject of design call D10.

pub mod apt;
pub mod dnf;
pub mod pacman;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::diag::Diagnostic;
use crate::progress::{Event, Sink, Stream, pump, read_lines};

use super::safety::{Distro, Operation, PackageManager};

/// How old the distribution's package index may be before an apply refreshes it. `spec/09`'s
/// default, fixed in this build because there is no configuration file to move it (D12, LD-124).
pub const REFRESH_AFTER: Duration = Duration::from_secs(6 * 60 * 60);

/// One process an apply would run, exactly as it would run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The bare name the seam resolved through `PATH`.
    pub program: String,
    /// The absolute path that resolution produced; recorded in the journal.
    pub path: PathBuf,
    pub args: Vec<String>,
    /// The fixed environment of [`noninteractive_env`], plus anything a backend adds.
    pub env: Vec<(String, String)>,
}

impl Invocation {
    pub fn new(pm: &PackageManager, program: &str, args: &[&str]) -> Option<Invocation> {
        Some(Invocation {
            program: program.to_string(),
            path: pm.path_of(program)?.to_path_buf(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            env: noninteractive_env(),
        })
    }

    /// The same, for a backend that resolved the program itself. A backend needs tools the gate
    /// does not require to be present (`apt-cache`, `apt-mark`), and resolves them by bare name
    /// exactly as the gate does ([`super::safety::resolve_program`]).
    pub fn resolved(
        program: &str,
        path: &std::path::Path,
        args: &[String],
        env: Vec<(String, String)>,
    ) -> Invocation {
        Invocation {
            program: program.to_string(),
            path: path.to_path_buf(),
            args: args.to_vec(),
            env,
        }
    }

    /// The argv as one line, for the plan and for a journal a human reads. It is a rendering,
    /// never something that is handed to a shell.
    pub fn command_line(&self) -> String {
        let mut line = self.program.clone();
        for arg in &self.args {
            line.push(' ');
            line.push_str(arg);
        }
        line
    }

    /// The `Command` this invocation is, with the environment it fixes: cleared, then exactly
    /// [`Invocation::env`] and the fixed `PATH`. Nothing here runs it.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.path);
        command.args(&self.args);
        command.env_clear();
        command.env("PATH", super::safety::FIXED_PATH);
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
    }
}

/// The environment every package-manager invocation gets (`spec/09` §5.1): no prompt, no pager,
/// and a stable locale so that what a tool prints does not depend on the operator's session.
pub fn noninteractive_env() -> Vec<(String, String)> {
    vec![
        ("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string()),
        (
            "DEBCONF_NONINTERACTIVE_SEEN".to_string(),
            "true".to_string(),
        ),
        ("LC_ALL".to_string(), "C".to_string()),
        ("LANG".to_string(), "C".to_string()),
        ("PAGER".to_string(), "cat".to_string()),
    ]
}

/// What the machine shows right now: the installed set and the two flags an apply maintains.
///
/// It is read once per plan and once again after a transaction, because a plan derived from a
/// remembered state would be a plan about a machine that no longer exists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    /// Installed package name -> installed version.
    pub installed: BTreeMap<String, String>,
    /// The names the package manager considers automatically installed.
    pub auto: BTreeSet<String>,
    /// The names the package manager will not touch until they are released.
    pub held: BTreeSet<String>,
}

/// How fundamental the distribution itself says a package is (`Priority` in a Debian package
/// record). It is the field the import baseline subtracts by: what the installer put on the
/// machine is `required`, `important` or `standard`, and what a person chose afterwards is not.
///
/// pacman has no such field, so every name on Arch is [`Priority::Unset`] and that family's
/// baseline is the dependency closure of `base` instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Required,
    Important,
    Standard,
    Optional,
    Extra,
    /// The family states no priority, or the record carried none.
    #[default]
    Unset,
}

impl Priority {
    /// The field as a package record spells it. An unrecognised word is [`Priority::Unset`],
    /// which is the safe answer: it subtracts nothing.
    pub fn from_field(text: &str) -> Priority {
        match text.trim() {
            "required" => Priority::Required,
            "important" => Priority::Important,
            "standard" => Priority::Standard,
            "optional" => Priority::Optional,
            "extra" => Priority::Extra,
            _ => Priority::Unset,
        }
    }

    /// Whether a package of this priority is part of what the distribution's own installer puts
    /// on a machine, and therefore not something the person chose.
    pub fn is_baseline(self) -> bool {
        matches!(
            self,
            Priority::Required | Priority::Important | Priority::Standard
        )
    }
}

/// Where the **installed** version of one package came from.
///
/// It is the question R-2 replaced candidate existence with (LD-NEXT). On apt an installed
/// package always has a candidate, because `apt-cache policy` counts `/var/lib/dpkg/status`
/// itself as a source at priority 100 — so "the index offers a candidate" was true of a
/// hand-installed `.deb`, of a package from a source the machine no longer has, and of every
/// package from an enabled third-party repository, and all of them were declared into
/// `[packages]` where an apply on a fresh machine stops at `E_UNKNOWN_PACKAGE`.
///
/// Every variant is derived from readings of the machine itself — the source lines of the
/// installed version, matched against the labels the package manager's own configuration gives
/// each source — and never from a table shipped in the binary (design call D3, LD-271).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    /// This distribution's own repositories, in a suite a fresh install of the same release
    /// enables by default. Only these names are declared.
    Base,
    /// This distribution's own repositories, but a suite a fresh install does not enable —
    /// `bookworm-backports`, `noble-proposed`. The name is declared, because the release's own
    /// repositories do offer it; the **version** is not what a fresh machine would install, and
    /// the emitted file says so beside the name.
    Suite(String),
    /// A repository that is not this distribution's own, by the label its own `Release` file
    /// gives it (`Docker, bookworm/stable`, `LP-PPA-…, noble/main`) — **never** by its address.
    ThirdParty(String),
    /// The installed version is known to the local package database and to no repository: a
    /// package file installed by hand, or a source the machine no longer has.
    LocalOnly,
    /// **The machine could not be asked.** It has no package index at all — no enabled source
    /// but its own database on apt, no sync database on pacman — so every answer above would be
    /// the same answer for everything installed, and none of them would mean anything.
    ///
    /// A name with this origin **is** declared, because the machine's own database is still the
    /// record of what a person chose, and an import that emitted an empty file on a machine
    /// whose index has simply never been fetched would be useless. What it is not is *checked*:
    /// the emitted file says so above the list, and the import says so once on standard error.
    /// A fresh cloud image is exactly this machine until `apt-get update` has run on it.
    Unchecked,
}

impl Origin {
    /// Whether a name with this origin is written into `[packages]`.
    pub fn is_declarable(&self) -> bool {
        matches!(self, Origin::Base | Origin::Suite(_) | Origin::Unchecked)
    }
}

/// What the import core reads beyond [`Backend::observe`], and what no apply ever needs.
///
/// It is **data, never an [`Invocation`]**: a survey cannot be turned into a mutation by any
/// caller, which is how `src/hostscope/import/` keeps its promise that nothing it builds changes
/// a machine. Every field is a projection of one read command the family already knows how to
/// run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Survey {
    /// The distribution's own priority for each installed name, where the family has one.
    pub priority: BTreeMap<String, Priority>,
    /// The names each installed package directly depends on — `Depends` and `Recommends` on apt,
    /// `Depends On` on pacman — with version relations and architecture qualifiers dropped. It is
    /// what the baseline walks to reach a metapackage's closure.
    pub depends: BTreeMap<String, BTreeSet<String>>,
    /// The installed names that are this distribution's **own** metapackages: a package whose
    /// section is `metapackages` on apt, and `base` on pacman.
    pub metapackages: BTreeSet<String>,
    /// Installed names that came from no repository this package manager knows: an AUR build, a
    /// hand-installed package file. They are named in the emitted comment block and never
    /// declared (D4, LD-272).
    pub foreign: BTreeSet<String>,
    /// Names the baseline keeps whatever else it subtracts: the family's own kernels and
    /// firmware, which a machine that is restored without them does not boot.
    pub keep: BTreeSet<String>,
}

/// The two readings [`Backend::origins`] classifies by, **before** it classifies them: which
/// enabled sources offer each named package's installed version, and what each enabled source's
/// own `Release` file says it is. It is what `src/hostscope/import/sources.rs` attributes a
/// package to a repository by, so that the attribution and the origin the import declares by are
/// one reading of the machine and never two (LD-367).
///
/// Like [`Survey`], it is data and never an [`Invocation`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Offers {
    /// For each name asked about, the sources that offer its **installed** version, as the
    /// package manager spells them (`https://… bookworm/stable amd64 Packages`), in its order,
    /// with its own database of installed packages left out. A name no source offers has an
    /// empty list.
    pub installed_from: BTreeMap<String, Vec<String>>,
    /// For each enabled source, spelled the same way, the `o=` its `Release` file states (empty
    /// where it states none).
    pub origin_of: BTreeMap<String, String>,
    /// For each enabled source, spelled the same way, the `n=` its `Release` file states (empty
    /// where it states none): what tells a Debian suite written by its alias (`oldstable`) from
    /// one a fresh install does not enable (LD-414).
    pub codename_of: BTreeMap<String, String>,
}

/// What the package manager says about one configuration file it tracks.
///
/// It is the whole of what the import capture is allowed to treat as a candidate: a file the
/// package manager itself records and reports on, never a file found by walking `/etc`. The two
/// families answer the question differently — dpkg records a digest and pacman states a verdict
/// — so the seam carries the **verdict**, and each family reaches it its own way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conffile {
    /// The absolute path as the package manager states it, inside the root.
    pub path: String,
    pub status: ConffileStatus,
}

/// What the package manager says happened to a configuration file it tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConffileStatus {
    /// The file on disk differs from the one the package shipped: a file the person changed.
    Modified,
    /// The file on disk is the one the package shipped, so there is nothing to import.
    Unmodified,
    /// The family could not say, and **nothing was opened to find out**: the file is not a
    /// regular file, or its mode denies read to *other*, which is the one case where answering
    /// the question would mean reading a file the capture is going to refuse anyway. It is a
    /// candidate, so that the refusal is named rather than silent.
    Undetermined,
}

/// What the package manager's own simulation says one change would do (LD-375). It is read by
/// a command that changes nothing — `pacman -Rsp`, `apt-get install -s` — and it is data, never
/// an [`Invocation`] a caller could run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Simulation {
    /// Every name the change would take off the machine: the ones asked for, and whatever the
    /// package manager's own rules take with them.
    pub removed: BTreeSet<String>,
    /// What apt would call "automatically installed and no longer required" once the change is
    /// made. pacman has no such list: its `-Rs` takes those names itself, into `removed`.
    pub orphans: BTreeSet<String>,
    /// Every name the change would install that is not installed now: what it was asked for, and
    /// what that pulls in. pacman's `-Rsp` installs nothing, so it is empty there.
    pub installed: BTreeSet<String>,
}

/// What a pinned transaction asks of the package manager beyond the plain one (M-Pin, LD-395).
/// The default asks nothing, and a transaction built with it is the plain transaction, byte for
/// byte.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pinning {
    /// Read the private dated source set staged below the root (`super::pin::STAGE`) instead of
    /// the machine's own sources and lists, which are neither read as a source nor written.
    pub private: bool,
    /// The plan names a downgrade: only then may the transaction install a lower version.
    pub allow_downgrades: bool,
    /// The plan moves a name the machine holds to another version: only then may the
    /// transaction change a held package.
    pub allow_change_held: bool,
}

/// How a family arranges the mutations of one apply.
///
/// It is **data, not behaviour**, and it is stated once, here, beside the `match` that chooses
/// the arm — because that is the one place in Lodi allowed to know that the families differ at
/// all. Everything upstream of the seam reads this and branches on a named difference, never on
/// a distribution: the plan asks "are removals a second action here", not "is this Arch".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    /// The index refresh is its own action before the transaction (`apt-get update`), rather
    /// than part of the transaction itself (`pacman -Syu`).
    ///
    /// When it is false there is no index action at all, and a name the index cannot place is
    /// therefore settled at plan time rather than carried past a refresh that never happens as
    /// its own step.
    pub refresh_is_an_action: bool,
    /// Removals are a second action after the install (`pacman -Rns`), rather than travelling
    /// inside the install transaction (apt's `name-` suffix).
    pub removals_are_a_second_action: bool,
    /// A `hold` is state the package manager itself keeps (`apt-mark hold`), rather than an
    /// option Lodi puts on its own transactions and nothing else (`pacman --ignore`, D11).
    pub hold_is_machine_state: bool,
    /// This family has a partial-upgrade mode at all. Only Arch does: on Debian and Ubuntu
    /// installing without upgrading the machine is the ordinary, supported thing, and Lodi
    /// already does it.
    pub has_partial_upgrade: bool,
    /// A plan line names the build it installs or removes, as the package manager spells it
    /// (dnf: `EPOCH:VERSION-RELEASE.ARCH`, LD-432). Only dnf does; the other families' lines
    /// are unchanged.
    pub names_builds: bool,
    /// The one sentence an ambiguous stop adds: the operator's own next step for a package
    /// database this family left part way through. Lodi names it and never runs it.
    pub repair: &'static str,
}

/// The dpkg/apt family: `apt-get update` is its own command, one `apt-get install` solves the
/// installs and the removals together, and a hold is machine state until it is released.
const APT: Shape = Shape {
    refresh_is_an_action: true,
    removals_are_a_second_action: false,
    hold_is_machine_state: true,
    has_partial_upgrade: false,
    names_builds: false,
    repair: "a package database left part way through is put right by hand with \
             `sudo dpkg --configure -a`, which lodi does not run for you",
};

/// pacman: the refresh travels inside `-Syu` because fetching the databases alone is the
/// partial state Arch does not support (D10), removals are their own `-Rns`, and a hold binds
/// Lodi's transactions only (D11). The repair sentence names `db.lck`, which is the file an
/// operator finds and the file Lodi will not remove for them.
const PACMAN: Shape = Shape {
    refresh_is_an_action: false,
    removals_are_a_second_action: true,
    hold_is_machine_state: false,
    has_partial_upgrade: true,
    names_builds: false,
    repair: "a pacman transaction left part way through leaves `var/lib/pacman/db.lck` behind; \
             read the tail of `/var/log/pacman.log`, and when no pacman is running remove that \
             lock file yourself — lodi does not remove it for you",
};

/// dnf5 (LD-432): `dnf5 makecache` is its own action, removals are a second `dnf5 remove` (whose
/// simulation says what it takes), and a hold is `--exclude` on lodi's own transactions only.
const DNF: Shape = Shape {
    refresh_is_an_action: true,
    removals_are_a_second_action: true,
    hold_is_machine_state: false,
    has_partial_upgrade: false,
    names_builds: true,
    repair: "an rpm transaction left part way through is found with `sudo dnf5 check` and put \
             right by hand, a duplicate with `sudo dnf5 remove --duplicates`, which lodi does not \
             run for you",
};

impl Shape {
    /// The shape of the family that serves this distribution.
    ///
    /// It is deliberately readable without building a backend.
    pub fn of(distro: Distro) -> Shape {
        match distro {
            Distro::Debian | Distro::Ubuntu => APT,
            Distro::Arch => PACMAN,
            Distro::Fedora => DNF,
        }
    }
}

/// What the two package managers must provide. The operations are the ones one apply needs and
/// no more: what is installed, what the index offers, the transaction, the removals, the marks
/// and the holds.
pub trait Backend {
    /// The distribution this backend serves.
    fn distro(&self) -> Distro;
    /// The installed set and the marks, as the machine shows them now.
    fn observe(&self) -> Result<Observed, Diagnostic>;
    /// The version the index would install for each name, absent for a name it does not know.
    fn candidates(&self, names: &[String]) -> Result<BTreeMap<String, String>, Diagnostic>;
    /// The names nearest a name the index does not know, closest first; empty when none is near.
    fn nearest(&self, name: &str) -> Vec<String>;
    /// Where the **installed** version of each named package came from.
    ///
    /// It is a read, like [`Backend::survey`], and it is the import's own question: an apply
    /// asks what the index would install, and only an import asks where what is already
    /// installed came from. The map carries an entry for every name it was given, so a caller
    /// never has to decide what a missing answer means.
    fn origins(&self, names: &[String]) -> Result<BTreeMap<String, Origin>, Diagnostic>;
    /// The uncollapsed readings behind [`Backend::origins`], for the import's attribution of a
    /// package to one repository. It is a read of the local index and makes no network request.
    ///
    /// The default is nothing: a family whose repositories the host scope does not arm (pacman)
    /// has no repository to attribute a package to.
    fn offers(&self, names: &[String]) -> Result<Offers, Diagnostic> {
        let _ = names;
        Ok(Offers::default())
    }
    /// What the import core reads and an apply does not: priorities, direct dependencies, this
    /// distribution's own metapackages, the foreign names and the kernels.
    ///
    /// It returns a [`Survey`] and never an [`Invocation`], so the import core cannot mutate a
    /// machine even by mistake; the commands behind it are reads of the package database.
    fn survey(&self) -> Result<Survey, Diagnostic>;
    /// Every configuration file this package manager tracks, with what it says about each.
    ///
    /// It is a **read**, like [`Backend::survey`]: it returns data and never an [`Invocation`],
    /// and it opens no file whose mode denies read to *other* (`ConffileStatus::Undetermined`
    /// is the answer there). `src/hostscope/import/files.rs` is its only caller.
    fn conffiles(&self) -> Result<Vec<Conffile>, Diagnostic>;
    /// How old the on-disk index is, or `None` when there is no index at all (which counts as
    /// stale: a machine that has never fetched an index cannot install anything).
    fn index_age(&self) -> Option<Duration>;
    /// Refresh the distribution's index. It is a mutation, so it is an action in the journal.
    ///
    /// A family whose [`Shape::refresh_is_an_action`] is false has no such action and returns
    /// nothing here: its refresh happens inside the transaction.
    fn refresh(&self) -> Result<Vec<Invocation>, Diagnostic>;
    /// The refresh [`Backend::refresh`] built has just succeeded. A family whose index does not
    /// show every refresh by itself records it here; the default does nothing.
    fn refreshed(&self) {}
    /// **One** transaction carrying every install, and every removal this family solves
    /// together with them. `remove` is empty for a family whose
    /// [`Shape::removals_are_a_second_action`] is true.
    fn transaction(
        &self,
        install: &[String],
        remove: &[String],
    ) -> Result<Vec<Invocation>, Diagnostic>;
    /// The same transaction, told which names it must leave alone.
    ///
    /// The default is the family whose holds are machine state: it has nothing to leave alone
    /// per transaction, so it never receives a name here and runs the ordinary transaction. A
    /// family whose [`Shape::hold_is_machine_state`] is false overrides this, and that override
    /// is the only place a hold reaches the package manager at all.
    fn transaction_ignoring(
        &self,
        install: &[String],
        remove: &[String],
        ignore: &[String],
    ) -> Result<Vec<Invocation>, Diagnostic> {
        debug_assert!(
            ignore.is_empty(),
            "a family whose holds are machine state never gets a name to ignore"
        );
        self.transaction(install, remove)
    }
    /// [`Backend::transaction`] for a pinned apply (M-Pin): `install` may carry `NAME=VERSION`,
    /// and `pinning` says what else the transaction may do. With the default `pinning` it is the
    /// plain transaction. A family with no pinned transaction refuses anything else.
    fn transaction_pinned(
        &self,
        install: &[String],
        remove: &[String],
        pinning: &Pinning,
    ) -> Result<Vec<Invocation>, Diagnostic> {
        if *pinning == Pinning::default() {
            return self.transaction(install, remove);
        }
        Err(Diagnostic::new(
            "E_UNSUPPORTED",
            format!(
                "a pinned transaction is not built for {} by this build",
                self.distro().name()
            ),
        ))
    }
    /// A pacman dated sync and local-file install, with Lodi-only holds. Only the pacman backend
    /// implements it; apt keeps its dated source-set transaction instead (LD-396).
    fn arch_pinned(
        &self,
        _install: &[String],
        _files: &[String],
        _ignore: &[String],
        _dated: bool,
    ) -> Result<Vec<Invocation>, Diagnostic> {
        Err(Diagnostic::new(
            "E_UNSUPPORTED",
            "this backend has no Arch pin transaction",
        ))
    }
    /// Pacman's read-only local-file dry run, run after the file's digest is checked and
    /// before the journalled transaction. The default backend has no local package file.
    fn arch_simulate(&self, _files: &[String]) -> Result<(), Diagnostic> {
        Err(Diagnostic::new(
            "E_UNSUPPORTED",
            "this backend has no Arch local-file dry run",
        ))
    }
    /// A Fedora pinned transaction (fk-1): the unpinned names and each pinned build's verified
    /// file from the pin stage, in one dnf5 install. Only the dnf backend implements it.
    fn fedora_pinned(
        &self,
        _install: &[String],
        _files: &[String],
        _ignore: &[String],
    ) -> Result<Vec<Invocation>, Diagnostic> {
        Err(Diagnostic::new(
            "E_UNSUPPORTED",
            "this backend has no Fedora pin transaction",
        ))
    }
    /// dnf5's own `-C --assumeno` of exactly that transaction, once the files are staged.
    fn fedora_simulate(
        &self,
        _install: &[String],
        _files: &[String],
        _ignore: &[String],
    ) -> Result<(), Diagnostic> {
        Err(Diagnostic::new(
            "E_UNSUPPORTED",
            "this backend has no Fedora pin transaction",
        ))
    }
    /// The refresh of the private dated source set a pinned transaction reads: into its own lists,
    /// never the machine's. Nothing for a family with no such set.
    fn private_refresh(&self) -> Result<Vec<Invocation>, Diagnostic> {
        Ok(Vec::new())
    }
    /// [`Backend::simulate`] of exactly the pinned transaction: what a pin the index cannot
    /// satisfy is refused by, before anything is installed (`E_PIN_UNSATISFIABLE`).
    fn simulate_pinned(
        &self,
        install: &[String],
        remove: &[String],
        pinning: &Pinning,
    ) -> Result<Simulation, Diagnostic> {
        if *pinning == Pinning::default() {
            return self.simulate(install, remove);
        }
        Err(Diagnostic::new(
            "E_UNSUPPORTED",
            format!(
                "a pinned transaction is not simulated for {} by this build",
                self.distro().name()
            ),
        ))
    }
    /// The second action of a family that does not solve removals with the installs.
    ///
    /// The default is no such action, which is what
    /// [`Shape::removals_are_a_second_action`] `== false` means: the removals already travelled
    /// in [`Backend::transaction`], and asking twice would remove them twice.
    fn removal(&self, remove: &[String]) -> Result<Vec<Invocation>, Diagnostic> {
        let _ = remove;
        Ok(Vec::new())
    }
    /// `mark_auto` and the manual marks for everything else.
    fn marks(&self, auto: &[String], manual: &[String]) -> Result<Vec<Invocation>, Diagnostic>;
    /// `hold`, and the release of what left it.
    fn holds(&self, hold: &[String], unhold: &[String]) -> Result<Vec<Invocation>, Diagnostic>;
    /// For each installed package, the installed packages that require it, as the package
    /// manager itself resolves requirement — through what a package provides, not only by its
    /// name (`Required By` of `pacman -Qi`). A family that decides removals by simulating the
    /// whole change instead answers with nothing, which is the default.
    fn required_by(&self) -> Result<BTreeMap<String, BTreeSet<String>>, Diagnostic> {
        Ok(BTreeMap::new())
    }
    /// What the package manager says installing `install` and removing `remove` would take off
    /// the machine, from its own simulation of exactly the command the apply would run. It is a
    /// read: nothing changes.
    fn simulate(&self, install: &[String], remove: &[String]) -> Result<Simulation, Diagnostic>;
    /// What installing `install` would bring onto the machine (LD-412): every package the
    /// transaction would install that is not installed now, each with the record
    /// [`Backend::survey`] reads of an installed package — the distribution's priority for it,
    /// whether it is one of the distribution's own metapackages, and the names it depends on.
    /// Every such package is a key of `depends`; on a family whose requirement is resolved
    /// through what packages provide, a name it depends on is also answered by the package that
    /// provides it. It is a read: nothing is installed. An exact plan reads the base system as
    /// the transaction will leave the machine, not as it finds it.
    fn arriving(&self, install: &[String]) -> Result<Survey, Diagnostic>;
    /// The removal of an exact apply (LD-375): the names given, and what the package manager's
    /// own rules take with them, keeping every configuration file a person changed. The default
    /// is no such action, for a family whose removals travel in its transaction.
    fn exact_removal(&self, remove: &[String]) -> Result<Vec<Invocation>, Diagnostic> {
        let _ = remove;
        Ok(Vec::new())
    }
    /// The members of each name that is a **group** of packages rather than a package, for a
    /// family that has groups. A name that is no group has no entry.
    fn groups(&self, names: &[String]) -> Result<BTreeMap<String, Vec<String>>, Diagnostic> {
        let _ = names;
        Ok(BTreeMap::new())
    }
    /// `[host] packages = "exact"`: install and simulate with the package manager's own default
    /// for what a package recommends, rather than 1.1.0's narrower transaction (LD-375 validator
    /// repair). The default does nothing: a family with no such option has nothing to keep.
    fn keep_default_recommends(&mut self) {}
    /// Whatever the package database says is not in a clean state, one line per complaint. An
    /// empty vector means the database is clean; anything else makes an interrupted transaction
    /// **ambiguous**, and Lodi never repairs it itself.
    fn unclean(&self) -> Result<Vec<String>, Diagnostic>;

    /// The package a line of this family's output names, if any: the step list counts these.
    fn item(&self, _line: &str) -> Option<String> {
        None
    }

    /// The download-only form of one of this family's transaction invocations, which fetches
    /// what it would install into the package cache and changes nothing else (#694).
    fn download(&self, _transaction: &Invocation) -> Option<Invocation> {
        None
    }
}

/// The `match` on the distribution: `apt` for Debian and Ubuntu (T-4), `pacman` for Arch (T-5).
///
/// The `match` is **total** since M-0.6 T-6 converged the two lanes: every distribution the
/// safety gate accepts has an arm, so there is no "no backend" answer to give and no `Option` to
/// unwrap. A `[packages]` declaration is therefore never refused for want of a backend on any
/// machine this build runs on, and `E_UNSUPPORTED` has left that path entirely.
///
/// `operation` is the command being run. A backend uses it for one thing: the hint of an
/// `E_NO_RUNTIME` names that command, so a plan or an import is never told to "run the apply".
pub fn backend_for(
    distro: Distro,
    root: &std::path::Path,
    operation: Operation,
) -> Box<dyn Backend> {
    match distro {
        Distro::Debian | Distro::Ubuntu => Box::new(apt::Apt::new(distro, root, operation)),
        Distro::Arch => Box::new(pacman::Pacman::new(root, operation)),
        Distro::Fedora => Box::new(dnf::Dnf::new(root, operation)),
    }
}

/// Run one invocation to completion and fail with what it printed.
///
/// `E_APPLY` (exit 8) is the right code for every failure here: the apply stopped in the middle
/// of a transaction it had already written down, which is exactly what that status means.
pub fn run(invocation: &Invocation) -> Result<(), Diagnostic> {
    run_reporting(invocation, &mut crate::progress::Silent, &|_| None)
}

/// [`run`], read line by line while it runs (#694): the command and every line of both streams
/// go to `sink`, each line `items` recognises becomes an item, and the sink ticks while the
/// process is quiet. The exit status and the `E_APPLY` diagnostic are [`run`]'s.
pub fn run_reporting(
    invocation: &Invocation,
    sink: &mut dyn Sink,
    items: &dyn Fn(&str) -> Option<String>,
) -> Result<(), Diagnostic> {
    use std::process::Stdio;
    sink.event(Event::Command {
        line: invocation.command_line(),
    });
    let mut child = invocation
        .command()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| spawn_failed(invocation, &e.to_string()))?;
    let (send, lines) = std::sync::mpsc::channel();
    let readers = [
        child
            .stdout
            .take()
            .map(|out| read_lines(out, Stream::Stdout, send.clone())),
        child
            .stderr
            .take()
            .map(|err| read_lines(err, Stream::Stderr, send)),
    ];
    let mut kept: [Vec<String>; 2] = [Vec::new(), Vec::new()];
    pump(&lines, sink, |(stream, line), sink| {
        let kept = &mut kept[usize::from(stream == Stream::Stderr)];
        if !line.trim().is_empty() {
            if kept.len() == TAIL {
                kept.remove(0);
            }
            kept.push(line.clone());
        }
        let item = items(&line);
        sink.event(Event::Output { stream, line });
        if let Some(name) = item {
            sink.event(Event::Item { name });
        }
    });
    for reader in readers.into_iter().flatten() {
        let _ = reader.join();
    }
    let status = child
        .wait()
        .map_err(|e| spawn_failed(invocation, &e.to_string()))?;
    if status.success() {
        return Ok(());
    }
    let [stdout, stderr] = kept;
    let quoted = if stderr.is_empty() { stdout } else { stderr };
    Err(Diagnostic::new(
        "E_APPLY",
        format!(
            "`{}` exited {}: {}",
            invocation.command_line(),
            status
                .code()
                .map_or_else(|| "on a signal".to_string(), |c| c.to_string()),
            if quoted.is_empty() {
                "it printed nothing".to_string()
            } else {
                quoted.join("; ")
            }
        ),
    ))
}

/// Run one invocation and take its standard output. A reading command that fails is `E_APPLY`
/// too: an apply that cannot read the machine must not carry on guessing at it.
pub fn capture(invocation: &Invocation) -> Result<String, Diagnostic> {
    capture_inner(invocation, false)
}

/// [`capture`], for a query whose *nothing matched* answer is an exit status and not an empty
/// list.
///
/// `pacman -Q…` exits 1 when its query matches no package, and on a stock machine that is the
/// ordinary answer to "which packages came from no repository you know" — M-Import T-5 found
/// `lodi import` stopping at `E_APPLY` on every fresh Arch guest for exactly that reason
/// (LD-301). The tolerance is that one shape and nothing wider: exit status 1, nothing on
/// standard output and nothing on standard error. A query that fails for a reason the tool
/// explains, or with any other status, is still `E_APPLY`.
pub fn capture_query(invocation: &Invocation) -> Result<String, Diagnostic> {
    capture_inner(invocation, true)
}

fn capture_inner(invocation: &Invocation, empty_is_an_answer: bool) -> Result<String, Diagnostic> {
    let output = invocation
        .command()
        .output()
        .map_err(|e| spawn_failed(invocation, &e.to_string()))?;
    if empty_is_an_answer
        && output.status.code() == Some(1)
        && output.stdout.iter().all(u8::is_ascii_whitespace)
        && output.stderr.iter().all(u8::is_ascii_whitespace)
    {
        return Ok(String::new());
    }
    if !output.status.success() {
        return Err(Diagnostic::new(
            "E_APPLY",
            format!(
                "`{}` exited {}: {}",
                invocation.command_line(),
                output
                    .status
                    .code()
                    .map_or_else(|| "on a signal".to_string(), |c| c.to_string()),
                tail(&output.stderr, &output.stdout)
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn spawn_failed(invocation: &Invocation, error: &str) -> Diagnostic {
    Diagnostic::new(
        "E_APPLY",
        format!(
            "cannot run {}: {error}",
            invocation.path.display(),
            error = error
        ),
    )
    .hint("the package manager was resolved on PATH when the command started")
}

/// How many of a failing tool's last lines its diagnostic quotes.
const TAIL: usize = 5;

/// The last lines a failing tool printed: its standard error when it said anything there, and
/// otherwise its standard output, bounded so that a message stays a message.
fn tail(stderr: &[u8], stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(if stderr.iter().any(|b| !b.is_ascii_whitespace()) {
        stderr
    } else {
        stdout
    });
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let kept: Vec<&str> = lines.iter().rev().take(TAIL).rev().copied().collect();
    if kept.is_empty() {
        "it printed nothing".to_string()
    } else {
        kept.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn pm() -> PackageManager {
        PackageManager {
            distro: Distro::Debian,
            programs: vec![
                ("apt-get".to_string(), PathBuf::from("/usr/bin/apt-get")),
                (
                    "dpkg-query".to_string(),
                    PathBuf::from("/usr/bin/dpkg-query"),
                ),
            ],
        }
    }

    #[test]
    fn an_invocation_is_an_argv_and_a_resolved_path() {
        let pm = pm();
        let invocation = Invocation::new(&pm, "apt-get", &["install", "-y", "git"]).unwrap();
        assert_eq!(invocation.path, Path::new("/usr/bin/apt-get"));
        assert_eq!(invocation.command_line(), "apt-get install -y git");
        assert!(
            invocation
                .env
                .contains(&("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string()))
        );
        assert!(Invocation::new(&pm, "pacman", &["-Q"]).is_none());
    }

    #[test]
    fn every_distribution_the_gate_accepts_has_an_arm() {
        let root = Path::new("/nonexistent-root");
        for distro in [Distro::Debian, Distro::Ubuntu, Distro::Arch, Distro::Fedora] {
            let backend = backend_for(distro, root, Operation::Plan);
            assert_eq!(backend.distro(), distro);
        }
    }

    #[test]
    fn each_family_states_its_own_shape_and_only_arch_upgrades_partially() {
        for distro in [Distro::Debian, Distro::Ubuntu] {
            let shape = Shape::of(distro);
            assert!(
                shape.refresh_is_an_action,
                "apt-get update is its own command"
            );
            assert!(!shape.removals_are_a_second_action);
            assert!(shape.hold_is_machine_state, "an apt hold outlives lodi");
            assert!(!shape.has_partial_upgrade);
            assert!(shape.repair.contains("dpkg --configure -a"));
        }
        let arch = Shape::of(Distro::Arch);
        assert!(
            !arch.refresh_is_an_action,
            "the refresh travels inside -Syu"
        );
        assert!(arch.removals_are_a_second_action);
        assert!(
            !arch.hold_is_machine_state,
            "an ignore binds one transaction"
        );
        assert!(arch.has_partial_upgrade);
        assert!(
            arch.repair.contains("db.lck"),
            "the operator needs the file named"
        );
        assert!(
            !arch.repair.contains("lodi remove") && arch.repair.contains("yourself"),
            "the lock is the operator's to remove, never lodi's"
        );
    }

    #[test]
    fn a_family_that_solves_removals_with_its_installs_has_no_second_action() {
        // The trait's defaults and the shape table say the same thing, which is what keeps a
        // plan built from the shape from asking a backend for an action it does not have.
        let root = Path::new("/nonexistent-root");
        let apt = backend_for(Distro::Debian, root, Operation::Plan);
        assert!(!Shape::of(Distro::Debian).removals_are_a_second_action);
        assert!(apt.removal(&["zip".to_string()]).unwrap().is_empty());
    }

    #[test]
    fn the_seam_never_builds_a_shell_string_or_a_sudo() {
        let invocation = Invocation::new(&pm(), "apt-get", &["install", "a b"]).unwrap();
        assert_eq!(invocation.args, vec!["install", "a b"]);
        assert!(!invocation.program.contains("sudo"));
        assert!(!invocation.command_line().contains("sudo"));
    }
}
