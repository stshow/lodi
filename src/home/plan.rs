//! The home plan: what a switch would do to the user's files, before anything changes
//! (M-0.5 T-2 and T-3, design calls D5, D6, D7, D12, D13 — `LD-101`, `LD-102`, `LD-108`, `LD-109`).
//!
//! **A plan writes nothing at all.** Not a state file, not a lock, not a directory — not even the
//! configuration directory the manifest lives in. That is the one property this command must
//! have, and `tests/home_plan.rs` asserts it the only way that cannot be argued with: the write
//! ledger of [`crate::home::fsops`] is empty afterwards and the scratch root's listing is
//! byte-identical before and after. Everything below therefore reads, and the module holds no
//! call that could mutate a file.
//!
//! The lines are one per declared path, in lexicographic order, then a summary:
//!
//! ```text
//! create    .config/git/ignore        0644
//! replace   .inputrc                  0600    (backup: first unmanaged copy kept)
//! restore   .netrc                    0600    (entry removed: the original is restored)
//! drift     .gitconfig                0644    (edited by hand: …)
//! unchanged .profile-that-is-fine     0644
//! 5 files: 1 create, 1 replace, 1 restore, 1 drift, 1 unchanged
//! ```
//!
//! M-Home (1.1) adds three verbs (`docs/design/HOME_PROGRAMS.md` §3.2, §4.3, §4.4): **`adopt`**,
//! for a path that already holds exactly the bytes and the mode Lodi would write — recorded (and its bytes
//! kept once for `restore`) without writing them and without `W_REPLACED_UNMANAGED`, because
//! nothing is replaced — and **`seed`**, for a `state = "seed"` entry written once where nothing
//! is. A seeded path is never compared again, so it is never drift. A step a program module
//! rendered carries its table in the note: `(programs.git)`. The plan also carries the program
//! warning `W_SHADOWED`, which every verb prints on standard error. The third is **`update`**:
//! new bytes over a file Lodi wrote, for an entry of the 1.1 tables (`[home.file]`,
//! `[home.xdg_config]`, a program module), which print `create` with the backup note over a file
//! Lodi did not write (§4.3) and never `replace`. A `[files]` entry keeps 1.0's verbs exactly —
//! `replace` for both, and `unchanged`, adopting nothing, for identical bytes — so a 1.0
//! manifest plans, applies and reports as 1.0 did (§10 I10).
//!
//! Since T-3 the plan is computed against the **state record** as well as the manifest, so it is
//! literally the same computation the home apply performs: `apply` renders these steps in the
//! past tense and executes the [`Action`] each one carries. A path in the state that the manifest
//! no longer declares is `restore`, `remove` or `keep` from the `on_remove` recorded with it
//! (design call D5); a managed path whose bytes are not the ones Lodi wrote is `drift`, and it is
//! what makes an apply refuse (design call D6).

use std::collections::BTreeSet;
use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::diag::Diagnostic;
use crate::home::backup::BackupIndex;
use crate::home::fsops::{RelPath, Root};
use crate::home::manifest::{FileEntry, FileState, HomeManifest, Origin, load_home_manifest_with};
use crate::home::programs::{self, Module};
use crate::home::state::{self, HomeState, ManagedFile};
use crate::manifest::ManifestErrors;
use crate::roots::Roots;
use crate::util::sha256_hex;

/// How many characters of a hash the drift warning shows.
pub const HASH_PREFIX: usize = 8;

/// What an apply would do to one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Create,
    Seed,
    Adopt,
    Update,
    Restore,
    Remove,
    Keep,
    Drift,
    Unchanged,
}

impl Verb {
    pub fn name(self) -> &'static str {
        match self {
            Verb::Create => "create",
            Verb::Seed => "seed",
            Verb::Adopt => "adopt",
            Verb::Update => "update",
            Verb::Restore => "restore",
            Verb::Remove => "remove",
            Verb::Keep => "keep",
            Verb::Drift => "drift",
            Verb::Unchanged => "unchanged",
        }
    }
}

/// The verbs in the order the summary counts them, so two runs of the same plan print the same
/// summary (design call D13).
const VERBS: &[Verb] = &[
    Verb::Create,
    Verb::Seed,
    Verb::Adopt,
    Verb::Update,
    Verb::Restore,
    Verb::Remove,
    Verb::Keep,
    Verb::Drift,
    Verb::Unchanged,
];

