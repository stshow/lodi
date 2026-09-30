//! `lodi host import` over an existing manifest **reconciles** the machine into it (LD-378, W2).
//!
//! It is a three-way merge. The **base** is the baseline `host.lock` schema 2 records at the last
//! sync — an import, a writing reconcile or an apply; **ours** is the manifest; **theirs** is the
//! machine now, read exactly as an import reads it. [`merge::merge`] decides, [`edit::apply`]
//! makes the decision in the person's own file, and [`diff::unified`] shows it. Names and
//! digests are compared, never TOML text.
//!
//! - A change made outside Lodi is adopted: a package installed from the distribution is added
//!   to `common`, one removed leaves every list. A configuration file changed on the machine is
//!   named by `W_RECONCILE` and never copied (si-1): the capture below finds no bytes to take.
//! - The person's own edits are kept: a deleted line stays deleted, an added one stays.
//! - A conflict keeps the manifest's side and is named by `W_RECONCILE`; the exit status is 0.
//! - **No record** — no lock, a version-1 lock, or a lock whose last sync read another directory
//!   — is a two-way report and nothing written: missing history is never guessed.
//! - `--dry-run` prints the diff and the changed captured paths with their digests (never file
//!   contents) and writes nothing, the lock included. Otherwise the same is printed and written,
//!   and a reconcile that writes on `/` needs root, because the base lives in the root-owned lock.
//! - A reconcile that changes nothing writes nothing: the manifest keeps its bytes and its mtime.
//!
//! The baseline is advanced by the import, by a writing reconcile and by every apply
//! ([`packages_baseline`], [`files_baseline`]); without that, a name the person deleted after a
//! reconcile adopted it would be adopted again by the next one.

pub mod diff;
pub mod edit;
pub mod merge;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use crate::diag::Diagnostic;

use super::HostError;
use super::import::{self, Machine, baseline::Selection, files::Capture};
use super::landing;
use super::lock::{Baseline, FileRecord, HostLock};
use super::manifest::{self, FileState, HostManifest};
use super::plan::{Content, Desired, Facts};
use super::pm::{self, Observed};
use super::safety::{Distro, Gate, Options};

use merge::{Meta, OurFile, Ours, Theirs, W_RECONCILE};

/// The package half of the baseline a sync leaves: of the names the last baseline had and the
/// names the manifest declares, those the machine now has explicitly installed; and of the holds
/// the last baseline had and the manifest declares, those the machine now holds, at the version
/// it has. A family whose hold is not machine state records no hold.
pub fn packages_baseline(
    previous: Option<&Baseline>,
    declared: &[String],
    observed: &Observed,
    hold: &[String],
    hold_is_state: bool,
) -> (BTreeSet<String>, BTreeMap<String, String>) {
    let explicit =
        |name: &String| observed.installed.contains_key(name) && !observed.auto.contains(name);
    let mut names: BTreeSet<String> = previous
        .map(|b| b.explicit.iter().cloned().collect())
        .unwrap_or_default();
    names.extend(declared.iter().cloned());
    names.retain(explicit);
    let mut holds = BTreeMap::new();
    if hold_is_state {
        let mut held: BTreeSet<&String> = previous
            .map(|b| b.holds.keys().collect())
            .unwrap_or_default();
        held.extend(hold);
        for name in held {
            if observed.held.contains(name)
                && let Some(version) = observed.installed.get(name)
            {
                holds.insert(name.clone(), version.clone());
            }
        }
    }
    (names, holds)
}

/// The file half: each declared file as the record has it, and each path the last baseline had
/// that is no longer declared, carried as it was.
pub fn files_baseline(
    previous: Option<&Baseline>,
    records: &BTreeMap<String, FileRecord>,
) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = previous.map(|b| b.files.clone()).unwrap_or_default();
    for (path, record) in records {
        out.insert(path.clone(), record.digest.clone());
    }
    out
}

