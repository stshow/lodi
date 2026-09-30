//! The intent journal: what an apply is about to do, on disk and fsynced **before** it does any
//! of it, and the classifier that reads it back after an interruption.
//!
//! ADR-013 is the whole design here. There are no generations and there is no rollback. What the
//! journal buys is the ability to answer one question after a machine was interrupted: *did that
//! action happen or not?* The answer has three shapes, and only two of them let a second apply
//! continue:
//!
//! - **completed** — every action the journal announced has an `end` record, or the machine's
//!   observed state matches the action's recorded `post` state. Nothing is outstanding.
//! - **incomplete** — the interrupted action's `pre` state is what the machine still shows. It
//!   did not happen; the next apply simply plans again and does it.
//! - **ambiguous** — the machine matches neither the `pre` nor the `post` state of the
//!   interrupted action. Lodi does not guess and does not "repair": it stops with
//!   `E_JOURNAL_AMBIGUOUS` (exit 8), names the action and the journal, and waits for a human to
//!   look and then say so with `--resolved JOURNAL-ID`.
//!
//! The declaration is not a flag that silences a warning: it is recorded in the **next** journal,
//! so the record of the machine keeps the fact that a human, not Lodi, decided the outcome.
//!
//! One journal is one file under `<root>/var/lib/lodi/host/journal/`, named
//! `<YYYYMMDDTHHMMSSZ>-<32 hex>.json`, whose stem is the **journal id**. Its content is a stream
//! of JSON objects, one per line, each appended and fsynced before the action it announces. A
//! line-per-record stream is what makes a half-written tail detectable: the classifier discards a
//! trailing line that does not parse, which is exactly the record whose write was interrupted.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::diag::Diagnostic;
use crate::util::{now_utc, sha256_hex, snapshot_id};

use super::plan::{Facts, Kind, PackageFact, Plan};
use super::safety::Gate;

/// The journal format this build writes and is willing to read.
pub const VERSION: u32 = 1;

/// The registry row a journal is registered as (`src/schema.rs`, M-1.0 T-1).
pub const ARTIFACT: &str = "host-journal";

/// One announced action: what it is, and the two states that bracket it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    pub kind: String,
    pub summary: String,
    /// Everything the action expects to be true **before** it runs.
    pub pre: Vec<Facts>,
    /// Everything that must be true **after** it has fully run.
    pub post: Vec<Facts>,
    /// The package names this action touches and what each should look like before it runs.
    /// Empty for a file action, which is what the two vectors above describe.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub packages_pre: BTreeMap<String, PackageFact>,
    /// The same names, as they should look once the action has fully run.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub packages_post: BTreeMap<String, PackageFact>,
    /// The argv and resolved path of every process the action will run, written down **before**
    /// it runs — which is what makes "did that transaction happen?" a question with an answer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<Vec<String>>,
    /// The absolute path each of those programs resolved to on `PATH`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub programs: Vec<String>,
}

/// The first line of a journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Header {
    pub record: String,
    pub version: u32,
    pub journal_id: String,
    pub started_at: String,
    pub lodi_version: String,
    pub root: String,
    pub manifest_digest: String,
    pub distro: String,
    /// The package-manager programs this apply resolved, name and absolute path. The seam is
    /// recorded so that a reader can see which binaries a transaction would have run.
    pub programs: Vec<(String, String)>,
    pub euid: u32,
    /// The journal id a human declared resolved before this apply was allowed to start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<String>,
    pub actions: Vec<Entry>,
}

/// Every record after the header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "camelCase")]
pub enum Event {
    Begin {
        action: String,
        at: String,
    },
    End {
        action: String,
        at: String,
    },
    Failed {
        action: String,
        at: String,
        error: String,
    },
    #[serde(rename = "finished", alias = "commit")]
    Finished {
        at: String,
    },
}

impl Event {
    fn action(&self) -> Option<&str> {
        match self {
            Event::Begin { action, .. }
            | Event::End { action, .. }
            | Event::Failed { action, .. } => Some(action),
            Event::Finished { .. } => None,
        }
    }
}

