//! The plan: an ordered list of actions derived from the manifest, the lock and the machine's
//! **observed** state, and the documented line grammar it prints.
//!
//! The printed form is a contract with the operator and is asserted line by line by
//! `tests/host_core.rs`. It is deliberately **not** a `--json` output contract: the roadmap
//! promises those at 1.0, and one grammar that a human reads is what 0.6 ships (design call
//! D4, LD-116).
//!
//! ```text
//! + source NAME (keyring, sources)
//! ~ source NAME (sources: content)
//! - source NAME
//! = source NAME (unchanged)
//! ~ index (apt-get update)
//! + package NAME
//! + package NAME (deferred: after the source refresh)
//! - package NAME
//! ~ package NAME (hold)
//! ~ package NAME (hold: lodi transactions only)
//! + file /etc/example.conf (mode 0644, owner root:root)
//! ~ file /etc/example.conf (content, mode 0644 -> 0600)
//! - file /etc/example.conf
//! = file /etc/example.conf (unchanged)
//! + service NAME (systemctl enable --now -- NAME)
//! - service NAME (systemctl disable --now -- NAME)
//! ~ service NAME (systemctl preset -- NAME)
//! = service NAME (enabled)
//! ```
//!
//! A pinned host (M-Pin, LD-395) adds, in one fixed wording each:
//!
//! ```text
//! = index (private: the transaction installs from the dated archive at 2026-09-18T14:00:00Z, through a source set refreshed into /var/lib/lodi/host/pin)
//! = index (private: the transaction reads a dated source set refreshed into /var/lib/lodi/host/pin)
//! + package NAME (pinned VERSION from REPOSITORY at 2026-01-04T00:00:00Z)
//! ~ package NAME (pinned VERSION from source NAME; downgrade from VERSION)
//! ~ package NAME (unhold: no longer pinned; VERSION stays installed)
//! = package NAME (pinned VERSION from REPOSITORY at INSTANT; behind latest VERSION)
//! = package NAME (pinned VERSION from REPOSITORY at INSTANT; not recorded in pins.lock, …)
//! ~ index (remove the pin stage an interrupted apply left under /var/lib/lodi/host/pin)
//! ```
//!
//! The unrecorded note ends `record it with: lodi host pin NAME --to VALUE`, naming the host as
//! this plan chose it, and `behind latest` is said when the live index offers a newer version.
//!
//! A managed file whose bytes differ from the digest the lock records is **drift**: the plan
//! prints `W_DRIFT` for it and shows the action it would take, and an apply stops rather than
//! overwriting it unless it was told to (`E_DECLINED`, design call D8/LD-120).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::diag::Diagnostic;
use crate::util::sha256_hex;

use super::lock::HostLock;
use super::manifest::{FileEntry, FileState, HostManifest, OnRemove, PackagesMode};
use super::pm;
use super::safety::Gate;
use super::services::{Change, ServiceAction};
use super::sources::SourceAction;

/// The one wording a package waiting for a source this apply arms is printed with (LD-365).
pub const DEFERRED: &str = "deferred: after the source refresh";

/// What a pinned transaction's plan says before its actions: it reads the private dated source
/// set, not the machine's own sources and lists (M-Pin, P4).
pub const PRIVATE_NOTE: &str = "= index (private: the transaction reads a dated source set refreshed into /var/lib/lodi/host/pin)";

/// [`PRIVATE_NOTE`], naming the instant a file-level snapshot installs from (D-A).
pub fn private_note(snapshot: Option<i64>) -> String {
    match snapshot {
        Some(instant) => format!(
            "= index (private: the transaction installs from the dated archive at {}, through a \
             source set refreshed into /var/lib/lodi/host/pin)",
            crate::util::format_utc(instant)
        ),
        None => PRIVATE_NOTE.to_string(),
    }
}
/// What a plan says when a file-level snapshot and a declared `[sources]` repository meet (§7).
pub const SOURCES_NOTE: &str = "= sources (the [host] snapshot does not reach a declared [sources] package, which installs from its repository's current index)";
/// The one line of the action that removes a stage an interrupted apply left (P4).
pub const SWEEP_LINE: &str =
    "~ index (remove the pin stage an interrupted apply left under /var/lib/lodi/host/pin)";
/// Appended to a pinned name's line when `pins.lock` does not record the pin (P13), with the
/// command that records it (LD-397).
pub const UNRECORDED: &str = "not recorded in pins.lock";
/// Appended to a pinned name's line when the live index offers a newer version (LD-397).
pub const BEHIND: &str = "behind latest";

/// The warning a drifted managed file carries (`spec/11`).
pub const W_DRIFT: &str = "W_DRIFT";
/// A path not previously managed is about to be replaced and its bytes are kept beside it.
pub const W_REPLACED_UNMANAGED: &str = "W_REPLACED_UNMANAGED";
/// A package in `[packages] optional` that the distribution's index does not offer: it is left
/// out and the command carries on, which is the whole point of declaring it optional.
pub const W_OPTIONAL_SKIPPED: &str = "W_OPTIONAL_SKIPPED";
/// A `[files]` entry declares a mode with a set-uid or set-gid bit, or one that lets anyone
/// write the file. The manifest's author is trusted, so the mode is applied as declared; the
/// warning says so out loud rather than applying it in silence.
pub const W_FILE_MODE: &str = "W_FILE_MODE";
/// `--unsupported-partial-upgrade` was given, so this apply installs on Arch **without**
/// upgrading the machine (design call D10). Arch does not support that state; the warning says
/// what the risk is and the flag's own name says the mode is unsupported.
pub const W_ARCH_PARTIAL: &str = "W_ARCH_PARTIAL";
/// The manifest has the legacy `[files]` table: it applies as before, and `[etc."PATH"]` is the
/// form that replaces it (si-1, LD-421).
pub const W_LEGACY_FILES: &str = "W_LEGACY_FILES";

/// What is observed of one path, and what an action intends it to become.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Facts {
    /// The absolute path **inside the root**, as the manifest names it.
    pub path: String,
    pub exists: bool,
    /// A path that exists but is not a regular file: a symlink, a directory, a device.
    #[serde(default)]
    pub regular: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
}

impl Facts {
    pub fn absent(path: &str) -> Facts {
        Facts {
            path: path.to_string(),
            exists: false,
            regular: false,
            digest: None,
            mode: None,
            uid: None,
            gid: None,
        }
    }

    /// Observe one path without following a symlink for its final component.
    pub fn observe(root: &Path, path: &str) -> Facts {
        let real = join(root, path);
        let Ok(meta) = fs::symlink_metadata(&real) else {
            return Facts::absent(path);
        };
        if !meta.is_file() {
            return Facts {
                path: path.to_string(),
                exists: true,
                regular: false,
                digest: None,
                mode: Some(meta.permissions().mode() & 0o7777),
                uid: Some(meta.uid()),
                gid: Some(meta.gid()),
            };
        }
        // The bytes are read through the managed-path open, which never blocks and keeps only
        // a regular file: what was seen above may have been swapped since.
        let digest = super::files::open_regular(&real, path)
            .ok()
            .and_then(|mut handle| digest_reader(&mut handle).ok());
        Facts {
            path: path.to_string(),
            exists: true,
            regular: true,
            digest,
            mode: Some(meta.permissions().mode() & 0o7777),
            uid: Some(meta.uid()),
            gid: Some(meta.gid()),
        }
    }
}

/// What a declared file mode grants that deserves a word before it is applied: a set-uid or
/// set-gid bit, or write access for anyone. `None` for every other mode.
pub fn notable_mode(mode: u32) -> Option<String> {
    let mut grants = Vec::new();
    if mode & 0o4000 != 0 {
        grants.push("is set-uid");
    }
    if mode & 0o2000 != 0 {
        grants.push("is set-gid");
    }
    if mode & 0o002 != 0 {
        grants.push("lets anyone write the file");
    }
    (!grants.is_empty()).then(|| grants.join(" and "))
}

/// Join an in-root absolute path onto the root.
pub fn join(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

/// `sha256:` of a file's bytes, streamed rather than held.
pub fn digest_of(path: &Path) -> std::io::Result<String> {
    digest_reader(&mut fs::File::open(path)?)
}

/// `sha256:` of everything `file` yields, streamed rather than held.
fn digest_reader(file: &mut impl Read) -> std::io::Result<String> {
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        sha2::Digest::update(&mut hasher, &buffer[..read]);
    }
    let out = sha2::Digest::finalize(hasher);
    let mut hex = String::from("sha256:");
    for byte in out {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok(hex)
}

/// Where the bytes of a managed file come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// `content = "…"` in the manifest, or the bytes of its `source = "…"`, read once before
    /// the first plan (LD-379): the bytes planned are the bytes written.
    Inline(Vec<u8>),
    /// A backup this scope kept below the root, restored when its declaration leaves the
    /// manifest. It is a managed path, so it is read only if it is a regular file.
    Kept(PathBuf),
}

/// What a file action intends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub digest: String,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub owner: String,
    pub group: String,
    pub content: Content,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Create,
    Replace,
    Metadata,
    Remove,
    Restore,
    Keep,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAction {
    pub path: String,
    pub op: Op,
    pub desired: Option<Desired>,
    /// The in-root absolute path of the backup this action will take, when it takes one.
    pub backup: Option<String>,
    /// The original unmanaged backup already recorded by an earlier apply.
    pub retained_backup: Option<String>,
    /// A recorded backup to restore when this declaration left the manifest.
    pub restore_from: Option<String>,
    /// This action adopts a file already equal to its declaration, with no record of it, whose
    /// entry keeps a copy and restores it: the apply that records it keeps its original in the
    /// store first (LD-387). A plan takes no copy.
    pub adopt: bool,
    pub before: Facts,
    pub after: Facts,
    /// The bytes differ from the digest the lock records: this file was edited by hand.
    pub drift: bool,
    pub on_remove: String,
    /// The parenthesised part of the printed line.
    pub changes: Vec<String>,
}

/// What one package name is expected to look like on the machine.
///
/// A field that is `None` was **not recorded**, never "this must be absent" — the same rule the
/// file [`Facts`] follow, and the reason a bracket can describe the two or three names an action
/// touches without claiming anything about the thousand it does not.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageFact {
    pub installed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<bool>,
}

/// Which of the three package steps an action is. They are separate actions because each is a
/// separate mutation, and the journal has to be able to say which one an interruption caught.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// `apt-get update`: refreshing the index is a mutation like any other. A family that
    /// refreshes inside its transaction has no action of this step at all.
    Index,
    /// The transaction: the installs, and — for a family that solves removals with them — the
    /// removals too.
    Transaction,
    /// The removals of a family that does not solve them with the installs: `pacman -Rns`, the
    /// order `spec/09` §6.2 gives. It is its own action so that the journal can say which of the
    /// two an interruption caught.
    Removal,
    /// The marks and the holds, after the transaction that made them meaningful.
    Marks,
}