/// Where a reconcile reads and writes.
pub struct Target<'a> {
    /// The directory holding `host.toml` and `files/`.
    pub dir: &'a Path,
    /// The `source` a baseline for this manifest records: `/etc/lodi` for the manifest in place,
    /// the host directory for one a positional `SOURCE` chose (LD-379).
    pub source: &'a str,
    /// Whether this is the manifest in place.
    pub in_place: bool,
    /// Whether this reconcile writes the record: in place, and into a host directory as root.
    pub writes_record: bool,
    /// The host directory a positional `SOURCE` chose, whose manifest is read by its rules.
    pub host: Option<&'a super::source::Host>,
    /// The lock as it stands, read before the machine was.
    pub previous: Option<HostLock>,
    /// Become the owner of a host directory: called after the record is written and before the
    /// first byte under the directory (LD-379).
    pub become_writer: &'a dyn Fn(&mut Gate) -> Result<(), HostError>,
}

/// Reconcile the machine the gate opened into the manifest at `target`.
pub fn run(gate: &mut Gate, options: &Options, target: Target) -> Result<String, HostError> {
    let manifest_path = target.dir.join(landing::MANIFEST);
    let shown = manifest_path.display().to_string();
    let text = read_manifest(gate, &target, &manifest_path)?;
    let ctx = manifest::Context::from_gate(gate);
    let parsed = manifest::parse(&text, &shown, &ctx)?;
    gate.os
        .assert_distro(parsed.host.distro.as_deref(), &shown)?;

    let machine = import::read(gate)?;
    let capture = import::files::capture(gate)?;
    let selection = import::baseline::select(&machine);
    let ours = ours(gate, &parsed, target.dir, target.host);
    let theirs = theirs(gate, &machine, &selection, &capture, &parsed);

    let mut report = String::new();
    for warning in selection.warnings(machine.distro) {
        let _ = writeln!(report, "{warning}");
    }
    for warning in capture.warnings() {
        let _ = writeln!(report, "{warning}");
    }
    if let Some(line) = snapshot_note(gate, options, &parsed, target.in_place) {
        let _ = writeln!(report, "{line}");
    }

    let base = target
        .previous
        .as_ref()
        .and_then(|lock| lock.base_for(target.source));
    let Some(base) = base else {
        two_way(&mut report, gate, &target, &ours, &theirs);
        return Ok(report);
    };

    let recorded = recorded_meta(gate, target.previous.as_ref());
    let outcome = merge::merge(base, &recorded, &ours, &theirs);
    let mut notes = outcome.notes.clone();
    // A file changed only on the machine, or whose mode, owner or group changed, is not copied
    // again: the import copies no file from /etc (si-1). It is reported and keeps its
    // declaration.
    let mut again: Vec<&String> = outcome.recapture.iter().collect();
    again.extend(
        outcome
            .metadata
            .iter()
            .filter(|path| !outcome.recapture.contains(path)),
    );
    again.sort();
    let recaptured: Vec<import::files::Captured> = Vec::new();
    for path in again {
        notes.push(format!(
            "{W_RECONCILE}: {path} changed on this machine and is not captured again: {}; \
             the declaration is kept",
            import::files::Reason::Changed.why()
        ));
    }
    let declared: Vec<import::files::Captured> = capture
        .captured
        .iter()
        .filter(|file| outcome.declare.contains(&file.path))
        .cloned()
        .collect();

    let mut edits = edit::Edits {
        adopt: outcome.adopt.clone(),
        // Fedora's names are declared under its own table, as the import writes them (LD-432).
        adopt_into: (machine.distro == Distro::Fedora).then_some("fedora"),
        remove: outcome.remove.clone(),
        hold_add: outcome.hold_add.clone(),
        hold_remove: outcome.hold_remove.clone(),
        recapture: recaptured
            .iter()
            .map(|file| {
                let entry = parsed.files.get(&file.path);
                let mut keys = edit::FileKeys {
                    path: entry.map_or_else(|| file.path.clone(), |entry| entry.declared.clone()),
                    source: file.source.clone(),
                    mode: file.mode,
                    owner: file.owner.clone(),
                    group: file.group.clone(),
                };
                // The manifest's own metadata stays unless the machine's is taken.
                if !outcome.metadata.contains(&file.path)
                    && let Some(entry) = entry
                {
                    keys.mode = entry.mode;
                    keys.owner.clone_from(&entry.owner);
                    keys.group.clone_from(&entry.group);
                }
                keys
            })
            .collect(),
        declare: declared
            .iter()
            .map(|file| edit::FileKeys {
                path: file.path.clone(),
                source: file.source.clone(),
                mode: file.mode,
                owner: file.owner.clone(),
                group: file.group.clone(),
            })
            .collect(),
        not_captured: None,
    };
    // The names the manifest declares after the merge: what the block no longer names.
    let mut after: BTreeSet<String> = ours.declared.clone();
    after.extend(outcome.adopt.iter().cloned());
    for name in &outcome.remove {
        after.remove(name);
    }
    edits.not_captured = Some(block(&machine, &selection, &capture, &after));
    let new_text = edit::apply(&text, &edits).map_err(HostError::from)?;
    let reparsed = manifest::parse(&new_text, &shown, &ctx)?;

    for line in outcome.conflicts.iter().chain(&notes) {
        let _ = writeln!(report, "{line}");
    }
    report.push_str(&diff::unified(&shown, &text, &new_text));
    for (file, keys) in recaptured.iter().zip(&edits.recapture) {
        let _ = writeln!(
            report,
            "captured again: {} {} (mode {:04o}, {}:{})",
            file.path,
            digest(&file.bytes),
            keys.mode,
            keys.owner,
            keys.group
        );
    }
    for file in &declared {
        let _ = writeln!(
            report,
            "captured: {} {} (mode {:04o}, {}:{})",
            file.path,
            digest(&file.bytes),
            file.mode,
            file.owner,
            file.group
        );
    }
    let counts = format!(
        "{} added, {} removed, {} kept, {} conflict(s)",
        outcome.added(),
        outcome.removed(),
        outcome.kept,
        outcome.conflicts.len()
    );
    if options.dry_run {
        let _ = writeln!(
            report,
            "would reconcile {shown}: {counts}; nothing was written (--dry-run)"
        );
        return Ok(report);
    }

    // In place, the bytes first, then the manifest that names them, then the record: at every
    // instant the manifest on disk finds every `source` it declares (LD-325). Into a host
    // directory, the record comes first, as root, and then the owner of the directory writes
    // every byte under it (LD-379).
    let mut record_changed = false;
    let mut next = target.writes_record.then(|| {
        let previous = target.previous.as_ref().expect("a base came from a record");
        let (record, adopted) = next_record(
            gate,
            previous,
            base,
            target.source,
            &reparsed,
            &machine,
            &theirs,
            &recaptured,
            &declared,
        );
        let changed = !same_record(previous, &record);
        (record, adopted, changed)
    });
    if !target.in_place {
        if let Some((record, adopted, true)) = &mut next {
            // A file captured for the first time is adopted: its original is kept in the store,
            // as root, with the record and before the privilege drop (LD-387).
            for line in super::originals::adopt(&gate.root, &mut record.files, adopted) {
                let _ = writeln!(report, "{line}");
            }
            super::apply::materialise_lock(gate, record)?;
            record_changed = true;
        }
        (target.become_writer)(gate)?;
    }
    // Only bytes captured again travel; a file taken again for its metadata alone keeps the
    // copy it has, which holds the same bytes, or its inline `content`.
    let bundle = Capture {
        captured: recaptured
            .iter()
            .filter(|file| outcome.recapture.contains(&file.path))
            .chain(&declared)
            .cloned()
            .collect(),
        ..Capture::default()
    };
    bundle.write_bundle(target.dir)?;
    let manifest_changed = new_text != text;
    if manifest_changed {
        super::files::materialise(
            target.dir,
            &format!("/{}", landing::MANIFEST),
            &Desired {
                digest: String::new(),
                mode: 0o644,
                uid: gate.euid,
                gid: gate.egid,
                owner: gate.euid.to_string(),
                group: gate.egid.to_string(),
                content: Content::Inline(new_text.clone().into_bytes()),
            },
        )?;
    }
    if target.in_place
        && let Some((record, adopted, true)) = &mut next
    {
        for line in super::originals::adopt(&gate.root, &mut record.files, adopted) {
            let _ = writeln!(report, "{line}");
        }
        super::apply::materialise_lock(gate, record)?;
        record_changed = true;
    }
    let wrote = match (manifest_changed, record_changed) {
        (false, false) => "nothing changed, nothing written".to_string(),
        (true, true) => "wrote it and the record".to_string(),
        (true, false) => "wrote it".to_string(),
        (false, true) => "the manifest is unchanged; the record was updated".to_string(),
    };
    let wrote = if target.host.is_some() && !target.writes_record {
        format!(
            "{wrote}; wrote no record: the record is root's, and the next `sudo lodi host apply` \
             of this host writes it"
        )
    } else {
        wrote
    };
    let next = if gate.system_root {
        "sudo lodi host plan".to_string()
    } else {
        format!("lodi host plan --root {}", gate.root.display())
    };
    let _ = writeln!(
        report,
        "reconciled {shown}: {counts}; {wrote}; next: {next}"
    );
    Ok(report)
}