/// A managed path whose current bytes are not the ones Lodi wrote (design call D6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    /// The sha256 the state records for the bytes Lodi wrote.
    pub recorded: String,
    /// The sha256 of what is there now.
    pub now: String,
}

impl Drift {
    /// `W_DRIFT` as `status` and `apply` print it on standard error, with both prefixes.
    pub fn warning(&self, path: &str) -> String {
        format!(
            "lodi: warning W_DRIFT: {path} has changed since lodi wrote it (recorded {}, now {})",
            prefix(&self.recorded),
            prefix(&self.now)
        )
    }
}

/// The first [`HASH_PREFIX`] characters of a digest, never a slice through a character: the
/// state reader only lets hex through (LD-361), and this holds whatever it is handed.
fn prefix(digest: &str) -> &str {
    digest.get(..HASH_PREFIX).unwrap_or(digest)
}

/// The work one step carries, which `apply` executes and `plan` only describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Write the entry's bytes at its mode, backing up an unmanaged file first when asked.
    Write(FileEntry),
    /// `state = "absent"`: remove the file, backing up an unmanaged one first when asked.
    Absent(FileEntry),
    /// The path already holds the entry's bytes: record it without writing them, keeping the
    /// bytes once as the original when the entry asks for a backup, and re-set a differing mode.
    Adopt(FileEntry),
    /// Re-record the entry without touching the file: a managed path that became a seed.
    Record(FileEntry),
    /// The entry disappeared with `on_remove = "restore"`: put the backup back, then drop the row.
    Restore {
        path: RelPath,
        backup: String,
        mode: u32,
    },
    /// The entry disappeared with `on_remove = "delete"`, or with `restore` and no backup kept.
    Delete { path: RelPath, no_backup: bool },
    /// The entry disappeared with `on_remove = "keep"`: drop the state row and leave the file.
    Forget { path: RelPath },
    /// Nothing to do.
    Nothing,
}

/// One planned path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub verb: Verb,
    pub path: String,
    pub mode_text: String,
    /// The parenthesized note after the mode, when the step has one.
    pub note: Option<String>,
    /// What the step does, for `apply`.
    pub action: Action,
    /// Set when the path has drifted, whether or not the verb is `drift`.
    pub drift: Option<Drift>,
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:<9} {:<25} {}",
            self.verb.name(),
            self.path,
            self.mode_text
        )?;
        match &self.note {
            Some(note) => write!(f, "    ({note})"),
            None => Ok(()),
        }
    }
}

/// The whole plan: the steps in lexicographic path order and the summary line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// The [`program_warnings`] lines, printed on standard error by `plan`.
    pub warnings: Vec<String>,
    /// `[services]`, after the files' summary; empty for a home without them.
    pub services: crate::home::services::Services,
}

impl Plan {
    /// `3 files: 1 create, 1 replace, 1 unchanged`, with the zero counts left out.
    pub fn summary(&self) -> String {
        let counts: Vec<String> = VERBS
            .iter()
            .filter_map(|verb| {
                let n = self.steps.iter().filter(|s| s.verb == *verb).count();
                (n > 0).then(|| format!("{n} {}", verb.name()))
            })
            .collect();
        let files = format!(
            "{} file{}",
            self.steps.len(),
            if self.steps.len() == 1 { "" } else { "s" }
        );
        if counts.is_empty() {
            format!("{files}: nothing to do")
        } else {
            format!("{files}: {}", counts.join(", "))
        }
    }

    /// Every line the command prints, the summary last.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self.steps.iter().map(Step::to_string).collect();
        lines.push(self.summary());
        lines.extend(self.services.lines());
        lines
    }

    /// The drifted steps, in path order: what makes `apply` refuse (design call D6).
    pub fn drifted(&self) -> Vec<&Step> {
        self.steps.iter().filter(|s| s.drift.is_some()).collect()
    }

    /// Whether anything at all would change.
    pub fn is_empty(&self) -> bool {
        self.steps.iter().all(|s| s.action == Action::Nothing) && self.services.is_empty()
    }

    /// The units whose inline file this plan writes or removes, by unit name.
    pub fn unit_files(&self) -> Vec<String> {
        let dir = format!("{}/", crate::home::manifest::USER_UNIT_DIR);
        self.steps
            .iter()
            .filter(|step| step.action != Action::Nothing)
            .filter_map(|step| step.path.strip_prefix(&dir))
            .map(str::to_string)
            .collect()
    }
}

