//! The host scope: `/etc/lodi/host.toml`, the distro's packages and root-owned files, applied in
//! **one planned, journalled transaction** (`spec/01` §5, ADR-013, OD-15).
//!
//! `lodi host plan`, `apply`, `import` and `arm` reach this module, and so do M-Pin's `versions`,
//! `pin` and `unpin` ([`pin::verbs`], LD-397). Root-owned files and distribution packages both
//! work end to end, on every distribution the safety gate accepts.
//!
//! # The safety rule this module makes structural
//!
//! The host scope is never run against the development machine (`AGENTS.md` §8, LD-45). Nothing
//! here relies on anyone remembering that:
//!
//! - [`safety::Gate::open`] refuses **before it opens anything at all** unless the root carries
//!   the marker `etc/lodi/host-allowed` — `E_HOST_NOT_ARMED`, exit 9 (design call D1, LD-113).
//!   The one command that writes that marker is `lodi host arm` ([`arm`]), run deliberately as
//!   root; nothing else writes it (LD-320).
//! - `--root DIR` is a real, documented flag, not a test seam: every test runs against a scratch
//!   directory it owns, with the same arming rule applied to it (D2, LD-114).
//! - The package managers are found on `PATH` by bare name and run as argv vectors in a fixed
//!   non-interactive environment, so a test's own shims are the whole seam (D3, LD-115). There is
//!   no test-only branch, environment variable or hidden flag anywhere in these paths.
//! - Lodi never invokes `sudo` or `doas`. A root-owned action without the privilege for it is
//!   `E_NEED_ROOT`, and the operator runs `sudo lodi host apply` themselves.
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
//! input this build does not implement, such as a version-qualified package name (OD-15) or
//! `--unsupported-partial-upgrade` on a family with no such mode. There are no generations, no
//! `rollback` and no command that claims to undo an apply; an interrupted apply is classified,
//! and an ambiguous one stops for a human (see [`journal`]).

pub mod apply;
pub mod arm;
pub mod basics;
pub mod boot;
pub mod converge;
pub mod files;
pub mod firewall;
pub mod flake;
pub mod homepart;
pub mod import;
pub mod journal;
pub mod kernel;
pub mod landing;
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
pub mod trial;
pub mod users;

use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use crate::diag::{Diagnostic, EXIT_MANIFEST, exit_status};
use crate::manifest::ManifestErrors;

pub use safety::{Operation, Options};

/// The environment variable that makes `--root` mandatory for every host verb (LD-376). Only the
/// exact value `1` sets it; any other value, and its absence, change nothing.
pub const REQUIRE_ROOT_VAR: &str = "LODI_HOST_REQUIRE_ROOT";

/// The LD-376 guard, as a function of its inputs alone: `verb` is what the operator typed after
/// `lodi host`, `root` its `--root`, and `setting` the value of [`REQUIRE_ROOT_VAR`]. It reads no
/// file and no environment, so a refusal happens before the verb has looked at anything.
pub fn require_root(
    verb: &str,
    root: Option<&Path>,
    setting: Option<&std::ffi::OsStr>,
) -> Result<(), Diagnostic> {
    require_root_for(&format!("lodi host {verb}"), root, setting)
}

/// [`require_root`] for any command that reaches the host scope, named as typed: the host
/// verbs, and the top-level `lodi plan`, `apply`, `import` and `update` (F-22, LD-416), whether
/// or not a `SOURCE` was given.
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
            "{REQUIRE_ROOT_VAR}=1 is set, so `{command}` needs --root DIR and was given none; \
             nothing was read"
        ),
    )
    .hint(format!(
        "this environment never lets a host verb reach this machine: name a scratch root with \
         --root DIR, or run it where {REQUIRE_ROOT_VAR} is not set"
    )))
}