/// A package action: what it changes, exactly what it will run, and the two states that
/// bracket it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageAction {
    pub step: Step,
    pub install: Vec<String>,
    pub remove: Vec<String>,
    pub auto: Vec<String>,
    pub manual: Vec<String>,
    pub hold: Vec<String>,
    pub unhold: Vec<String>,
    /// The names lodi's own transaction is told to leave alone, for a family whose `hold` is not
    /// machine state (`pacman --ignore`, design call D11). Empty everywhere else.
    pub ignore: Vec<String>,
    /// Every package this manifest declares for this machine and does not declare `absent`:
    /// the set the apply converges, whether or not this apply has to install any of it.
    ///
    /// The lock is written from this, intersected with what the machine shows installed
    /// afterwards, and **not** from the installs of this one apply. A declared package that was
    /// already there is still a package Lodi manages, and the lock is the record of the managed
    /// set (OD-15) — which is also what `auto_remove` is bounded by, so a name that never
    /// reached the lock could never leave the manifest cleanly either.
    pub managed: Vec<String>,
    /// Every process this action will run, in order, argv and resolved path. It is derived at
    /// plan time so that `plan` can print it and the journal can record it before it runs.
    pub invocations: Vec<pm::Invocation>,
    /// Why a name is removed or demoted, printed beside it (LD-375). A name with no reason here
    /// prints as 1.1.0 printed it.
    pub reasons: BTreeMap<String, String>,
    /// The package manager's own answer, asked again just before this action runs: an exact
    /// removal whose answer is no longer `remove` stops there, with nothing removed (LD-375),
    /// and so does an exact install of a name carried past the refresh (LD-379).
    pub recheck: Option<Recheck>,
    /// An index action a source step forced: its invocation is the source-aware refresh, and a
    /// failure of it puts this apply's source files back (S9, LD-365).
    pub forced: bool,
    /// Changing declared and departed sources, each with its URIs, in name order.
    pub sources: Vec<(String, Vec<String>)>,
    /// Installs the current index does not offer, waiting for a source this apply arms: printed
    /// with one fixed wording and settled by the check after the refresh (LD-365).
    pub deferred: Vec<String>,
    /// Why an install or a version change is where it is from, printed beside it: a pin's
    /// resolved version and origin, or the file-level snapshot (M-Pin).
    pub notes: BTreeMap<String, String>,
    /// Installed names this transaction moves to another version: a pin (M-Pin).
    pub changed: Vec<String>,
    /// The action removes the pin stage an interrupted apply left, and runs nothing (P4).
    pub sweep: bool,
    /// What a pinned transaction reads and may do (M-Pin); the default for any other.
    pub pinning: pm::Pinning,
    pub before: BTreeMap<String, PackageFact>,
    pub after: BTreeMap<String, PackageFact>,
    /// The build each install and removal line names, for a family whose plan names it
    /// ([`pm::Shape::names_builds`], LD-432); empty for every other.
    pub builds: BTreeMap<String, String>,
}

/// The simulation an exact removal is checked against just before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recheck {
    /// What the simulated command installs, for a family that solves the two together.
    pub install: Vec<String>,
    /// The names the removal command is given.
    pub roots: Vec<String>,
    /// What the pinned transaction it simulates reads and may do (M-Pin); the default for any
    /// other transaction.
    pub pinning: pm::Pinning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    File(Box<FileAction>),
    Package(Box<PackageAction>),
    /// A declared third-party apt repository: its keyring and its stanza, as one step ordered
    /// before the index it changes (`super::sources`).
    Source(Box<SourceAction>),
    /// A declared systemd unit, or one no longer declared (`super::services`).
    Service(Box<ServiceAction>),
    /// A declared account or group, or an account's public keys (su-1, `super::users`).
    Identity(Box<super::users::IdentityAction>),
    /// An OS basic: a `[system]` setting, the firewall or the network (sd-1, `super::basics`).
    Basic(Box<super::basics::BasicAction>),
}

impl Kind {
    /// A file action, boxed: a `FileAction` is many times the size of an `Action`'s other half.
    fn file(action: FileAction) -> Kind {
        Kind::File(Box::new(action))
    }

    fn package(action: PackageAction) -> Kind {
        Kind::Package(Box::new(action))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    /// Stable within one plan, and the identity the journal and every message use.
    pub id: String,
    pub kind: Kind,
}

impl Action {
    /// The printed line, or lines for a package action that both installs and removes.
    pub fn lines(&self) -> Vec<String> {
        match &self.kind {
            Kind::Service(service) => vec![service.line()],
            Kind::Source(source) => {
                let verb = match source.op {
                    Op::Create => "+",
                    Op::Replace | Op::Metadata | Op::Restore => "~",
                    Op::Remove => "-",
                    Op::Keep | Op::Unchanged => "=",
                };
                let mut line = format!("{verb} source {}", source.name);
                if !source.changes.is_empty() {
                    line.push_str(&format!(" ({})", source.changes.join(", ")));
                }
                vec![line]
            }
            Kind::Identity(identity) => vec![identity.line()],
            Kind::Basic(basic) => vec![basic.line()],
            Kind::File(file) => {
                let verb = match file.op {
                    Op::Create => "+",
                    Op::Replace | Op::Metadata | Op::Restore => "~",
                    Op::Remove => "-",
                    Op::Keep | Op::Unchanged => "=",
                };
                let mut line = format!("{verb} file {}", file.path);
                if !file.changes.is_empty() {
                    line.push_str(&format!(" ({})", file.changes.join(", ")));
                }
                vec![line]
            }
            Kind::Package(packages) => match packages.step {
                Step::Index if packages.sweep => vec![SWEEP_LINE.to_string()],
                Step::Index => packages
                    .invocations
                    .iter()
                    .map(|invocation| format!("~ index ({})", invocation.command_line()))
                    .collect(),
                Step::Transaction => packages
                    .install
                    .iter()
                    .map(|name| {
                        let shown = packages.shown(name);
                        if packages.deferred.contains(name) {
                            format!("+ package {shown} ({DEFERRED})")
                        } else if let Some(note) = packages.notes.get(name) {
                            format!("+ package {shown} ({note})")
                        } else {
                            format!("+ package {shown}")
                        }
                    })
                    .chain(packages.changed.iter().map(|name| {
                        format!(
                            "~ package {name} ({})",
                            packages.notes.get(name).cloned().unwrap_or_default()
                        )
                    }))
                    .chain(
                        packages
                            .remove
                            .iter()
                            .map(|name| packages.reasoned("-", name, None)),
                    )
                    // A hold this family expresses as an option on lodi's own transaction is
                    // said here, on the transaction it binds, and it says exactly how far it
                    // reaches: pacman itself is unchanged by it (D11).
                    .chain(
                        packages
                            .ignore
                            .iter()
                            .map(|name| format!("~ package {name} (hold: lodi transactions only)")),
                    )
                    .collect(),
                Step::Removal => packages
                    .remove
                    .iter()
                    .map(|name| packages.reasoned("-", name, None))
                    .collect(),
                Step::Marks => [
                    (&packages.auto, "auto"),
                    (&packages.manual, "manual"),
                    (&packages.hold, "hold"),
                    (&packages.unhold, "unhold"),
                ]
                .into_iter()
                .flat_map(|(names, what)| {
                    names
                        .iter()
                        .map(move |name| packages.reasoned("~", name, Some(what)))
                })
                .collect(),
            },
        }
    }

    /// One line for the journal and for a message that names this action.
    pub fn summary(&self) -> String {
        self.lines().join("; ")
    }

    pub fn kind_name(&self) -> &'static str {
        match &self.kind {
            Kind::Service(service) => service.kind_name(),
            Kind::Identity(identity) => identity.kind_name(),
            Kind::Basic(basic) => basic.kind_name(),
            Kind::Source(source) => match source.op {
                Op::Create => "source.create",
                Op::Replace | Op::Metadata => "source.replace",
                Op::Remove => "source.remove",
                Op::Restore => "source.restore",
                Op::Keep | Op::Unchanged => "source.unchanged",
            },
            Kind::File(file) => match file.op {
                Op::Create => "file.create",
                Op::Replace => "file.replace",
                Op::Metadata => "file.metadata",
                Op::Remove => "file.remove",
                Op::Restore => "file.restore",
                Op::Keep => "file.keep",
                Op::Unchanged => "file.unchanged",
            },
            Kind::Package(packages) => match packages.step {
                Step::Index => "packages.index",
                Step::Transaction => "packages.transaction",
                Step::Removal => "packages.removal",
                Step::Marks => "packages.marks",
            },
        }
    }
}

impl PackageAction {
    /// One package line: the verb, the name, and the reason this plan has for it — or, with
    /// none, what 1.1.0 printed.
    fn reasoned(&self, verb: &str, name: &str, what: Option<&str>) -> String {
        let shown = self.shown(name);
        match (self.reasons.get(name), what) {
            (Some(reason), _) => format!("{verb} package {shown} ({reason})"),
            (None, Some(what)) => format!("{verb} package {shown} ({what})"),
            (None, None) => format!("{verb} package {shown}"),
        }
    }
}

/// What a plan decided about packages as a whole, beyond its actions (LD-375).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packages {
    /// `[host] packages = "exact"`.
    pub exact: bool,
    /// Every package the manifest declares for this machine and does not declare absent, and
    /// that the index can place.
    pub managed: Vec<String>,
    /// Everything this plan removes.
    pub removed: Vec<String>,
    /// The names the manifest holds: the only holds the record calls Lodi's.
    pub hold: Vec<String>,
    /// Names a family whose hold is not machine state holds on lodi's own transactions.
    pub ignore: Vec<String>,
    /// The package half of the record as it stands if this plan changes nothing on the machine:
    /// what a converged apply writes.
    pub record: BTreeMap<String, super::lock::PackageRecord>,
    /// The package half of the baseline as it stands if this plan changes nothing: the explicit
    /// names and the holds a converged apply records (LD-378).
    pub baseline: (std::collections::BTreeSet<String>, BTreeMap<String, String>),
}

/// A plan over one root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub root: PathBuf,
    pub actions: Vec<Action>,
    /// `W_…` lines, printed before the plan.
    pub warnings: Vec<String>,
    /// `= package NAME (reason)` lines: packages a plan leaves on the machine for a reason it
    /// says, printed after the actions (LD-375).
    pub kept: Vec<String>,
    /// Packages an exact plan would remove or demote that the record never named: drift.
    pub package_drift: Vec<String>,
    /// `None` when this plan did not consider packages at all.
    pub packages: Option<Packages>,
    /// The record differs from what the machine now is, so an apply that changes nothing on
    /// the machine still writes it.
    pub record_changes: bool,
    /// The baseline a converged apply of this plan records: the base of the next reconcile
    /// (LD-378).
    pub baseline: super::lock::Baseline,
    /// The `source` the record of this plan's apply names (LD-379): `/etc/lodi`, or the host
    /// directory.
    pub source: String,
    /// The record names another source, and that is all that differs: the apply that changes
    /// nothing still records the new source, and says it had nothing to do.
    pub source_only: bool,
    pub manifest_digest: String,
    pub lock_digest: Option<String>,
    /// `= …` lines printed before the actions: what a pinned host reads (M-Pin).
    pub notes: Vec<String>,
    /// Pinned names moved outside lodi since the record was written: LD-375's drift, which an
    /// apply refuses with `E_DECLINED` until `--overwrite-drift` (P6).
    pub pin_drift: Vec<String>,
    /// What an apply of a pinned transaction does before it (P4, P7, P13).
    pub pinned: Option<Pinned>,
    /// The git revision the manifest was read from, which the record of this plan's apply locks
    /// (LD-401); `None` for a host directory.
    pub git: Option<super::lock::GitRecord>,
    /// The declared services a converged apply records (sc-1).
    pub services: BTreeMap<String, String>,
    /// The accounts and groups the record of this plan's apply manages (su-1).
    pub identities: (BTreeSet<String>, BTreeSet<String>),
    /// The OS basics a converged apply records (sd-1).
    pub basics: BTreeMap<String, super::lock::BasicRecord>,
}