/// Load `<config>/home.toml` and the state record and work out what an apply would do. Reads
/// only: a state file that is not there is the empty state, and nothing is created to find that
/// out.
pub fn plan(roots: &Roots) -> Result<Plan, ManifestErrors> {
    plan_with(roots, programs::registry())
}

/// [`plan`] with an explicit program-module registry (`manifest::load_home_manifest_with`).
pub fn plan_with(roots: &Roots, registry: &[Module]) -> Result<Plan, ManifestErrors> {
    let manifest = load_home_manifest_with(roots, registry)?;
    let mut plan =
        plan_for(roots, &manifest, false, &Default::default()).map_err(|d| ManifestErrors {
            diagnostics: vec![d],
        })?;
    plan.warnings = program_warnings(roots, &manifest);
    Ok(plan)
}

/// Every warning a manifest's program modules give plan, apply and status, exit status
/// unchanged: `W_PROGRAM_NOT_FOUND` per module in name order, then [`shadow_warnings`].
pub fn program_warnings(roots: &Roots, manifest: &HomeManifest) -> Vec<String> {
    let mut warnings = not_found_warnings(manifest);
    warnings.extend(shadow_warnings(roots, manifest));
    warnings
}

/// `W_PROGRAM_NOT_FOUND` (decision D1): one line per declared, enabled module whose executable is
/// not on this process's `PATH`. Its files are rendered all the same, and nothing is installed:
/// only whether an executable file of that name exists in a `PATH` directory is asked.
pub fn not_found_warnings(manifest: &HomeManifest) -> Vec<String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<std::path::PathBuf> = std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .collect();
    let found = |name: &str| {
        dirs.iter().any(|dir| {
            std::fs::metadata(dir.join(name)).is_ok_and(|meta| {
                use std::os::unix::fs::PermissionsExt;
                meta.is_file() && meta.permissions().mode() & 0o111 != 0
            })
        })
    };
    manifest
        .programs
        .iter()
        .filter(|program| !found(&program.executable))
        .map(|program| {
            format!(
                "lodi: warning W_PROGRAM_NOT_FOUND: [programs.{}] is declared, but `{}` is not \
                 on PATH; its files are rendered all the same, and lodi installs nothing: \
                 install it with [tools], the host scope's [packages] or the distribution",
                program.name, program.executable
            )
        })
        .collect()
}

/// The `W_SHADOWED` lines of a manifest, in module order, exit status unchanged: one per file
/// that exists where an application reads before, or alongside, a file Lodi renders for it
/// (§4.6). The shadowing file is only looked at, never read, moved or deleted; a dangling link
/// counts as there.
pub fn shadow_warnings(roots: &Roots, manifest: &HomeManifest) -> Vec<String> {
    let home = roots.home();
    let shown = |path: &Path| match path.strip_prefix(home) {
        Ok(rest) => rest.display().to_string(),
        Err(_) => path.display().to_string(),
    };
    let mut warnings = Vec::new();
    for program in &manifest.programs {
        for shadow in &program.shadowed_by {
            if !shadow.by.exists() && std::fs::read_link(&shadow.by).is_err() {
                continue;
            }
            warnings.push(format!(
                "lodi: warning W_SHADOWED: {} exists, and {} reads it before or alongside {}; \
                 move its content into [programs.{}] and delete it",
                shown(&shadow.by),
                program.name,
                shown(&shadow.of),
                program.name
            ));
        }
    }
    warnings
}

/// The plan of a loaded manifest against the roots, with `overwrite_drift` deciding whether a
/// drifted path is shown as `drift` (an apply would refuse) or as the work it would then do.
pub fn plan_for(
    roots: &Roots,
    manifest: &HomeManifest,
    overwrite_drift: bool,
    context: &crate::home::services::Context,
) -> Result<Plan, Diagnostic> {
    let data = roots.data_root();
    let state = state::read(&data)?;
    let index = crate::home::backup::read(&data)?;
    let mut plan = plan_all(
        manifest,
        &state,
        &index,
        &roots.home_root(),
        overwrite_drift,
    )?;
    plan.services = crate::home::services::plan(manifest, &state, &plan.unit_files(), context)?;
    Ok(plan)
}

