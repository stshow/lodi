//! Importing again into a host's own file (#708, LD-506, LD-519): a three-way merge
//! ([`merge::merge`]) and an in-place editor ([`edit::apply`]), which keeps the person's
//! comments, order and edits. The base is the host record's baseline of the last sync of this
//! folder ([`Last`], read as root before this import records the host again, LD-515): a name
//! the person deleted stays deleted, one installed by hand is added, one removed by hand leaves,
//! and a declared file changed on the machine is named and never captured again (si-1). With no
//! such record (none yet, or one of another folder), the base is what the file and the machine
//! agree on, and each place where the two disagree is one `W_RECONCILE` line keeping the file's
//! side (LD-506).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::diag::Diagnostic;
use crate::hostscope::import::files::{Capture, Captured};
use crate::hostscope::lock::Baseline;
use crate::hostscope::manifest::{self, FileState};
use crate::hostscope::plan::{resolve_group, resolve_user};
use crate::hostscope::reconcile::edit::{self, Edits, FileKeys};
use crate::hostscope::reconcile::merge::{self, Meta, OurFile, Ours, Theirs, W_RECONCILE};

/// The machine's side of a merge, as the elevated capture reads it and returns it as JSON.
#[derive(Debug, Clone, Default)]
pub struct Machine {
    /// Explicitly installed from the distribution's own repositories: what an import declares.
    pub chosen: BTreeSet<String>,
    /// Every explicitly installed name.
    pub explicit: BTreeSet<String>,
    /// Explicitly installed from elsewhere, with the words that say from where.
    pub elsewhere: BTreeMap<String, String>,
    /// The held names with their versions, on a family whose hold is machine state.
    pub holds: Option<BTreeMap<String, String>>,
    pub arch: String,
    pub distro: String,
    pub release: String,
    pub codename: String,
}

impl Machine {
    pub fn to_json(&self) -> Value {
        json!({
            "chosen": self.chosen, "explicit": self.explicit, "elsewhere": self.elsewhere,
            "holds": self.holds, "arch": self.arch, "distro": self.distro,
            "release": self.release, "codename": self.codename,
        })
    }

    pub fn from_json(value: &Value) -> Option<Machine> {
        let text = |key: &str| value.get(key)?.as_str().map(str::to_string);
        let names = |key: &str| -> Option<BTreeSet<String>> {
            value
                .get(key)?
                .as_array()?
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect()
        };
        let map = |value: &Value| -> Option<BTreeMap<String, String>> {
            value
                .as_object()?
                .iter()
                .map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        };
        let holds = match value.get("holds")? {
            Value::Null => None,
            held => Some(map(held)?),
        };
        Some(Machine {
            chosen: names("chosen")?,
            explicit: names("explicit")?,
            elsewhere: map(value.get("elsewhere")?)?,
            holds,
            arch: text("arch")?,
            distro: text("distro")?,
            release: text("release")?,
            codename: text("codename")?,
        })
    }

    fn context(&self) -> manifest::Context {
        manifest::Context::new(&self.arch, &self.distro, &self.release, &self.codename)
    }
}

/// A path of interest as the machine has it now. `digest` is `None` for a path that is not a
/// regular file, or one this process cannot read.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Seen {
    pub digest: Option<String>,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Seen {
    /// `path` under `root` now: `None` when it is not there, or not safe to read (a symlink on
    /// the way, say); `Some(None)` when it is missing.
    pub fn observe(root: &Path, path: &str) -> Option<Option<Seen>> {
        crate::hostscope::files::inspect(root, path, true).ok()?;
        let facts = crate::hostscope::plan::Facts::observe(root, path);
        if !facts.exists {
            return Some(None);
        }
        Some(Some(Seen {
            digest: facts.digest,
            mode: facts.mode?,
            uid: facts.uid?,
            gid: facts.gid?,
        }))
    }
}

/// What the host record said before this import records the host again (LD-515): the merge's
/// base, read as root and handed back as JSON ([`Last::to_json`]); never written to disk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Last {
    /// The folder of the host file its last sync read (`lock.source`).
    pub source: Option<String>,
    /// The baseline of that sync, from a record of schema 2 or later.
    pub base: Option<Baseline>,
    /// Each recorded file's mode, owner and group: the base of a file's metadata.
    pub files: BTreeMap<String, (u32, String, String)>,
    /// Each recorded file as the machine has it now, `None` when it is missing.
    pub seen: BTreeMap<String, Option<Seen>>,
}