/// What the plan knows of pins (M-Pin, LD-395): resolved once, by the command's first plan, and
/// handed unchanged to the re-plan under the apply lock, so that an apply installs exactly what
/// its plan resolved and never resolves again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pins {
    pub resolution: super::pin::Resolution,
    /// Every name the host directory's `pins.lock` records.
    pub recorded: std::collections::BTreeSet<String>,
    /// A pin stage an interrupted apply left behind (P4).
    pub stale_stage: bool,
    /// What goes before and after `lodi host pin NAME --to VALUE` in the command an unrecorded
    /// pin's line names: `sudo ` for the host in place on the running system, and the host this
    /// plan chose (`SOURCE`, `--host`, `--root`), so that the line runs as printed (LD-397).
    pub record_with: (String, String),
}

/// What an apply of a pinned transaction does before the transaction: stage the private dated
/// source set, refresh it into its own lists, check the index apt downloaded against the digests
/// resolved or recorded, and simulate exactly the transaction (`E_PIN_UNSATISFIABLE`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    /// The private source file on apt, or `pin.conf` on Arch (LD-396).
    pub set: String,
    /// Arch's verified local package files staged below the root, before `pacman -U`.
    pub arch_files: Vec<ArchFile>,
    /// Fedora's pinned builds, staged and verified below the root before dnf5 installs them.
    pub fedora_files: Vec<super::pin::fedora::FedoraFile>,
    /// This is the pacman configuration; the apt source-set path is otherwise unchanged.
    pub arch_day: Option<i64>,
    /// `[packages.arch] archived_keyring`: pin.conf verifies against the dated keyring (#170).
    pub archived_keyring: bool,
    /// The refresh of the private set into its own lists.
    pub refresh: Vec<pm::Invocation>,
    /// The transaction's own arguments, for its simulation.
    pub install: Vec<String>,
    pub remove: Vec<String>,
    pub pinning: pm::Pinning,
    /// Each dated instant the set reads with the `Release` digests it must match.
    pub checks: Vec<(i64, BTreeMap<String, String>)>,
    /// Each pinned name the transaction installs or moves, with its version and where it comes
    /// from: what the read-back holds the machine to (P6) and what a refusal names (P7).
    pub expected: BTreeMap<String, (String, String)>,
}

/// One Arch artifact: fetched only from the catalogue's dated repository and held to its
/// resolved SHA-256. `path` is a basename under the private pin stage, never outside the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchFile {
    pub name: String,
    pub url: String,
    pub digest: String,
    pub path: String,
}

impl Plan {
    /// The actions that would change something.
    pub fn changing(&self) -> impl Iterator<Item = &Action> {
        self.actions.iter().filter(|action| match &action.kind {
            Kind::File(file) => file.op != Op::Unchanged,
            Kind::Package(_) | Kind::Identity(_) => true,
            Kind::Source(source) => source.changing(),
            Kind::Service(service) => service.change != Change::Unchanged,
            Kind::Basic(basic) => basic.change != super::basics::Change::Unchanged,
        })
    }

    /// Whether this plan would change anything at all.
    pub fn is_noop(&self) -> bool {
        self.changing().next().is_none()
    }

    /// Every managed file the plan would overwrite that carries drift.
    pub fn drifted(&self) -> Vec<&FileAction> {
        self.actions
            .iter()
            .flat_map(|action| match &action.kind {
                Kind::File(file) if file.drift && file.op != Op::Unchanged => vec![&**file],
                Kind::Source(source) => source
                    .files
                    .iter()
                    .filter(|file| file.drift && file.op != Op::Unchanged)
                    .collect(),
                _ => Vec::new(),
            })
            .collect()
    }

    /// Every file of a `[sources]` step the plan would overwrite that carries drift: the part of
    /// [`Plan::drifted`] an apply refuses with `E_DRIFT` rather than a `[files]` entry's
    /// `E_DECLINED` (S10).
    pub fn drifted_sources(&self) -> Vec<&FileAction> {
        self.actions
            .iter()
            .flat_map(|action| match &action.kind {
                Kind::Source(source) => source
                    .files
                    .iter()
                    .filter(|file| file.drift && file.op != Op::Unchanged)
                    .collect(),
                _ => Vec::new(),
            })
            .collect()
    }

    /// Every file action the plan holds, the ones a source step carries included.
    pub fn file_actions(&self) -> impl Iterator<Item = &FileAction> {
        self.actions.iter().flat_map(|action| match &action.kind {
            Kind::File(file) => vec![&**file],
            Kind::Source(source) => source.files.iter().collect(),
            Kind::Package(_) | Kind::Service(_) | Kind::Identity(_) | Kind::Basic(_) => Vec::new(),
        })
    }

    /// The plan as the operator sees it: the warnings, then one line per action, then a summary.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for warning in &self.warnings {
            out.push_str(warning);
            out.push('\n');
        }
        for note in &self.notes {
            out.push_str(note);
            out.push('\n');
        }
        for action in &self.actions {
            for line in action.lines() {
                out.push_str(&line);
                out.push('\n');
            }
        }
        for line in &self.kept {
            out.push_str(line);
            out.push('\n');
        }
        let changing = self.changing().count();
        if changing == 0 && self.record_changes {
            out.push_str("no machine changes; apply would update the record\n");
        } else if changing == 0 {
            out.push_str("nothing to do\n");
        } else {
            out.push_str(&format!("{changing} action(s)\n"));
        }
        out
    }
}

/// Derive the plan. Nothing below the root is written here, and the package manager is only
/// ever **read**: `dpkg-query`, `apt-cache` and `apt-mark show…` observe the machine, and the
/// commands that would change it are built, printed and journalled, not run.
///
/// This is the plan of a host with nothing pinned; [`build_pinned`] is the one command uses.
#[allow(clippy::too_many_arguments)]
pub fn build(
    gate: &Gate,
    manifest: &HostManifest,
    sources: &BTreeMap<String, Vec<u8>>,
    source: &str,
    lock: Option<&HostLock>,
    manifest_digest: &str,
    no_update: bool,
) -> Result<Plan, Diagnostic> {
    build_pinned(
        gate,
        manifest,
        sources,
        source,
        lock,
        manifest_digest,
        no_update,
        &Pins::default(),
    )
}

/// [`build`], with what the plan knows of pins (M-Pin, LD-395). With [`Pins::default`] — no
/// `[host] snapshot`, no pin and no `pins.lock` — it is exactly [`build`], line for line and
/// argv for argv (P3).
#[allow(clippy::too_many_arguments)]
pub fn build_pinned(
    gate: &Gate,
    manifest: &HostManifest,
    sources: &BTreeMap<String, Vec<u8>>,
    source: &str,
    lock: Option<&HostLock>,
    manifest_digest: &str,
    no_update: bool,
    pins: &Pins,
) -> Result<Plan, Diagnostic> {
    let root = gate.root.clone();
    let mut actions = Vec::new();
    let mut warnings = Vec::new();
    let mut kept = Vec::new();
    let mut package_drift = Vec::new();
    let mut packages = None;
    let mut notes = Vec::new();
    let mut pin_drift = Vec::new();
    let mut pinned = None;

    // A stage an interrupted apply left is removed first, as an action of its own, so the
    // journal records it (P4).
    if pins.stale_stage {
        let mut sweep = PackageAction::of(Step::Index);
        sweep.sweep = true;
        actions.push(Action {
            id: "s0".to_string(),
            kind: Kind::package(sweep),
        });
    }
    if pins.resolution.snapshot.is_some() && !manifest.sources.is_empty() {
        notes.push(SOURCES_NOTE.to_string());
    }
    if manifest.legacy_files {
        warnings.push(format!(
            "{W_LEGACY_FILES}: host.toml declares [files]; it applies as before, and \
             [etc.\"PATH\"] with the file's text is the form that replaces it"
        ));
    }

    // Sources first (LD-365): a repository is armed before the index that lists it is refreshed
    // and before the transaction that installs from it. A source that changes forces the
    // refresh, whatever the age of the index, or the new repository's packages could not be
    // placed.
    let refresh = super::sources::plan(
        gate,
        manifest,
        sources,
        lock,
        no_update,
        &mut actions,
        &mut warnings,
    )?;

    let exact = manifest.host.packages == Some(PackagesMode::Exact);
    if !manifest.packages.is_empty() || exact {
        let mut backend = pm::backend_for(
            gate.distro,
            &gate.root,
            gate.partial_upgrade,
            gate.operation,
        );
        if exact {
            backend.keep_default_recommends();
        }
        let mut sink = PackageSink {
            actions: &mut actions,
            warnings: &mut warnings,
            kept: &mut kept,
            drift: &mut package_drift,
            notes: &mut notes,
            pin_drift: &mut pin_drift,
            pinned: &mut pinned,
        };
        packages = Some(plan_packages(
            gate,
            manifest,
            lock,
            backend.as_ref(),
            Refresh {
                no_update,
                sources: &refresh,
            },
            pins,
            &mut sink,
        )?);
    } else if refresh.forced() && !no_update && pm::Shape::of(gate.distro).refresh_is_an_action {
        // A manifest with sources and no packages still refreshes the index a changing source
        // makes stale, so that the source is proven (S9) by the apply that arms it.
        let backend = pm::backend_for(
            gate.distro,
            &gate.root,
            gate.partial_upgrade,
            gate.operation,
        );
        let stale = backend
            .index_age()
            .is_none_or(|age| age >= pm::REFRESH_AFTER);
        actions.push(index_action(backend.as_ref(), &refresh, Vec::new(), stale)?);
    }

    // Accounts and groups after the packages, so that a shell a package installs is there
    // (su-1, LD-419).
    let identities = super::users::plan(gate, manifest, lock)?;
    kept.extend(identities.kept);
    for (index, action) in identities.actions.into_iter().enumerate() {
        actions.push(Action {
            id: format!("u{}", index + 1),
            kind: Kind::Identity(Box::new(action)),
        });
    }

    let mut paths = std::collections::BTreeSet::new();
    paths.extend(manifest.files.keys().cloned());
    if let Some(lock) = lock {
        paths.extend(lock.files.keys().cloned());
    }
    // The two paths of a source are that source's step, planned above, and never a file line.
    let family = super::sources::Family::of(gate.distro);
    paths.retain(|path| super::sources::name_of(family, path).is_none());

    for (index, path) in paths.iter().enumerate() {
        let id = format!("f{}", index + 1);
        let action = plan_path(
            gate,
            &id,
            path,
            manifest.files.get(path),
            sources,
            lock,
            &mut warnings,
        )?;
        actions.push(action);
    }

    // The OS basics after the accounts and files, the network last of them (sd-1, LD-420).
    let mut basics = Vec::new();
    let recorded_basics = super::basics::plan(gate, manifest, lock, &mut basics, &mut kept)?;
    for (index, basic) in basics.into_iter().enumerate() {
        actions.push(Action {
            id: format!("b{}", index + 1),
            kind: Kind::Basic(Box::new(basic)),
        });
    }

    // Services last (sc-1): a unit is enabled once the package that ships it and the files that
    // configure it are in place.
    let installs = actions.iter().any(|action| {
        matches!(&action.kind, Kind::Package(p) if p.step == Step::Transaction && !p.install.is_empty())
    });
    let mut services = Vec::new();
    let recorded_services = super::services::plan(gate, manifest, lock, installs, &mut services)?;
    for (index, service) in services.into_iter().enumerate() {
        actions.push(Action {
            id: format!("v{}", index + 1),
            kind: Kind::Service(Box::new(service)),
        });
    }

    let mut plan = Plan {
        root,
        actions,
        warnings,
        kept,
        package_drift,
        packages,
        record_changes: false,
        baseline: super::lock::Baseline::default(),
        source: source.to_string(),
        source_only: false,
        manifest_digest: manifest_digest.to_string(),
        lock_digest: lock.map(|lock| sha256_hex(&lock.to_bytes())),
        notes,
        pin_drift,
        pinned,
        git: None,
        services: recorded_services,
        identities: (identities.users, identities.groups),
        basics: recorded_basics,
    };
    // The record is part of what an apply keeps true: the packages as the machine now has them,
    // and every declared file as this plan would record it — a file adopted where it stands
    // included, and a declaration whose owner is now spelled another way (LD-377).
    let packages_differ = plan.packages.as_ref().is_some_and(|packages| {
        let recorded = lock.map(|lock| &lock.packages);
        match recorded {
            Some(recorded) => *recorded != packages.record,
            None => !packages.record.is_empty(),
        }
    });
    let files = file_records(&plan);
    let files_differ = match lock {
        Some(lock) => lock.files != files,
        None => !files.is_empty(),
    };
    // The baseline, too (LD-378): every apply advances it, and one an older release wrote — or
    // none — is written by the first apply, even one with nothing else to do.
    let previous = lock.and_then(|lock| lock.baseline.as_ref());
    let (explicit, holds) = match &plan.packages {
        Some(packages) => packages.baseline.clone(),
        None => previous
            .map(|b| (b.explicit.clone(), b.holds.clone()))
            .unwrap_or_default(),
    };
    plan.baseline = super::lock::Baseline {
        explicit,
        holds,
        files: super::reconcile::files_baseline(previous, &files),
    };
    let baseline_differs = match lock {
        Some(lock) => lock.baseline.as_ref() != Some(&plan.baseline),
        None => plan.baseline != super::lock::Baseline::default(),
    };
    let source_differs = lock.is_some_and(|lock| lock.source.as_deref() != Some(source));
    let services_differ = match lock {
        Some(lock) => lock.services != plan.services,
        None => !plan.services.is_empty(),
    };
    let identities_differ = match lock {
        Some(lock) => (&lock.users, &lock.groups) != (&plan.identities.0, &plan.identities.1),
        None => !plan.identities.0.is_empty() || !plan.identities.1.is_empty(),
    };
    let basics_differ = match lock {
        Some(lock) => lock.basics != plan.basics,
        None => !plan.basics.is_empty(),
    };
    let others = packages_differ
        || files_differ
        || baseline_differs
        || services_differ
        || identities_differ
        || basics_differ;
    plan.record_changes = plan.is_noop() && (others || source_differs);
    plan.source_only = plan.record_changes && !others;
    Ok(plan)
}