/// The plan of a manifest and a state against the files below `home`.
pub fn plan_all(
    manifest: &HomeManifest,
    state: &HomeState,
    index: &BackupIndex,
    home: &Root,
    overwrite_drift: bool,
) -> Result<Plan, Diagnostic> {
    let paths: BTreeSet<&String> = manifest.files.keys().chain(state.files.keys()).collect();
    let mut steps = Vec::new();
    for path in paths {
        let entry = manifest.files.get(path);
        let mut step = step(
            path,
            entry,
            state.files.get(path),
            index,
            home,
            overwrite_drift,
        )?;
        // A rendered file names the table that declared it (§4.4).
        if let Some(entry) = entry
            && matches!(entry.origin, Origin::Program(_))
        {
            step.note = Some(match step.note.take() {
                Some(note) => format!("{}; {note}", entry.table),
                None => entry.table.clone(),
            });
        }
        steps.push(step);
    }
    Ok(Plan {
        steps,
        warnings: Vec::new(),
        services: Default::default(),
    })
}

/// The note a `replace` or a `remove` carries when an apply would keep the first copy of a file
/// the user had there already. A managed path, or one that already has an original backup, never
/// gets a second one (design call D5).
fn backup_note(entry: &FileEntry, managed: Option<&ManagedFile>, index: &BackupIndex) -> bool {
    entry.backup && managed.is_none() && index.original_for(entry.path.as_str()).is_none()
}

