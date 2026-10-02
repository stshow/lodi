//! The host scope: `/etc/lodi/host.toml`, the distro's packages and root-owned files, applied in
//! **one planned, journalled transaction** (`spec/01` §5, ADR-013, OD-15).
//!
//! `lodi switch` plans and applies through this module, and `lodi import`, `update`, `pin` and
//! `unpin` read the machine through it ([`pin::verbs`], LD-397). Root-owned files and
//! distribution packages both work end to end, on every distribution the safety gate accepts.
//!
//! # The safety rule this module makes structural
//!
//! The host scope is never run against the development machine (`AGENTS.md` §8, LD-45). Nothing
//! here relies on anyone remembering that:
//!
//! - [`safety::Gate::open`] refuses **before it opens anything at all** unless the root carries
//!   its marker — `E_HOST_NOT_ARMED`, exit 9 (design call D1, LD-113): `etc/lodi/may-manage`,
//!   which only `lodi import` writes once the owner agreed (#705).
//! - `--root DIR` is a real, documented flag, not a test seam: every test runs against a scratch
//!   directory it owns, with the same arming rule applied to it (D2, LD-114).
//! - The package managers are found on `PATH` by bare name and run as argv vectors in a fixed
//!   non-interactive environment, so a test's own shims are the whole seam (D3, LD-115). There is
//!   no test-only branch, environment variable or hidden flag anywhere in these paths.
//! - A lodi started as the user gets root for one typed request through [`crate::elevate`],
//!   which runs lodi's own binary again through `sudo`, `doas` or `run0`. A root-owned action
//!   that cannot get root is `E_NEED_ROOT`.
//! - A machine that must never be the root — a development lane, `scripts/gate.sh` — says so
//!   with [`REQUIRE_ROOT_VAR`]`=1`, and then every host verb without `--root` is
//!   `E_HOST_ROOT_REQUIRED`, exit 9, before it has resolved, opened or examined any path at all
//!   ([`require_root`], LD-376). It is a guard an environment puts on itself, not a test seam:
//!   with `--root` or without the variable nothing changes.
//!
//! # What this build does and does not do
//!
//! Files and packages are both planned and applied. [`pm::backend_for`] is total since M-0.6
//! T-6 — apt for Debian and Ubuntu, pacman for Arch — so `[packages]` is never refused for want
//! of a backend and `E_UNSUPPORTED` has left that path; what still carries that code here is an
//! input this build does not implement, such as a version-qualified package name (OD-15). There
//! are no generations, no `rollback` and no command that claims to undo an apply; an interrupted
//! apply is classified, and an ambiguous one stops for a human (see [`journal`]).

pub mod apply;
pub mod basics;
pub mod boot;
pub mod converge;
pub mod fallback;
pub mod files;
pub mod firewall;
pub mod flake;
pub mod import;
pub mod journal;
pub mod kernel;
pub mod lock;
pub mod manifest;
pub mod network;
pub mod originals;
pub mod pin;
pub mod plan;
pub mod pm;
pub mod privilege;
pub mod reconcile;
pub mod remote;
pub mod reposet;
pub mod safety;
pub mod services;
pub mod source;
pub mod sources;
pub mod sourceset;
pub mod users;

use std::fmt;
use std::fs;
use std::path::Path;

use crate::diag::{Diagnostic, EXIT_MANIFEST, exit_status};
use crate::manifest::ManifestErrors;

pub use safety::{Operation, Options};

/// The environment variable that makes `--root` mandatory for every host verb (LD-376). Only the
/// exact value `1` sets it; any other value, and its absence, change nothing.
pub const REQUIRE_ROOT_VAR: &str = "LODI_HOST_REQUIRE_ROOT";