impl Last {
    pub fn to_json(&self) -> Value {
        let seen: BTreeMap<&String, Value> = self
            .seen
            .iter()
            .map(|(at, seen)| {
                let value = seen.as_ref().map_or(Value::Null, |seen| {
                    json!({
                        "digest": seen.digest, "mode": seen.mode, "uid": seen.uid, "gid": seen.gid,
                    })
                });
                (at, value)
            })
            .collect();
        json!({
            "source": self.source,
            "base": self.base.as_ref().and_then(|base| serde_json::to_value(base).ok()),
            "files": self.files, "seen": seen,
        })
    }

    /// Read the record at `gate`'s root, and each file it records as the machine has it now. An
    /// unreadable record is none: the merge falls back on what the file and the machine agree on.
    pub fn read(gate: &crate::hostscope::safety::Gate) -> Last {
        let Ok(Some(lock)) = crate::hostscope::lock::HostLock::read(&gate.lock_path()) else {
            return Last::default();
        };
        let mut last = Last {
            source: lock.source.clone(),
            base: (lock.version >= 2).then(|| lock.baseline.clone()).flatten(),
            ..Last::default()
        };
        for (path, record) in &lock.files {
            if let Ok(mode) = u32::from_str_radix(&record.mode, 8) {
                last.files.insert(
                    path.clone(),
                    (mode, record.owner.clone(), record.group.clone()),
                );
            }
            if let Some(seen) = Seen::observe(&gate.root, path) {
                last.seen.insert(path.clone(), seen);
            }
        }
        last
    }
}

/// What importing again changes in the file.
pub struct Merged {
    pub text: String,
    /// The entries added and removed, as `package NAME`, `hold NAME` or `file PATH`.
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// How many of the person's own edits the machine disagrees with and were kept, and how
    /// many conflicts kept the file's side.
    pub kept: usize,
    pub conflicts: usize,
    /// One `W_RECONCILE` line per conflict, or per change that is named and not taken.
    pub warnings: Vec<String>,
    /// The captured files declared for the first time, to write beside the file.
    pub declare: Vec<Captured>,
}

/// Merge the machine (`machine`, `capture`, and `fresh`, the text an import writes for it now)
/// into `text`, the host file at `path`, whose sources resolve beside it. `root` resolves owners
/// and is where a declared file the record does not name is looked at; `last` is the record
/// before this import, and `source` the folder as a record names it.
#[allow(clippy::too_many_arguments)]
pub fn merge(
    text: &str,
    path: &Path,
    root: &Path,
    machine: &Machine,
    capture: &Capture,
    fresh: &str,
    last: &Last,
    source: &str,
) -> Result<Merged, Diagnostic> {
    let shown = path.display().to_string();
    let ctx = machine.context();
    let parsed = manifest::parse(text, &shown, &ctx).map_err(first_error(&shown))?;
    if let Some(declared) = parsed.host.distro.as_deref()
        && declared != machine.distro
    {
        return Err(Diagnostic::new(
            "E_HOST_MISMATCH",
            format!(
                "{shown} declares distro {declared:?}, and this host is {:?}",
                machine.distro
            ),
        )
        .hint("import this host into a config of its own; nothing was written"));
    }
    let dir = path.parent().unwrap_or(Path::new("/"));
    let theirs = theirs(&parsed, root, machine, capture, last);
    let ours = ours(&parsed, dir, root, machine, &theirs);
    let recorded = last
        .base
        .as_ref()
        .filter(|_| last.source.as_deref() == Some(source));
    let (outcome, mut warnings) = match recorded {
        Some(base) => from_record(base, last, root, &ours, &theirs),
        None => from_agreement(&ours, &theirs, &shown),
    };
    warnings.extend(not_installed(&ours, &theirs, &outcome, &shown));
    let declare: Vec<Captured> = capture
        .captured
        .iter()
        .filter(|file| outcome.declare.contains(&file.path))
        .cloned()
        .collect();
    let new = edit::apply(text, &edits(&outcome, machine, &declare, fresh))?;
    manifest::parse(&new, &shown, &ctx).map_err(first_error(&shown))?;
    Ok(merged(new, &outcome, warnings, declare))
}

/// The first of a host file's parse errors.
fn first_error(shown: &str) -> impl Fn(crate::manifest::ManifestErrors) -> Diagnostic + '_ {
    move |errors| {
        errors.diagnostics.into_iter().next().unwrap_or_else(|| {
            Diagnostic::new("E_SYNTAX", format!("{shown} cannot be read as a host"))
        })
    }
}