fn step(
    path: &str,
    entry: Option<&FileEntry>,
    managed: Option<&ManagedFile>,
    index: &BackupIndex,
    home: &Root,
    overwrite_drift: bool,
) -> Result<Step, Diagnostic> {
    let rel = match entry {
        Some(entry) => entry.path.clone(),
        None => RelPath::new(path)?,
    };
    let current = look(home, &rel)?;
    // A seeded path is never compared again (§3.2), and neither is one about to become a seed.
    let seeding = entry.is_some_and(|e| e.state == FileState::Seed);
    let drift = match (managed, &current) {
        (Some(managed), Some((bytes, _))) if !managed.is_seed() && !seeding => {
            let now = sha256_hex(bytes);
            (now != managed.sha256).then(|| Drift {
                recorded: managed.sha256.clone(),
                now,
            })
        }
        _ => None,
    };
    let blocked = drift.is_some() && !overwrite_drift;
    let drift_note = || Some("drift: the edited bytes are kept in the backup store".to_string());
    let stopped = |mode_text: String, action: Action| Step {
        verb: Verb::Drift,
        path: path.to_string(),
        mode_text,
        note: Some(
            "edited by hand: lodi switch --home --overwrite-drift takes it back".to_string(),
        ),
        action,
        drift: drift.clone(),
    };

    let step = match (entry, managed) {
        // A seed: written once where nothing is, then left alone for good (§3.2).
        (Some(entry), _) if entry.state == FileState::Seed => {
            let (verb, note, action) = match (managed, &current) {
                (Some(managed), _) if managed.is_seed() => (Verb::Unchanged, None, Action::Nothing),
                (Some(_), _) => (
                    Verb::Unchanged,
                    Some("now a seed: lodi stops comparing it".to_string()),
                    Action::Record(entry.clone()),
                ),
                (None, None) => (Verb::Seed, None, Action::Write(entry.clone())),
                (None, Some(_)) => (
                    Verb::Unchanged,
                    Some("seed: a file is already there, and lodi leaves it".to_string()),
                    Action::Nothing,
                ),
            };
            Step {
                verb,
                path: path.to_string(),
                mode_text: entry.mode_text.clone(),
                note,
                action,
                drift: None,
            }
        }
        // A declared file that must be there. A seeded record is not Lodi's bytes any more, so
        // for a path that stops being a seed it counts as unmanaged: backed up, or adopted.
        (Some(entry), managed) if entry.state == FileState::Managed => {
            let managed = managed.filter(|m| !m.is_seed());
            let action = Action::Write(entry.clone());
            if blocked {
                return Ok(stopped(entry.mode_text.clone(), action));
            }
            // `update` over Lodi's own file, `create` with the backup note over one it did not
            // write (§4.3), and `adopt` for identical bytes.
            let (verb, note, action) = match (&drift, &current) {
                (Some(_), _) => (Verb::Update, drift_note(), action),
                (None, None) => (Verb::Create, None, action),
                (None, Some((bytes, mode))) => {
                    if *bytes == entry.bytes && *mode == entry.mode {
                        if managed.is_none() {
                            let note = backup_note(entry, managed, index)
                                .then(|| "backup: first unmanaged copy kept".to_string());
                            (Verb::Adopt, note, Action::Adopt(entry.clone()))
                        } else {
                            (Verb::Unchanged, None, Action::Nothing)
                        }
                    } else {
                        let note = backup_note(entry, managed, index)
                            .then(|| "backup: first unmanaged copy kept".to_string());
                        let verb = match managed {
                            Some(_) => Verb::Update,
                            None => Verb::Create,
                        };
                        (verb, note, action)
                    }
                }
            };
            Step {
                verb,
                path: path.to_string(),
                mode_text: entry.mode_text.clone(),
                note,
                action,
                drift,
            }
        }
        // A declared file that must *not* be there.
        (Some(entry), _) => {
            let action = Action::Absent(entry.clone());
            if current.is_none() {
                Step {
                    verb: Verb::Unchanged,
                    path: path.to_string(),
                    mode_text: entry.mode_text.clone(),
                    note: None,
                    action: Action::Nothing,
                    drift: None,
                }
            } else if blocked {
                return Ok(stopped(entry.mode_text.clone(), action));
            } else {
                let note = match &drift {
                    Some(_) => drift_note(),
                    None => backup_note(entry, managed, index)
                        .then(|| "backup: first unmanaged copy kept".to_string()),
                };
                Step {
                    verb: Verb::Remove,
                    path: path.to_string(),
                    mode_text: entry.mode_text.clone(),
                    note,
                    action,
                    drift,
                }
            }
        }
        // A seeded file the application has changed since stays, whatever `on_remove` says:
        // removal deletes or restores only what Lodi wrote and still matches (§3.2).
        (None, Some(managed))
            if managed.is_seed()
                && current
                    .as_ref()
                    .is_some_and(|(bytes, _)| sha256_hex(bytes) != managed.sha256) =>
        {
            Step {
                verb: Verb::Keep,
                path: path.to_string(),
                mode_text: managed.mode.clone(),
                note: Some("entry removed: the seeded file changed since, so it stays".to_string()),
                action: Action::Forget { path: rel },
                drift: None,
            }
        }
        // The entry disappeared: what happens is the `on_remove` recorded with the file (D5).
        (None, Some(managed)) => {
            let backup = managed
                .backup
                .clone()
                .or_else(|| index.original_for(path).map(|(name, _)| name.clone()));
            let backup_mode = backup
                .as_ref()
                .and_then(|name| index.backups.get(name))
                .map(|record| record.mode.clone())
                .unwrap_or_else(|| managed.mode.clone());
            match managed.on_remove.as_str() {
                "keep" => Step {
                    verb: Verb::Keep,
                    path: path.to_string(),
                    mode_text: managed.mode.clone(),
                    note: Some("entry removed: the file stays".to_string()),
                    action: Action::Forget { path: rel },
                    // Nothing is written or removed here, so an edited file destroys nothing
                    // and is not a reason to stop the whole apply.
                    drift: None,
                },
                "restore" => match backup {
                    Some(name) => {
                        let action = Action::Restore {
                            path: rel,
                            mode: octal(&backup_mode),
                            backup: name,
                        };
                        if blocked {
                            return Ok(stopped(backup_mode, action));
                        }
                        Step {
                            verb: Verb::Restore,
                            path: path.to_string(),
                            mode_text: backup_mode,
                            note: Some("entry removed: the original is restored".to_string()),
                            action,
                            drift,
                        }
                    }
                    None => {
                        let action = Action::Delete {
                            path: rel,
                            no_backup: true,
                        };
                        if blocked {
                            return Ok(stopped(managed.mode.clone(), action));
                        }
                        Step {
                            verb: Verb::Remove,
                            path: path.to_string(),
                            mode_text: managed.mode.clone(),
                            note: Some("entry removed: no backup was kept".to_string()),
                            action,
                            drift,
                        }
                    }
                },
                "delete" => {
                    let action = Action::Delete {
                        path: rel,
                        no_backup: false,
                    };
                    if blocked {
                        return Ok(stopped(managed.mode.clone(), action));
                    }
                    Step {
                        verb: Verb::Remove,
                        path: path.to_string(),
                        mode_text: managed.mode.clone(),
                        note: Some("entry removed: delete".to_string()),
                        action,
                        drift,
                    }
                }
                other => {
                    return Err(Diagnostic::new(
                        "E_STORE_IO",
                        format!(
                            "the state record gives `{path}` an on_remove of `{}`, which is not \
                             restore, delete or keep",
                            other.escape_debug()
                        ),
                    )
                    .hint("this state file was not written by a lodi this build understands"));
                }
            }
        }
        (None, None) => unreachable!("a path is in the manifest, in the state, or in neither"),
    };
    Ok(step)
}