/// The LD-376 guard, as a function of its inputs alone: `command` is the command that reaches
/// the host scope, as typed after `lodi`, `root` its `--root`, and `setting` the value of
/// [`REQUIRE_ROOT_VAR`]. It reads no file and no environment, so a refusal happens before the
/// command has looked at anything.
pub fn require_root_for(
    command: &str,
    root: Option<&Path>,
    setting: Option<&std::ffi::OsStr>,
) -> Result<(), Diagnostic> {
    if root.is_some() || setting.is_none_or(|value| value != "1") {
        return Ok(());
    }
    Err(Diagnostic::new(
        "E_HOST_ROOT_REQUIRED",
        format!(
            "{REQUIRE_ROOT_VAR}=1 is set, so `lodi {command}` needs --root DIR and was given none; \
             nothing was read"
        ),
    )
    .hint(format!(
        "this environment never lets a host verb reach this machine: name a scratch root with \
         --root DIR, or run it where {REQUIRE_ROOT_VAR} is not set"
    )))
}

/// The most bytes one `[files]` `source` may have (LD-379).
pub const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;

/// Everything a host command can fail with: one diagnostic, or a manifest's worth of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    pub diagnostics: Vec<Diagnostic>,
    /// What the command did before it failed, reported as a success's report is: a combined
    /// apply's host part when its home part fails (LD-399).
    pub done: String,
}

impl HostError {
    /// The status the process exits with: that of the first diagnostic (`spec/11` §2).
    pub fn exit_status(&self) -> u8 {
        self.diagnostics
            .first()
            .map_or(EXIT_MANIFEST, |d| exit_status(d.code))
    }

    /// The diagnostic codes in report order.
    pub fn codes(&self) -> Vec<&'static str> {
        self.diagnostics.iter().map(|d| d.code).collect()
    }
}

impl From<Diagnostic> for HostError {
    fn from(diagnostic: Diagnostic) -> HostError {
        HostError {
            diagnostics: vec![diagnostic],
            done: String::new(),
        }
    }
}

impl From<ManifestErrors> for HostError {
    fn from(errors: ManifestErrors) -> HostError {
        HostError {
            diagnostics: errors.diagnostics,
            done: String::new(),
        }
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, diagnostic) in self.diagnostics.iter().enumerate() {
            if index > 0 {
                writeln!(f)?;
            }
            write!(f, "{diagnostic}")?;
        }
        Ok(())
    }
}

impl std::error::Error for HostError {}

/// What a command loaded before it could do anything: the gate it passed, the manifest, the lock,
/// what the plan acts on as it was read once, and the plan derived from all of them.
pub struct Loaded {
    pub gate: safety::Gate,
    pub manifest: manifest::HostManifest,
    pub lock: Option<lock::HostLock>,
    pub plan: plan::Plan,
    pub input: source::Input,
    /// The host's pins from the config's `lodi.lock`, read once with the manifest (M-Pin, P13).
    pub pins_lock: Option<pin::PinsLock>,
    /// What the first plan resolved of pins, which a re-plan reuses and never resolves again.
    pub pins: plan::Pins,
}

/// Open the gate, choose and judge the host, and read what the plan acts on — the manifest and
/// every `source` it declares — once (LD-379).
pub fn load(options: &Options, operation: Operation) -> Result<Loaded, HostError> {
    let gate = safety::Gate::open(options, operation)?;
    let owners = privilege::owners(&privilege::System, &gate.root);
    let host = source::select(&gate, options, &owners, false)?;
    let manifest_path = host.manifest_path();
    let shown = manifest_path.display().to_string();
    let bytes = source::read_manifest(&host)?.ok_or_else(|| {
        Diagnostic::new("E_NO_MANIFEST", format!("no host manifest at {shown}"))
            .hint("write one there with `lodi import`, or by hand")
    })?;
    let parsed = manifest::from_bytes(&bytes, &shown, &manifest::Context::from_gate(&gate))?;
    let mut sources = source::read_sources(&host, &parsed)?;
    // A declared source's keyring is read once with them, from the same host (LD-365).
    sources::read_keyrings(&host, &parsed, &mut sources)?;
    // A keyring by URL already at its managed path with its digest is read here, so that
    // neither the plan nor the apply asks the network for it (LD-366).
    sources::read_installed(&gate, &parsed, &mut sources);
    // The pins are the config's `lodi.lock` section; 2.0 reads no `pins.lock` and no 1.x
    // repository lock (#710).
    let pins_lock = match &options.config {
        Some(config) => {
            check_locked_keys(&parsed, &config.keys)?;
            config.pins.clone()
        }
        None => None,
    };
    let input = source::Input {
        host,
        manifest: bytes,
        sources,
    };
    plan_input(gate, input, parsed, options, pins_lock, None)
}

