//! `lodi home apply` and `lodi home status` (M-0.5 T-3, design calls D5, D6, D7, D13 —
//! `LD-101`, `LD-102`, `LD-103`, `LD-173`, `LD-174`, `LD-175`; repaired by M-0.5
//! `validator-repair-1`, `LD-185`, `LD-186`).
//!
//! `apply` is the only verb of this scope that changes a file, and it is built so that the
//! dangerous case — a file the user edited by hand — is refused **before anything is touched**:
//!
//! 0. **The pre-flight, before anything exists.** Load the manifest
//!    ([`crate::home::manifest`]) and the state ([`crate::home::state`]), compute exactly the
//!    plan [`crate::home::plan`] prints, and raise every refusal from it: a path that leaves the
//!    root, a path this user cannot write, and above all **drift** — a managed path whose bytes
//!    are not the ones Lodi wrote is `W_DRIFT` on standard error and `E_DRIFT` at exit 8. This
//!    runs before the scope directory and the lock are created, so an apply that refuses leaves
//!    the roots byte-identical: not a file, not a directory, not a lock (design call D6).
//!    `--overwrite-drift` is the confirmation: the drifted bytes are copied into the backup store
//!    under their own name first, so confirming is not destructive either.
//! 1. Take `<data>/home-scope/.lock` exclusively, waiting, through
//!    [`crate::home::fsops::lock`] — the data root plus a relative path, like every other
//!    mutation of this scope, with the store's one `flock` implementation underneath — so two
//!    applies serialize rather than race. `plan` and `status` take it **shared**, and only when
//!    it already exists, because a report creates nothing (`LD-185`).
//! 2. Read again with the lock held — another apply may have finished in between — and check the
//!    same refusals against that plan, which is the one that is executed.
//! 3. Apply in lexicographic path order: create the missing parent directories at 0755 and
//!    record them (they are **never** removed again, design call D7), back up a pre-existing
//!    unmanaged file once when the entry asks for it, write through [`crate::home::fsops`], set
//!    the mode.
//! 4. Paths in the state the manifest no longer declares are handled from the `on_remove`
//!    recorded with them: `restore` puts the backup back, `delete` removes the file, `keep`
//!    leaves it and drops the row only.
//! 5. Write the state atomically, **last**, so a state file that exists describes finished work
//!    — and **only when it would differ from the state on disk**: an apply whose plan is empty
//!    and whose state is already current rewrites nothing at all, so a re-apply changes no
//!    file's mtime anywhere under the roots (`LD-186`).
//!
//! `status` is the report of the same three inputs and **exits 0 whatever it finds** — a warning
//! never changes an exit status (`spec/11` §1); `apply` is the gate (design call D6).
//!
//! A mode that differs from the declaration is simply re-set: resetting a mode destroys nothing,
//! so it is not drift and needs no confirmation (design call D6). No directory is ever removed,
//! no owner or group is ever changed, and no shell configuration file is ever touched: the only
//! paths written are the ones `home.toml` declares.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::path::Path;

use crate::diag::{Diagnostic, exit_status};
use crate::fetch::Fetcher;
use crate::home::backup::{self, BackupIndex, Kind};
use crate::home::fsops::{self, RelPath, Root};
use crate::home::manifest::{FileEntry, HomeManifest, load_home_manifest_with};
use crate::home::plan::{Action, Plan, Step, Verb, look, manifest_warnings, plan_all};
use crate::home::profile;
use crate::home::programs::{self, Module};
use crate::home::services;
use crate::home::state::{self, HomeState, ManagedFile, mode_text};
use crate::home::tools;
use crate::manifest::ManifestErrors;
use crate::roots::Roots;
use crate::util::sha256_hex;

/// The mode an apply gives a parent directory it had to create (design call D7).
pub const DIRECTORY_MODE: u32 = 0o755;

/// `lodi home apply`'s own options.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// Take a drifted path back, keeping the edited bytes in the backup store first.
    pub overwrite_drift: bool,
    /// Refuse a missing or stale `home.lock` instead of resolving it.
    pub locked: bool,
    /// How `[services]` reaches the user manager (hc-1).
    pub services: crate::home::services::Context,
}

/// What a command printed: its lines on standard output, its `W_…` lines on standard error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub lines: Vec<String>,
    pub warnings: Vec<String>,
    /// The home asks root to turn its lingering off (`Context::linger_by_root`).
    pub linger_off: bool,
}