/// An open journal. Every write is appended and fsynced before the caller is allowed to proceed.
#[derive(Debug)]
pub struct Journal {
    pub id: String,
    pub path: PathBuf,
    file: fs::File,
}

impl Journal {
    /// Create the journal for `plan` and fsync it, together with its directory, **before** the
    /// apply is allowed to touch anything. Returns the open handle.
    pub fn create(gate: &Gate, plan: &Plan, resolved: Option<&str>) -> Result<Journal, Diagnostic> {
        let dir = super::files::ensure_dir_trusted(
            &gate.root,
            &format!("/{}/journal", super::safety::STATE),
            0o700,
        )?;
        let started = now_utc();
        let entries = entries(plan);
        // The stem is sortable to the second; the hex distinguishes two applies that begin
        // inside the same second, which two processes on one root can.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let id = format!(
            "{}-{}",
            snapshot_id(started),
            &sha256_hex(
                format!(
                    "{}|{}|{started}|{nanos}|{}",
                    plan.root.display(),
                    plan.manifest_digest,
                    std::process::id()
                )
                .as_bytes()
            )[..32]
        );
        let header = Header {
            record: "header".to_string(),
            version: VERSION,
            journal_id: id.clone(),
            started_at: crate::util::format_utc(started),
            lodi_version: crate::schema::LODI_VERSION.to_string(),
            root: gate.root.display().to_string(),
            manifest_digest: plan.manifest_digest.clone(),
            distro: gate.distro.name().to_string(),
            programs: gate
                .pm
                .programs
                .iter()
                .map(|(name, path)| (name.clone(), path.display().to_string()))
                .collect(),
            euid: gate.euid,
            resolved: resolved.map(|id| id.to_string()),
            actions: entries,
        };
        let path = dir.join(format!("{id}.json"));
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .map_err(|e| io_error(&path, e))?;
        let line = serde_json::to_string(&header).map_err(|e| {
            Diagnostic::new("E_STORE_IO", format!("cannot serialise the journal: {e}"))
        })?;
        file.write_all(line.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|e| io_error(&path, e))?;
        // The file's own fsync does not promise its name is in the directory; the directory is
        // synced too, so that the journal is findable after a power loss, not only after a kill.
        if let Ok(handle) = fs::File::open(&dir) {
            let _ = handle.sync_all();
        }
        Ok(Journal { id, path, file })
    }

    /// Append one record and fsync it before returning. The caller performs the mutation the
    /// record announces only after this has returned.
    pub fn append(&mut self, event: &Event) -> Result<(), Diagnostic> {
        let line = serde_json::to_string(event).map_err(|e| {
            Diagnostic::new(
                "E_STORE_IO",
                format!("cannot serialise a journal record: {e}"),
            )
        })?;
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.write_all(b"\n"))
            .and_then(|()| self.file.sync_all())
            .map_err(|e| io_error(&self.path, e))
    }

    pub fn begin(&mut self, action: &str) -> Result<(), Diagnostic> {
        self.append(&Event::Begin {
            action: action.to_string(),
            at: crate::util::format_utc(now_utc()),
        })
    }

    pub fn end(&mut self, action: &str) -> Result<(), Diagnostic> {
        self.append(&Event::End {
            action: action.to_string(),
            at: crate::util::format_utc(now_utc()),
        })
    }

    pub fn failed(&mut self, action: &str, error: &str) -> Result<(), Diagnostic> {
        self.append(&Event::Failed {
            action: action.to_string(),
            at: crate::util::format_utc(now_utc()),
            error: error.to_string(),
        })
    }

    pub fn commit(&mut self) -> Result<(), Diagnostic> {
        self.append(&Event::Finished {
            at: crate::util::format_utc(now_utc()),
        })
    }
}