/// Q3 (LD-416): a `signed_by_url` key the root lock records must be the key the manifest
/// declares. A declaration that moved on is a stale lock, never a silent choice between the two.
fn check_locked_keys(
    parsed: &manifest::HostManifest,
    keys: &std::collections::BTreeMap<String, String>,
) -> Result<(), HostError> {
    for (name, locked) in keys {
        let Some(source) = parsed.sources.get(name) else {
            continue;
        };
        if source.signed_by_url.is_none() {
            continue;
        }
        let declared = format!("sha256:{}", source.signed_by_sha256);
        if &declared != locked {
            return Err(Diagnostic::new(
                "E_LOCK_STALE",
                format!(
                    "[sources.{name}] declares signed_by_sha256 {declared}, and the repository's \
                     lodi.lock locks {locked}"
                ),
            )
            .hint("run `lodi update` in the repository to lock the declared key, then commit")
            .into());
        }
    }
    Ok(())
}

/// Plan again from bytes already read — the re-plan under the apply lock, which acts on exactly
/// what the first plan did.
///
/// The pins are the first plan's, never resolved again (LD-395), and `pins.lock` is the one the
/// first plan read.
fn reload(
    options: &Options,
    operation: Operation,
    input: &source::Input,
    pins_lock: Option<&pin::PinsLock>,
    pins: &plan::Pins,
) -> Result<Loaded, HostError> {
    let gate = safety::Gate::open(options, operation)?;
    let shown = input.host.manifest_path().display().to_string();
    let parsed = manifest::from_bytes(
        &input.manifest,
        &shown,
        &manifest::Context::from_gate(&gate),
    )?;
    plan_input(
        gate,
        input.clone(),
        parsed,
        options,
        pins_lock.cloned(),
        Some(pins),
    )
}

/// What the plan knows of pins (M-Pin, LD-395): the pins this machine's manifest declares and its
/// file-level snapshot, resolved from `pins.lock` where it records them and in memory otherwise,
/// through the verified fetch — built only when something is unrecorded.
fn resolve_pins(
    gate: &safety::Gate,
    parsed: &manifest::HostManifest,
    pins_lock: Option<&pin::PinsLock>,
    machine: Option<&lock::HostLock>,
    manifest_digest: &str,
    host_source: &str,
) -> Result<plan::Pins, Diagnostic> {
    let stage = gate.root.join(pin::STAGE.trim_start_matches('/'));
    let stale_stage = fs::symlink_metadata(&stage).is_ok();
    let declared = parsed.packages.pins(gate.distro.name());
    let recorded = pins_lock
        .map(|lock| lock.pins.keys().cloned().collect())
        .unwrap_or_default();
    if declared.is_empty() && parsed.host.snapshot.is_none() {
        return Ok(plan::Pins {
            recorded,
            stale_stage,
            ..plan::Pins::default()
        });
    }
    if gate.distro == safety::Distro::Arch
        && pins_lock.is_none()
        && let Some(machine) = machine
        && let Some(resolution) = pin::arch::reuse_completed(
            gate,
            machine,
            manifest_digest,
            host_source,
            parsed.host.snapshot.as_deref(),
            &declared,
        )
    {
        return Ok(plan::Pins {
            resolution,
            recorded,
            stale_stage,
            ..plan::Pins::default()
        });
    }
    let cx = pin::Context {
        distro: gate.distro,
        codename: match gate.distro {
            safety::Distro::Arch => "rolling",
            safety::Distro::Fedora => &gate.os.version_id,
            _ => &gate.os.codename,
        },
        arch: gate.arch(),
        sources: &parsed.sources,
        now: crate::util::now_utc(),
    };
    let mut fetcher = || -> Result<Box<dyn crate::fetch::Fetcher>, Diagnostic> {
        crate::fetch::HttpFetcher::from_env()
            .map(|fetcher| Box::new(fetcher) as Box<dyn crate::fetch::Fetcher>)
            .map_err(|error| Diagnostic::new("E_CONFIG", error))
    };
    let resolution = pin::resolve(
        &cx,
        parsed.host.snapshot.as_deref(),
        &declared,
        pins_lock,
        &mut fetcher,
    )?;
    Ok(plan::Pins {
        resolution,
        recorded,
        stale_stage,
        ..plan::Pins::default()
    })
}