/// Where the package half of a plan puts what it decides.
struct PackageSink<'a> {
    actions: &'a mut Vec<Action>,
    warnings: &'a mut Vec<String>,
    kept: &'a mut Vec<String>,
    drift: &'a mut Vec<String>,
    notes: &'a mut Vec<String>,
    pin_drift: &'a mut Vec<String>,
    pinned: &'a mut Option<Pinned>,
}

/// One managed path: observed, compared with the lock, and planned as present, absent or
/// departed. `entry` is the declaration, or `None` for a path only the lock still records. The
/// warnings a file carries are pushed here, so that a `[files]` entry and a source's file say
/// them alike (LD-365).
pub(super) fn plan_path(
    gate: &Gate,
    id: &str,
    path: &str,
    entry: Option<&FileEntry>,
    sources: &BTreeMap<String, Vec<u8>>,
    lock: Option<&HostLock>,
    warnings: &mut Vec<String>,
) -> Result<Action, Diagnostic> {
    let root = &gate.root;
    super::files::inspect_trusted(root, path, false)?;
    let before = Facts::observe(root, path);
    if before.exists && !before.regular {
        return Err(Diagnostic::new(
            "E_PATH_ESCAPE",
            format!("{path} exists and is not a regular file"),
        )
        .hint(
            "lodi never follows a symlink for the final component of a managed path; move \
             the object aside by hand",
        ));
    }
    let recorded = lock.and_then(|lock| lock.files.get(path));
    // A record an earlier release wrote with no copy may be of a file Lodi created or of one
    // it found (LD-377 validator repair): it is read as possibly an original.
    let recorded_earlier = recorded
        .filter(|record| unbacked_restore(record))
        .and(lock)
        .and_then(|lock| written_before_adoption(&lock.generated_by));
    let drift = matches!((recorded, &before.digest), (Some(record), Some(digest))
        if &record.digest != digest);
    let action = match entry {
        Some(entry) => match entry.state {
            FileState::Absent => plan_absent(id, path, before, drift, &entry.on_remove),
            FileState::Present => plan_present(
                gate,
                id,
                path,
                entry,
                sources,
                before,
                recorded,
                recorded_earlier.is_some(),
            )?,
        },
        None => plan_departed(
            gate,
            id,
            path,
            recorded.expect("a departed lock record"),
            before,
            drift,
            recorded_earlier.as_deref(),
        )?,
    };
    // An owner or mode change reaches the file itself, and through it every other name the
    // file has; it is made only to a file with one.
    if let Kind::File(file) = &action.kind
        && file.op == Op::Metadata
    {
        let links = fs::symlink_metadata(join(root, path))
            .map(|meta| std::os::unix::fs::MetadataExt::nlink(&meta))
            .unwrap_or(1);
        if links > 1 {
            return Err(super::files::hard_linked(path, links));
        }
    }
    if let Kind::File(file) = &action.kind
        && file.drift
        && file.op != Op::Unchanged
    {
        warnings.push(format!(
            "{W_DRIFT}: {path} was changed since lodi last wrote it"
        ));
    }
    // Said whenever an action is about to set such a mode, and not again once it stands.
    if let Kind::File(file) = &action.kind
        && matches!(file.op, Op::Create | Op::Replace | Op::Metadata)
        && let Some(entry) = entry
        && entry.state == FileState::Present
        && let Some(what) = notable_mode(entry.mode)
    {
        warnings.push(format!(
            "{W_FILE_MODE}: {path} declares mode {:04o}, which {what}; it is applied as declared",
            entry.mode
        ));
    }
    // A backup is only ever taken of an original: one that was there before any record,
    // or one Lodi adopted in place and is about to write for the first time (LD-377).
    if let Kind::File(file) = &action.kind
        && file.backup.is_some()
    {
        warnings.push(format!(
            "{W_REPLACED_UNMANAGED}: {path} already exists; its original bytes will be kept"
        ));
    }
    Ok(action)
}

impl PackageAction {
    fn of(step: Step) -> PackageAction {
        PackageAction {
            step,
            install: Vec::new(),
            remove: Vec::new(),
            auto: Vec::new(),
            manual: Vec::new(),
            hold: Vec::new(),
            unhold: Vec::new(),
            ignore: Vec::new(),
            managed: Vec::new(),
            invocations: Vec::new(),
            reasons: BTreeMap::new(),
            recheck: None,
            forced: false,
            sources: Vec::new(),
            deferred: Vec::new(),
            notes: BTreeMap::new(),
            changed: Vec::new(),
            sweep: false,
            pinning: pm::Pinning::default(),
            before: BTreeMap::new(),
            after: BTreeMap::new(),
            builds: BTreeMap::new(),
        }
    }

    /// The name as a line prints it: with the build it installs or removes, where the plan
    /// names one.
    fn shown(&self, name: &str) -> String {
        match self.builds.get(name) {
            Some(build) => format!("{name} {build}"),
            None => name.to_string(),
        }
    }
}

fn installed(version: Option<&String>) -> PackageFact {
    PackageFact {
        installed: true,
        version: version.cloned(),
        auto: None,
        held: None,
    }
}

fn not_installed() -> PackageFact {
    PackageFact::default()
}

/// What the plan already knows about the index refresh before the backend is asked how old the
/// index is: whether the operator declined one, and whether a source this apply arms makes one
/// necessary whatever the age (LD-365).
#[derive(Debug, Clone, Copy)]
struct Refresh<'a> {
    no_update: bool,
    sources: &'a super::sources::Refresh,
}

/// The index action: the backend's refresh, or — when a source forces it — the source-aware
/// refresh built beside it, which fails closed on a declared source's error or warning (S9).
/// On dnf that refresh covers the forcing repositories alone, unless the index is `stale`
/// anyway: then it is the one full refresh, which covers them too (LD-433).
fn index_action(
    backend: &dyn pm::Backend,
    sources: &super::sources::Refresh,
    managed: Vec<String>,
    stale: bool,
) -> Result<Action, Diagnostic> {
    let mut action = PackageAction::of(Step::Index);
    action.managed = managed;
    action.invocations = backend.refresh()?;
    if sources.forced() {
        let forcing: Vec<String> = if stale {
            Vec::new()
        } else {
            sources.forcing.iter().map(|(n, _)| n.clone()).collect()
        };
        action.invocations = action
            .invocations
            .iter()
            .map(|invocation| super::sources::refresh_invocation(invocation, &forcing))
            .collect();
        action.sources = sources
            .forcing
            .iter()
            .chain(&sources.departing)
            .cloned()
            .collect();
        action.sources.sort_by(|a, b| a.0.cmp(&b.0));
        action.forced = true;
    }
    Ok(Action {
        id: "i1".to_string(),
        kind: Kind::package(action),
    })
}