/// Every entry an apply of `plan` announces, in the order it may run them. A source step is one
/// entry per file it changes, each a file action of its own, so that an interruption between the
/// keyring and the stanza is classified like any file's (LD-365). When a source forces the
/// index refresh, the restore of each of those files is announced too, last change first: it
/// runs only if that refresh fails (S9), and is written down before it could.
fn entries(plan: &Plan) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut restores = Vec::new();
    for action in plan.changing() {
        let Kind::Source(step) = &action.kind else {
            out.push(entry_for(action));
            continue;
        };
        for (id, file) in step.steps(&action.id) {
            out.push(entry_for(&super::plan::Action {
                id: id.clone(),
                kind: Kind::File(Box::new(file.clone())),
            }));
            let (pre, post) = super::sources::restore_bracket(file);
            restores.push(Entry {
                id: super::sources::file_restore_id(&id),
                kind: "source.restore".to_string(),
                summary: format!("restore {}", file.path),
                pre,
                post,
                packages_pre: BTreeMap::new(),
                packages_post: BTreeMap::new(),
                commands: Vec::new(),
                programs: Vec::new(),
            });
        }
    }
    let forced = plan
        .actions
        .iter()
        .any(|action| matches!(&action.kind, Kind::Package(packages) if packages.forced));
    if forced {
        out.extend(restores.into_iter().rev());
    }
    out
}

/// One announced action, with the two states that bracket it and every command it will run.
fn entry_for(action: &super::plan::Action) -> Entry {
    let (pre, post) = bracket(action);
    let (packages_pre, packages_post, commands, programs) = match &action.kind {
        Kind::File(_) | Kind::Source(_) => {
            (BTreeMap::new(), BTreeMap::new(), Vec::new(), Vec::new())
        }
        Kind::Service(service) => {
            let invocations = service.invocation.iter();
            (
                BTreeMap::new(),
                BTreeMap::new(),
                invocations
                    .clone()
                    .map(|invocation| {
                        let mut argv = vec![invocation.program.clone()];
                        argv.extend(invocation.args.iter().cloned());
                        argv
                    })
                    .collect(),
                invocations
                    .map(|invocation| invocation.path.display().to_string())
                    .collect(),
            )
        }
        Kind::Basic(basic) => {
            let invocations = basic.invocations();
            (
                BTreeMap::new(),
                BTreeMap::new(),
                invocations
                    .iter()
                    .map(|invocation| {
                        let mut argv = vec![invocation.program.clone()];
                        argv.extend(invocation.args.iter().cloned());
                        argv
                    })
                    .collect(),
                invocations
                    .iter()
                    .map(|invocation| invocation.path.display().to_string())
                    .collect(),
            )
        }
        Kind::Identity(identity) => (
            BTreeMap::new(),
            BTreeMap::new(),
            identity
                .invocations
                .iter()
                .map(|invocation| {
                    let mut argv = vec![invocation.program.clone()];
                    argv.extend(invocation.args.iter().cloned());
                    argv
                })
                .collect(),
            identity
                .invocations
                .iter()
                .map(|invocation| invocation.path.display().to_string())
                .collect(),
        ),
        Kind::Package(packages) => (
            packages.before.clone(),
            packages.after.clone(),
            packages
                .invocations
                .iter()
                .map(|invocation| {
                    let mut argv = vec![invocation.program.clone()];
                    argv.extend(invocation.args.iter().cloned());
                    argv
                })
                .collect(),
            packages
                .invocations
                .iter()
                .map(|invocation| invocation.path.display().to_string())
                .collect(),
        ),
    };
    Entry {
        id: action.id.clone(),
        kind: action.kind_name().to_string(),
        summary: action.summary(),
        pre,
        post,
        packages_pre,
        packages_post,
        commands,
        programs,
    }
}

/// The two states that bracket an action, as the journal records them.
fn bracket(action: &super::plan::Action) -> (Vec<Facts>, Vec<Facts>) {
    match &action.kind {
        Kind::File(file) => file_bracket(file),
        // A source step is its files' brackets, concatenated: an interruption between the
        // keyring and the stanza shows as neither state, which is the ambiguity to report.
        Kind::Source(source) => {
            let (mut pre, mut post) = (Vec::new(), Vec::new());
            for file in &source.files {
                let (p, q) = file_bracket(file);
                pre.extend(p);
                post.extend(q);
            }
            (pre, post)
        }
        // A package action's brackets are package states, not paths: they are recorded in the
        // entry's own `packagesPre`/`packagesPost` maps, which need a backend to compare. A
        // service action has none: it is classified as not finished (see below).
        Kind::Package(_) | Kind::Service(_) => (Vec::new(), Vec::new()),
        // An account or group lives in the account databases, which the journal does not read.
        Kind::Identity(_) => (Vec::new(), Vec::new()),
        // A basic lives where its tool keeps it; it is classified as not finished (see below).
        Kind::Basic(_) => (Vec::new(), Vec::new()),
    }
}