/// Why a command stopped. The warnings are carried with it because the `W_DRIFT` lines are how
/// the user learns *which* paths refused the apply.
#[derive(Debug, Clone)]
pub struct Failure {
    pub warnings: Vec<String>,
    pub text: String,
    pub status: u8,
    pub code: &'static str,
}

impl Failure {
    fn of(warnings: Vec<String>, d: Diagnostic) -> Failure {
        Failure {
            warnings,
            text: d.to_string(),
            status: exit_status(d.code),
            code: d.code,
        }
    }

    fn manifest(errors: ManifestErrors) -> Failure {
        let code = errors
            .diagnostics
            .first()
            .map(|d| d.code)
            .unwrap_or("E_NO_MANIFEST");
        Failure {
            warnings: Vec::new(),
            text: errors.to_string(),
            status: ManifestErrors::EXIT_STATUS,
            code,
        }
    }
}

impl From<Diagnostic> for Failure {
    fn from(d: Diagnostic) -> Failure {
        Failure::of(Vec::new(), d)
    }
}

impl From<ManifestErrors> for Failure {
    fn from(errors: ManifestErrors) -> Failure {
        Failure::manifest(errors)
    }
}

fn tool_failure(warnings: Vec<String>, failure: crate::lock::Failure) -> Failure {
    let code = failure
        .diagnostics
        .first()
        .map(|d| d.code)
        .unwrap_or("E_STORE_IO");
    Failure {
        warnings,
        text: failure.to_string(),
        status: failure.exit_status,
        code,
    }
}

/// The three inputs every verb of this scope reads, with the lock held while they are read.
struct Loaded {
    /// The manifest's `[tools]`, resolved after the pre-flight.
    tools: crate::tools::ToolSet,
    state: HomeState,
    index: BackupIndex,
    plan: Plan,
    /// `W_DEPRECATED` for `[files]`, `W_PROGRAM_NOT_FOUND` and `W_SHADOWED`, printed first
    /// ([`manifest_warnings`]).
    manifest_warnings: Vec<String>,
    /// Whether `[programs.fish]` renders: the tool profile then gets its fish twin.
    fish: bool,
    /// The shell files whose line apply prints last (`HomeManifest::hooks`).
    hooks: Vec<(String, std::path::PathBuf)>,
}

fn load(
    roots: &Roots,
    options: &Options,
    registry: &[Module],
    preloaded: Option<&HomeManifest>,
) -> Result<Loaded, Failure> {
    let overwrite_drift = options.overwrite_drift;
    let data = roots.data_root();
    let manifest = match preloaded {
        Some(manifest) => manifest.clone(),
        None => load_home_manifest_with(roots, registry)?,
    };
    let state = state::read(&data)?;
    let index = backup::read(&data)?;
    let mut plan = plan_all(
        &manifest,
        &state,
        &index,
        &roots.home_root(),
        overwrite_drift,
    )?;
    plan.services = services::plan(&manifest, &state, &plan.unit_files(), &options.services)?;
    let manifest_warnings = manifest_warnings(roots, &manifest);
    Ok(Loaded {
        fish: manifest.uses_program("fish"),
        hooks: manifest.hooks(),
        tools: manifest.tools,
        state,
        index,
        plan,
        manifest_warnings,
    })
}

/// `<data>/home-scope`, created only when it is not there, so an apply that has nothing to do
/// writes exactly one thing: the state file.
fn ensure_scope_dir(data: &Root) -> Result<(), Diagnostic> {
    if data.path().join(state::HOME_SCOPE_DIR).is_dir() {
        return Ok(());
    }
    let rel = RelPath::new(state::HOME_SCOPE_DIR).expect("the scope directory is a relative path");
    fsops::mkdir_private(data, &rel)
}

/// `<data>/home-scope/.lock` as the type wall states it: a relative path that cannot leave the
/// data root, which is the only thing `fsops` will lock.
fn lock_path() -> RelPath {
    RelPath::new(state::LOCK_FILE).expect("the lock is a relative path inside the data root")
}

/// The apply lock, taken exclusively and waited for, with one line on standard error when
/// somebody else holds it.
fn take_lock(data: &Root) -> Result<fsops::Lock, Diagnostic> {
    let rel = lock_path();
    match fsops::try_lock(data, &rel, true)? {
        Some(lock) => Ok(lock),
        None => {
            // Printed now rather than collected, because the point of the line is to be seen
            // while the wait is happening.
            eprintln!("waiting for the home lock");
            fsops::lock(data, &rel, true)
        }
    }
}