/// The package half of the plan: at most three actions, always in this order — the index, the
/// one transaction, then the marks.
///
/// They are three actions and not one because each is a separate mutation, and the journal has
/// to be able to say which of them an interruption caught. Nothing here runs a command that
/// changes the machine: the backend is asked what is installed, what the index offers and how
/// old it is, and the commands that would change anything are **built**, not run.
fn plan_packages(
    gate: &Gate,
    manifest: &HostManifest,
    lock: Option<&HostLock>,
    backend: &dyn pm::Backend,
    refresh: Refresh,
    pins: &Pins,
    sink: &mut PackageSink,
) -> Result<Packages, Diagnostic> {
    let actions = &mut *sink.actions;
    let warnings = &mut *sink.warnings;
    let declared = &manifest.packages;
    let wants = declared.effective(gate.distro.name(), gate.arch());
    let absent: std::collections::BTreeSet<&String> = declared.absent.iter().collect();
    let optional: std::collections::BTreeSet<&String> = declared.optional.iter().collect();
    let mark_auto: std::collections::BTreeSet<&String> = declared.mark_auto.iter().collect();
    // M-Pin (LD-395): the file-level snapshot, and each pin that names a package of this machine.
    // A pinned name is held (D10), so the holds this plan keeps are the declared ones and the
    // pinned ones.
    let snapshot = pins.resolution.snapshot.as_ref().map(|s| s.instant);
    let pinned: BTreeMap<&String, &super::pin::Resolved> = pins
        .resolution
        .pins
        .iter()
        .filter(|(name, _)| wants.contains(name) && !absent.contains(name))
        .collect();
    let mut holds: Vec<String> = declared.hold.clone();
    for name in pinned.keys() {
        if !holds.contains(name) {
            holds.push((*name).clone());
        }
    }
    let hold: std::collections::BTreeSet<&String> = holds.iter().collect();

    let shape = pm::Shape::of(gate.distro);
    if shape.has_partial_upgrade && gate.partial_upgrade {
        warnings.push(format!(
            "{W_ARCH_PARTIAL}: --unsupported-partial-upgrade installs without upgrading this \
             machine, which {} does not support; a package built against newer libraries than \
             this machine has can fail to run",
            gate.distro.name()
        ));
    }
    let observed = backend.observe()?;
    let candidates = backend.candidates(&wants)?;
    // A machine whose index is older than the refresh window, or that has no index at all,
    // cannot answer "does this name exist?" — so when this apply is going to refresh it as its
    // own action, a name it cannot place is carried to the transaction and asked about again
    // after the refresh (`apply::run`), rather than called unknown on the strength of a stale
    // answer. A family that refreshes **inside** its transaction has no such window: the
    // question is settled here, once, before anything is written down.
    let stale = backend
        .index_age()
        .is_none_or(|age| age >= pm::REFRESH_AFTER);
    // A source this apply arms is not in the index yet, however fresh the index is (LD-365).
    let forced = refresh.sources.forced();
    let refreshing = (stale || forced) && !refresh.no_update && shape.refresh_is_an_action;

    let mut wanted: Vec<String> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    let mut deferred: Vec<String> = Vec::new();
    for name in &wants {
        if absent.contains(name) {
            // A name both declared and absent is refused by the manifest (LD-377); this is
            // never reached by a manifest that loaded.
            continue;
        }
        // A pinned name is placed by its pin, and under a file-level snapshot every name is
        // placed by the dated archive, which the preflight simulation asks (M-Pin).
        let placeable = candidates.contains_key(name)
            || observed.installed.contains_key(name)
            || pinned.contains_key(name);
        if !placeable && optional.contains(name) {
            // An optional name is decided here and never carried past a refresh. Carrying it
            // would be the one case where `optional` could still stop an apply: the settlement
            // after the refresh cannot drop a name from a transaction whose argv the journal has
            // already recorded, so it would have to raise. `optional` says leave it out where
            // the index does not offer it, and a refreshed index offers it to the next apply.
            let refreshed = if refreshing {
                "; this apply refreshes the index, so a later one will see it if it is there"
            } else {
                ""
            };
            warnings.push(format!(
                "{W_OPTIONAL_SKIPPED}: `{name}` is optional and the {} index offers no such \
                 package; it is left out{refreshed}",
                gate.distro.name()
            ));
            continue;
        }
        if !placeable
            && snapshot.is_some()
            && !backend.groups(std::slice::from_ref(name))?.is_empty()
        {
            // An Arch group is not a package, even when a dated snapshot could otherwise
            // place a missing name. Never let the snapshot turn a group into an install.
            return Err(unknown_package(backend, std::slice::from_ref(name)));
        }
        if placeable || refreshing || snapshot.is_some() {
            // A name only a source this apply arms can offer is said to wait for it, in one
            // fixed wording; the post-refresh check settles it before anything is installed.
            if !placeable && forced {
                deferred.push(name.clone());
            }
            wanted.push(name.clone());
        } else {
            unknown.push(name.clone());
        }
    }
    if !unknown.is_empty() {
        return Err(unknown_package(backend, &unknown));
    }

    let install: Vec<String> = wanted
        .iter()
        .filter(|name| !observed.installed.contains_key(*name))
        .cloned()
        .collect();

    let mut remove: Vec<String> = declared
        .absent
        .iter()
        .filter(|name| observed.installed.contains_key(*name))
        .cloned()
        .collect();
    // What the manifest declares for this machine: every name it lists, placeable or not.
    let declared_set: std::collections::BTreeSet<String> = wants
        .iter()
        .filter(|name| !absent.contains(name))
        .cloned()
        .collect();
    let mode = manifest.host.packages;
    // The installs the index can place now. A name carried past the refresh is not one of them:
    // apt cannot locate it until `i1` has run, so the simulation an exact removal is decided by
    // is made without it, and the transaction asks about it again after the refresh (LD-379,
    // the supervisor's guest run, where a fresh cloud image's first apply failed right here).
    let placed: Vec<String> = install
        .iter()
        .filter(|name| candidates.contains_key(*name))
        .cloned()
        .collect();
    // `packages = "exact"` (LD-375): the package manager decides what leaves, the record only
    // fences off what was installed by hand.
    let mut exact = None;
    if mode == Some(PackagesMode::Exact) {
        let decision = super::converge::decide(
            gate,
            backend,
            &observed,
            lock,
            &declared_set,
            &remove,
            &placed,
            &declared.mark_auto,
        )?;
        for name in &decision.drift {
            warnings.push(format!(
                "{W_DRIFT}: package {name} was installed by hand since lodi last recorded this \
                 machine, and host.toml does not declare it; apply removes it only with \
                 --overwrite-drift"
            ));
        }
        sink.drift.extend(decision.drift.iter().cloned());
        sink.kept.extend(decision.kept.iter().cloned());
        remove = decision.removed.clone();
        exact = Some(decision);
    } else if mode.is_none() {
        warnings.extend(super::converge::undeclared(
            gate,
            backend,
            &observed,
            &declared_set,
            lock,
        )?);
    }
    // `auto_remove` is bounded by the lock's record of what **Lodi** installed (OD-15). It is
    // not `apt-get autoremove`, and a package Lodi never installed is never removed by it.
    if exact.is_none()
        && manifest.host.auto_remove
        && let Some(lock) = lock
    {
        for name in lock.packages.keys() {
            if wanted.iter().any(|w| w == name) || remove.iter().any(|r| r == name) {
                continue;
            }
            if observed.installed.contains_key(name) {
                remove.push(name.clone());
            }
        }
    }

    // A pinned name installed at another version moves to exactly the pinned one; one that
    // lodi had put there and that moved since is LD-375's drift (P6).
    let mut changes: Vec<(String, String, String)> = Vec::new();
    for (name, resolved) in &pinned {
        let Some(now) = observed.installed.get(*name) else {
            continue;
        };
        if *now == resolved.version {
            continue;
        }
        if let Some(record) = lock.and_then(|lock| lock.packages.get(*name))
            && record.held
            && record.version == resolved.version
        {
            warnings.push(format!(
                "{W_DRIFT}: package {name} is pinned to {} and was changed to {now} outside \
                 lodi; apply restores it only with --overwrite-drift",
                resolved.version
            ));
            sink.pin_drift
                .push(format!("{name} (pinned {}, found {now})", resolved.version));
        }
        changes.push(((*name).clone(), now.clone(), resolved.version.clone()));
    }
    // The transaction reads the private dated source set when it installs anything under a
    // file-level snapshot, or installs or moves a pinned name (P4); the machine's own index is
    // then neither refreshed nor read.
    let private = (snapshot.is_some() && !install.is_empty())
        || install.iter().any(|name| pinned.contains_key(name))
        || !changes.is_empty();
    // The machine's own index is refreshed as it always was for a host with nothing pinned
    // (P3). A pinned host's installs read the private dated set instead (D-A), so its own index
    // is refreshed only for a transaction that does not read that set, or when a source this
    // apply arms forces it: a settled pinned machine asks nothing of any archive and writes
    // nothing (P8, A14).
    let pinned_host = snapshot.is_some() || !pinned.is_empty();
    let transacts = !install.is_empty() || !remove.is_empty() || !changes.is_empty();
    if refreshing && (forced || (!private && (!pinned_host || transacts))) {
        actions.push(index_action(
            backend,
            refresh.sources,
            wanted.clone(),
            stale,
        )?);
    }

    // What lodi's own transaction must leave alone, for a family whose `hold` is not machine
    // state (D11). It is re-passed on every transaction, because it is an option on this
    // invocation and not something that stays set on the machine.
    //
    // Only a name the machine **already has** is held. A hold says "leave this at the version it
    // is at"; a declared package that is not installed yet is at no version, and ignoring it
    // would quietly mean "never install this", which is not what `hold` says anywhere else.
    let ignore: Vec<String> = if shape.hold_is_machine_state {
        Vec::new()
    } else {
        wanted
            .iter()
            .filter(|name| hold.contains(name) && observed.installed.contains_key(*name))
            .cloned()
            .collect()
    };
    // Removals travel with the installs, or are a second action of their own. Either way they
    // are the same names; what differs is which action carries them and therefore which
    // brackets the journal writes.
    let in_transaction: Vec<String> = if shape.removals_are_a_second_action {
        Vec::new()
    } else {
        remove.clone()
    };
    // The names an exact removal's command is given, and the rest is what the package manager
    // takes with them. A family that removes in its transaction names them there.
    let roots: Vec<String> = exact
        .as_ref()
        .map_or_else(|| remove.clone(), |decision| decision.roots.clone());
    let reasons = exact
        .as_ref()
        .map(|decision| decision.reasons.clone())
        .unwrap_or_default();

    if !install.is_empty()
        || !in_transaction.is_empty()
        || !ignore.is_empty()
        || !changes.is_empty()
    {
        let mut action = PackageAction::of(Step::Transaction);
        action.managed = wanted.clone();
        let given: &[String] = if shape.removals_are_a_second_action {
            &[]
        } else {
            &roots
        };
        // A pinned name is asked for by exact version, and the options go no further than the
        // plan: a downgrade only where it names one, a held name moved only where it moves one.
        let specs: Vec<String> = install
            .iter()
            .map(|name| match pinned.get(name) {
                Some(resolved) => format!("{name}={}", resolved.version),
                None => name.clone(),
            })
            .chain(changes.iter().map(|(name, _, to)| format!("{name}={to}")))
            .collect();
        let pinning = pm::Pinning {
            private,
            allow_downgrades: changes.iter().any(|(_, from, to)| {
                super::pin::compare_for(gate.distro, to, from) == std::cmp::Ordering::Less
            }),
            allow_change_held: changes
                .iter()
                .any(|(name, _, _)| observed.held.contains(name)),
        };
        action.pinning = pinning;
        let arch_files: Vec<ArchFile> = if shape.has_partial_upgrade {
            pinned
                .iter()
                .filter(|(name, _)| {
                    install.contains(*name) || changes.iter().any(|(n, _, _)| n == **name)
                })
                .map(|(_, resolved)| super::pin::arch::file(resolved))
                .collect::<Result<_, _>>()?
        } else {
            Vec::new()
        };
        if !arch_files.is_empty() {
            warnings.push(format!("{W_ARCH_PARTIAL}: a per-entry Arch pin is installed from a verified local package file with pacman -U; other packages still follow the dated or live sync archive"));
        }
        // Fedora (fk-1): each pinned build that moves is installed from its verified file.
        let fedora_files: Vec<super::pin::fedora::FedoraFile> =
            if gate.distro == super::safety::Distro::Fedora {
                pinned
                    .iter()
                    .filter(|(name, _)| {
                        install.contains(*name) || changes.iter().any(|(n, _, _)| n == **name)
                    })
                    .map(|(_, resolved)| super::pin::fedora::file(resolved))
                    .collect::<Result<_, _>>()?
            } else {
                Vec::new()
            };
        let fedora_install: Vec<String> = install
            .iter()
            .filter(|name| !fedora_files.iter().any(|file| &file.name == *name))
            .cloned()
            .collect();
        let fedora_ignore: Vec<String> = ignore
            .iter()
            .filter(|name| !fedora_files.iter().any(|file| &file.name == *name))
            .cloned()
            .collect();
        let fedora_paths: Vec<String> = fedora_files
            .iter()
            .map(|file| {
                join(&gate.root, super::pin::STAGE)
                    .join(&file.path)
                    .display()
                    .to_string()
            })
            .collect();
        action.invocations = if !fedora_files.is_empty() {
            backend.fedora_pinned(&fedora_install, &fedora_paths, &fedora_ignore)?
        } else if shape.has_partial_upgrade && private {
            let unpinned: Vec<String> = install
                .iter()
                .filter(|name| !pinned.contains_key(*name))
                .cloned()
                .collect();
            let paths: Vec<String> = arch_files
                .iter()
                .map(|file| {
                    join(&gate.root, super::pin::STAGE)
                        .join(&file.path)
                        .display()
                        .to_string()
                })
                .collect();
            backend.arch_pinned(&unpinned, &paths, &ignore, snapshot.is_some())?
        } else if pinning == pm::Pinning::default() && specs == install {
            backend.transaction_ignoring(&install, given, &ignore)?
        } else {
            backend.transaction_pinned(&specs, given, &pinning)?
        };
        // An exact transaction is asked about again just before it runs when it removes
        // anything, and when it installs a name the plan's simulation could not include: what
        // that name takes off the machine is known only once the refresh has run.
        if exact.is_some() && (!in_transaction.is_empty() || placed.len() != install.len()) {
            action.reasons = reasons.clone();
            action.recheck = Some(Recheck {
                install: if pinning == pm::Pinning::default() {
                    install.clone()
                } else {
                    specs.clone()
                },
                roots: roots.clone(),
                pinning,
            });
        }
        for name in &install {
            action.before.insert(name.clone(), not_installed());
            let version = match pinned.get(name) {
                Some(resolved) => Some(&resolved.version),
                None => candidates.get(name),
            };
            action.after.insert(name.clone(), installed(version));
            if let Some(resolved) = pinned.get(name) {
                action.notes.insert(
                    name.clone(),
                    pin_note(resolved, pins, None, candidates.get(name), gate.distro),
                );
            }
        }
        for (name, from, to) in &changes {
            action.before.insert(name.clone(), installed(Some(from)));
            action.after.insert(name.clone(), installed(Some(to)));
            let direction =
                if super::pin::compare_for(gate.distro, to, from) == std::cmp::Ordering::Less {
                    "downgrade"
                } else {
                    "upgrade"
                };
            action.notes.insert(
                name.clone(),
                pin_note(
                    pinned[name],
                    pins,
                    Some((direction, from)),
                    candidates.get(name),
                    gate.distro,
                ),
            );
            action.changed.push(name.clone());
        }
        if !fedora_files.is_empty() {
            let expected = pinned
                .iter()
                .filter(|(name, _)| fedora_files.iter().any(|file| file.name == ***name))
                .map(|(name, resolved)| {
                    (
                        (*name).clone(),
                        (resolved.version.clone(), resolved.repository.clone()),
                    )
                })
                .collect();
            sink.notes.push(
                "= index (private: each pinned fedora build is fetched to the pin stage, from a \
                 configured repository or Fedora's signed Koji copy, and checked against the \
                 lock's SHA-256 before dnf5 installs it)"
                    .to_string(),
            );
            *sink.pinned = Some(Pinned {
                set: String::new(),
                arch_files: Vec::new(),
                fedora_files: fedora_files.clone(),
                arch_day: None,
                archived_keyring: false,
                refresh: Vec::new(),
                install: fedora_install.clone(),
                remove: fedora_ignore.clone(),
                pinning,
                checks: Vec::new(),
                expected,
            });
        } else if private && shape.has_partial_upgrade {
            // Pacman cannot read apt's deb822 set. Its own configuration is entirely under the
            // root, and the repo URLs come only from the catalogue's snapshot_url (LD-396).
            let day = snapshot
                .or_else(|| pinned.values().filter_map(|p| p.snapshot).next())
                .ok_or_else(|| {
                    Diagnostic::new("E_UNSUPPORTED", "an Arch pin needs a dated archive")
                })?;
            let mut every = std::collections::BTreeSet::new();
            every.insert(day);
            every.extend(pinned.values().filter_map(|p| p.snapshot));
            let mut checks = Vec::new();
            for instant in every {
                let recorded = pins
                    .resolution
                    .snapshot
                    .as_ref()
                    .filter(|s| s.instant == instant && s.recorded && !s.indexes.is_empty())
                    .map(|s| s.indexes.clone());
                if let Some(digests) =
                    recorded.or_else(|| pins.resolution.fetched.get(&instant).cloned())
                {
                    checks.push((instant, digests));
                }
            }
            let expected = pinned
                .iter()
                .filter(|(name, _)| {
                    install.contains(**name) || changes.iter().any(|(n, _, _)| n == **name)
                })
                .map(|(name, resolved)| {
                    (
                        (*name).clone(),
                        (resolved.version.clone(), resolved.origin()),
                    )
                })
                .collect();
            sink.notes.push(format!(
                "= index (private: pacman reads the dated Arch archive at {} through pin.conf{})",
                crate::util::format_utc(day),
                if declared.archived_keyring {
                    ", verifying against that day's archlinux-keyring in a private keyring"
                } else {
                    ""
                }
            ));
            *sink.pinned = Some(Pinned {
                set: super::pin::arch::render_config(&crate::util::format_utc(day))?,
                arch_day: Some(day),
                archived_keyring: declared.archived_keyring,
                arch_files,
                fedora_files: Vec::new(),
                refresh: backend.private_refresh()?,
                install: specs.clone(),
                remove: given.to_vec(),
                pinning,
                checks,
                expected,
            });
        } else if private {
            let entries = super::sourceset::read_machine_entries(&gate.root, &|_| false)?;
            let mirrors = super::sourceset::read_mirror_lists(&gate.root, &entries)?;
            let mut instants = std::collections::BTreeSet::new();
            for (name, resolved) in &pinned {
                let moving = install.contains(*name) || changes.iter().any(|(n, _, _)| n == *name);
                if let Some(instant) = resolved.snapshot
                    && moving
                    && Some(instant) != snapshot
                {
                    instants.insert(instant);
                }
            }
            let stanzas = super::pin::private_stanzas(
                gate.distro,
                &gate.os.codename,
                &entries,
                &mirrors,
                snapshot,
                &instants,
            )?;
            let mut every = instants.clone();
            every.extend(snapshot);
            let mut checks = Vec::new();
            for instant in every {
                let recorded = pins
                    .resolution
                    .snapshot
                    .as_ref()
                    .filter(|s| s.instant == instant && s.recorded && !s.indexes.is_empty())
                    .map(|s| s.indexes.clone());
                if let Some(digests) =
                    recorded.or_else(|| pins.resolution.fetched.get(&instant).cloned())
                {
                    checks.push((instant, digests));
                }
            }
            let expected = pinned
                .iter()
                .filter(|(name, _)| {
                    install.contains(**name) || changes.iter().any(|(n, _, _)| n == **name)
                })
                .map(|(name, resolved)| {
                    (
                        (*name).clone(),
                        (resolved.version.clone(), resolved.origin()),
                    )
                })
                .collect();
            sink.notes.push(private_note(snapshot));
            *sink.pinned = Some(Pinned {
                set: super::sourceset::render_pin_set(&stanzas),
                arch_files: Vec::new(),
                fedora_files: Vec::new(),
                arch_day: None,
                archived_keyring: false,
                refresh: backend.private_refresh()?,
                install: specs.clone(),
                remove: given.to_vec(),
                pinning,
                checks,
                expected,
            });
        }
        for name in &in_transaction {
            action
                .before
                .insert(name.clone(), installed(observed.installed.get(name)));
            action.after.insert(name.clone(), not_installed());
        }
        // A held name is expected to be exactly where it was: that is what holding it means,
        // and it is what makes an interrupted transaction's brackets say so.
        for name in &ignore {
            if let Some(version) = observed.installed.get(name) {
                let fact = installed(Some(version));
                action.before.insert(name.clone(), fact.clone());
                action.after.insert(name.clone(), fact);
            }
        }
        if shape.names_builds {
            for name in &install {
                let pin = pinned.get(name).map(|resolved| &resolved.version);
                if let Some(version) = pin.or_else(|| candidates.get(name)) {
                    action.builds.insert(name.clone(), version.clone());
                }
            }
        }
        action.install = install.clone();
        action.deferred = deferred.clone();
        action.remove = in_transaction.clone();
        action.ignore = ignore.clone();
        // A transaction with nothing to install and nothing to remove is not a transaction: a
        // hold on its own binds invocations that are not being made.
        if action.invocations.is_empty() {
            debug_assert!(install.is_empty() && in_transaction.is_empty() && changes.is_empty());
        } else {
            actions.push(Action {
                id: "p1".to_string(),
                kind: Kind::package(action),
            });
        }
    }

    if shape.removals_are_a_second_action && !remove.is_empty() {
        let mut action = PackageAction::of(Step::Removal);
        action.managed = wanted.clone();
        if exact.is_some() {
            action.invocations = backend.exact_removal(&roots)?;
            action.reasons = reasons.clone();
            action.recheck = Some(Recheck {
                install: Vec::new(),
                roots: roots.clone(),
                pinning: pm::Pinning::default(),
            });
        } else {
            action.invocations = backend.removal(&remove)?;
        }
        for name in &remove {
            action
                .before
                .insert(name.clone(), installed(observed.installed.get(name)));
            action.after.insert(name.clone(), not_installed());
            if shape.names_builds
                && let Some(version) = observed.installed.get(name)
            {
                action.builds.insert(name.clone(), version.clone());
            }
        }
        action.remove = remove.clone();
        actions.push(Action {
            id: "p2".to_string(),
            kind: Kind::package(action),
        });
    }

    // What the marks will be once the transaction has run: apt marks a name it was asked for
    // `manual`, and leaves every other name exactly as it found it.
    let will_be_auto = |name: &String| -> bool {
        !install.iter().any(|n| n == name) && observed.auto.contains(name)
    };
    let mut action = PackageAction::of(Step::Marks);
    action.managed = wanted.clone();
    // A candidate something still needs stays, as a dependency, and leaves with the last package
    // that needs it (LD-375). One that already is a dependency needs no mark: it is said.
    if let Some(decision) = &exact {
        for name in &decision.demote {
            let reason = decision.reasons.get(name).cloned().unwrap_or_default();
            if observed.auto.contains(name) {
                sink.kept.push(format!("= package {name} ({reason})"));
            } else {
                action.auto.push(name.clone());
                action.reasons.insert(name.clone(), reason);
            }
        }
    }
    for name in &wanted {
        let is_auto = will_be_auto(name);
        if mark_auto.contains(name) {
            if !is_auto {
                action.auto.push(name.clone());
            }
        } else if is_auto {
            action.manual.push(name.clone());
        }
        if shape.hold_is_machine_state && hold.contains(name) && !observed.held.contains(name) {
            action.hold.push(name.clone());
        }
    }
    // A hold **Lodi** set, on a name the manifest no longer holds, is released. A hold the
    // operator put on by hand is not Lodi's to take off. Neither applies to a family whose hold
    // was never on the machine: there is nothing set, so there is nothing to release.
    if shape.hold_is_machine_state
        && let Some(lock) = lock
    {
        for (name, record) in &lock.packages {
            if record.held && !hold.contains(name) && observed.held.contains(name) {
                action.unhold.push(name.clone());
                // A pin the host directory recorded is gone: the hold goes with it, and the
                // version it installed stays (P9).
                if pins.recorded.contains(name)
                    && let Some(version) = observed.installed.get(name)
                {
                    action.reasons.insert(
                        name.clone(),
                        format!("unhold: no longer pinned; {version} stays installed"),
                    );
                }
            }
        }
    }
    if !(action.auto.is_empty()
        && action.manual.is_empty()
        && action.hold.is_empty()
        && action.unhold.is_empty())
    {
        action.invocations = backend.marks(&action.auto, &action.manual)?;
        action
            .invocations
            .extend(backend.holds(&action.hold, &action.unhold)?);
        for (names, auto, held) in [
            (&action.auto, Some(true), None),
            (&action.manual, Some(false), None),
            (&action.hold, None, Some(true)),
            (&action.unhold, None, Some(false)),
        ] {
            for name in names {
                let before = action.before.entry(name.clone()).or_insert(PackageFact {
                    installed: true,
                    version: None,
                    auto: None,
                    held: None,
                });
                if auto.is_some() {
                    before.auto = Some(will_be_auto(name));
                }
                if held.is_some() {
                    before.held = Some(observed.held.contains(name));
                }
                let after = action.after.entry(name.clone()).or_insert(PackageFact {
                    installed: true,
                    version: None,
                    auto: None,
                    held: None,
                });
                if auto.is_some() {
                    after.auto = auto;
                }
                if held.is_some() {
                    after.held = held;
                }
            }
        }
        actions.push(Action {
            id: "m1".to_string(),
            kind: Kind::package(action),
        });
    }

    // The package half of the record as the machine is now: what a converged apply writes. An
    // exact record is what the manifest declares and the machine has; a managed one is what it
    // recorded and declares, less what leaves (1.1.0's rule).
    let mut names: std::collections::BTreeSet<String> = wanted.iter().cloned().collect();
    if exact.is_none()
        && let Some(lock) = lock
    {
        names.extend(lock.packages.keys().cloned());
    }
    for name in &remove {
        names.remove(name);
    }
    // A pinned name the machine already has at its version is said, with where it came from.
    for (name, resolved) in &pinned {
        let moving = install.contains(*name) || changes.iter().any(|(n, _, _)| n == *name);
        if !moving && observed.installed.get(*name) == Some(&resolved.version) {
            sink.kept.push(format!(
                "= package {name} ({})",
                pin_note(resolved, pins, None, candidates.get(*name), gate.distro)
            ));
        }
    }
    let record = records(&names, &observed, &holds, &ignore);
    let baseline = super::reconcile::packages_baseline(
        lock.and_then(|lock| lock.baseline.as_ref()),
        &wanted,
        &observed,
        &declared.hold,
        shape.hold_is_machine_state,
    );
    Ok(Packages {
        exact: exact.is_some(),
        managed: wanted,
        removed: remove,
        hold: holds.clone(),
        ignore,
        record,
        baseline,
    })
}