/// A re-import keeps `[host] snapshot`, every pin and `pins.lock` as they are (LD-397). When the
/// snapshot is older than the instant a pin would take now, one line says so and names the
/// command that moves it; the file is not changed for it.
fn snapshot_note(
    gate: &Gate,
    options: &Options,
    parsed: &HostManifest,
    in_place: bool,
) -> Option<String> {
    let written = parsed.host.snapshot.as_deref()?;
    let now =
        super::pin::verbs::rewrite::default_snapshot(gate.distro.name(), crate::util::now_utc())
            .ok()?;
    (crate::util::parse_utc(written)? < crate::util::parse_utc(&now)?).then(|| {
        format!(
            "[host] snapshot stays {written}, older than this reading of the machine; to pin \
             the file to now ({now}): {}lodi host pin --all{}",
            super::pin::verbs::sudo_for(gate.system_root, in_place),
            super::pin::verbs::selection(options)
        )
    })
}

/// The manifest as it stands: in place it is judged as a plan judges it, owner and mode
/// included; under `--out` it is a file of the operator's.
fn read_manifest(gate: &Gate, target: &Target, path: &Path) -> Result<String, HostError> {
    let bytes = if let Some(host) = target.host {
        super::source::read_file(host, landing::MANIFEST, crate::manifest::MAX_MANIFEST_BYTES)?
            .unwrap_or_default()
    } else if target.in_place {
        super::safety::read_config(
            &gate.root,
            super::safety::MANIFEST,
            gate.system_root,
            gate.euid,
            crate::manifest::MAX_MANIFEST_BYTES,
        )?
        .unwrap_or_default()
    } else {
        fs::read(path).map_err(|e| {
            Diagnostic::new("E_STORE_IO", format!("cannot read {}: {e}", path.display()))
        })?
    };
    if bytes.len() > crate::manifest::MAX_MANIFEST_BYTES {
        return Err(Diagnostic::new(
            "E_SYNTAX",
            format!(
                "{} is over {} bytes",
                path.display(),
                crate::manifest::MAX_MANIFEST_BYTES
            ),
        )
        .into());
    }
    String::from_utf8(bytes).map_err(|_| {
        Diagnostic::new("E_SYNTAX", format!("{} is not valid UTF-8", path.display())).into()
    })
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", crate::util::sha256_hex(bytes))
}