fn file_bracket(file: &super::plan::FileAction) -> (Vec<Facts>, Vec<Facts>) {
    let mut pre = vec![file.before.clone()];
    let mut post = vec![file.after.clone()];
    if let Some(backup) = &file.backup {
        // Before: no backup exists. After: the backup holds exactly the bytes that were
        // there. A machine that shows the old target *and* the backup is neither, which
        // is the ambiguity this bracket exists to detect.
        pre.push(Facts::absent(backup));
        post.push(Facts {
            path: backup.clone(),
            exists: true,
            regular: true,
            digest: file.before.digest.clone(),
            mode: None,
            uid: None,
            gid: None,
        });
    }
    if let Some(backup) = &file.restore_from {
        // The caller's plan has already verified the backup. Only the properties that
        // make the restart classification meaningful are needed here; its digest is the
        // target's post digest.
        pre.push(Facts {
            path: backup.clone(),
            exists: true,
            regular: true,
            digest: file.desired.as_ref().map(|d| d.digest.clone()),
            mode: None,
            uid: None,
            gid: None,
        });
        post.push(Facts::absent(backup));
    }
    (pre, post)
}

/// `<root>/var/lib/lodi/host/journal`.
pub fn journal_dir(gate: &Gate) -> PathBuf {
    gate.state_dir().join("journal")
}

/// How a journal left the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    /// Every announced action finished, or the transaction committed.
    Completed,
    /// The interrupted action did not happen: the machine still shows its `pre` state.
    Incomplete { action: String, summary: String },
    /// The machine matches neither state of the interrupted action.
    Ambiguous {
        action: String,
        summary: String,
        /// What did not match.
        detail: Ambiguity,
    },
}

/// What an ambiguous classification found, in the grammar the operator's message needs.
///
/// The two are not interchangeable. A `Subject` is a thing the operator can put into a state —
/// a path, or `package NAME` — and reads as the subject of a sentence. A `Condition` is a state
/// of the machine that names no single such thing: pacman's transaction lock, or a `dpkg --audit`
/// line. Rendering one where the other belongs is how the message asked an operator to "put the
/// package database is not clean ... into the state you want".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ambiguity {
    /// A path or a `package NAME` the operator can put into a state of their choosing.
    Subject(String),
    /// A condition of the machine that no single subject names.
    Condition(String),
}

impl Ambiguity {
    /// The text itself, which both forms carry after a colon in the plan's own line.
    pub fn text(&self) -> &str {
        match self {
            Self::Subject(text) | Self::Condition(text) => text,
        }
    }
}

impl std::fmt::Display for Ambiguity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.text())
    }
}

/// A journal read back from disk.
#[derive(Debug, Clone)]
pub struct Record {
    pub id: String,
    pub path: PathBuf,
    pub header: Header,
    pub events: Vec<Event>,
}

impl Record {
    /// Whether the transaction reached its commit record.
    pub fn committed(&self) -> bool {
        self.events
            .iter()
            .any(|event| matches!(event, Event::Finished { .. }))
    }

    /// The action that was announced and never ended, if any.
    pub fn interrupted(&self) -> Option<&Entry> {
        let mut open: Option<&str> = None;
        for event in &self.events {
            match event {
                Event::Begin { action, .. } => open = Some(action),
                Event::End { action, .. } | Event::Failed { action, .. } => {
                    if open == Some(action.as_str()) {
                        open = None;
                    }
                }
                Event::Finished { .. } => open = None,
            }
        }
        let id = open?;
        self.header.actions.iter().find(|entry| entry.id == id)
    }