/// A pinned name's resolved version and where it came from, in the one wording every plan line
/// uses (M-Pin): then the move this apply makes, if any; `behind latest` with the version the
/// live index offers when that is newer than the pin and not the version the move leaves; and,
/// when `pins.lock` does not record the pin, the command that records it (LD-397).
fn pin_note(
    resolved: &super::pin::Resolved,
    pins: &Pins,
    moving: Option<(&str, &String)>,
    offered: Option<&String>,
    distro: super::safety::Distro,
) -> String {
    let mut note = format!("pinned {} from {}", resolved.version, resolved.origin());
    if let Some((direction, from)) = moving {
        note.push_str(&format!("; {direction} from {from}"));
    }
    if let Some(offered) = offered
        && moving.is_none_or(|(_, from)| from != offered)
        && super::pin::compare_for(distro, offered, &resolved.version)
            == std::cmp::Ordering::Greater
    {
        note.push_str(&format!("; {BEHIND} {offered}"));
    }
    if !resolved.recorded {
        let (before, after) = &pins.record_with;
        note.push_str(&format!(
            "; {UNRECORDED}, record it with: {before}lodi host pin {} --to {}{after}",
            resolved.name,
            super::pin::verbs::word(&resolved.requested)
        ));
    }
    note
}

/// The record of each name the machine shows installed: the version it shows, its mark, and
/// whether it is held — by the machine, or by lodi's own transactions (D11).
pub fn records(
    names: &std::collections::BTreeSet<String>,
    observed: &pm::Observed,
    holds: &[String],
    ignored: &[String],
) -> BTreeMap<String, super::lock::PackageRecord> {
    let mut out = BTreeMap::new();
    for name in names {
        let Some(version) = observed.installed.get(name) else {
            continue;
        };
        out.insert(
            name.clone(),
            super::lock::PackageRecord {
                version: version.clone(),
                mark: if observed.auto.contains(name) {
                    "auto"
                } else {
                    "manual"
                }
                .to_string(),
                // A hold is recorded only where the manifest declares it, so that the one Lodi
                // releases when the declaration goes is never one set by hand (LD-377).
                held: (observed.held.contains(name) && holds.iter().any(|n| n == name))
                    || ignored.iter().any(|n| n == name),
            },
        );
    }
    out
}