/// A file's mode, owner and group, the owner and group resolved under `root`.
fn meta(root: &Path, mode: u32, owner: &str, group: &str) -> Meta {
    Meta {
        mode,
        uid: resolve_user(root, owner),
        gid: resolve_group(root, group),
    }
}

/// The machine's side: what it installed and holds, and each declared or captured file as it
/// has it. A declared file is read as root read it when the record names it, else as this
/// process can read it; one it cannot read is not compared.
fn theirs(
    parsed: &manifest::HostManifest,
    root: &Path,
    machine: &Machine,
    capture: &Capture,
    last: &Last,
) -> Theirs {
    let mut files: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut metas: BTreeMap<String, Meta> = BTreeMap::new();
    for at in parsed.files.keys() {
        let seen = match last.seen.get(at) {
            Some(seen) => seen.clone(),
            None => match Seen::observe(root, at) {
                Some(Some(Seen { digest: None, .. })) | None => continue,
                Some(seen) => seen,
            },
        };
        let Some(Seen {
            digest: Some(digest),
            mode,
            uid,
            gid,
        }) = seen
        else {
            files.insert(at.clone(), None);
            continue;
        };
        let meta = Meta {
            mode,
            uid: Some(uid),
            gid: Some(gid),
        };
        metas.insert(at.clone(), meta);
        files.insert(at.clone(), Some(digest));
    }
    for file in &capture.captured {
        let digest = crate::util::sha256_tagged(&file.bytes);
        files.insert(file.path.clone(), Some(digest));
        let meta = meta(root, file.mode, &file.owner, &file.group);
        metas.insert(file.path.clone(), meta);
    }
    Theirs {
        chosen: machine.chosen.clone(),
        explicit: machine.explicit.clone(),
        elsewhere: machine.elsewhere.clone(),
        holds: machine.holds.clone(),
        files,
        meta: metas,
        captured: capture.captured.iter().map(|f| f.path.clone()).collect(),
    }
}

/// The file's side: the names it declares, keeps off and lists, its holds, and each present file
/// the machine has too, with what its declaration digests to.
fn ours(
    parsed: &manifest::HostManifest,
    dir: &Path,
    root: &Path,
    machine: &Machine,
    theirs: &Theirs,
) -> Ours {
    let packages = &parsed.packages;
    let mut kept_off: BTreeSet<String> = packages.absent.iter().cloned().collect();
    let mut listed = BTreeSet::new();
    for list in [
        &packages.common,
        &packages.hold,
        &packages.mark_auto,
        &packages.optional,
    ] {
        listed.extend(list.iter().cloned());
    }
    let tables = packages
        .per_distro
        .values()
        .chain(packages.per_arch.values());
    for table in tables {
        listed.extend(table.add.iter().cloned());
    }
    let here = [
        packages.per_distro.get(&machine.distro),
        packages.per_arch.get(&machine.arch),
    ];
    for table in here.into_iter().flatten() {
        kept_off.extend(table.remove.iter().cloned());
    }
    let mut files = BTreeMap::new();
    for (at, entry) in &parsed.files {
        if entry.state != FileState::Present || !theirs.files.contains_key(at) {
            continue;
        }
        let (digest, inline) = match (&entry.content, &entry.source) {
            (Some(content), _) => (Some(crate::util::sha256_tagged(content.as_bytes())), true),
            (None, Some(source)) => (
                crate::hostscope::plan::digest_of(&manifest::source_path(dir, source)).ok(),
                false,
            ),
            (None, None) => (None, false),
        };
        let meta = meta(root, entry.mode, &entry.owner, &entry.group);
        let file = OurFile {
            digest,
            inline,
            meta,
        };
        files.insert(at.clone(), file);
    }
    Ours {
        declared: packages
            .effective(&machine.distro, &machine.arch)
            .into_iter()
            .collect(),
        kept_off,
        listed,
        hold: packages.hold.iter().cloned().collect(),
        mark_auto: packages.mark_auto.iter().cloned().collect(),
        files,
        file_paths: parsed.files.keys().cloned().collect(),
    }
}