    /// The failure a `failed` record names, if the transaction stopped on one.
    pub fn failure(&self) -> Option<(&str, &str)> {
        self.events.iter().rev().find_map(|event| match event {
            Event::Failed { action, error, .. } => Some((action.as_str(), error.as_str())),
            _ => None,
        })
    }

    /// Classify this journal against what the machine now shows, **packages included**.
    ///
    /// This is the entry point every command uses. [`Record::classify`] is the file half of it,
    /// kept as its own function because a file action's brackets are answered by reading the
    /// root, and a package action's are answered by asking the package manager.
    pub fn classify_in(&self, gate: &Gate) -> Classification {
        if self.committed() {
            return Classification::Completed;
        }
        let Some(entry) = self.interrupted() else {
            return Classification::Completed;
        };
        if entry.packages_pre.is_empty() && entry.packages_post.is_empty() {
            // A refresh of the index changes no package and leaves nothing to compare. It is
            // also the one mutation here that can simply be done again, so an interrupted one
            // is reported as what it is: it did not finish, and the next apply redoes it.
            // So is a service action: `systemctl enable --now`, `disable --now` and `preset`
            // leave the same unit however often they run, and the next apply plans it again, and
            // so is an OS basic (sd-1): a network change that was cut short is put back by its
            // own timer.
            if entry.kind == "packages.index"
                || entry.kind.starts_with("service.")
                || entry.kind.starts_with("basic.")
            {
                return Classification::Incomplete {
                    action: entry.id.clone(),
                    summary: entry.summary.clone(),
                };
            }
            return self.classify(&gate.root);
        }
        let ambiguous = |detail: Ambiguity| Classification::Ambiguous {
            action: entry.id.clone(),
            summary: entry.summary.clone(),
            detail,
        };
        let backend = super::pm::backend_for(
            gate.distro,
            &gate.root,
            gate.partial_upgrade,
            gate.operation,
        );
        let Ok(observed) = backend.observe() else {
            return ambiguous(Ambiguity::Condition(
                "the package database could not be read".to_string(),
            ));
        };
        // A package database that is not clean makes the answer ambiguous whatever the installed
        // set says: dpkg stopped part way through, and only a human may decide what that means.
        match backend.unclean() {
            Ok(lines) if !lines.is_empty() => {
                return ambiguous(Ambiguity::Condition(format!(
                    "the package database is not clean: {}",
                    lines[0]
                )));
            }
            Err(_) => {
                return ambiguous(Ambiguity::Condition(
                    "the package database could not be audited".to_string(),
                ));
            }
            Ok(_) => {}
        }
        if packages_match(&observed, &entry.packages_post) {
            return Classification::Completed;
        }
        if packages_match(&observed, &entry.packages_pre) {
            return Classification::Incomplete {
                action: entry.id.clone(),
                summary: entry.summary.clone(),
            };
        }
        let name = entry
            .packages_post
            .iter()
            .chain(entry.packages_pre.iter())
            .find(|(name, fact)| !package_matches(&observed, name, fact))
            .map_or_else(|| entry.summary.clone(), |(name, _)| name.clone());
        ambiguous(Ambiguity::Subject(format!("package {name}")))
    }

    /// Classify the file half of this journal against what the root now shows.
    pub fn classify(&self, root: &Path) -> Classification {
        if self.committed() {
            return Classification::Completed;
        }
        let Some(entry) = self.interrupted() else {
            return Classification::Completed;
        };
        if matches_all(root, &entry.post) {
            return Classification::Completed;
        }
        if matches_all(root, &entry.pre) {
            return Classification::Incomplete {
                action: entry.id.clone(),
                summary: entry.summary.clone(),
            };
        }
        // The path an operator wants named first is the one the action was for, so the search
        // starts at the state the action was to leave.
        let path = entry
            .post
            .iter()
            .chain(entry.pre.iter())
            .find(|facts| !matches_one(root, facts))
            .map_or_else(|| entry.summary.clone(), |facts| facts.path.clone());
        Classification::Ambiguous {
            action: entry.id.clone(),
            summary: entry.summary.clone(),
            detail: Ambiguity::Subject(path),
        }
    }
}