/// A mode, owner and group as the merge compares them: the owner and group resolved against the
/// root's own `passwd` and `group`, so that a name and its id are the same owner.
fn meta_of(gate: &Gate, mode: u32, owner: &str, group: &str) -> Meta {
    Meta {
        mode,
        uid: super::plan::resolve_user(&gate.root, owner),
        gid: super::plan::resolve_group(&gate.root, group),
    }
}

/// The metadata half of the base: the mode, owner and group the lock records for each file at the
/// last sync. It lives in the record's `files`, as it always has; the baseline holds digests only,
/// and schema 2 is unchanged (validator-repair-1).
fn recorded_meta(gate: &Gate, previous: Option<&HostLock>) -> BTreeMap<String, Meta> {
    previous
        .into_iter()
        .flat_map(|lock| &lock.files)
        .filter_map(|(path, record)| {
            let mode = u32::from_str_radix(&record.mode, 8).ok()?;
            Some((
                path.clone(),
                meta_of(gate, mode, &record.owner, &record.group),
            ))
        })
        .collect()
}

/// The manifest, in the terms the merge compares. A host directory's sources are read by its
/// walk, as a plan reads them.
fn ours(
    gate: &Gate,
    parsed: &HostManifest,
    dir: &Path,
    host: Option<&super::source::Host>,
) -> Ours {
    let packages = &parsed.packages;
    let distro = gate.distro.name();
    let arch = gate.arch();
    let mut kept_off: BTreeSet<String> = packages.absent.iter().cloned().collect();
    let mut listed: BTreeSet<String> = BTreeSet::new();
    for list in [
        &packages.common,
        &packages.hold,
        &packages.mark_auto,
        &packages.optional,
    ] {
        listed.extend(list.iter().cloned());
    }
    for table in packages
        .per_distro
        .values()
        .chain(packages.per_arch.values())
    {
        listed.extend(table.add.iter().cloned());
    }
    for table in [packages.per_distro.get(distro), packages.per_arch.get(arch)]
        .into_iter()
        .flatten()
    {
        kept_off.extend(table.remove.iter().cloned());
    }
    let mut files = BTreeMap::new();
    for (path, entry) in &parsed.files {
        if entry.state != FileState::Present {
            continue;
        }
        let (digest, inline) = match (&entry.content, &entry.source) {
            (Some(content), _) => (Some(digest(content.as_bytes())), true),
            (None, Some(source)) => {
                let digest = match host {
                    Some(host) => super::source::read_file(host, source, super::MAX_SOURCE_BYTES)
                        .ok()
                        .flatten()
                        .map(|bytes| digest(&bytes)),
                    None => super::plan::digest_of(&manifest::source_path(dir, source)).ok(),
                };
                (digest, false)
            }
            (None, None) => (None, false),
        };
        files.insert(
            path.clone(),
            OurFile {
                digest,
                inline,
                meta: meta_of(gate, entry.mode, &entry.owner, &entry.group),
            },
        );
    }
    Ours {
        declared: packages.effective(distro, arch).into_iter().collect(),
        kept_off,
        listed,
        hold: packages.hold.iter().cloned().collect(),
        mark_auto: packages.mark_auto.iter().cloned().collect(),
        files,
        file_paths: parsed.files.keys().cloned().collect(),
    }
}