/// [`require_root`] with the process's own environment: what `main` calls first for every host
/// verb, before the verb itself runs.
pub fn guard(verb: &str, options: &Options) -> Result<(), HostError> {
    require_root(
        verb,
        options.root.as_deref(),
        std::env::var_os(REQUIRE_ROOT_VAR).as_deref(),
    )
    .map_err(HostError::from)
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
    /// The host directory's `pins.lock`, read once with the manifest (M-Pin, P13).
    pub pins_lock: Option<pin::PinsLock>,
    /// What the first plan resolved of pins, which a re-plan reuses and never resolves again.
    pub pins: plan::Pins,
    /// The source line of a host read from a git URL, printed first (LD-401).
    pub source_line: Option<String>,
}

/// Open the gate, choose and judge the host, and read what the plan acts on — the manifest and
/// every `source` it declares — once (LD-379).
fn load(options: &Options, operation: Operation) -> Result<Loaded, HostError> {
    // A URL's grammar is decided before anything is read or asked (LD-401, U1).
    let url = match &options.source {
        Some(named) => remote::parse(named.as_os_str())?,
        None => None,
    };
    let gate = safety::Gate::open(options, operation)?;
    let (host, git, source_line) = match &url {
        Some(url) => {
            safety::read_config(&gate.root, safety::LOCK, gate.system_root, gate.euid, 0)?;
            let record = lock::HostLock::read(&gate.lock_path())?;
            let fetched = remote::load(&gate, options, url, record.as_ref())?;
            (fetched.host, Some(fetched.git), Some(fetched.line))
        }
        None => (
            source::select(
                &gate,
                options,
                &privilege::owners(&privilege::System),
                false,
            )?,
            None,
            None,
        ),
    };
    let manifest_path = host.manifest_path();
    let shown = manifest_path.display().to_string();
    let bytes = source::read_manifest(&gate, &host)?.ok_or_else(|| {
        let hint = if host.in_place {
            manifest::import_hint(&gate.root, gate.system_root, &shown)
        } else {
            format!(
                "write one there with `lodi host import {}`, or by hand",
                host.named.as_deref().unwrap_or(&host.dir).display()
            )
        };
        Diagnostic::new("E_NO_MANIFEST", format!("no host manifest at {shown}")).hint(hint)
    })?;
    let parsed = manifest::from_bytes(&bytes, &shown, &manifest::Context::from_gate(&gate))?;
    let mut sources = source::read_sources(&host, &parsed)?;
    // A declared source's keyring is read once with them, from the same host (LD-365).
    sources::read_keyrings(&host, &parsed, &mut sources)?;
    // A keyring by URL already at its managed path with its digest is read here, so that
    // neither the plan nor the apply asks the network for it (LD-366).
    sources::read_installed(&gate, &parsed, &mut sources);
    // The host directory's pin lock is read once under the manifest's W3 trust rule (P13): its
    // section of the repository's root lock, else its own 1.4 `pins.lock` (LD-416).
    let read = crate::flakelock::read_host(&host, pin::read_lock(&host)?)?;
    check_locked_keys(&parsed, &read.keys)?;
    let pins_lock = read.pins;
    let input = source::Input {
        host,
        manifest: bytes,
        sources,
        git,
    };
    let mut loaded = plan_input(gate, input, parsed, options, pins_lock, None)?;
    loaded.source_line = source_line;
    Ok(loaded)
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
    pins.record_with = (
        pin::verbs::sudo_for(gate.system_root, input.host.in_place).to_string(),
        pin::verbs::selection(options),
    );
    let mut plan = plan::build_pinned(
        &gate,
        &parsed,
        &input.sources,
        &input.host.source,
        existing.as_ref(),
        &digest,
        options.no_update,
        &pins,
    )?;
    // The git revision is part of the record: a new one is recorded even when the machine
    // already is what the manifest says (LD-401).
    plan.git = input.git.clone();
    if plan.is_noop() && existing.as_ref().and_then(|l| l.git.as_ref()) != plan.git.as_ref() {
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
        source_line: None,
    })
}