/// A declared name the distribution's index does not offer. An unknown input is an error, never
/// a silent default — and the nearest names it does offer are what a typo actually needs.
fn unknown_package(backend: &dyn pm::Backend, unknown: &[String]) -> Diagnostic {
    // A pacman group is a name the index knows and no package: it is refused as what it is,
    // never expanded, because what a group holds is the distribution's to change and an exact
    // apply would then remove what it no longer holds (LD-375).
    if let Some((group, members)) = backend
        .groups(unknown)
        .unwrap_or_default()
        .into_iter()
        .next()
    {
        return Diagnostic::new(
            "E_UNKNOWN_PACKAGE",
            format!(
                "`{group}` is a pacman group, not a package: it stands for {}",
                members.join(", ")
            ),
        )
        .hint(
            "declare the packages of the group this machine should have, by name; lodi does not \
             expand a group",
        );
    }
    let name = &unknown[0];
    let others = if unknown.len() > 1 {
        format!(" (nor {})", unknown[1..].join(", "))
    } else {
        String::new()
    };
    let diagnostic = Diagnostic::new(
        "E_UNKNOWN_PACKAGE",
        format!(
            "the {} index offers no package called `{name}`{others}",
            backend.distro().name()
        ),
    );
    let nearest = backend.nearest(name);
    if nearest.is_empty() {
        diagnostic.hint(
            "check the name against the distribution's own package list, or put it in \
             `[packages] optional` to skip it where it does not exist",
        )
    } else {
        diagnostic.hint(format!(
            "the nearest names the index knows are {}; or put it in `[packages] optional` to \
             skip it where it does not exist",
            nearest.join(", ")
        ))
    }
}