/// The shared lock a report takes, **only** when the lock file is already there: `plan` and
/// `status` create nothing, not even this.
fn share_lock(data: &Root) -> Result<Option<fsops::Lock>, Diagnostic> {
    fsops::lock_if_present(data, &lock_path(), false)
}

/// The `W_DRIFT` line of every drifted step, in path order: how the user learns *which* paths
/// refused the apply.
fn drift_warnings(plan: &Plan) -> Vec<String> {
    plan.drifted()
        .iter()
        .map(|step| {
            step.drift
                .as_ref()
                .expect("a drifted step carries its drift")
                .warning(&step.path)
        })
        .collect()
}

/// The gate of design call D6: a managed file edited by hand stops the whole apply, and the
/// paths are named so that the user can look at them before confirming.
fn refuse_drift(plan: &Plan, warnings: &[String]) -> Result<(), Failure> {
    let paths: Vec<String> = plan.drifted().iter().map(|s| s.path.clone()).collect();
    if paths.is_empty() {
        return Ok(());
    }
    let mut d = Diagnostic::new(
        "E_DRIFT",
        format!(
            "{} managed file{} changed since lodi wrote {}; nothing was applied",
            paths.len(),
            if paths.len() == 1 { " has" } else { "s have" },
            if paths.len() == 1 { "it" } else { "them" }
        ),
    );
    for path in &paths {
        d.notes.push(path.clone());
    }
    let d =
        d.hint("run lodi home status, then lodi home apply --overwrite-drift to take them back");
    Err(Failure::of(warnings.to_vec(), d))
}

/// Everything an apply refuses to start on: a manifest that does not parse, a path that leaves
/// the home root, a file edited by hand, a path this user cannot write. Read-only, so that a
/// refused apply has changed nothing whatever.
fn preflight(
    roots: &Roots,
    options: &Options,
    registry: &[Module],
    preloaded: Option<&HomeManifest>,
) -> Result<(Loaded, Vec<String>), Failure> {
    let loaded = load(roots, options, registry, preloaded)?;
    let mut warnings = loaded.manifest_warnings.clone();
    warnings.extend(drift_warnings(&loaded.plan));
    if !options.overwrite_drift {
        refuse_drift(&loaded.plan, &warnings)?;
    }
    refuse_unwritable(&roots.home_root(), &loaded.plan)
        .map_err(|d| Failure::of(warnings.clone(), d))?;
    Ok((loaded, warnings))
}

/// `lodi home apply [--overwrite-drift] [--locked]`.
pub fn apply(
    roots: &Roots,
    options: &Options,
    fetcher: &dyn Fetcher,
    now: i64,
) -> Result<Outcome, Failure> {
    apply_with(roots, options, fetcher, now, programs::registry())
}

/// [`apply`] with an explicit program-module registry (`manifest::load_home_manifest_with`).
pub fn apply_with(
    roots: &Roots,
    options: &Options,
    fetcher: &dyn Fetcher,
    now: i64,
    registry: &[Module],
) -> Result<Outcome, Failure> {
    apply_loaded(roots, options, fetcher, now, registry, None)
}

pub fn apply_preloaded(
    roots: &Roots,
    manifest: &HomeManifest,
    lock: Option<crate::lock::LockFile>,
    source: &str,
    options: &Options,
    fetcher: &dyn Fetcher,
    now: i64,
) -> Result<Outcome, Failure> {
    apply_preloaded_to(roots, manifest, lock, None, source, options, fetcher, now)
}

/// [`apply_preloaded`] whose tool lock is the home's section of its repository's root lock,
/// `target`, when there is one (LD-416).
#[allow(clippy::too_many_arguments)]
pub fn apply_preloaded_to(
    roots: &Roots,
    manifest: &HomeManifest,
    lock: Option<crate::lock::LockFile>,
    target: Option<&crate::flakelock::HomeTarget>,
    source: &str,
    options: &Options,
    fetcher: &dyn Fetcher,
    now: i64,
) -> Result<Outcome, Failure> {
    apply_loaded(
        roots,
        options,
        fetcher,
        now,
        programs::registry(),
        Some(Preloaded {
            manifest,
            lock,
            target,
            source,
        }),
    )
}

struct Preloaded<'a> {
    manifest: &'a HomeManifest,
    lock: Option<crate::lock::LockFile>,
    target: Option<&'a crate::flakelock::HomeTarget>,
    source: &'a str,
}