fn plan_input(
    gate: safety::Gate,
    input: source::Input,
    parsed: manifest::HostManifest,
    options: &Options,
    pins_lock: Option<pin::PinsLock>,
    resolved: Option<&plan::Pins>,
) -> Result<Loaded, HostError> {
    gate.os.assert_distro(
        parsed.host.distro.as_deref(),
        &input.host.manifest_path().display().to_string(),
    )?;
    let digest = crate::util::sha256_tagged(&input.manifest);
    // The lock is judged like the manifest in place before `HostLock::read` opens it by name. Its
    // directory passed the arming step, so nobody but its owner can replace it in between. It is
    // machine-local, whichever directory the host is read from.
    safety::read_config(&gate.root, safety::LOCK, gate.system_root, gate.euid, 0)?;
    let existing = lock::HostLock::read(&gate.lock_path())?;
    let mut pins = match resolved {
        Some(pins) => {
            // The re-plan under the apply lock sees the machine again, including a stage an
            // interrupted apply may have left since; what the pins resolved to it does not ask.
            let mut pins = pins.clone();
            pins.stale_stage =
                fs::symlink_metadata(gate.root.join(pin::STAGE.trim_start_matches('/'))).is_ok();
            pins
        }
        None => resolve_pins(
            &gate,
            &parsed,
            pins_lock.as_ref(),
            existing.as_ref(),
            &digest,
            &input.host.source,
        )?,
    };
    // An unrecorded pin's line names the command that records it, for this host (LD-397).
    pins.record_with = pin::verbs::selection(options);
    let mut plan = plan::build_pinned(
        &gate,
        &parsed,
        &input.sources,
        &input.host.source,
        existing.as_ref(),
        &digest,
        &pins,
    )?;
    // A lock that still records a git source records the folder now, even when the machine
    // already is what the manifest says.
    if plan.is_noop() && existing.as_ref().is_some_and(|l| l.git.is_some()) {
        plan.record_changes = true;
    }
    Ok(Loaded {
        gate,
        manifest: parsed,
        lock: existing,
        plan,
        input,
        pins_lock,
        pins,
    })
}

/// The host's plan: everything an apply would do, and nothing else. Opens the manifest, reads
/// the machine, writes nothing anywhere. The host alone: `lodi switch` plans the home part.
pub fn plan(options: &Options) -> Result<String, HostError> {
    let loaded = load(options, Operation::Plan)?;
    let mut out = String::new();
    if let Some(record) = journal::outstanding(&loaded.gate)? {
        let classification = record.classify_in(&loaded.gate);
        out.push_str(&format!(
            "journal {} is outstanding: {}\n",
            record.id,
            describe(&classification)
        ));
    }
    out.push_str(&loaded.plan.render());
    Ok(out)
}

/// The host's apply: the plan, written down, then performed, reporting nothing.
pub fn apply(options: &Options) -> Result<String, HostError> {
    apply_with(options, None)
}

/// [`apply`], with the fetcher a keyring by URL is fetched through; `None` is the one `lodi`
/// itself uses, configured from the environment ([`crate::fetch::HttpFetcher::from_env`]) and
/// built only when a keyring has to be fetched.
pub fn apply_with(
    options: &Options,
    fetcher: Option<&dyn crate::fetch::Fetcher>,
) -> Result<String, HostError> {
    let first = load(options, Operation::Apply)?;
    apply_loaded_to(options, fetcher, first, &mut crate::progress::Silent)
}