/// The fence of LD-379: the record names the host the last apply or import read, and a bare
/// apply — which reads `<root>/etc/lodi` — stops when that was another directory, so that an old
/// manifest left in `/etc/lodi` cannot quietly take the machine back.
fn fence(host: &source::Host, record: Option<&lock::HostLock>) -> Option<Diagnostic> {
    let named = record?.source.as_deref()?;
    if !host.in_place || named == lock::IN_PLACE_SOURCE {
        return None;
    }
    Some(
        Diagnostic::new(
            "E_DECLINED",
            format!(
                "the last apply or import on this machine read the host at {named}, and a bare \
                 apply reads {}",
                host.manifest_path().display()
            ),
        )
        .hint(format!(
            "apply that host by naming it: sudo lodi host apply {named}; to apply {} instead, \
             name it the same way: sudo lodi host apply {}",
            host.dir.display(),
            lock::IN_PLACE_SOURCE
        )),
    )
}

/// `lodi host plan`: everything the apply would do, and nothing else. Opens the manifest, reads
/// the machine, writes nothing anywhere.
pub fn plan(options: &Options) -> Result<String, HostError> {
    plan_with_privilege(options, &privilege::System)
}

/// [`plan`] through a [`privilege::Privilege`]: the home part is planned as the user, after the
/// same permanent drop an apply makes, so root never reads the user's home (LD-399).
pub fn plan_with_privilege(
    options: &Options,
    privilege: &dyn privilege::Privilege,
) -> Result<String, HostError> {
    let loaded = load(options, Operation::Plan)?;
    let home = if options.no_home {
        None
    } else {
        homepart::select(&loaded.gate, &loaded.input.host, privilege)?
    };
    let mut out = String::new();
    if let Some(line) = &loaded.source_line {
        out.push_str(line);
        out.push('\n');
    }
    if let Some(fenced) = fence(&loaded.input.host, loaded.lock.as_ref()) {
        out.push_str(&format!("a bare apply would refuse: {}\n", fenced.message));
    }
    if let Some(record) = journal::outstanding(&loaded.gate)? {
        let classification = record.classify_in(&loaded.gate);
        out.push_str(&format!(
            "journal {} is outstanding: {}\n",
            record.id,
            describe(&classification)
        ));
    }
    out.push_str(&loaded.plan.render());
    if options.no_home {
        out.push_str("home skipped (--no-home)\n");
    }
    if let Some(home) = home {
        homepart::drop_to_user(privilege, &home.roots)?;
        let _ = writeln!(out, "home {}", home.name);
        let context = crate::home::services::Context {
            fixed_path: loaded.gate.system_root,
            ..Default::default()
        };
        let plan = crate::home::plan::plan_for(&home.roots, &home.manifest, false, &context)?;
        for line in plan.lines() {
            let _ = writeln!(out, "{line}");
        }
    }
    Ok(out)
}

/// `lodi host apply`: the plan, written down, then performed.
pub fn apply(options: &Options) -> Result<String, HostError> {
    let mut out = apply_with(options, None)?;
    if options.no_home {
        out.push_str("home skipped (--no-home)\n");
    }
    Ok(out)
}

/// [`apply`], with the fetcher a keyring by URL is fetched through; `None` is the one `lodi`
/// itself uses, configured from the environment ([`crate::fetch::HttpFetcher::from_env`]) and
/// built only when a keyring has to be fetched.
pub fn apply_with(
    options: &Options,
    fetcher: Option<&dyn crate::fetch::Fetcher>,
) -> Result<String, HostError> {
    apply_with_privilege(options, fetcher, &privilege::System)
}