fn resolve_tools(
    roots: &Roots,
    tools: crate::tools::ToolSet,
    fetcher: &dyn Fetcher,
    now: i64,
    locked: bool,
    preloaded: Option<&Preloaded<'_>>,
    notes: &mut Vec<String>,
) -> Result<Option<tools::Resolution>, crate::lock::Failure> {
    match preloaded {
        Some(preloaded) => tools::resolve_set_with_lock_to(
            roots,
            tools,
            fetcher,
            now,
            locked,
            preloaded.lock.clone(),
            preloaded.target,
            notes,
        ),
        None => tools::resolve_set(roots, tools, fetcher, now, locked),
    }
}

fn apply_loaded(
    roots: &Roots,
    options: &Options,
    fetcher: &dyn Fetcher,
    now: i64,
    registry: &[Module],
    preloaded: Option<Preloaded<'_>>,
) -> Result<Outcome, Failure> {
    let data = roots.data_root();
    let home = roots.home_root();

    // 1 and 2. The read-only pre-flight, run **before the scope directory and the lock exist**:
    // an apply that is going to refuse creates nothing at all, not even its own lock file, so
    // `E_DRIFT` and `E_PATH_ESCAPE` leave the roots byte-identical (design call D6).
    let (first, _) = preflight(
        roots,
        options,
        registry,
        preloaded.as_ref().map(|p| p.manifest),
    )?;
    // A frozen apply also proves its tool lock is present and fresh before creating the scope
    // directory or its coordination lock. In frozen mode resolution is read-only.
    if options.locked {
        resolve_tools(
            roots,
            first.tools,
            fetcher,
            now,
            true,
            preloaded.as_ref(),
            &mut Vec::new(),
        )
        .map_err(|failure| tool_failure(Vec::new(), failure))?;
    }

    ensure_scope_dir(&data)?;
    let _lock = take_lock(&data)?;

    // Read again with the lock held: another apply may have finished between the pre-flight and
    // the lock, and what is executed must be the plan of what is on disk *now*. The refusals are
    // therefore checked a second time, on this plan.
    let (loaded, mut warnings) = preflight(
        roots,
        options,
        registry,
        preloaded.as_ref().map(|p| p.manifest),
    )?;
    let Loaded {
        tools: tool_set,
        mut state,
        mut index,
        plan,
        manifest_warnings: _,
        fish,
        hooks,
    } = loaded;
    // What the state file holds right now, so that step 5 can tell "already current" from
    // "changed by this run" and leave the file alone in the first case.
    let state_before = state.clone();
    // The state names the source that last applied the home, also when that apply had nothing
    // else to change: a host that moved to another source moves its home's fence with it (#226).
    if let Some(source) = preloaded.as_ref().map(|p| p.source)
        && (!plan.is_empty() || state.source() != source)
    {
        state.source = Some(source.to_string());
    }

    // T-4's realization path is the command path here. Resolution happens after both read-only
    // pre-flights and under the home apply lock, then realization roots the complete store set.
    // The profile is built only after the ordinary files have been applied successfully.
    let mut lock_notes = Vec::new();
    let resolution = resolve_tools(
        roots,
        tool_set,
        fetcher,
        now,
        options.locked,
        preloaded.as_ref(),
        &mut lock_notes,
    )
    .map_err(|failure| tool_failure(warnings.clone(), failure))?;
    if let Some(resolution) = &resolution {
        // The host environment's own bin farm has project-scope last-wins semantics. It is an
        // implementation detail here: the home profile below applies D11's first-wins rule and
        // is the only one put on the user's PATH, so those host warnings are not printed.
        tools::realize(roots, fetcher, resolution)
            .map_err(|failure| tool_failure(warnings.clone(), failure))?;
    }

    // 3 and 4. The work, in the lexicographic order the plan is already in. A path the state
    // already names holds bytes Lodi wrote, so it is never copied into the backup store: the one
    // copy kept there is the user's own (design call D5).
    // A seeded record is not Lodi's bytes any more (§3.2), so a seeded path counts as unmanaged:
    // what is there is the user's and is backed up before it is replaced.
    let managed_before: BTreeSet<String> = state
        .files
        .iter()
        .filter(|(_, record)| !record.is_seed())
        .map(|(path, _)| path.clone())
        .collect();
    let mut directories: BTreeSet<String> = state.directories.iter().cloned().collect();
    let result = (|| -> Result<(), Diagnostic> {
        services::apply_before(&plan.services)?;
        // Recorded as soon as the restores are done: a declared unit is Lodi's to restore
        // from here on, also when a later step fails.
        state.services = plan.services.records.clone();
        state.linger = plan.services.linger_managed;
        for step in &plan.steps {
            match &step.action {
                Action::Nothing => {}
                Action::Write(entry) => {
                    let kept = back_up(
                        &data,
                        &home,
                        &mut index,
                        step,
                        entry,
                        managed_before.contains(entry.path.as_str()),
                        &mut warnings,
                    )?;
                    make_parents(&home, &entry.path, &mut directories)?;
                    fsops::write(&home, &entry.path, &entry.bytes, entry.mode)?;
                    let backup = kept.or_else(|| {
                        index
                            .original_for(entry.path.as_str())
                            .map(|(name, _)| name.clone())
                    });
                    state.files.insert(
                        entry.path.as_str().to_string(),
                        ManagedFile::of(entry, backup),
                    );
                }
                Action::Adopt(entry) => {
                    // Nothing is replaced, so nothing is announced: the bytes are kept once as
                    // the original only so that `on_remove = "restore"` has them (§4.3).
                    let kept = adopt(&data, &home, &mut index, entry)?;
                    let backup = kept.or_else(|| {
                        index
                            .original_for(entry.path.as_str())
                            .map(|(name, _)| name.clone())
                    });
                    state.files.insert(
                        entry.path.as_str().to_string(),
                        ManagedFile::of(entry, backup),
                    );
                }
                Action::Record(entry) => {
                    let backup = state
                        .files
                        .get(entry.path.as_str())
                        .and_then(|record| record.backup.clone());
                    state.files.insert(
                        entry.path.as_str().to_string(),
                        ManagedFile::of(entry, backup),
                    );
                }
                Action::Absent(entry) => {
                    back_up(
                        &data,
                        &home,
                        &mut index,
                        step,
                        entry,
                        managed_before.contains(entry.path.as_str()),
                        &mut warnings,
                    )?;
                    fsops::remove(&home, &entry.path)?;
                    state.files.remove(entry.path.as_str());
                }
                Action::Restore {
                    path,
                    backup: name,
                    mode,
                } => {
                    let bytes = backup::bytes_of(&data, name)?;
                    make_parents(&home, path, &mut directories)?;
                    fsops::write(&home, path, &bytes, *mode)?;
                    state.files.remove(path.as_str());
                }
                Action::Delete { path, no_backup } => {
                    if *no_backup {
                        warnings.push(format!(
                            "lodi: warning W_MISSING_REF: {path} has no backup to restore; it is \
                             removed instead"
                        ));
                    }
                    fsops::remove(&home, path)?;
                    state.files.remove(path.as_str());
                }
                Action::Forget { path } => {
                    state.files.remove(path.as_str());
                }
            }
        }
        services::apply_after(&plan.services, &options.services)
    })();

    let mut profile_notes = Vec::new();
    let profile_result = if result.is_ok() {
        match &resolution {
            Some(resolution) => profile::rebuild(roots, resolution, fish).map(|built| {
                profile_notes = built.notes;
                warnings.extend(built.warnings);
            }),
            None => (|| {
                profile_notes = profile::clear(roots)?;
                let root = RelPath::new(tools::HOME_GC_ROOT)
                    .expect("the persistent home root is inside the data root");
                fsops::remove(&data, &root)
            })(),
        }
    } else {
        Ok(())
    };

    // 5. The state is written last, and it is written **even when a step failed**, so what did
    // happen before the failure is recorded rather than lost (`E_STORE_IO` names the path) —
    // but an apply with an empty plan whose state is already exactly what is on disk writes
    // nothing whatever, not even the same bytes over themselves (`LD-186`).
    state.directories = directories.into_iter().collect();
    let written = if plan.is_empty() && state == state_before {
        Ok(())
    } else {
        state::write(&data, &state)
    };
    result.map_err(|d| Failure::of(warnings.clone(), d))?;
    profile_result.map_err(|d| Failure::of(warnings.clone(), d))?;
    written.map_err(|d| Failure::of(warnings.clone(), d))?;

    let mut lines = if plan.is_empty() {
        vec!["nothing to do".to_string()]
    } else {
        let mut lines = applied_lines(&plan);
        lines.extend(plan.services.lines());
        lines
    };
    lines.extend(profile_notes);
    lines.extend(lock_notes);
    if resolution.is_some() {
        let notice = profile::notice(roots, fish);
        warnings.extend(notice.warnings);
        lines.extend(notice.lines);
    }
    lines.extend(profile::hook_lines(roots, &hooks));
    Ok(Outcome {
        lines,
        warnings,
        linger_off: plan.services.linger_off(),
    })
}