/// `lodi switch`'s host part (#695): the host's apply of `loaded`, its steps reported to `sink`.
pub fn apply_for_switch(
    options: &Options,
    loaded: Loaded,
    sink: &mut dyn crate::progress::Sink,
) -> Result<String, HostError> {
    apply_loaded_to(options, None, loaded, sink)
}

fn apply_loaded_to(
    options: &Options,
    fetcher: Option<&dyn crate::fetch::Fetcher>,
    first: Loaded,
    sink: &mut dyn crate::progress::Sink,
) -> Result<String, HostError> {
    // Every refusal that can be decided by reading is decided before the apply lock creates the
    // host state directory. This is why unsupported packages, drift and bad ownership leave a
    // pristine root pristine.
    journal::require_clear(&first.gate, options.resolved.as_deref())?;
    refuse_drift(&first.plan, options.overwrite_drift)?;
    apply::preflight(&first.gate, &first.plan)?;
    // Every keyring by URL the plan could not place is fetched and checked only now — after
    // every refusal above, before anything is locked, journalled or written — and the plan is
    // made again from those bytes and checked again (LD-366).
    let first = fetched(options, first, fetcher)?;
    if first.plan.is_noop() && !first.plan.record_changes {
        // Nothing to do, so nothing is locked, journalled or written — but a declaration that
        // was left out, and a package kept for a reason, is still reported. This is the whole
        // of a converged apply's output.
        let mut out = String::new();
        for line in first.plan.warnings.iter().chain(&first.plan.kept) {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("nothing to do\n");
        return Ok(out);
    }

    let mut gate = first.gate;
    gate.lock_for_apply()?;

    // Plan again while holding the lock, from the same bytes (LD-379): another process may have
    // finished between the read-only pre-flight and our attempt to acquire it, but what this
    // apply acts on was read once and is not read again.
    let fresh = reload(
        options,
        Operation::Apply,
        &first.input,
        first.pins_lock.as_ref(),
        &first.pins,
    )?;
    let mut out = String::new();
    if let Some((record, classification)) =
        journal::require_clear(&gate, options.resolved.as_deref())?
    {
        out.push_str(&format!(
            "journal {} was {}\n",
            record.id,
            describe(&classification)
        ));
    }
    refuse_drift(&fresh.plan, options.overwrite_drift)?;
    apply::preflight(&gate, &fresh.plan)?;
    // The warnings come before the no-op report: a declaration that was left out is worth
    // saying even — especially — when there was then nothing else to do.
    for line in fresh.plan.warnings.iter().chain(&fresh.plan.kept) {
        out.push_str(line);
        out.push('\n');
    }
    if fresh.plan.is_noop() {
        // The machine is what the manifest says; the record may still not be (LD-375). It is
        // kept true, and `nothing to do` is said only when neither changes — or when all that
        // changes is the host the record names (LD-379), which is no change to the machine.
        if fresh.plan.record_changes {
            for line in apply::record_only(&gate, &fresh.plan, fresh.lock.as_ref())? {
                out.push_str(&line);
                out.push('\n');
            }
            if fresh.plan.source_only {
                out.push_str(&format!(
                    "nothing to do; the record now names {} as this machine's host\n",
                    fresh.plan.source
                ));
            } else {
                out.push_str("no machine changes; record updated\n");
            }
        } else {
            out.push_str("nothing to do\n");
        }
        return Ok(out);
    }

    out.push_str(&apply::run(
        &gate,
        &fresh.plan,
        options.resolved.as_deref(),
        fresh.lock.as_ref(),
        sink,
    )?);
    Ok(out)
}

/// `loaded` with every keyring by URL its plan needs fetched into its input and planned again;
/// `loaded` itself when there is none. A failure here has written nothing.
fn fetched(
    options: &Options,
    loaded: Loaded,
    fetcher: Option<&dyn crate::fetch::Fetcher>,
) -> Result<Loaded, HostError> {
    let wanted = sources::wanted(&loaded.plan);
    if wanted.is_empty() {
        return Ok(loaded);
    }
    let own;
    let fetcher = match fetcher {
        Some(fetcher) => fetcher,
        None => {
            own = crate::fetch::HttpFetcher::from_env()
                .map_err(|error| Diagnostic::new("E_CONFIG", error))?;
            &own
        }
    };
    let bytes = sources::fetch(&loaded.manifest, &wanted, fetcher)?;
    let mut input = loaded.input.clone();
    input.sources.extend(bytes);
    let again = reload(
        options,
        Operation::Apply,
        &input,
        loaded.pins_lock.as_ref(),
        &loaded.pins,
    )?;
    // A source whose keyring bytes are in the input is always planned file by file.
    assert!(
        sources::wanted(&again.plan).is_empty(),
        "a fetched keyring is planned from its bytes"
    );
    refuse_drift(&again.plan, options.overwrite_drift)?;
    apply::preflight(&again.gate, &again.plan)?;
    Ok(again)
}

pub fn refuse_drift(plan: &plan::Plan, overwrite: bool) -> Result<(), HostError> {
    let drifted = plan.drifted();
    if (drifted.is_empty() && plan.package_drift.is_empty() && plan.pin_drift.is_empty())
        || overwrite
    {
        return Ok(());
    }
    let mut diagnostics = Vec::new();
    // A hand edit of a declared source's keyring or stanza is `E_DRIFT` (S10); a `[files]`
    // entry's stays `E_DECLINED`, as it was in 1.2.
    let sources: Vec<&str> = plan
        .drifted_sources()
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    if !sources.is_empty() {
        diagnostics.push(
            Diagnostic::new(
                "E_DRIFT",
                format!(
                    "{} of a declared source was changed since lodi last wrote it; nothing was \
                     applied",
                    sources.join(", ")
                ),
            )
            .hint(
                "look at the change, then run lodi switch again with --overwrite-drift to put the \
                 declared keyring and stanza back, or fold the change into the manifest",
            ),
        );
    }
    let drifted: Vec<_> = drifted
        .into_iter()
        .filter(|file| !sources.contains(&file.path.as_str()))
        .collect();
    if !drifted.is_empty() {
        let paths: Vec<&str> = drifted.iter().map(|file| file.path.as_str()).collect();
        diagnostics.push(
            Diagnostic::new(
                "E_DECLINED",
                format!("{} was changed since lodi last wrote it", paths.join(", ")),
            )
            .hint(
                "run lodi switch again with --overwrite-drift to replace it, or fold the change \
                 into the manifest",
            ),
        );
    }
    if !plan.package_drift.is_empty() {
        // A package installed by hand is not taken off the machine on the strength of a
        // manifest that never saw it (LD-375).
        diagnostics.push(
            Diagnostic::new(
                "E_DECLINED",
                format!(
                    "package {} was installed by hand since lodi last recorded this machine, \
                     and host.toml does not declare it",
                    plan.package_drift.join(", ")
                ),
            )
            .hint(
                "declare it in host.toml to keep it, or run lodi switch again with \
                 --overwrite-drift to remove it",
            ),
        );
    }
    if !plan.pin_drift.is_empty() {
        // A pinned version moved outside lodi is drift, refused like any other (LD-375, P6).
        diagnostics.push(
            Diagnostic::new(
                "E_DECLINED",
                format!(
                    "package {} was moved outside lodi since it was installed at its pin",
                    plan.pin_drift.join(", ")
                ),
            )
            .hint(
                "run lodi switch again with --overwrite-drift to put the pinned version back, or \
                 change the pin in host.toml",
            ),
        );
    }
    Err(HostError {
        diagnostics,
        done: String::new(),
    })
}

pub fn describe(classification: &journal::Classification) -> String {
    match classification {
        journal::Classification::Completed => "complete".to_string(),
        journal::Classification::Incomplete { action, summary } => {
            format!("incomplete at {action} ({summary})")
        }
        journal::Classification::Ambiguous {
            action,
            summary,
            detail,
        } => format!("ambiguous at {action} ({summary}): {detail}"),
    }
}

/// `lodi import`'s record (2.0, LD-515): [`fresh_record`] of what it captured, over the record
/// there is, its captured files adopted where they stand, written as root before the config is,
/// so that the first switch after an edit of the host file applies it. `folder` is the host
/// file's folder as the person names it; the record names it as a switch from it does.
/// Returns a line for each original that could not be kept.
pub fn record_import(
    gate: &safety::Gate,
    machine: &import::Machine,
    capture: &import::files::Capture,
    folder: &Path,
) -> Result<Vec<String>, Diagnostic> {
    let source = record_source(&gate.root, folder);
    let selection = import::baseline::select(machine);
    let previous = lock::HostLock::read(&gate.lock_path())?;
    // An import into another folder leaves the record of the one switched from while that one
    // still has its host file: else what a switch installed and the import cannot declare (a
    // vendor's package) or does not (a group) is forgotten, and leaves no more (LD-530).
    if let Some(other) = previous.as_ref().and_then(|lock| lock.source.as_deref())
        && other != source
        && gate
            .root
            .join(other.trim_start_matches('/'))
            .join("host.toml")
            .is_file()
    {
        return Ok(Vec::new());
    }
    let base = previous
        .as_ref()
        .and_then(|lock| lock.base_for(&source))
        .map(|base| base.files.clone());
    let recorded: Vec<String> = previous
        .iter()
        .flat_map(|lock| lock.files.keys().cloned())
        .collect();
    let (mut record, adopted) =
        fresh_record(gate, machine, &selection, Some(capture), previous, &source);
    // A file the record names keeps its place in the base, at its digest on the machine now, as
    // 1.x's reconcile moved it (LD-519): else the next import has no base for a declared file.
    if let Some(baseline) = &mut record.baseline {
        let mut files = base.unwrap_or_default();
        for path in recorded {
            let seen = files::inspect(&gate.root, &path, true).ok();
            if let Some(digest) = seen.and_then(|_| plan::Facts::observe(&gate.root, &path).digest)
            {
                files.insert(path, digest);
            }
        }
        files.append(&mut baseline.files);
        baseline.files = files;
    }
    let lines = originals::adopt(&gate.root, &mut record.files, &adopted);
    apply::materialise_lock(gate, &record)?;
    Ok(lines)
}

/// The folder of a host file as a record names it (`lock.source`): itself on `/`, else its path
/// inside the root `root`.
pub fn record_source(root: &Path, folder: &Path) -> String {
    if root == Path::new("/") {
        folder.display().to_string()
    } else {
        format!("/{}", folder.strip_prefix(root).unwrap_or(folder).display())
    }
}

/// The record a fresh import writes: what it declared and the machine has, with its holds —
/// the baseline an exact apply fences hand-made changes against, so that an edit of the manifest
/// never needs --overwrite-drift. Nothing else is recorded — not the base system, and not a
/// package the file names as not captured.
///
/// Returned with it: the files it records for the first time, which the caller adopts into the
/// store with the record (LD-387).
fn fresh_record(
    gate: &safety::Gate,
    machine: &import::Machine,
    selection: &import::baseline::Selection,
    capture: Option<&import::files::Capture>,
    previous: Option<lock::HostLock>,
    source: &str,
) -> (lock::HostLock, Vec<originals::Planned>) {
    let names: std::collections::BTreeSet<String> = selection.common.iter().cloned().collect();
    let mut record = lock::HostLock::new(
        crate::util::format_utc(crate::util::now_utc()),
        gate.distro.name().to_string(),
        gate.os.version_id.clone(),
    );
    record.packages = plan::records(&names, &machine.observed, &selection.hold, &[]);
    let mut kept_basics = std::collections::BTreeMap::new();
    if let Some(previous) = previous {
        record.files = previous.files;
        // The services the last apply declared stay recorded: an import declares none (sc-1).
        record.set_services(previous.services);
        // What the basics were stays known (sd-1).
        kept_basics = previous.basics;
    }
    // The `[system]` basics the import declares are recorded as the machine has them, as a
    // converged apply records them, so its plan is `nothing to do` (#585).
    let declared = machine
        .system
        .iter()
        .map(|(key, value)| (*key, Some(value)));
    record.set_basics(basics::record_as_read(
        declared,
        &machine.system,
        &kept_basics,
    ));
    // Each captured file is declared with the bytes, mode and owner it has, so it is adopted
    // where it stands: recorded with no copy and `keep` here, and its original then kept in the
    // store by the caller, which records it with `restore` (LD-387). A record an apply already
    // wrote wins, and is given no copy.
    let mut adopted = Vec::new();
    for file in capture.iter().flat_map(|capture| &capture.captured) {
        if !record.files.contains_key(&file.path)
            && let Some(planned) = originals::Planned::captured(&gate.root, file)
        {
            adopted.push(planned);
        }
        record
            .files
            .entry(file.path.clone())
            .or_insert_with(|| lock::FileRecord {
                digest: format!("sha256:{}", crate::util::sha256_hex(&file.bytes)),
                mode: format!("{:04o}", file.mode),
                owner: file.owner.clone(),
                group: file.group.clone(),
                backup: None,
                on_remove: "keep".to_string(),
            });
    }
    // The base a later reconcile merges from (LD-378): the machine as this import read it.
    let holds = if pm::Shape::of(gate.distro).hold_is_machine_state {
        selection
            .hold
            .iter()
            .filter_map(|name| {
                machine
                    .observed
                    .installed
                    .get(name)
                    .map(|version| (name.clone(), version.clone()))
            })
            .collect()
    } else {
        std::collections::BTreeMap::new()
    };
    record.baseline = Some(lock::Baseline {
        explicit: names,
        holds,
        files: capture
            .iter()
            .flat_map(|capture| &capture.captured)
            .map(|file| {
                (
                    file.path.clone(),
                    format!("sha256:{}", crate::util::sha256_hex(&file.bytes)),
                )
            })
            .collect(),
    });
    record.source = Some(source.to_string());
    // The accounts the import declares are the ones the record manages (su-1).
    record.set_identities(
        machine.users.iter().map(|user| user.name.clone()).collect(),
        std::collections::BTreeSet::new(),
    );
    (record, adopted)
}

/// An absolute path for something that may not exist yet: the deepest ancestor that does exist
/// is canonicalized, and the rest is appended to it unchanged.
pub fn resolve_for_write(path: &Path) -> std::path::PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = absolute.as_path();
    loop {
        if let Ok(real) = fs::canonicalize(probe) {
            let mut out = real;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        let Some(parent) = probe.parent() else {
            return absolute;
        };
        match probe.file_name() {
            Some(name) => tail.push(name.to_os_string()),
            None => return absolute,
        }
        probe = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn the_guard_refuses_only_a_rootless_command_under_the_exact_setting() {
        for command in ["switch", "import", "update", "pin"] {
            let refused =
                require_root_for(command, None, Some(OsStr::new("1"))).expect_err(command);
            assert_eq!(refused.code, "E_HOST_ROOT_REQUIRED");
            assert_eq!(exit_status(refused.code), 9);
            assert!(refused.message.contains(&format!("`lodi {command}`")));
            assert!(refused.message.contains("nothing was read"));
            let root = Path::new("/srv/scratch");
            assert_eq!(
                require_root_for(command, Some(root), Some(OsStr::new("1"))),
                Ok(())
            );
            for other in ["", "0", "true", "yes", " 1", "1 ", "01"] {
                assert_eq!(
                    require_root_for(command, None, Some(OsStr::new(other))),
                    Ok(())
                );
            }
            assert_eq!(require_root_for(command, None, None), Ok(()));
        }
    }
}