/// Four octal digits back into a mode. A record this build did not write falls back to 0644
/// rather than failing a restore over a cosmetic field.
fn octal(text: &str) -> u32 {
    u32::from_str_radix(text, 8).unwrap_or(0o644) & 0o7777
}

/// What is at `rel` inside `root` today: its bytes and its permission bits, or `None` when
/// nothing is there.
///
/// Every component of the path — the last one included — is refused when it is a symbolic link,
/// with the `E_PATH_ESCAPE` [`crate::home::fsops`] would raise for the same path, so a plan
/// predicts the refusal instead of promising a write that would not happen. This function reads;
/// it creates, opens for writing, renames and removes nothing.
pub fn look(root: &Root, rel: &RelPath) -> Result<Option<(Vec<u8>, u32)>, Diagnostic> {
    let mut at = root.path().to_path_buf();
    for component in rel.components() {
        at.push(component);
        if std::fs::read_link(&at).is_ok() {
            return Err(Diagnostic::new(
                "E_PATH_ESCAPE",
                format!(
                    "`{component}` of `{rel}` is a symbolic link, and lodi does not follow one \
                     inside a root"
                ),
            )
            .hint("remove the link, or point the entry at the file it names"));
        }
    }
    read_at(&at)
}

fn read_at(path: &Path) -> Result<Option<(Vec<u8>, u32)>, Diagnostic> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(path, e)),
    };
    if !meta.is_file() {
        return Err(Diagnostic::new(
            "E_PATH_ESCAPE",
            format!("`{}` is not a regular file", path.display()),
        )
        .hint("move what is in the way, or choose another path"));
    }
    let bytes = std::fs::read(path).map_err(|e| io(path, e))?;
    Ok(Some((bytes, meta.permissions().mode() & 0o7777)))
}

fn io(path: &Path, e: std::io::Error) -> Diagnostic {
    Diagnostic::new("E_STORE_IO", format!("cannot read {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step_of(verb: Verb, path: &str) -> Step {
        Step {
            verb,
            path: path.into(),
            mode_text: "0644".into(),
            note: None,
            action: Action::Nothing,
            drift: None,
        }
    }

    #[test]
    fn the_summary_counts_in_a_fixed_order_and_omits_the_zeroes() {
        let plan = Plan {
            steps: vec![
                step_of(Verb::Unchanged, "a"),
                step_of(Verb::Create, "b"),
                step_of(Verb::Unchanged, "c"),
            ],
            warnings: vec![],
            services: Default::default(),
        };
        assert_eq!(plan.summary(), "3 files: 1 create, 2 unchanged");
        assert_eq!(
            Plan {
                steps: vec![],
                warnings: vec![],
                services: Default::default(),
            }
            .summary(),
            "0 files: nothing to do"
        );
        assert_eq!(
            Plan {
                steps: vec![step_of(Verb::Create, "a")],
                warnings: vec![],
                services: Default::default(),
            }
            .summary(),
            "1 file: 1 create"
        );
    }

    #[test]
    fn a_step_prints_its_verb_path_and_mode_in_columns() {
        let mut step = step_of(Verb::Update, ".inputrc");
        assert_eq!(step.to_string(), "update    .inputrc                  0644");
        step.note = Some("backup: first unmanaged copy kept".into());
        assert!(
            step.to_string()
                .ends_with("0644    (backup: first unmanaged copy kept)")
        );
    }

    #[test]
    fn the_drift_warning_names_both_hash_prefixes() {
        let drift = Drift {
            recorded: "aaaaaaaabbbb".into(),
            now: "ccccccccdddd".into(),
        };
        let text = drift.warning(".inputrc");
        assert!(
            text.starts_with("lodi: warning W_DRIFT: .inputrc"),
            "{text}"
        );
        assert!(text.contains("recorded aaaaaaaa"), "{text}");
        assert!(text.contains("now cccccccc"), "{text}");
    }

    #[test]
    fn a_mode_text_that_is_not_octal_falls_back_rather_than_failing_a_restore() {
        assert_eq!(octal("0600"), 0o600);
        assert_eq!(octal("644"), 0o644);
        assert_eq!(octal("nonsense"), 0o644);
    }
}