/// The three-way merge on the record of this folder's last sync, and its lines. A file changed
/// only on the machine, bytes or mode, owner and group, is named and never copied again (si-1);
/// its declaration is kept.
fn from_record(
    base: &Baseline,
    last: &Last,
    root: &Path,
    ours: &Ours,
    theirs: &Theirs,
) -> (merge::Outcome, Vec<String>) {
    let metadata: BTreeMap<String, Meta> = last
        .files
        .iter()
        .map(|(at, (mode, owner, group))| (at.clone(), meta(root, *mode, owner, group)))
        .collect();
    let outcome = merge::merge(base, &metadata, ours, theirs);
    let mut warnings: Vec<String> = outcome.conflicts.clone();
    warnings.extend(outcome.notes.iter().cloned());
    let again: BTreeSet<&String> = outcome.recapture.iter().chain(&outcome.metadata).collect();
    for at in again {
        warnings.push(format!(
            "{W_RECONCILE}: {at} changed on this machine and is not captured again: {}; \
             the declaration is kept",
            crate::hostscope::import::files::Reason::Changed.why()
        ));
    }
    (outcome, warnings)
}

/// The merge with no record of this folder's last sync: the base is what the file and the
/// machine agree on (LD-506), and a file they disagree on keeps the file's side.
fn from_agreement(ours: &Ours, theirs: &Theirs, shown: &str) -> (merge::Outcome, Vec<String>) {
    let base = Baseline {
        explicit: ours
            .declared
            .intersection(&theirs.explicit)
            .cloned()
            .collect(),
        holds: theirs
            .holds
            .iter()
            .flatten()
            .filter(|(name, _)| ours.hold.contains(*name))
            .map(|(name, version)| (name.clone(), version.clone()))
            .collect(),
        files: ours
            .files
            .iter()
            .filter_map(|(at, file)| {
                let now = theirs.files.get(at).cloned().flatten()?;
                (file.digest.as_ref() == Some(&now)).then(|| (at.clone(), now))
            })
            .collect(),
    };
    let outcome = merge::merge(&base, &BTreeMap::new(), ours, theirs);
    let mut warnings: Vec<String> = outcome.conflicts.clone();
    warnings.extend(outcome.notes.iter().cloned());
    for (at, file) in &ours.files {
        let there = theirs.files.get(at).is_some_and(Option::is_some);
        if there && file.digest.is_some() && !base.files.contains_key(at) {
            warnings.push(format!(
                "{W_RECONCILE}: {at} differs between {shown} and this host; the file is kept"
            ));
        }
    }
    (outcome, warnings)
}

/// A declared name the machine lacks and the merge keeps: the next switch installs it.
fn not_installed(
    ours: &Ours,
    theirs: &Theirs,
    outcome: &merge::Outcome,
    shown: &str,
) -> Vec<String> {
    ours.declared
        .difference(&theirs.explicit)
        .filter(|name| !ours.mark_auto.contains(*name) && !outcome.remove.contains(*name))
        .map(|name| {
            format!(
                "{W_RECONCILE}: package {name} is in {shown} and not installed on this host; \
                 the file is kept, and the next lodi switch installs it"
            )
        })
        .collect()
}

/// The edits the merge makes to the file, in place.
fn edits(outcome: &merge::Outcome, machine: &Machine, declare: &[Captured], fresh: &str) -> Edits {
    Edits {
        adopt: outcome.adopt.clone(),
        adopt_into: (machine.distro == "fedora").then_some("fedora"),
        remove: outcome.remove.clone(),
        hold_add: outcome.hold_add.clone(),
        hold_remove: outcome.hold_remove.clone(),
        recapture: Vec::new(),
        declare: declare
            .iter()
            .map(|file| FileKeys {
                path: file.path.clone(),
                source: file.source.clone(),
                mode: file.mode,
                owner: file.owner.clone(),
                group: file.group.clone(),
            })
            .collect(),
        not_captured: edit::block_start(fresh).map(|at| fresh[at..].to_string()),
    }
}

/// What importing again changed in the file, named entry by entry.
fn merged(
    text: String,
    outcome: &merge::Outcome,
    warnings: Vec<String>,
    declare: Vec<Captured>,
) -> Merged {
    let mut added: Vec<String> = outcome
        .adopt
        .iter()
        .map(|n| format!("package {n}"))
        .collect();
    added.extend(outcome.hold_add.iter().map(|n| format!("hold {n}")));
    added.extend(declare.iter().map(|file| format!("file {}", file.path)));
    let mut removed: Vec<String> = outcome
        .remove
        .iter()
        .map(|n| format!("package {n}"))
        .collect();
    removed.extend(outcome.hold_remove.iter().map(|n| format!("hold {n}")));
    Merged {
        text,
        added,
        removed,
        kept: outcome.kept,
        conflicts: outcome.conflicts.len(),
        warnings,
        declare,
    }
}