/// The machine, in the terms the merge compares.
fn theirs(
    gate: &Gate,
    machine: &Machine,
    selection: &Selection,
    capture: &Capture,
    parsed: &HostManifest,
) -> Theirs {
    let observed = &machine.observed;
    let explicit: BTreeSet<String> = import::baseline::chosen(observed).into_iter().collect();
    let mut elsewhere = BTreeMap::new();
    let distro = machine.distro.name();
    for (name, label) in &selection.third_party {
        elsewhere.insert(
            name.clone(),
            format!("from a repository that is not {distro}'s own ({label})"),
        );
    }
    for name in &selection.local_only {
        elsewhere.insert(
            name.clone(),
            "from a package file, or from a source this machine no longer has".to_string(),
        );
    }
    for name in &selection.foreign {
        elsewhere.insert(
            name.clone(),
            "from no repository this package manager knows".to_string(),
        );
    }
    let holds = pm::Shape::of(gate.distro).hold_is_machine_state.then(|| {
        selection
            .hold
            .iter()
            .filter_map(|name| {
                observed
                    .installed
                    .get(name)
                    .map(|version| (name.clone(), version.clone()))
            })
            .collect()
    });
    let mut files = BTreeMap::new();
    let mut meta = BTreeMap::new();
    for path in parsed
        .files
        .keys()
        .chain(capture.captured.iter().map(|file| &file.path))
    {
        let facts = super::files::inspect(&gate.root, path, true)
            .ok()
            .map(|_| Facts::observe(&gate.root, path));
        if let Some(Facts {
            digest: Some(_),
            mode: Some(mode),
            uid,
            gid,
            ..
        }) = &facts
        {
            meta.insert(
                path.clone(),
                Meta {
                    mode: *mode,
                    uid: *uid,
                    gid: *gid,
                },
            );
        }
        files.insert(path.clone(), facts.and_then(|facts| facts.digest));
    }
    Theirs {
        chosen: selection.common.iter().cloned().collect(),
        explicit,
        elsewhere,
        holds,
        files,
        meta,
        captured: capture
            .captured
            .iter()
            .map(|file| file.path.clone())
            .collect(),
    }
}