/// Keep the one copy of a file Lodi did not write, or the copy `--overwrite-drift` takes back.
/// Returns the backup's name when one was made.
fn back_up(
    data: &Root,
    home: &Root,
    index: &mut BackupIndex,
    step: &Step,
    entry: &FileEntry,
    managed: bool,
    warnings: &mut Vec<String>,
) -> Result<Option<String>, Diagnostic> {
    let Some((bytes, mode)) = look(home, &entry.path)? else {
        return Ok(None);
    };
    let path = entry.path.as_str();
    if step.drift.is_some() {
        // A confirmed take-back: the edited bytes are kept under their own name, and the
        // original this path already has is never replaced (design calls D5, D6).
        let name = backup::keep(data, index, path, &bytes, mode, Kind::Drift)?;
        warnings.push(format!(
            "lodi: warning W_DRIFT: {path} was taken back; the edited bytes are kept as {name}"
        ));
        return Ok(None);
    }
    if managed || !entry.backup || index.original_for(path).is_some() {
        return Ok(None);
    }
    let name = backup::keep(data, index, path, &bytes, mode, Kind::Original)?;
    warnings.push(format!(
        "lodi: warning W_REPLACED_UNMANAGED: {path} was not written by lodi; the copy it had is \
         kept as {name}"
    ));
    Ok(Some(name))
}