pub fn apply_with_privilege(
    options: &Options,
    fetcher: Option<&dyn crate::fetch::Fetcher>,
    privilege: &dyn privilege::Privilege,
) -> Result<String, HostError> {
    if options.no_home {
        return apply_host_with(options, fetcher);
    }
    // A URL's tree is fetched once and is the source of both parts; it is never written, so its
    // home applies only from the lock it carries, checked before the host changes (LD-401, U7).
    let url = match &options.source {
        Some(named) => remote::parse(named.as_os_str())?,
        None => None,
    };
    let (home, mut out, system_root) = match &url {
        Some(_) => {
            let first = load(options, Operation::Apply)?;
            let home = homepart::select(&first.gate, &first.input.host, privilege)?;
            if let Some(home) = &home {
                homepart::require_lock(home)?;
            }
            let system_root = first.gate.system_root;
            (
                home,
                apply_host_loaded(options, fetcher, first)?,
                system_root,
            )
        }
        None => {
            let gate = safety::Gate::open(options, Operation::Apply)?;
            let host = source::select(&gate, options, &privilege::owners(privilege), false)?;
            let home = homepart::select(&gate, &host, privilege)?;
            (home, apply_host_with(options, fetcher)?, gate.system_root)
        }
    };
    let Some(home) = home else {
        return Ok(out);
    };
    homepart::drop_to_user(privilege, &home.roots)?;
    let default_fetcher = crate::fetch::HttpFetcher::from_env()
        .map_err(|error| Diagnostic::new("E_CONFIG", error))?;
    let fetcher = fetcher.unwrap_or(&default_fetcher);
    // The home state names the URL, as the host's record does, and never the cached tree.
    let (source, locked) = match &url {
        Some(url) => (url.canonical.clone(), true),
        None => (home.roots.config().display().to_string(), false),
    };
    // A fetched tree is never written: its home's lock is read, never resolved (F-15).
    let target = if url.is_some() {
        None
    } else {
        home.target.as_ref()
    };
    let result = crate::home::apply::apply_preloaded_to(
        &home.roots,
        &home.manifest,
        home.lock,
        target,
        &source,
        &crate::home::apply::Options {
            locked,
            services: crate::home::services::Context {
                fixed_path: system_root,
                ..Default::default()
            },
            ..crate::home::apply::Options::default()
        },
        fetcher,
        crate::util::now_utc(),
    );
    match result {
        Ok(applied) => {
            let _ = writeln!(out, "home {}", home.name);
            for line in applied.lines {
                let _ = writeln!(out, "{line}");
            }
            Ok(out)
        }
        Err(failure) => {
            let prefix = format!("lodi: error {}: ", failure.code);
            let text = failure.text.strip_prefix(&prefix).unwrap_or(&failure.text);
            let mut diagnostic = Diagnostic::new(
                failure.code,
                format!("host applied; home not applied: {text}"),
            );
            diagnostic.notes.extend(failure.warnings);
            Err(HostError {
                diagnostics: vec![diagnostic],
                done: out,
            })
        }
    }
}

fn apply_host_with(
    options: &Options,
    fetcher: Option<&dyn crate::fetch::Fetcher>,
) -> Result<String, HostError> {
    apply_host_loaded(options, fetcher, load(options, Operation::Apply)?)
}

fn apply_host_loaded(
    options: &Options,
    fetcher: Option<&dyn crate::fetch::Fetcher>,
    first: Loaded,
) -> Result<String, HostError> {
    let (line, git, root) = (
        first.source_line.clone(),
        first.input.git.clone(),
        first.gate.root.clone(),
    );
    let out = apply_loaded(options, fetcher, first)?;
    // The lock now names this revision, so every other cached tree goes (F-11).
    if let Some(git) = &git {
        remote::prune(&root, git);
    }
    Ok(match line {
        Some(line) => format!("{line}\n{out}"),
        None => out,
    })
}