fn plan_absent(id: &str, path: &str, before: Facts, drift: bool, on_remove: &OnRemove) -> Action {
    let op = if before.exists {
        Op::Remove
    } else {
        Op::Unchanged
    };
    let changes = if op == Op::Unchanged {
        vec!["unchanged".to_string()]
    } else {
        Vec::new()
    };
    Action {
        id: id.to_string(),
        kind: Kind::file(FileAction {
            path: path.to_string(),
            op,
            desired: None,
            backup: None,
            retained_backup: None,
            adopt: false,
            after: Facts::absent(path),
            before,
            drift,
            restore_from: None,
            on_remove: on_remove.name().to_string(),
            changes,
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn plan_present(
    gate: &Gate,
    id: &str,
    path: &str,
    entry: &FileEntry,
    sources: &BTreeMap<String, Vec<u8>>,
    before: Facts,
    recorded: Option<&super::lock::FileRecord>,
    recorded_earlier: bool,
) -> Result<Action, Diagnostic> {
    let drift = matches!((recorded, &before.digest), (Some(record), Some(digest))
        if &record.digest != digest);
    let (content, digest) = match (&entry.content, &entry.source) {
        (Some(text), _) => {
            let bytes = text.as_bytes().to_vec();
            let digest = format!("sha256:{}", sha256_hex(&bytes));
            (Content::Inline(bytes), digest)
        }
        (None, Some(source)) => {
            let bytes = sources.get(source).cloned().ok_or_else(|| {
                Diagnostic::new(
                    "E_STORE_IO",
                    format!("{path}: source {source:?} was not read before the plan"),
                )
            })?;
            let digest = format!("sha256:{}", sha256_hex(&bytes));
            (Content::Inline(bytes), digest)
        }
        (None, None) => {
            return Err(Diagnostic::new(
                "E_ATTR_CONFLICT",
                format!("{path} declares neither `content` nor `source`"),
            ));
        }
    };
    let uid = resolve_user(&gate.root, &entry.owner).ok_or_else(|| {
        Diagnostic::new(
            "E_APPLY",
            format!(
                "`owner = \"{}\"` of {path} names no user of this root",
                entry.owner
            ),
        )
        .hint("use a name from the root's own /etc/passwd, or a numeric id")
    })?;
    let gid = resolve_group(&gate.root, &entry.group).ok_or_else(|| {
        Diagnostic::new(
            "E_APPLY",
            format!(
                "`group = \"{}\"` of {path} names no group of this root",
                entry.group
            ),
        )
        .hint("use a name from the root's own /etc/group, or a numeric id")
    })?;

    let mut changes = Vec::new();
    let content_differs = before.digest.as_deref() != Some(digest.as_str());
    if !before.exists {
        changes.push(format!("mode {:04o}", entry.mode));
        changes.push(format!("owner {}:{}", entry.owner, entry.group));
    } else {
        if content_differs {
            changes.push("content".to_string());
        }
        if before.mode != Some(entry.mode) {
            changes.push(format!(
                "mode {:04o} -> {:04o}",
                before.mode.unwrap_or(0),
                entry.mode
            ));
        }
        if before.uid != Some(uid) || before.gid != Some(gid) {
            changes.push(format!("owner {}:{}", entry.owner, entry.group));
        }
    }
    let op = if !before.exists {
        Op::Create
    } else if content_differs {
        Op::Replace
    } else if changes.is_empty() {
        Op::Unchanged
    } else {
        Op::Metadata
    };
    if op == Op::Unchanged {
        changes.push("unchanged".to_string());
    }
    if drift && op != Op::Unchanged {
        changes.insert(0, "drift".to_string());
    }
    // Whether the bytes at this path are still the original, the ones Lodi did not write
    // (LD-377): a file that was there before any record of it, or one whose record says Lodi
    // adopted it in place and has not written it since.
    //
    // A record an earlier release wrote with no copy cannot say which it is, so the file there is
    // treated as an original too: its bytes are kept before Lodi writes over them, and without a
    // copy its removal leaves it (LD-377 validator repair).
    let unwritten = match recorded {
        None => before.exists,
        Some(record) => {
            (adopted_in_place(record) || (recorded_earlier && before.exists))
                && entry.on_remove == OnRemove::Restore
        }
    };
    // The original is kept, once, the first time Lodi writes over it — its bytes or only its
    // mode and owner — when `backup` says so. It is then carried forward by `retained_backup`
    // on later applies. A file adopted unchanged keeps its original where it is, and nothing is
    // copied until something would change it.
    let writes = matches!(op, Op::Replace | Op::Metadata);
    let backup = (writes && entry.backup && unwritten).then(|| {
        if entry.kept_in_store {
            super::originals::store_path(path)
        } else {
            backup_path(path)
        }
    });
    // Without a copy there is nothing to restore from, and a restore of a file Lodi did not
    // create is never a delete: the record says `keep`, and the plan says so when it matters.
    let on_remove = if unwritten && backup.is_none() && entry.on_remove == OnRemove::Restore {
        if writes {
            changes.push("no copy kept: left in place when removed".to_string());
        }
        "keep"
    } else {
        entry.on_remove.name()
    };
    // Adoption of a file already equal (LD-387): no record, nothing to write, and an entry that
    // keeps a copy and restores it. The apply that records it keeps its original in the store,
    // so that a later hand edit cannot become what the removal puts back.
    let adopt = recorded.is_none()
        && op == Op::Unchanged
        && entry.backup
        && entry.on_remove == OnRemove::Restore;
    let after = Facts {
        path: path.to_string(),
        exists: true,
        regular: true,
        digest: Some(digest.clone()),
        mode: Some(entry.mode),
        uid: Some(uid),
        gid: Some(gid),
    };
    Ok(Action {
        id: id.to_string(),
        kind: Kind::file(FileAction {
            path: path.to_string(),
            op,
            desired: Some(Desired {
                digest,
                mode: entry.mode,
                uid,
                gid,
                owner: entry.owner.clone(),
                group: entry.group.clone(),
                content,
            }),
            backup,
            retained_backup: recorded.and_then(|record| record.backup.clone()),
            adopt,
            before,
            after,
            drift,
            restore_from: None,
            on_remove: on_remove.to_string(),
            changes,
        }),
    })
}

/// A record of a file Lodi adopted where it stood and has not written: no copy, and `keep`,
/// which is what a restore of an original still in place is (LD-377).
///
/// A file Lodi created and declared `on_remove = "keep"` has the same record, and a later
/// `restore` of it is then read as `keep` too: the ambiguity errs towards leaving a file, never
/// towards deleting one.
fn adopted_in_place(record: &super::lock::FileRecord) -> bool {
    record.backup.is_none() && record.on_remove == "keep"
}

/// A record with no copy whose removal is `restore`: this release writes it only for a file Lodi
/// created, and an earlier one wrote it for a file it found already right as well.
fn unbacked_restore(record: &super::lock::FileRecord) -> bool {
    record.backup.is_none() && record.on_remove == "restore"
}

/// The first release whose record tells a file Lodi adopted from one it created (LD-377).
const ADOPTION_SINCE: &str = "1.1.1";

/// The writer a lock's `generatedBy` names, when it is a release before [`ADOPTION_SINCE`] or no
/// release this build can read — whose unbacked `restore` records are therefore ambiguous.
fn written_before_adoption(generated_by: &str) -> Option<String> {
    let since = crate::version::Version::parse(ADOPTION_SINCE).expect("a version");
    let written = generated_by
        .strip_prefix("lodi ")
        .and_then(|version| crate::version::Version::parse(version).ok());
    match written {
        Some(version) if version >= since => None,
        _ => Some(generated_by.to_string()),
    }
}

fn plan_departed(
    gate: &Gate,
    id: &str,
    path: &str,
    record: &super::lock::FileRecord,
    before: Facts,
    drift: bool,
    recorded_earlier: Option<&str>,
) -> Result<Action, Diagnostic> {
    let policy = match record.on_remove.as_str() {
        "restore" => OnRemove::Restore,
        "delete" => OnRemove::Delete,
        "keep" => OnRemove::Keep,
        other => {
            return Err(Diagnostic::new(
                "E_LOCK_VERSION",
                format!(
                    "{} records unknown onRemove policy {other:?}",
                    gate.lock_path().display()
                ),
            ));
        }
    };
    let (op, desired, restore_from, after, changes) = match (policy, &record.backup) {
        (OnRemove::Restore, Some(backup)) => {
            super::files::inspect_trusted(&gate.root, backup, true)?;
            let kept = Facts::observe(&gate.root, backup);
            if !kept.exists || !kept.regular || kept.digest.is_none() {
                return Err(Diagnostic::new(
                    "E_APPLY",
                    format!(
                        "the recorded backup {backup} for {path} is missing or not a regular file"
                    ),
                )
                .hint("put the recorded backup back before applying"));
            }
            let desired = Desired {
                digest: kept.digest.clone().expect("checked"),
                mode: kept.mode.unwrap_or(0o644),
                uid: kept.uid.unwrap_or(gate.euid),
                gid: kept.gid.unwrap_or(gate.euid),
                owner: kept.uid.unwrap_or(gate.euid).to_string(),
                group: kept.gid.unwrap_or(gate.euid).to_string(),
                content: Content::Kept(join(&gate.root, backup)),
            };
            let after = Facts {
                path: path.to_string(),
                exists: true,
                regular: true,
                digest: Some(desired.digest.clone()),
                mode: Some(desired.mode),
                uid: Some(desired.uid),
                gid: Some(desired.gid),
            };
            (
                Op::Restore,
                Some(desired),
                Some(backup.clone()),
                after,
                vec![format!("restore {backup}")],
            )
        }
        // An earlier release's record with no copy: the file may be one Lodi found, so it is never
        // deleted — it is left where it stands and the record forgets it (LD-377 validator
        // repair). Deleting a file Lodi created is then a step the operator takes.
        (OnRemove::Restore, None) if before.exists && recorded_earlier.is_some() => (
            Op::Keep,
            None,
            None,
            before.clone(),
            vec![format!(
                "recorded by {} with no copy, so it may not be lodi's: left in place, forget \
                 record",
                recorded_earlier.unwrap_or_default()
            )],
        ),
        (OnRemove::Restore | OnRemove::Delete, None) => {
            if before.exists {
                (Op::Remove, None, None, Facts::absent(path), Vec::new())
            } else {
                (
                    Op::Keep,
                    None,
                    None,
                    Facts::absent(path),
                    vec!["already absent; forget record".to_string()],
                )
            }
        }
        (OnRemove::Delete, Some(_)) => {
            if before.exists {
                (Op::Remove, None, None, Facts::absent(path), Vec::new())
            } else {
                (
                    Op::Keep,
                    None,
                    None,
                    Facts::absent(path),
                    vec!["already absent; forget record".to_string()],
                )
            }
        }
        (OnRemove::Keep, _) => (
            Op::Keep,
            None,
            None,
            before.clone(),
            vec!["kept; forget record".to_string()],
        ),
    };
    Ok(Action {
        id: id.to_string(),
        kind: Kind::file(FileAction {
            path: path.to_string(),
            op,
            desired,
            backup: None,
            retained_backup: None,
            adopt: false,
            restore_from,
            before,
            after,
            drift,
            on_remove: record.on_remove.clone(),
            changes,
        }),
    })
}

/// Where the backup of a replaced unmanaged file goes: beside the managed path, with a UTC
/// suffix. The name is chosen while planning so the journal can announce it before mutation.
pub fn backup_path(path: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.subsec_nanos());
    format!(
        "{path}.lodi-backup-{}-{nanos:09}",
        crate::util::snapshot_id(crate::util::now_utc()),
    )
}

/// Resolve `owner` against the **root's own** `/etc/passwd`, or a numeric id.
pub fn resolve_user(root: &Path, name: &str) -> Option<u32> {
    if let Ok(id) = name.parse::<u32>() {
        return Some(id);
    }
    crate::passwd::lookup(&root.join("etc/passwd"), name)
}

/// Resolve `group` against the **root's own** `/etc/group`, or a numeric id.
pub fn resolve_group(root: &Path, name: &str) -> Option<u32> {
    if let Ok(id) = name.parse::<u32>() {
        return Some(id);
    }
    crate::passwd::lookup(&root.join("etc/group"), name)
}

/// The file half of the record this plan leaves: every declared file it acts on or finds
/// already right, as it will stand, with the original it keeps and what its removal does.
pub fn file_records(plan: &Plan) -> BTreeMap<String, super::lock::FileRecord> {
    let mut out = BTreeMap::new();
    // A source's keyring and stanza are recorded as the files they are (LD-365): the lock's
    // shape is unchanged, and a departed source is planned from these records.
    for file in plan.file_actions() {
        let Some(desired) = &file.desired else {
            continue;
        };
        // A restore puts the original back and forgets the path; only a declared file is
        // recorded.
        if file.op == Op::Restore {
            continue;
        }
        out.insert(
            file.path.clone(),
            super::lock::FileRecord {
                digest: desired.digest.clone(),
                mode: format!("{:04o}", desired.mode),
                owner: desired.owner.clone(),
                group: desired.group.clone(),
                backup: file.backup.clone().or_else(|| file.retained_backup.clone()),
                on_remove: file.on_remove.clone(),
            },
        );
    }
    out
}

#[cfg(test)]
mod tests {
    /// The modes that are named before they are applied, with the rest left alone.
    #[test]
    fn a_set_id_or_world_writable_mode_is_named() {
        for (mode, says) in [
            (0o4755, "is set-uid"),
            (0o2755, "is set-gid"),
            (0o6755, "is set-uid and is set-gid"),
            (0o0666, "lets anyone write the file"),
            (0o1777, "lets anyone write the file"),
            (0o4757, "is set-uid and lets anyone write the file"),
        ] {
            assert_eq!(notable_mode(mode).as_deref(), Some(says), "{mode:04o}");
        }
        for mode in [0o644, 0o600, 0o755, 0o664, 0o1755, 0o0775, 0o0] {
            assert_eq!(notable_mode(mode), None, "{mode:04o}");
        }
    }

    use super::*;

    fn file_action(op: Op, path: &str, changes: &[&str]) -> Action {
        Action {
            id: "f1".into(),
            kind: Kind::file(FileAction {
                path: path.into(),
                op,
                desired: None,
                backup: None,
                retained_backup: None,
                adopt: false,
                restore_from: None,
                before: Facts::absent(path),
                after: Facts::absent(path),
                drift: false,
                on_remove: "restore".into(),
                changes: changes.iter().map(|c| (*c).to_string()).collect(),
            }),
        }
    }

    #[test]
    fn the_printed_grammar_is_the_one_the_packet_documents() {
        assert_eq!(
            file_action(Op::Create, "/etc/x.conf", &["mode 0644", "owner root:root"]).lines(),
            vec!["+ file /etc/x.conf (mode 0644, owner root:root)"]
        );
        assert_eq!(
            file_action(
                Op::Replace,
                "/etc/x.conf",
                &["content", "mode 0644 -> 0600"]
            )
            .lines(),
            vec!["~ file /etc/x.conf (content, mode 0644 -> 0600)"]
        );
        assert_eq!(
            file_action(Op::Remove, "/etc/x.conf", &[]).lines(),
            vec!["- file /etc/x.conf"]
        );
        assert_eq!(
            file_action(Op::Unchanged, "/etc/x.conf", &["unchanged"]).lines(),
            vec!["= file /etc/x.conf (unchanged)"]
        );
        let mut transaction = PackageAction::of(Step::Transaction);
        transaction.install = vec!["git".into()];
        transaction.remove = vec!["nano".into()];
        let packages = Action {
            id: "p1".into(),
            kind: Kind::package(transaction),
        };
        assert_eq!(
            packages.lines(),
            vec!["+ package git".to_string(), "- package nano".to_string()]
        );
        assert_eq!(packages.kind_name(), "packages.transaction");

        let mut marks = PackageAction::of(Step::Marks);
        marks.auto = vec!["libfoo".into()];
        marks.manual = vec!["git".into()];
        marks.hold = vec!["nano".into()];
        marks.unhold = vec!["ed".into()];
        let marks = Action {
            id: "m1".into(),
            kind: Kind::package(marks),
        };
        assert_eq!(
            marks.lines(),
            vec![
                "~ package libfoo (auto)".to_string(),
                "~ package git (manual)".to_string(),
                "~ package nano (hold)".to_string(),
                "~ package ed (unhold)".to_string(),
            ]
        );
        assert_eq!(marks.kind_name(), "packages.marks");
    }

    #[test]
    fn a_backup_name_is_beside_the_path_and_has_a_utc_suffix() {
        let name = backup_path("/etc/ssh/sshd_config");
        assert!(
            name.starts_with("/etc/ssh/sshd_config.lodi-backup-"),
            "{name}"
        );
    }
}