/// Keep the bytes an adopted path already holds as its original, once, when the entry asks for a
/// backup and none is kept yet. No warning: nothing was replaced.
fn adopt(
    data: &Root,
    home: &Root,
    index: &mut BackupIndex,
    entry: &FileEntry,
) -> Result<Option<String>, Diagnostic> {
    let path = entry.path.as_str();
    if !entry.backup || index.original_for(path).is_some() {
        return Ok(None);
    }
    let Some((bytes, mode)) = look(home, &entry.path)? else {
        return Ok(None);
    };
    backup::keep(data, index, path, &bytes, mode, Kind::Original).map(Some)
}

/// Create the parent directories of `rel` that are not there, at [`DIRECTORY_MODE`], and record
/// each one. A directory that already exists keeps the mode it has: Lodi creates directories and
/// never re-modes or removes one (design call D7).
fn make_parents(
    home: &Root,
    rel: &RelPath,
    directories: &mut BTreeSet<String>,
) -> Result<(), Diagnostic> {
    let components: Vec<&str> = rel.components().collect();
    let mut prefix = String::new();
    for component in &components[..components.len() - 1] {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(component);
        if home.path().join(&prefix).is_dir() {
            continue;
        }
        let dir = RelPath::new(&prefix)?;
        fsops::mkdir_p(home, &dir)?;
        fsops::set_mode(home, &dir, DIRECTORY_MODE)?;
        directories.insert(prefix.clone());
    }
    Ok(())
}

/// `E_STORE_PERM` (exit 9) for a path this user cannot write, raised **before** the first
/// mutation and naming the path, rather than as a half-applied plan.
fn refuse_unwritable(home: &Root, plan: &Plan) -> Result<(), Diagnostic> {
    for step in &plan.steps {
        let rel = match &step.action {
            Action::Write(entry) | Action::Absent(entry) | Action::Adopt(entry) => {
                entry.path.clone()
            }
            Action::Restore { path, .. } | Action::Delete { path, .. } => path.clone(),
            Action::Forget { .. } | Action::Record(_) | Action::Nothing => continue,
        };
        // The nearest existing ancestor is what has to be writable: the file itself when it is
        // there, otherwise the deepest directory on the way to it.
        let mut at = home.path().join(rel.as_str());
        loop {
            if at.exists() {
                break;
            }
            match at.parent() {
                Some(parent) if parent.starts_with(home.path()) => at = parent.to_path_buf(),
                _ => break,
            }
        }
        if at.exists() && !writable(&at) {
            return Err(Diagnostic::new(
                "E_STORE_PERM",
                format!(
                    "`{rel}` cannot be written: there is no write permission on {}",
                    at.display()
                ),
            )
            .hint("fix the permissions of that path, or drop the entry from home.toml"));
        }
    }
    Ok(())
}