fn packages_match(
    observed: &super::pm::Observed,
    expected: &BTreeMap<String, PackageFact>,
) -> bool {
    expected
        .iter()
        .all(|(name, fact)| package_matches(observed, name, fact))
}

/// Compare one recorded package expectation against the machine. A `None` field is "this was
/// not recorded", exactly as it is for a file's [`Facts`].
fn package_matches(observed: &super::pm::Observed, name: &str, expected: &PackageFact) -> bool {
    let version = observed.installed.get(name);
    if version.is_some() != expected.installed {
        return false;
    }
    if let Some(wanted) = &expected.version
        && version != Some(wanted)
    {
        return false;
    }
    if let Some(wanted) = expected.auto
        && observed.auto.contains(name) != wanted
    {
        return false;
    }
    if let Some(wanted) = expected.held
        && observed.held.contains(name) != wanted
    {
        return false;
    }
    true
}

fn matches_all(root: &Path, expected: &[Facts]) -> bool {
    expected.iter().all(|facts| matches_one(root, facts))
}

/// Compare one recorded expectation against the machine. Only the fields the journal actually
/// recorded are compared: a `None` is "this was not recorded", never "this must be absent".
fn matches_one(root: &Path, expected: &Facts) -> bool {
    let observed = Facts::observe(root, &expected.path);
    if observed.exists != expected.exists {
        return false;
    }
    if !expected.exists {
        return true;
    }
    if observed.regular != expected.regular {
        return false;
    }
    if expected.digest.is_some() && observed.digest != expected.digest {
        return false;
    }
    if expected.mode.is_some() && observed.mode != expected.mode {
        return false;
    }
    if expected.uid.is_some() && observed.uid != expected.uid {
        return false;
    }
    if expected.gid.is_some() && observed.gid != expected.gid {
        return false;
    }
    true
}

/// Read one journal file. A trailing line that does not parse is the record whose write was
/// interrupted, and is discarded — that is the point of one JSON object per line.
pub fn read(path: &Path) -> Result<Record, Diagnostic> {
    let text = fs::read_to_string(path).map_err(|e| io_error(path, e))?;
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default();
    let unreadable = |why: String| {
        Diagnostic::new(
            "E_LOCK_VERSION",
            format!(
                "{} is not a journal this build can read: {why}",
                path.display()
            ),
        )
        .hint("a journal is written by `lodi host apply`; do not edit one by hand")
    };
    // The header's schema version is decided from the raw JSON before the header is
    // deserialized (M-1.0 T-1, design calls D3, D18).
    let raw: serde_json::Value =
        serde_json::from_str(first).map_err(|e| unreadable(e.to_string()))?;
    let artifact = crate::schema::artifact(ARTIFACT);
    match crate::schema::version_of(ARTIFACT, &raw) {
        Some(v) if artifact.reads_version(v) => {}
        found => {
            let carries = match found {
                Some(v) => format!("is journal version {v}"),
                None => "records no journal version".to_string(),
            };
            let mut d = Diagnostic::new(
                "E_LOCK_VERSION",
                format!(
                    "{} {carries}, and this build writes and reads version {}",
                    path.display(),
                    artifact.read_set()
                ),
            );
            d.notes.push(crate::schema::writer_note(ARTIFACT, &raw));
            return Err(d);
        }
    }
    let header: Header = serde_json::from_value(raw).map_err(|e| unreadable(e.to_string()))?;
    let mut events = Vec::new();
    for line in lines {
        match serde_json::from_str::<Event>(line) {
            Ok(event) => events.push(event),
            // Only a truncated tail is tolerated; a corrupt record in the middle would be
            // followed by ones that parse, and those are the ones a reader must not skip.
            Err(_) => break,
        }
    }
    Ok(Record {
        id: header.journal_id.clone(),
        path: path.to_path_buf(),
        header,
        events,
    })
}