fn apply_loaded(
    options: &Options,
    fetcher: Option<&dyn crate::fetch::Fetcher>,
    first: Loaded,
) -> Result<String, HostError> {
    if let Some(fenced) = fence(&first.input.host, first.lock.as_ref()) {
        return Err(fenced.into());
    }
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
    if let Some(fenced) = fence(&fresh.input.host, fresh.lock.as_ref()) {
        return Err(fenced.into());
    }
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

fn refuse_drift(plan: &plan::Plan, overwrite: bool) -> Result<(), HostError> {
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
                "look at the change, then run the apply again with --overwrite-drift to put the \
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
                "run the apply again with --overwrite-drift to replace it, or fold the change into the manifest",
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
                "declare it in host.toml to keep it, or run the apply again with \
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
                "run the apply again with --overwrite-drift to put the pinned version back, or \
                 change the pin in host.toml",
            ),
        );
    }
    Err(HostError {
        diagnostics,
        done: String::new(),
    })
}

fn describe(classification: &journal::Classification) -> String {
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

/// `lodi host import`: read the machine the gate opened and write the manifest a fresh install
/// could apply (M-Import T-3). It is the command half of [`import`], which landed as a library
/// with nothing reaching it.
///
/// Below the root it opens files for reading only — the arming marker, `etc/os-release`, the
/// package manager's own answers and the configuration files that manager reports as changed —
/// takes no apply lock and never writes the arming marker. What it writes depends on the flags
/// (the owner's decision of 2026-09-22, LD-325, which supersedes forward LD-291's
/// standard-output default):
///
/// - **no flag**: the import lands in place, `<root>/etc/lodi/host.toml` at 0644 and the
///   captured bytes under `<root>/etc/lodi/files/`, where `lodi host plan` reads them
///   ([`landing`]). On `/` that needs root (`E_NEED_ROOT`); nothing is applied — the apply
///   stays a separate, deliberate `sudo lodi host apply`;
/// - **`--out DIR`**: the same bundle under a directory of the operator's, which needs no
///   privilege;
/// - **`--stdout`**: the manifest on standard output and no configuration file captured at all,
///   because a declaration whose `source` points at a bundle nobody wrote would be a manifest
///   that cannot apply.
///
/// What it leaves out, it says. Every chosen package whose **installed version** did not come
/// from the distribution's own repositories — from a third-party repository or a PPA, from a
/// package file installed by hand or a source the machine no longer has, or from no repository
/// the package manager knows — is named under its class in the emitted `NOT CAPTURED` block and
/// gets one `W_UNCAPTURED` line on standard error, in the same order. A warning never changes
/// the exit status: an import that declares nothing at all still exits 0.
///
/// Every refusal a destination can carry is decided **before the machine is read**, so an
/// import that is going to be refused reads nothing: for `--out`, a directory the host scope
/// itself owns (`<root>/etc/lodi`, `<root>/var/lib/lodi`) is `E_STORE_IO`; for the default, the
/// privilege and the symbolic links of [`landing::in_place`]. An existing `host.toml` without
/// `--force` is **reconciled** with the machine ([`reconcile`], LD-378, which supersedes the
/// `E_EXISTS` that case was until then); `--force` replaces it, and `--dry-run` shows the change
/// and writes nothing.
pub fn run_import(options: &Options) -> Result<String, HostError> {
    run_import_with(options, &privilege::System)
}

/// [`run_import`] with the privilege it runs with ([`privilege::System`] for the command).
///
/// With a positional `SOURCE` (LD-379) the import writes the host directory the source selects
/// ([`source::select`]), creating `SOURCE/<hostname>/` when it is not there yet, as the owner of
/// `SOURCE`: under `sudo` it reads the machine as root, writes the baseline into the lock, and
/// then becomes that owner ([`privilege::writer`]) before it writes anything under `SOURCE`, so
/// the files are born the user's. Unprivileged, it writes the host directory and no record — the
/// record is root's — and says so; the next apply writes it.
pub fn run_import_with(
    options: &Options,
    privilege: &dyn privilege::Privilege,
) -> Result<String, HostError> {
    let mut gate = safety::Gate::open(options, Operation::Import)?;
    let host = match &options.source {
        Some(_) => Some(source::select(
            &gate,
            options,
            &privilege::owners(privilege),
            true,
        )?),
        None => None,
    };
    let home_user = if host.is_some() && !options.dry_run && !options.no_home {
        Some(homepart::selected_user(&gate, privilege)?)
    } else {
        None
    };
    let out = match (options.stdout, options.out.as_deref(), &host) {
        (true, _, _) => None,
        (false, Some(dir), _) => Some(import_destination(&gate, dir)?),
        (false, None, Some(host)) => Some(host.dir.clone()),
        (false, None, None) => Some(landing::in_place(&gate, options.force, options.dry_run)?),
    };
    let in_place = !options.stdout && options.out.is_none() && host.is_none();
    // Who writes the record: the import in place always (LD-375), and an import into a host
    // directory only as root, because the record is root's (LD-379).
    let privileged = privilege.euid() == 0;
    let writes_record = in_place || (host.is_some() && privileged);
    // The import that writes the record keeps what an earlier apply recorded of files, and a
    // reconcile into a host directory merges from the record's baseline. A record this build
    // cannot read stops the import here, before the machine is read, as it stops a plan.
    let previous = if in_place || host.is_some() {
        safety::read_config(&gate.root, safety::LOCK, gate.system_root, gate.euid, 0)?;
        lock::HostLock::read(&gate.lock_path())?
    } else {
        None
    };
    // Whom the bytes under a host directory are written as: its owner, become after the record
    // is written and before the first byte under it (LD-379).
    let writer = match &host {
        Some(host) => {
            use std::os::unix::fs::MetadataExt;
            let named = host.named.as_deref().unwrap_or(&host.dir);
            let meta = fs::symlink_metadata(named).map_err(|e| store_io(named, &e.to_string()))?;
            privilege::writer(privilege, meta.uid(), meta.gid())
        }
        None => None,
    };
    let become_writer = |gate: &mut safety::Gate| -> Result<(), HostError> {
        if let Some((uid, gid)) = writer {
            privilege.become_user(uid, gid)?;
            gate.euid = uid;
            gate.egid = gid;
        }
        Ok(())
    };
    // An existing manifest is reconciled with the machine, never replaced without --force and
    // never refused (LD-378, which supersedes that case's E_EXISTS).
    if let Some(dir) = &out
        && !options.force
        && dir.join(landing::MANIFEST).symlink_metadata().is_ok()
    {
        let source = match &host {
            Some(host) => host.source.clone(),
            None if in_place => lock::IN_PLACE_SOURCE.to_string(),
            None => dir.display().to_string(),
        };
        let mut report = reconcile::run(
            &mut gate,
            options,
            reconcile::Target {
                dir,
                source: &source,
                in_place,
                writes_record,
                host: host.as_ref(),
                previous,
                become_writer: &become_writer,
            },
        )?;
        if let Some(user) = &home_user {
            if let Some((uid, gid)) = writer
                && gate.euid != uid
            {
                privilege.become_user(uid, gid)?;
                gate.euid = uid;
                gate.egid = gid;
            }
            write_import_home(&gate, dir, user, &mut report)?;
        }
        return Ok(report);
    }

    let machine = import::read(&gate)?;
    // A capture is taken only when there is somewhere for its bytes to travel to.
    let capture = match out {
        Some(_) => Some(import::files::capture(&gate)?),
        None => None,
    };
    let text = import::emit::manifest(&machine, &import::Snapshot::now(), capture.as_ref());

    // The same selection the emitter wrote the file from, so that what standard error says and
    // what the block names can never be two different answers. `select` is a pure function of
    // readings already in memory: it runs no command and reads no file.
    let selection = import::baseline::select(&machine);
    let mut report = String::new();
    // Packages first, in the order the emitted block names them, then the configuration files:
    // the file is written top to bottom and so is what is said about it.
    for warning in selection.warnings(machine.distro) {
        report.push_str(&warning);
        report.push('\n');
    }
    if let Some(capture) = &capture {
        for warning in capture.warnings() {
            report.push_str(&warning);
            report.push('\n');
        }
    }
    let Some(dir) = out else {
        report.push_str(&text);
        return Ok(report);
    };
    if options.dry_run {
        // What would be written, as a diff of what is there now, and nothing written.
        let path = dir.join(landing::MANIFEST);
        let before = fs::read_to_string(&path).unwrap_or_default();
        report.push_str(&reconcile::diff::unified(
            &path.display().to_string(),
            &before,
            &text,
        ));
        for file in capture.iter().flat_map(|capture| &capture.captured) {
            let _ = writeln!(
                report,
                "captured: {} sha256:{} (mode {:04o}, {}:{})",
                file.path,
                crate::util::sha256_hex(&file.bytes),
                file.mode,
                file.owner,
                file.group
            );
        }
        let _ = writeln!(
            report,
            "would write {}: {} package(s) declared; nothing was written (--dry-run)",
            path.display(),
            selection.common.len()
        );
        return Ok(report);
    }
    let mut record = writes_record.then(|| {
        fresh_record(
            &gate,
            &machine,
            &selection,
            capture.as_ref(),
            previous,
            host.as_ref()
                .map_or(lock::IN_PLACE_SOURCE, |host| host.source.as_str()),
        )
    });
    if host.is_some() {
        // Into a host directory: the record first, as root, then the owner of the directory is
        // who writes every byte under it (LD-379). The adoption copies are root's too, taken
        // with the record and before the privilege drop (LD-387).
        if let Some((record, adopted)) = &mut record {
            for line in originals::adopt(&gate.root, &mut record.files, adopted) {
                let _ = writeln!(report, "{line}");
            }
            apply::materialise_lock(&gate, record)?;
        }
        become_writer(&mut gate)?;
    }
    if options.out.is_some() || host.is_some() {
        // Born 0755, never at the umask's mode: the manifest below is written only into a
        // directory nobody but its owner can write (LD-333), and under Ubuntu's umask 002 a
        // directory made the default way was refused by the import that made it (LD-343).
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(&dir)
            .map_err(|e| store_io(&dir, &e.to_string()))?;
    }
    // The keyrings the commented [sources] blocks name travel with the captured files, so that
    // they land where the capture lands and `--force` keeps them (LD-367).
    let bundle = capture
        .as_ref()
        .map(|capture| machine.repositories.bundle(capture));
    let manifest_path = landing::write(&gate, &dir, &text, bundle.as_ref(), options.force)?;
    if let Some(user) = &home_user {
        write_import_home(&gate, &dir, user, &mut report)?;
    }
    if in_place && let Some((record, adopted)) = &mut record {
        // Each captured file is adopted where it stands, and its original kept in the store
        // below /etc/lodi before the record names it (LD-387).
        for line in originals::adopt(&gate.root, &mut record.files, adopted) {
            let _ = writeln!(report, "{line}");
        }
        apply::materialise_lock(&gate, record)?;
    }
    if options.out.is_some() {
        let _ = writeln!(
            report,
            "wrote {}: {} package(s) declared, {} not captured; no file copied from /etc",
            manifest_path.display(),
            selection.common.len(),
            selection.not_captured()
        );
    } else {
        // What landed and that nothing else changed, what to read before going on, and the
        // command that reads it, which writes nothing (LD-380).
        let named = host
            .as_ref()
            .and_then(|host| host.named.as_deref())
            .map(|named| format!(" {}", named.display()))
            .unwrap_or_default();
        let next = if gate.system_root {
            format!("sudo lodi host plan{named}")
        } else {
            format!("lodi host plan{named} --root {}", gate.root.display())
        };
        let _ = writeln!(
            report,
            "wrote {}: {} package(s) declared, {} not captured, no file copied from /etc; no \
             package and no file outside {} changed",
            manifest_path.display(),
            selection.common.len(),
            selection.not_captured(),
            dir.display()
        );
        if host.is_some() && record.is_none() {
            let _ = writeln!(
                report,
                "wrote no record: the record {} is root's; the next \
                 `sudo lodi host apply{named}` writes it",
                gate.lock_path().display()
            );
        }
        let _ = writeln!(
            report,
            "read it first: NOT CAPTURED lists what was left out and how to declare it"
        );
        let _ = writeln!(report, "next: {next} (writes nothing)");
    }
    Ok(report)
}

fn write_import_home(
    gate: &safety::Gate,
    dir: &Path,
    user: &crate::passwd::User,
    report: &mut String,
) -> Result<(), HostError> {
    let roots = crate::roots::Roots::for_host(
        gate.root.join(user.home.trim_start_matches('/')),
        dir.to_path_buf(),
    );
    let path = dir
        .join("home")
        .join(&user.name)
        .join(crate::home::import::MANIFEST);
    let made = crate::home::import::stub_for_host(&roots, &user.name)?;
    let action = if made { "wrote" } else { "kept" };
    let _ = writeln!(report, "{action} {}", path.display());
    Ok(())
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

/// Where an import's bytes may go, decided before anything is read.
///
/// `--out` is resolved against the working directory and through the symbolic links of the
/// ancestors that exist, so that neither a relative path nor a link can point the bundle into a
/// directory the host scope owns without this seeing it.
fn import_destination(
    gate: &safety::Gate,
    requested: &Path,
) -> Result<std::path::PathBuf, HostError> {
    let resolved = resolve_for_write(requested);
    for owned in [safety::MANIFEST, safety::STATE] {
        // `etc/lodi/host.toml` and `var/lib/lodi/host` name a file and a directory inside the two
        // directories the host scope owns; the parents are what an import may not write into.
        let Some(dir) = gate.root.join(owned).parent().map(Path::to_path_buf) else {
            continue;
        };
        let dir = resolve_for_write(&dir);
        if resolved == dir || resolved.starts_with(&dir) {
            return Err(Diagnostic::new(
                "E_STORE_IO",
                format!(
                    "--out {} is inside {}, which the host scope owns",
                    requested.display(),
                    dir.display()
                ),
            )
            .hint(format!(
                "drop --out: with no flag, lodi host import itself writes {} and the files/ \
                 beside it; --out names a directory of your own",
                gate.root.join(safety::MANIFEST).display()
            ))
            .into());
        }
    }
    Ok(resolved)
}

/// An absolute path for something that may not exist yet: the deepest ancestor that does exist
/// is canonicalized, and the rest is appended to it unchanged.
fn resolve_for_write(path: &Path) -> std::path::PathBuf {
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

fn store_io(path: &Path, detail: &str) -> HostError {
    Diagnostic::new("E_STORE_IO", format!("{}: {detail}", path.display())).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn the_guard_refuses_only_a_rootless_verb_under_the_exact_setting() {
        for verb in ["plan", "apply", "import", "arm"] {
            let refused = require_root(verb, None, Some(OsStr::new("1"))).expect_err(verb);
            assert_eq!(refused.code, "E_HOST_ROOT_REQUIRED");
            assert_eq!(exit_status(refused.code), 9);
            assert!(refused.message.contains(&format!("`lodi host {verb}`")));
            assert!(refused.message.contains("nothing was read"));
            let root = Path::new("/srv/scratch");
            assert_eq!(
                require_root(verb, Some(root), Some(OsStr::new("1"))),
                Ok(())
            );
            for other in ["", "0", "true", "yes", " 1", "1 ", "01"] {
                assert_eq!(require_root(verb, None, Some(OsStr::new(other))), Ok(()));
            }
            assert_eq!(require_root(verb, None, None), Ok(()));
        }
    }
}