/// Whether this process may write `path`, asked of the kernel rather than derived from the mode
/// bits, so that a group, an ACL or a read-only mount all answer correctly.
fn writable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c` is a valid NUL-terminated path; `access` only reads it.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

/// The plan's own lines in the past tense, plus the summary: what `apply` prints (step 6).
fn applied_lines(plan: &Plan) -> Vec<String> {
    let mut lines: Vec<String> = plan
        .steps
        .iter()
        .map(|step| {
            let mut text = format!(
                "{:<9} {:<25} {}",
                past(step.verb),
                step.path,
                step.mode_text
            );
            if let Some(note) = &step.note {
                text.push_str(&format!("    ({note})"));
            }
            text
        })
        .collect();
    lines.push(plan.summary());
    lines
}

/// The past tense of a verb. `lodi gc` renders its plan the same way (`gc::as_applied`).
fn past(verb: Verb) -> &'static str {
    match verb {
        Verb::Create => "created",
        Verb::Seed => "seeded",
        Verb::Adopt => "adopted",
        Verb::Update => "updated",
        Verb::Replace => "replaced",
        Verb::Restore => "restored",
        Verb::Remove => "removed",
        Verb::Keep => "kept",
        Verb::Drift => "drifted",
        Verb::Unchanged => "unchanged",
    }
}

// ------------------------------------------------------------------------ lodi home status ---

/// What `status` found at one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Managed, and the bytes are the ones Lodi wrote.
    Ok,
    /// Managed, and the bytes are not.
    Drift,
    /// Managed, and the file is gone.
    Missing,
    /// Declared and not yet applied.
    Unmanaged,
    /// A backup kept for a path the state no longer names.
    StaleBackup,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Ok => "ok",
            State::Drift => "drift",
            State::Missing => "missing",
            State::Unmanaged => "unmanaged",
            State::StaleBackup => "stale-backup",
        }
    }
}

/// The order the summary counts the states in, so two runs print the same summary.
const STATES: &[State] = &[
    State::Ok,
    State::Drift,
    State::Missing,
    State::Unmanaged,
    State::StaleBackup,
];

/// One line of `lodi home status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub state: State,
    pub path: String,
    pub mode_text: String,
    pub note: Option<String>,
}

impl std::fmt::Display for Row {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:<12} {:<25} {}",
            self.state.name(),
            self.path,
            self.mode_text
        )?;
        match &self.note {
            Some(note) => write!(f, "    ({note})"),
            None => Ok(()),
        }
    }
}

/// `lodi home status`: the report, which **writes nothing and always exits 0**.
pub fn status(roots: &Roots) -> Result<Outcome, Failure> {
    status_with(roots, programs::registry())
}

/// [`status`] with an explicit program-module registry (`manifest::load_home_manifest_with`).
pub fn status_with(roots: &Roots, registry: &[Module]) -> Result<Outcome, Failure> {
    status_loaded(roots, registry, None)
}

pub fn status_preloaded(roots: &Roots, manifest: &HomeManifest) -> Result<Outcome, Failure> {
    status_loaded(roots, programs::registry(), Some(manifest))
}