/// The `# NOT CAPTURED` block for this machine, less the names the manifest now declares.
fn block(
    machine: &Machine,
    selection: &Selection,
    capture: &Capture,
    declared: &BTreeSet<String>,
) -> String {
    let mut left_out = selection.clone();
    left_out
        .third_party
        .retain(|(name, _)| !declared.contains(name));
    left_out.local_only.retain(|name| !declared.contains(name));
    left_out.foreign.retain(|name| !declared.contains(name));
    import::emit::not_captured_block(machine, &left_out, Some(capture))
}

/// No record of the last sync: what differs, both ways, and nothing written.
fn two_way(report: &mut String, gate: &Gate, target: &Target, ours: &Ours, theirs: &Theirs) {
    for name in theirs.chosen.difference(&ours.declared) {
        let _ = writeln!(
            report,
            "{W_RECONCILE}: package {name} is on this machine and not in host.toml"
        );
    }
    for name in ours.declared.difference(&theirs.explicit) {
        let _ = writeln!(
            report,
            "{W_RECONCILE}: package {name} is in host.toml and not explicitly installed on this \
             machine"
        );
    }
    for (path, file) in &ours.files {
        let now = theirs.files.get(path).cloned().flatten();
        if now.is_none() {
            let _ = writeln!(
                report,
                "{W_RECONCILE}: {path} is declared and missing from this machine"
            );
        } else if file.digest != now {
            let _ = writeln!(
                report,
                "{W_RECONCILE}: {path} differs between host.toml and this machine"
            );
        }
    }
    for path in theirs.captured.difference(&ours.file_paths) {
        let _ = writeln!(
            report,
            "{W_RECONCILE}: {path} changed on this machine and is not declared"
        );
    }
    let why = match &target.previous {
        None => "there is no host.lock".to_string(),
        Some(lock) if lock.version < 2 => format!(
            "host.lock is {} version {}, which records no baseline",
            lock.format, lock.version
        ),
        Some(_) => "host.lock records no sync of this manifest".to_string(),
    };
    let apply = if gate.system_root {
        "sudo lodi host apply".to_string()
    } else {
        format!("lodi host apply --root {}", gate.root.display())
    };
    let _ = writeln!(
        report,
        "not reconciled: {why}, so which side changed cannot be told; nothing was written"
    );
    let _ = writeln!(
        report,
        "hint: run `{apply}` (it writes the record) and import again, or run the import with \
         --force to replace {}",
        target.dir.join(landing::MANIFEST).display()
    );
}