/// Every journal in the root, oldest first (the id begins with a sortable UTC timestamp).
pub fn all(gate: &Gate) -> Result<Vec<PathBuf>, Diagnostic> {
    // Nothing is read from a state directory another principal could have written.
    let shown = format!("/{}/journal", super::safety::STATE);
    super::files::check_state_path(&gate.root, &shown)?;
    let dir = journal_dir(gate);
    // Only an absent journal directory is an empty one; a listing that fails for any other
    // reason is refused, never taken to mean that no journal is pending.
    let refuse = |error: std::io::Error| super::files::uninspectable(&shown, &dir, &error);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(refuse(error)),
    };
    let entries = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(refuse)?;
    // Ordered by when each was created rather than by name: two journals can share a second,
    // and "the newest" has to mean the newest.
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = entries
        .into_iter()
        .filter(|path| path.extension().is_some_and(|e| e == "json"))
        .map(|path| {
            let when = fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (when, path)
        })
        .collect();
    found.sort();
    Ok(found.into_iter().map(|(_, path)| path).collect())
}

/// The newest journal that did not commit, if there is one.
pub fn outstanding(gate: &Gate) -> Result<Option<Record>, Diagnostic> {
    // Only the newest is examined: an older uncommitted journal was superseded by a later
    // apply that ran to a commit, and nothing is outstanding about it.
    let Some(path) = all(gate)?.into_iter().next_back() else {
        return Ok(None);
    };
    let record = read(&path)?;
    Ok((!record.committed()).then_some(record))
}

/// The gate an apply passes before it is allowed to plan again: an outstanding journal is
/// classified, and an ambiguous one stops the command until a human declares it resolved.
///
/// Returns the classification, so that the caller can report what it found.
pub fn require_clear(
    gate: &Gate,
    resolved: Option<&str>,
) -> Result<Option<(Record, Classification)>, Diagnostic> {
    let Some(record) = outstanding(gate)? else {
        if let Some(id) = resolved {
            return Err(Diagnostic::new(
                "E_JOURNAL_AMBIGUOUS",
                format!("--resolved {id} names a journal, and no journal is outstanding"),
            )
            .hint("drop `--resolved`: there is nothing waiting on a human"));
        }
        return Ok(None);
    };
    let classification = record.classify_in(gate);
    // The package manager's own repair command, named for the operator and never run by Lodi:
    // only a human can decide what a half-configured dpkg was in the middle of.
    let repair = match record.interrupted() {
        Some(entry) if !entry.packages_pre.is_empty() || !entry.packages_post.is_empty() => {
            format!("; {}", super::pm::Shape::of(gate.distro).repair)
        }
        _ => String::new(),
    };
    if let Classification::Ambiguous {
        action,
        summary,
        detail,
    } = &classification
    {
        // The two shapes need two sentences. A subject is what the ambiguity is *about* and
        // reads as one; a condition is already a statement about the machine and cannot be
        // bent into the place a subject holds, either in the message or in the hint.
        let (subject, found) = match detail {
            Ambiguity::Subject(subject) => (
                subject.clone(),
                format!("{subject} matches neither the state before it nor the state after it"),
            ),
            Ambiguity::Condition(condition) => ("the machine".to_string(), condition.clone()),
        };
        match resolved {
            Some(id) if id == record.id => {}
            Some(id) => {
                return Err(Diagnostic::new(
                    "E_JOURNAL_AMBIGUOUS",
                    format!(
                        "--resolved {id} does not name the journal that is waiting, which is {}",
                        record.id
                    ),
                )
                .hint(format!(
                    "read {} and, once {subject} is what you want it to be, run the apply again \
                     with --resolved {}",
                    record.path.display(),
                    record.id
                )));
            }
            None => {
                return Err(Diagnostic::new(
                    "E_JOURNAL_AMBIGUOUS",
                    format!("the last apply stopped at action {action} ({summary}) and {found}"),
                )
                .hint(format!(
                    "lodi does not guess and cannot repair this for you; read {}, put {subject} \
                     into the state you want, then run the apply again with --resolved {}{repair}",
                    record.path.display(),
                    record.id
                )));
            }
        }
    } else if let Some(id) = resolved {
        return Err(Diagnostic::new(
            "E_JOURNAL_AMBIGUOUS",
            format!("--resolved {id} was given and no journal is waiting on a human"),
        )
        .hint("drop `--resolved` and run the apply again"));
    }
    Ok(Some((record, classification)))
}