fn status_loaded(
    roots: &Roots,
    registry: &[Module],
    preloaded: Option<&HomeManifest>,
) -> Result<Outcome, Failure> {
    let data = roots.data_root();
    let home = roots.home_root();
    let _shared = share_lock(&data)?;
    let manifest = match preloaded {
        Some(manifest) => manifest.clone(),
        None => load_home_manifest_with(roots, registry)?,
    };
    let state = state::read(&data)?;
    let index = backup::read(&data)?;

    let mut warnings = manifest_warnings(roots, &manifest);
    let mut rows: Vec<Row> = Vec::new();
    let paths: BTreeSet<&String> = manifest.files.keys().chain(state.files.keys()).collect();
    for path in paths {
        let rel = match manifest.files.get(path) {
            Some(entry) => entry.path.clone(),
            None => RelPath::new(path)?,
        };
        let current = look(&home, &rel)?;
        let row = match (state.files.get(path), current) {
            // A seeded file is the application's after the first write: never compared (§3.2).
            (Some(managed), current) if managed.is_seed() => Row {
                state: State::Ok,
                path: path.clone(),
                mode_text: current
                    .map(|(_, mode)| mode_text(mode))
                    .unwrap_or_else(|| managed.mode.clone()),
                note: Some("seeded: lodi does not compare it".to_string()),
            },
            (Some(managed), Some((bytes, mode))) => {
                let now = sha256_hex(&bytes);
                if now == managed.sha256 {
                    Row {
                        state: State::Ok,
                        path: path.clone(),
                        mode_text: mode_text(mode),
                        note: (mode_text(mode) != managed.mode)
                            .then(|| format!("mode {} was set by lodi", managed.mode)),
                    }
                } else {
                    warnings.push(
                        crate::home::plan::Drift {
                            recorded: managed.sha256.clone(),
                            now,
                        }
                        .warning(path),
                    );
                    Row {
                        state: State::Drift,
                        path: path.clone(),
                        mode_text: mode_text(mode),
                        note: Some("edited by hand since lodi wrote it".to_string()),
                    }
                }
            }
            (Some(managed), None) => Row {
                state: State::Missing,
                path: path.clone(),
                mode_text: managed.mode.clone(),
                note: Some("managed, but the file is gone".to_string()),
            },
            (None, current) => {
                let entry = manifest
                    .files
                    .get(path)
                    .expect("a path is declared or managed");
                match (entry.state, current) {
                    (crate::home::manifest::FileState::Seed, Some((_, mode))) => Row {
                        state: State::Ok,
                        path: path.clone(),
                        mode_text: mode_text(mode),
                        note: Some("seed: a file is already there, and lodi leaves it".to_string()),
                    },
                    (crate::home::manifest::FileState::Absent, None) => Row {
                        state: State::Ok,
                        path: path.clone(),
                        mode_text: entry.mode_text.clone(),
                        note: Some("declared absent, and it is".to_string()),
                    },
                    _ => Row {
                        state: State::Unmanaged,
                        path: path.clone(),
                        mode_text: entry.mode_text.clone(),
                        note: Some("declared, not yet applied".to_string()),
                    },
                }
            }
        };
        rows.push(row);
    }

    // A backup whose path the state no longer names: kept, never removed by lodi, and reported
    // so that the user can see what is still recoverable (design call D5).
    for (name, record) in &index.backups {
        if state.files.contains_key(&record.path) {
            continue;
        }
        rows.push(Row {
            state: State::StaleBackup,
            path: record.path.clone(),
            mode_text: record.mode.clone(),
            note: Some(format!(
                "{} kept as {}",
                record.kind,
                name.get(..16).unwrap_or(name)
            )),
        });
    }

    let mut lines: Vec<String> = rows.iter().map(Row::to_string).collect();
    lines.push(summary(&rows));
    if !manifest.tools.is_empty() {
        lines.push(profile::path_status(roots));
        let notice = profile::notice(roots, manifest.uses_program("fish"));
        warnings.extend(notice.warnings);
        lines.extend(notice.lines);
    }
    lines.extend(profile::hook_lines(roots, &manifest.hooks()));
    Ok(Outcome {
        lines,
        warnings,
        linger_off: false,
    })
}

fn summary(rows: &[Row]) -> String {
    let counts: Vec<String> = STATES
        .iter()
        .filter_map(|state| {
            let n = rows.iter().filter(|r| r.state == *state).count();
            (n > 0).then(|| format!("{n} {}", state.name()))
        })
        .collect();
    let paths = format!(
        "{} path{}",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
    if counts.is_empty() {
        format!("{paths}: nothing to report")
    } else {
        format!("{paths}: {}", counts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_past_tense_fits_the_same_column() {
        for verb in [
            Verb::Create,
            Verb::Seed,
            Verb::Adopt,
            Verb::Update,
            Verb::Replace,
            Verb::Restore,
            Verb::Remove,
            Verb::Keep,
            Verb::Unchanged,
        ] {
            assert!(past(verb).len() <= 9, "{}", past(verb));
        }
    }

    #[test]
    fn the_status_summary_counts_in_a_fixed_order() {
        let row = |state: State, path: &str| Row {
            state,
            path: path.into(),
            mode_text: "0644".into(),
            note: None,
        };
        let rows = vec![
            row(State::Drift, "a"),
            row(State::Ok, "b"),
            row(State::Ok, "c"),
        ];
        assert_eq!(summary(&rows), "3 paths: 2 ok, 1 drift");
        assert_eq!(summary(&[]), "0 paths: nothing to report");
    }

    #[test]
    fn a_status_row_prints_in_columns() {
        let row = Row {
            state: State::StaleBackup,
            path: ".netrc".into(),
            mode_text: "0600".into(),
            note: Some("original kept as abcd".into()),
        };
        assert!(row.to_string().starts_with("stale-backup .netrc"));
        assert!(row.to_string().ends_with("(original kept as abcd)"));
    }
}