/// The record a writing reconcile leaves: the new baseline, the package records a converged
/// apply of the new manifest would write, and the file records with each file captured now
/// adopted where it stands (LD-377).
///
/// Returned with it: the files it records for the first time whose declaration keeps a copy and
/// restores it, which the caller adopts into the store with the record (LD-387).
#[allow(clippy::too_many_arguments)]
fn next_record(
    gate: &Gate,
    previous: &HostLock,
    base: &Baseline,
    source: &str,
    manifest: &HostManifest,
    machine: &Machine,
    theirs: &Theirs,
    recaptured: &[import::files::Captured],
    declared: &[import::files::Captured],
) -> (HostLock, Vec<super::originals::Planned>) {
    let mut record = HostLock::new(
        crate::util::format_utc(crate::util::now_utc()),
        gate.distro.name().to_string(),
        gate.os.version_id.clone(),
    );
    let observed = &machine.observed;
    let shape = pm::Shape::of(gate.distro);
    let wanted = manifest.packages.effective(gate.distro.name(), gate.arch());
    // What the record named and the machine still has stays named: it is the fence an exact
    // apply removes by, and a line the person deleted is still a package Lodi knows (the owner's
    // `bat`), not one installed by hand.
    let mut names: BTreeSet<String> = wanted.iter().cloned().collect();
    names.extend(previous.packages.keys().cloned());
    let ignore: Vec<String> = if shape.hold_is_machine_state {
        Vec::new()
    } else {
        wanted
            .iter()
            .filter(|n| manifest.packages.hold.contains(n) && observed.installed.contains_key(*n))
            .cloned()
            .collect()
    };
    record.packages = super::plan::records(&names, observed, &manifest.packages.hold, &ignore);
    record.files = previous.files.clone();
    record.set_services(previous.services.clone());
    // The `[system]` basics the reconciled manifest declares and the machine has are recorded
    // as a converged apply records them (#585); what the record held before stays known.
    record.set_basics(super::basics::record_as_read(
        manifest.system.each(),
        &machine.system,
        &previous.basics,
    ));
    // The accounts and groups the reconciled manifest declares are the ones the record manages,
    // as a fresh import records them (su-1, #561).
    record.set_identities(
        manifest.users.keys().cloned().collect(),
        manifest.groups.keys().cloned().collect(),
    );
    let mut adopted = Vec::new();
    for file in recaptured.iter().chain(declared) {
        // A file captured for the first time is an adoption (LD-387); one the record already
        // names keeps its `backup` and `onRemove`, and only its facts move.
        let keeps_a_copy = manifest.files.get(&file.path).is_some_and(|entry| {
            entry.backup && entry.on_remove == super::manifest::OnRemove::Restore
        });
        if !record.files.contains_key(&file.path)
            && keeps_a_copy
            && let Some(planned) = super::originals::Planned::captured(&gate.root, file)
        {
            adopted.push(planned);
        }
        let entry = record
            .files
            .entry(file.path.clone())
            .or_insert_with(|| FileRecord {
                digest: String::new(),
                mode: String::new(),
                owner: String::new(),
                group: String::new(),
                backup: None,
                on_remove: "keep".to_string(),
            });
        entry.digest = digest(&file.bytes);
        entry.mode = format!("{:04o}", file.mode);
        entry.owner.clone_from(&file.owner);
        entry.group.clone_from(&file.group);
    }
    // The base moves to the machine as it is now: everything it has explicitly from the
    // distribution, and of the names the last base had or the manifest declares, those it has.
    let (declared_names, holds) = packages_baseline(
        Some(base),
        &wanted,
        observed,
        &manifest.packages.hold,
        shape.hold_is_machine_state,
    );
    let mut explicit = theirs.chosen.clone();
    explicit.extend(declared_names);
    let mut files = base.files.clone();
    for (path, digest) in &theirs.files {
        if let Some(digest) = digest {
            files.insert(path.clone(), digest.clone());
        }
    }
    record.baseline = Some(Baseline {
        explicit,
        holds: theirs.holds.clone().unwrap_or(holds),
        files,
    });
    record.source = Some(source.to_string());
    (record, adopted)
}

/// Whether two records say the same thing, whenever and by whichever lodi they were written.
fn same_record(a: &HostLock, b: &HostLock) -> bool {
    let strip = |lock: &HostLock| {
        let mut lock = lock.clone();
        lock.applied_at = String::new();
        lock.generated_by = String::new();
        lock
    };
    strip(a) == strip(b)
}