fn io_error(path: &Path, error: std::io::Error) -> Diagnostic {
    Diagnostic::new("E_STORE_IO", format!("{}: {error}", path.display()))
}

/// The journal's own view of what an apply did, for the record and for a test.
pub fn outcomes(record: &Record) -> BTreeMap<String, &'static str> {
    let mut out: BTreeMap<String, &'static str> = record
        .header
        .actions
        .iter()
        .map(|entry| (entry.id.clone(), "announced"))
        .collect();
    for event in &record.events {
        if let Some(action) = event.action() {
            let state = match event {
                Event::Begin { .. } => "begun",
                Event::End { .. } => "done",
                Event::Failed { .. } => "failed",
                Event::Finished { .. } => continue,
            };
            out.insert(action.to_string(), state);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Entry {
        Entry {
            id: "f1".into(),
            kind: "file.replace".into(),
            summary: "~ file /etc/x (content)".into(),
            pre: vec![Facts {
                path: "/etc/x".into(),
                exists: true,
                regular: true,
                digest: Some("sha256:old".into()),
                mode: None,
                uid: None,
                gid: None,
            }],
            post: vec![Facts {
                path: "/etc/x".into(),
                exists: true,
                regular: true,
                digest: Some("sha256:new".into()),
                mode: None,
                uid: None,
                gid: None,
            }],
            packages_pre: BTreeMap::new(),
            packages_post: BTreeMap::new(),
            commands: Vec::new(),
            programs: Vec::new(),
        }
    }

    fn record(events: Vec<Event>) -> Record {
        Record {
            id: "j".into(),
            path: PathBuf::from("/nowhere/j.json"),
            header: Header {
                record: "header".into(),
                version: VERSION,
                journal_id: "j".into(),
                started_at: "2026-09-21T00:00:00Z".into(),
                lodi_version: "0".into(),
                root: "/nowhere".into(),
                manifest_digest: "sha256:m".into(),
                distro: "debian".into(),
                programs: Vec::new(),
                euid: 1000,
                resolved: None,
                actions: vec![entry()],
            },
            events,
        }
    }

    #[test]
    fn a_committed_journal_is_complete_whatever_the_machine_shows() {
        let record = record(vec![
            Event::Begin {
                action: "f1".into(),
                at: "t".into(),
            },
            Event::End {
                action: "f1".into(),
                at: "t".into(),
            },
            Event::Finished { at: "t".into() },
        ]);
        assert_eq!(
            record.classify(Path::new("/nonexistent-root")),
            Classification::Completed
        );
    }

    #[test]
    fn an_action_that_began_and_never_ended_is_the_interrupted_one() {
        let record = record(vec![Event::Begin {
            action: "f1".into(),
            at: "t".into(),
        }]);
        assert_eq!(
            record.interrupted().map(|e| e.id.clone()),
            Some("f1".into())
        );
        // Neither bracket can hold when the path does not exist at all, so this is the stop.
        match record.classify(Path::new("/nonexistent-root")) {
            Classification::Ambiguous { action, detail, .. } => {
                assert_eq!(action, "f1");
                assert_eq!(detail, Ambiguity::Subject("/etc/x".to_string()));
            }
            other => panic!("expected an ambiguous classification, got {other:?}"),
        }
    }

    #[test]
    fn a_half_written_tail_is_discarded_and_the_rest_is_read() {
        let dir = std::env::temp_dir().join(format!("lodi-journal-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("j.json");
        let header = serde_json::to_string(&record(Vec::new()).header).unwrap();
        let begin = serde_json::to_string(&Event::Begin {
            action: "f1".into(),
            at: "t".into(),
        })
        .unwrap();
        fs::write(&path, format!("{header}\n{begin}\n{{\"record\":\"en")).unwrap();
        let read_back = read(&path).unwrap();
        assert_eq!(read_back.events.len(), 1);
        assert_eq!(
            read_back.interrupted().map(|e| e.id.clone()),
            Some("f1".into())
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
