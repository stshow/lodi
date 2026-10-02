//! The transaction driver: the pre-flight, the journal, the file actions and the lock.
//!
//! One apply is one transaction in the sense ADR-013 gives the word: it is planned in full,
//! written down in full before anything moves, and performed in order. It is **not** atomic and
//! does not pretend to be. An action that fails stops the command with `E_APPLY` (exit 8) naming
//! the journal and the next command; nothing is undone, because there is nothing to undo it
//! with, and a message that offered a rollback would be a lie (LD-119).
//!
//! Managed-file mutation is delegated to [`super::files`], the boundary that enforces rooted
//! paths, refuses symbolic links, and performs atomic writes.

use std::collections::{BTreeMap, BTreeSet};

use crate::diag::Diagnostic;
use crate::progress::{self, Sink};

use super::journal::{Event, Journal};
use super::lock::{HostLock, PackageRecord};
use super::plan::{
    Action, Content, Desired, Facts, FileAction, Kind, Op, PackageAction, PackageFact, Plan, Step,
};
use super::pm::{self, Backend};
use super::safety::Gate;

/// Refuse before any mutation what the process cannot possibly do.
///
/// Only the certain case is refused here: an `owner` that is not the invoking user when the
/// invoking user is not root. A `group` the user may or may not belong to is left to the
/// `chown` call itself, which answers the question exactly rather than guessing at it; that
/// refusal is the same code, raised at the action.
pub fn preflight(gate: &Gate, plan: &Plan) -> Result<(), Diagnostic> {
    if gate.euid == 0 {
        return Ok(());
    }
    for file in plan.file_actions() {
        if file.op != Op::Unchanged
            && let Some(desired) = &file.desired
            && desired.uid != gate.euid
        {
            return Err(need_root(&file.path, &desired.owner));
        }
    }
    Ok(())
}

fn need_root(path: &str, owner: &str) -> Diagnostic {
    Diagnostic::new(
        "E_NEED_ROOT",
        format!("{path} declares `owner = \"{owner}\"`, which this process cannot set"),
    )
    .hint("run the command as that user, as root, or declare an owner you already are")
}

/// Run the plan. The journal is on disk and fsynced before the first mutation.
///
/// Returns the report the caller prints, and reports the steps of [`steps`] into `sink` as they
/// run; a failed step is left for the caller to fail with the diagnostic it ends with.
pub fn run(
    gate: &Gate,
    plan: &Plan,
    resolved: Option<&str>,
    lock: Option<&HostLock>,
    sink: &mut dyn Sink,
) -> Result<String, Diagnostic> {
    let mut steps = Steps {
        sink,
        current: None,
    };
    preflight(gate, plan)?;

    let mut backend = pm::backend_for(gate.distro, &gate.root, gate.operation);
    // The re-check before an exact removal simulates the transaction the plan built, so it keeps
    // the same default for what a package recommends.
    if plan
        .packages
        .as_ref()
        .is_some_and(|packages| packages.exact)
    {
        backend.keep_default_recommends();
    }
    // A pinned transaction's preflight — the private dated source set staged, refreshed into its
    // own lists, the downloaded index checked and the transaction simulated — runs before the
    // journal exists, so a pin the index cannot satisfy changes nothing at all (P7). Where a
    // stage an interrupted apply left is swept first, or a declared source is armed first, it
    // runs inside the transaction instead, before its first command.
    let mut stage: Option<Stage> = None;
    let first_is_later = plan.changing().any(|action| {
        matches!(&action.kind, Kind::Package(packages) if packages.sweep || packages.forced)
    });
    if let Some(pinned) = &plan.pinned
        && !first_is_later
    {
        stage = Some(pin_preflight(gate, backend.as_ref(), pinned)?);
    }
    let mut journal = Journal::create(gate, plan, resolved)?;
    let mut out = String::new();
    out.push_str(&format!("journal {}\n", journal.id));

    // What this apply's source steps changed, kept in memory so that a forced refresh that fails
    // can put it back (S9, LD-365).
    let mut taken: Vec<super::sources::Taken> = Vec::new();
    let mut services_checked = false;
    for action in plan.changing() {
        if downloads(action) {
            steps.enter(DOWNLOAD, Some(action_installs(action)));
        } else {
            steps.enter(step_of(action), expected(action));
        }
        if let Kind::Source(step) = &action.kind {
            perform_source(gate, &mut journal, plan, action, step, &mut taken)?;
            for line in action.lines() {
                out.push_str(&line);
                out.push('\n');
            }
            continue;
        }
        if let Kind::Package(packages) = &action.kind
            && packages.forced
        {
            journal.begin(&action.id)?;
            let refreshed = packages.invocations.iter().try_for_each(|invocation| {
                super::sources::run_refresh(invocation, &packages.sources)
            });
            if let Err(error) = refreshed {
                journal.failed(&action.id, &error.to_string())?;
                return Err(put_back(gate, &mut journal, &taken, error));
            }
            backend.refreshed();
            journal.end(&action.id)?;
            for line in action.lines() {
                out.push_str(&line);
                out.push('\n');
            }
            continue;
        }
        journal.begin(&action.id)?;
        let result = match &action.kind {
            Kind::File(file) => perform(gate, file),
            Kind::Service(service) => (|| {
                // The first service action looks again for every unit the plan waited for,
                // before any service changes (sc-1).
                if !services_checked {
                    services_checked = true;
                    super::services::recheck(gate, deferred_services(plan))?;
                }
                pm::run_reporting(
                    service
                        .invocation
                        .as_ref()
                        .expect("a changing service runs"),
                    steps.sink,
                    &|_| None,
                )
            })(),
            Kind::Identity(identity) => super::users::perform(gate, identity),
            Kind::Basic(basic) => super::basics::perform(gate, basic),
            Kind::Package(packages) if packages.sweep => sweep(gate),
            Kind::Package(packages) => (|| {
                if packages.step == Step::Transaction
                    && stage.is_none()
                    && let Some(pinned) = &plan.pinned
                {
                    stage = Some(pin_preflight(gate, backend.as_ref(), pinned)?);
                }
                perform_packages(gate, backend.as_ref(), packages, &mut steps)
            })(),
            Kind::Source(_) => unreachable!("a source step is performed above"),
        };
        match result {
            Ok(()) => {
                if !matches!(action.kind, Kind::Package(_)) {
                    steps.sink.event(progress::Event::Item {
                        name: action.summary(),
                    });
                }
                journal.end(&action.id)?;
                for line in action.lines() {
                    out.push_str(&line);
                    out.push('\n');
                }
            }
            Err(error) => {
                journal.append(&Event::Failed {
                    action: action.id.clone(),
                    at: crate::util::format_utc(crate::util::now_utc()),
                    error: error.to_string(),
                })?;
                // A refusal that happened **instead of** the action, rather than during it, is
                // reported as itself: nothing was attempted, so calling it `E_APPLY` — which
                // means "some of this transaction happened" — would be a lie.
                // It changed nothing, so the journal holds all there is to know: it is closed,
                // and the next switch has nothing outstanding to report (LD-530).
                if never_started(error.code) {
                    journal.commit()?;
                    return Err(error);
                }
                let mut failed = Diagnostic::new(
                    "E_APPLY",
                    format!(
                        "action {} ({}) failed: {}",
                        action.id,
                        action.summary(),
                        error.message
                    ),
                );
                // What ran and what did not, by name, so that nobody has to read the journal to
                // know what the machine now is (LD-377). The action that failed is not said not
                // to have run: a package manager can change the machine before it fails, so what
                // it changed is read back from the machine (LD-377 validator repair).
                let mut reached = false;
                for other in plan.changing() {
                    if other.id == action.id {
                        reached = true;
                        failed
                            .notes
                            .push(format!("failed: {} ({})", other.id, other.summary()));
                        failed.notes.extend(part_way(gate, backend.as_ref(), other));
                    } else if reached {
                        failed.notes.push(format!(
                            "did not run: {} ({})",
                            other.id,
                            other.summary()
                        ));
                    } else {
                        failed
                            .notes
                            .push(format!("ran: {} ({})", other.id, other.summary()));
                    }
                }
                return Err(failed.hint(format!(
                    "the actions before it stand, and of the one that failed only what is named \
                     as changed happened; host.lock still holds the last record; the journal {} \
                     records what ran, and `{} --root {}` plans the rest from the machine as it \
                     is once the cause is fixed",
                    journal.path.display(),
                    super::safety::Operation::Apply.command(),
                    gate.root.display()
                )));
            }
        }
    }

    steps.end();
    let (packages, observed) = package_records(backend.as_ref(), plan, lock)?;
    for line in write_lock(gate, plan, packages, observed.as_ref())? {
        out.push_str(&line);
        out.push('\n');
    }
    journal.commit()?;
    drop(stage);
    // Each pinned name is read back after the transaction; the record above is already written,
    // and a name that is not at its pin is `E_CLOSURE_DRIFT` (P6).
    if let (Some(pinned), Some(observed)) = (&plan.pinned, observed.as_ref()) {
        let drifted: Vec<String> = pinned
            .expected
            .iter()
            .filter_map(|(name, (version, _))| {
                let found = observed.installed.get(name);
                (found != Some(version)).then(|| {
                    format!(
                        "{name} pinned {version}, found {}",
                        found.map_or("nothing installed", String::as_str)
                    )
                })
            })
            .collect();
        if !drifted.is_empty() {
            return Err(Diagnostic::new(
                "E_CLOSURE_DRIFT",
                format!(
                    "after the transaction the machine does not hold its pins: {}",
                    drifted.join("; ")
                ),
            )
            .hint(format!(
                "host.lock records what the machine now has; look at the package manager's log, \
                 then run `{} --root {}` again",
                super::safety::Operation::Apply.command(),
                gate.root.display()
            )));
        }
    }
    out.push_str(&format!("{} action(s) applied\n", plan.changing().count()));
    Ok(out)
}

/// The service actions that wait for this apply's packages (sc-1).
fn deferred_services(plan: &Plan) -> impl Iterator<Item = &super::services::ServiceAction> {
    plan.changing().filter_map(|action| match &action.kind {
        Kind::Service(service) if service.deferred => Some(&**service),
        _ => None,
    })
}

/// The private dated source set of one pinned apply, below the root, removed when the apply
/// ends — on success and on every error. Only a process that is killed leaves it, and the next
/// plan sweeps it as an action of its own (P4).
struct Stage {
    dir: std::path::PathBuf,
    /// A `pin.conf` naming a private keyring inside the stage, removed with it (#170).
    config: Option<std::path::PathBuf>,
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        if let Some(config) = &self.config {
            let _ = std::fs::remove_file(config);
        }
    }
}

/// Stage, refresh, check and simulate a pinned transaction (P4, P7, P13). Nothing on the machine
/// is written: the stage is Lodi's own state below `/var/lib/lodi/host/pin`, and apt reads the
/// set through its options, so `/etc/apt` and `/var/lib/apt/lists` are neither read as a source
/// nor written.
fn pin_preflight(
    gate: &Gate,
    backend: &dyn Backend,
    pinned: &super::plan::Pinned,
) -> Result<Stage, Diagnostic> {
    use super::pin::{SET_FILE, STAGE};
    if pinned.arch_day.is_some() {
        return arch_preflight(gate, backend, pinned);
    }
    if !pinned.fedora_files.is_empty() {
        return fedora_preflight(gate, backend, pinned);
    }
    let dir = super::plan::join(&gate.root, STAGE);
    if std::fs::symlink_metadata(&dir).is_ok() {
        sweep(gate)?;
    }
    super::files::ensure_dir_trusted(&gate.root, &format!("{STAGE}/sources.list.d"), 0o755)?;
    let stage = Stage {
        dir: dir.clone(),
        config: None,
    };
    super::files::ensure_dir_trusted(&gate.root, &format!("{STAGE}/lists/partial"), 0o755)?;
    let io = |path: &std::path::Path, error: std::io::Error| {
        Diagnostic::new(
            "E_STORE_IO",
            format!("cannot write {}: {error}", path.display()),
        )
    };
    let list = dir.join("sources.list");
    std::fs::write(&list, b"").map_err(|e| io(&list, e))?;
    let set = dir.join("sources.list.d").join(SET_FILE);
    std::fs::write(&set, pinned.set.as_bytes()).map_err(|e| io(&set, e))?;
    for invocation in &pinned.refresh {
        pm::run(invocation).map_err(|error| {
            Diagnostic::new(
                "E_REPO_UNREACHABLE",
                format!(
                    "the dated source set could not be refreshed: {}",
                    error.message
                ),
            )
            .hint(
                "nothing was installed or removed; run lodi switch again once the archive answers",
            )
        })?;
    }
    for (instant, digests) in &pinned.checks {
        super::pin::check_downloaded(
            &dir.join("lists"),
            gate.distro,
            &gate.os.codename,
            *instant,
            digests,
        )?;
    }
    backend
        .simulate_pinned(&pinned.install, &pinned.remove, &pinned.pinning)
        .map_err(|error| refused(pinned, &error))?;
    Ok(stage)
}

/// Fedora's pinned builds (fk-1, LD-434): each file fetched into the stage — from a configured
/// repository when it serves the locked bytes, else from Fedora's signed Koji copy — held to the
/// lock's SHA-256 and signing key, then the whole transaction simulated. A build no place
/// serves, a copy with another digest or key, or a closure dnf5 cannot resolve stops the apply
/// here, before anything is installed or removed.
fn fedora_preflight(
    gate: &Gate,
    backend: &dyn Backend,
    pinned: &super::plan::Pinned,
) -> Result<Stage, Diagnostic> {
    use super::pin::{STAGE, STATE};
    let dir = super::plan::join(&gate.root, STAGE);
    if std::fs::symlink_metadata(&dir).is_ok() {
        sweep(gate)?;
    }
    super::files::ensure_dir_trusted(&gate.root, STATE, 0o755)?;
    super::files::ensure_dir_trusted(&gate.root, STAGE, 0o700)?;
    let stage = Stage {
        dir: dir.clone(),
        config: None,
    };
    let dnf = pm::dnf::Dnf::new(&gate.root, gate.operation);
    let fetcher = crate::fetch::HttpFetcher::from_env()
        .map_err(|error| Diagnostic::new("E_CONFIG", error))?;
    let mut paths = Vec::new();
    for file in &pinned.fedora_files {
        let path = super::pin::fedora::stage(&dnf, &fetcher, file, &dir)?;
        paths.push(path.display().to_string());
    }
    backend
        .fedora_simulate(&pinned.install, &paths, &pinned.remove)
        .map_err(|error| {
            let builds: Vec<String> = pinned
                .fedora_files
                .iter()
                .map(|file| file.nevra.clone())
                .collect();
            let said = error
                .message
                .split_once("` exited ")
                .map_or(error.message.as_str(), |(_, rest)| rest);
            Diagnostic::new(
                "E_PIN_UNSATISFIABLE",
                format!(
                    "the pinned {} cannot be installed with what this machine's repositories \
                     offer: {said}",
                    builds.join(", ")
                ),
            )
            .hint(
                "nothing was installed or removed; pin a build whose dependencies are offered, \
                 or remove the pin",
            )
        })?;
    Ok(stage)
}

/// Pacman's sync is deliberately split: `-Syy --config pin.conf` forces dated .db
/// files over newer live databases; Lodi checks *those bytes* before a package change. This makes
/// the sync database's day visible and keeps the file's digest in the host directory authoritative
/// without writing into it (LD-396). An interrupted stage is journalled for removal on restart.
fn arch_preflight(
    gate: &Gate,
    backend: &dyn Backend,
    pinned: &super::plan::Pinned,
) -> Result<Stage, Diagnostic> {
    use super::pin::{STAGE, STATE};
    use crate::fetch::Fetcher;
    let day = pinned.arch_day.expect("the Arch branch has its day");
    let dir = super::plan::join(&gate.root, STAGE);
    if std::fs::symlink_metadata(&dir).is_ok() {
        sweep(gate)?;
    }
    super::files::ensure_dir_trusted(&gate.root, STATE, 0o755)?;
    super::files::ensure_dir_trusted(&gate.root, STAGE, 0o700)?;
    let config = super::plan::join(&gate.root, STATE).join("pin.conf");
    let stage = Stage {
        dir: dir.clone(),
        config: pinned.archived_keyring.then(|| config.clone()),
    };
    if std::fs::symlink_metadata(&config).is_ok_and(|meta| !meta.is_file()) {
        return Err(Diagnostic::new(
            "E_PATH_ESCAPE",
            "pin.conf is not a regular file",
        ));
    }
    let write = |text: &str| {
        std::fs::write(&config, text)
            .map_err(|e| Diagnostic::new("E_STORE_IO", format!("cannot write pin.conf: {e}")))
    };
    write(&pinned.set)?;
    let fetcher = crate::fetch::HttpFetcher::from_env()
        .map_err(|error| Diagnostic::new("E_CONFIG", error))?;
    if pinned.archived_keyring {
        let recorded = pinned
            .checks
            .iter()
            .find(|(instant, _)| *instant == day)
            .and_then(|(_, digests)| digests.get("core.db"));
        let gpgdir = super::pin::keyring::prepare(gate, day, recorded, &dir, &config, &fetcher)?;
        write(&super::pin::arch::with_gpgdir(&pinned.set, &gpgdir))?;
    }
    for invocation in &pinned.refresh {
        fetcher
            .retrying(
                || pm::run(invocation).map_err(|e| e.message),
                pm::pacman::passing_download_failure,
            )
            .map_err(|message| {
                Diagnostic::new(
                    "E_REPO_UNREACHABLE",
                    format!("the dated Arch databases could not be synchronised: {message}"),
                )
            })?;
    }
    for (instant, digests) in &pinned.checks {
        for repo in super::pin::dated_repositories(gate.distro, "rolling", *instant)? {
            let key = format!("{}.db", repo.name);
            let Some(want) = digests.get(&key) else {
                continue;
            };
            let expected = want.strip_prefix("sha256:").ok_or_else(|| {
                Diagnostic::new("E_LOCK_VERSION", "invalid dated Arch database digest")
            })?;
            // The base's verifier is an additive caller of the same bounded db parser the
            // resolver uses. The local sync bytes are checked independently below.
            crate::arch::base::verify_index(
                &fetcher,
                &repo.name,
                &format!("{}{key}", repo.url),
                expected,
            )?;
            if *instant == day {
                let path = gate.root.join("var/lib/pacman/sync").join(&key);
                let bytes = std::fs::read(&path).map_err(|e| {
                    Diagnostic::new(
                        "E_HASH_MISMATCH",
                        format!("the dated {key} was not synchronised: {e}"),
                    )
                })?;
                let got = crate::util::sha256_hex(&bytes);
                if got != expected {
                    return Err(Diagnostic::new(
                        "E_HASH_MISMATCH",
                        format!(
                            "the dated {key} pacman synchronised has sha256:{got}, \
                             lodi.lock records {want}; nothing was installed"
                        ),
                    ));
                }
            }
        }
    }
    let mut paths = Vec::new();
    for file in &pinned.arch_files {
        let bytes = fetcher
            .get(&file.url)
            .map_err(crate::debian::base::repo_error)?;
        let actual = format!("sha256:{}", crate::util::sha256_hex(&bytes));
        if actual != file.digest {
            return Err(Diagnostic::new(
                "E_HASH_MISMATCH",
                format!(
                    "the package file {} has {actual}, expected {}; nothing was kept",
                    file.path, file.digest
                ),
            ));
        }
        let signature = fetcher
            .get(&format!("{}.sig", file.url))
            .map_err(crate::debian::base::repo_error)?;
        let path = dir.join(&file.path);
        let signature_path = dir.join(format!("{}.sig", file.path));
        use std::io::Write;
        for (target, contents) in [
            (&path, bytes.as_slice()),
            (&signature_path, signature.as_slice()),
        ] {
            let mut handle = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(target)
                .map_err(|e| {
                    Diagnostic::new(
                        "E_STORE_IO",
                        format!("cannot stage {}: {e}", target.display()),
                    )
                })?;
            handle
                .write_all(contents)
                .and_then(|_| handle.sync_all())
                .map_err(|e| {
                    Diagnostic::new(
                        "E_STORE_IO",
                        format!("cannot stage {}: {e}", target.display()),
                    )
                })?;
        }
        paths.push(path.display().to_string());
    }
    backend
        .arch_simulate(&paths)
        .map_err(|e| simulated(pinned, &dir, e))?;
    Ok(stage)
}

/// Pacman's words for a signature it cannot trust: the key is missing, of unknown or too little
/// trust, disabled (revoked) or expired. A corrupt signature is not among them.
const UNTRUSTED: &[&str] = &[
    "required key missing from keyring",
    "is unknown",
    "unknown trust",
    "marginal trust",
    "never be trusted",
    "is disabled",
    "expired",
];

/// What pacman's read-only `-Up` refused, as the refusal it is. Only pacman's own dependency
/// refusal is an unsatisfiable pin, and only its trust refusal an untrusted one (#170); a missing
/// runtime, unreadable file or unexpected failure remains its own code, never hidden as a pin.
fn simulated(pinned: &super::plan::Pinned, dir: &std::path::Path, e: Diagnostic) -> Diagnostic {
    // What pacman said, without the argv that names every staged file.
    let said = e
        .message
        .split_once("` exited ")
        .map_or(e.message.as_str(), |(_, rest)| rest);
    // The pins pacman names; when it names none of them, every staged pin (#172).
    let named = |hit: &dyn Fn(&super::plan::ArchFile) -> bool| -> Vec<&super::plan::ArchFile> {
        let files: Vec<_> = pinned.arch_files.iter().filter(|f| hit(f)).collect();
        if files.is_empty() {
            pinned.arch_files.iter().collect()
        } else {
            files
        }
    };
    let pin = |name: &String| match pinned.expected.get(name) {
        Some((version, origin)) => format!("`{name}` {version} from {origin}"),
        None => format!("`{name}`"),
    };
    if UNTRUSTED.iter().any(|words| said.contains(words)) {
        // One fact per line, the owner's ruling in order (#170): what, which key, what it
        // means, how likely, what proceeding would cost; then the way on.
        let date = &crate::util::format_utc(pinned.arch_day.unwrap_or_default())[..10];
        let archived = pinned.archived_keyring;
        let mut refusal = Diagnostic::new(
            "E_PIN_UNTRUSTED",
            if archived {
                "the archived keyring cannot verify a dated Arch pin"
            } else {
                "a dated Arch pin is signed by a key not trusted here"
            },
        );
        for file in named(&|file| said.contains(&file.path)) {
            let key = std::fs::read(dir.join(format!("{}.sig", file.path)))
                .ok()
                .and_then(|bytes| super::pin::keyring::signing_key(&bytes))
                .unwrap_or_else(|| "(unreadable from its signature)".to_string());
            refusal.notes.push(format!("package: {}", pin(&file.name)));
            refusal.notes.push(format!("key: {key}"));
        }
        let lines: &[&str] = if archived {
            &["the archlinux-keyring of that day lacks or distrusts the key too"]
        } else {
            &[
                "lodi cannot prove this is the package Arch published",
                "likely: a routine key retirement, not tampering",
                "impact: installing it anyway runs unverified code as root",
            ]
        };
        refusal
            .notes
            .extend(lines.iter().map(|line| line.to_string()));
        refusal
            .notes
            .push("nothing was installed; the machine's keyring was not touched".into());
        return if archived {
            refusal.hint("pin another date, or remove the pin")
        } else {
            refusal.notes.push(format!(
                "archived_keyring verifies it with archlinux-keyring of {date}, privately"
            ));
            refusal.hint("set `archived_keyring = true` in [packages.arch]; it may still fail")
        };
    }
    if said.contains("could not satisfy dependencies") {
        let targets: Vec<&str> = said
            .split("required by ")
            .skip(1)
            .filter_map(|tail| tail.split(|c: char| c.is_whitespace() || c == ';').next())
            .collect();
        let files = named(&|file| targets.contains(&file.name.as_str()));
        if let [file] = files[..]
            && let Some((version, origin)) = pinned.expected.get(&file.name)
        {
            return pin_unsatisfiable(&file.name, version, origin);
        }
        let pins: Vec<String> = files.into_iter().map(|file| pin(&file.name)).collect();
        return Diagnostic::new(
            "E_PIN_UNSATISFIABLE",
            format!(
                "pacman cannot satisfy the dependencies of the pinned {}",
                pins.join(", ")
            ),
        )
        .hint(
            "nothing was installed or removed; pin a date whose dependencies the archive \
               offers, or remove the pin",
        );
    }
    e
}

/// Remove the pin stage an interrupted apply left, without following a link (P4).
fn sweep(gate: &Gate) -> Result<(), Diagnostic> {
    let dir = super::plan::join(&gate.root, super::pin::STAGE);
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&dir).map_err(|error| {
            Diagnostic::new(
                "E_STORE_IO",
                format!("cannot remove {}: {error}", dir.display()),
            )
        }),
        Ok(_) => std::fs::remove_file(&dir).map_err(|error| {
            Diagnostic::new(
                "E_STORE_IO",
                format!("cannot remove {}: {error}", dir.display()),
            )
        }),
        Err(_) => Ok(()),
    }
}

/// What the simulation of a pinned transaction refused, as the refusal it is: a pinned version
/// the source set does not offer is `E_PIN_UNSATISFIABLE`, naming the package, the version and
/// what is missing — never a fallback to another version or to the whole dated index (P7).
fn refused(pinned: &super::plan::Pinned, error: &Diagnostic) -> Diagnostic {
    for (name, (version, origin)) in &pinned.expected {
        if error.message.contains(&format!("'{version}' for '{name}'")) {
            return pin_unsatisfiable(name, version, origin);
        }
    }
    if let Some(at) = error.message.find("Unable to locate package ") {
        let name = error.message[at + 25..]
            .split(|c: char| c.is_whitespace() || c == ';')
            .next()
            .unwrap_or_default();
        return Diagnostic::new(
            "E_UNKNOWN_PACKAGE",
            format!("the source set this switch reads offers no package called `{name}`"),
        )
        .hint(
            "nothing was installed or removed; correct the name in the host manifest, or put it \
             in `[packages] optional`",
        );
    }
    Diagnostic::new(
        "E_APPLY",
        format!(
            "the package manager refused the pinned transaction in its simulation: {}",
            error.message
        ),
    )
    .hint("nothing was installed or removed")
}

/// `E_PIN_UNSATISFIABLE`: `name` is pinned to `version` from `origin`, and the source set the
/// apply reads does not offer it.
pub fn pin_unsatisfiable(name: &str, version: &str, origin: &str) -> Diagnostic {
    Diagnostic::new(
        "E_PIN_UNSATISFIABLE",
        format!(
            "`{name}` is pinned to {version} from {origin}, and the source set this switch reads \
             does not offer that version"
        ),
    )
    .hint(format!(
        "nothing was installed or removed; {version} of {name} is missing from {origin}: pin a \
         version or a date its repository offers, or remove the pin"
    ))
}

/// What the action that failed had changed when it failed, read back from the machine rather than
/// assumed (LD-377 validator repair): each thing it was to change, as `changed before it failed`,
/// `not changed`, or `changed part way` when the machine shows neither the state before it nor
/// the one it was to leave.
fn part_way(gate: &Gate, backend: &dyn Backend, action: &Action) -> Vec<String> {
    match &action.kind {
        // `systemctl` changes a unit or does not; what it did is `systemctl`'s to say.
        Kind::Service(_) => Vec::new(),
        // An account tool changes one database and says so when it fails; nothing is read back.
        // What a basics tool did is the tool's to say; a network change is put back or stays.
        Kind::Identity(_) | Kind::Basic(_) => Vec::new(),
        Kind::File(file) => {
            let now = Facts::observe(&gate.root, &file.path);
            let line = action.summary();
            vec![
                if same_file(&now, &file.after) && !same_file(&now, &file.before) {
                    format!("changed before it failed: {line}")
                } else if same_file(&now, &file.before) {
                    format!("not changed: {line}")
                } else {
                    format!(
                        "changed part way: {} is neither as it was nor as declared",
                        file.path
                    )
                },
            ]
        }
        Kind::Source(step) => step
            .files
            .iter()
            .filter(|file| file.op != Op::Unchanged)
            .flat_map(|file| {
                part_way(
                    gate,
                    backend,
                    &Action {
                        id: action.id.clone(),
                        kind: Kind::File(Box::new(file.clone())),
                    },
                )
            })
            .collect(),
        Kind::Package(packages) => {
            if packages.after.is_empty() {
                return Vec::new();
            }
            let observed = match backend.observe() {
                Ok(observed) => observed,
                Err(error) => {
                    return vec![format!(
                        "what it changed could not be read back: {}",
                        error.message
                    )];
                }
            };
            let lines = action.lines();
            packages
                .after
                .iter()
                .map(|(name, after)| {
                    let label = lines
                        .iter()
                        .find(|line| {
                            let mut words = line.split_whitespace().skip(1);
                            words.next() == Some("package") && words.next() == Some(name)
                        })
                        .cloned()
                        .unwrap_or_else(|| format!("package {name}"));
                    let was = packages
                        .before
                        .get(name)
                        .is_some_and(|before| package_is(&observed, name, before));
                    if was {
                        format!("not changed: {label}")
                    } else if package_is(&observed, name, after) {
                        format!("changed before it failed: {label}")
                    } else {
                        format!(
                            "changed part way: {label} (the machine shows neither the state \
                             before it nor the one planned)"
                        )
                    }
                })
                .collect()
        }
    }
}

/// One source step: each file it changes announced under its own journal id before it is
/// touched, its previous bytes kept in memory first, and performed in the plan's order — keyring
/// before stanza on the way in, stanza before keyring on the way out (S8, S9).
fn perform_source(
    gate: &Gate,
    journal: &mut Journal,
    plan: &Plan,
    action: &Action,
    step: &super::sources::SourceAction,
    taken: &mut Vec<super::sources::Taken>,
) -> Result<(), Diagnostic> {
    for (id, file) in step.steps(&action.id) {
        journal.begin(&id)?;
        let result = super::sources::take(gate, &id, file).and_then(|kept| {
            taken.push(kept);
            perform(gate, file)
        });
        if let Err(error) = result {
            journal.failed(&id, &error.to_string())?;
            let mut failed = Diagnostic::new(
                "E_APPLY",
                format!(
                    "action {id} ({}) of source {} failed: {}",
                    Action {
                        id: id.clone(),
                        kind: Kind::File(Box::new(file.clone())),
                    }
                    .summary(),
                    step.name,
                    error.message
                ),
            );
            for other in plan.changing() {
                if other.id == action.id {
                    break;
                }
                failed
                    .notes
                    .push(format!("ran: {} ({})", other.id, other.summary()));
            }
            return Err(failed.hint(format!(
                "the files of this step written before it stand; host.lock still holds the last \
                 record; the journal {} records what ran, and `{} --root {}` plans the rest from \
                 the machine as it is once the cause is fixed",
                journal.path.display(),
                super::safety::Operation::Apply.command(),
                gate.root.display()
            )));
        }
        journal.end(&id)?;
    }
    Ok(())
}

/// A forced refresh failed (S9): put back every source file this apply changed, last change
/// first — so a stanza is always put back before the keyring it names — each restore announced
/// in the journal before it happens, and stop at the first one that cannot be made. Returns the
/// `E_APPLY` the apply ends with; no transaction runs.
fn put_back(
    gate: &Gate,
    journal: &mut Journal,
    taken: &[super::sources::Taken],
    error: Diagnostic,
) -> Diagnostic {
    let mut failed = Diagnostic::new("E_APPLY", error.message.clone());
    let mut whole = true;
    for kept in taken.iter().rev() {
        let restored = journal.begin(&kept.id).and_then(|()| {
            super::sources::put_back(gate, kept)?;
            journal.end(&kept.id)
        });
        match restored {
            Ok(()) => failed
                .notes
                .push(format!("put back as it was: {}", kept.file.path)),
            Err(problem) => {
                let _ = journal.failed(&kept.id, &problem.to_string());
                failed.notes.push(format!(
                    "not put back: {} ({}); nothing after it was put back",
                    kept.file.path, problem.message
                ));
                whole = false;
                break;
            }
        }
    }
    let done = if whole {
        "every source file this switch wrote was put back as it was, and no package was installed \
         or removed"
    } else {
        "the source files named above were not all put back; no package was installed or removed"
    };
    failed.hint(format!(
        "{done}; host.lock still holds the last record; the journal {} records what ran. Fix the \
         repository or the declaration, then run `{} --root {}` again",
        journal.path.display(),
        super::safety::Operation::Apply.command(),
        gate.root.display()
    ))
}

/// Whether a path stands as recorded: there or not, and its bytes, mode and owner where recorded.
fn same_file(now: &Facts, expected: &Facts) -> bool {
    now.exists == expected.exists
        && (!expected.exists
            || (now.digest == expected.digest
                && now.mode == expected.mode
                && now.uid == expected.uid
                && now.gid == expected.gid))
}

/// Whether a package is in a recorded state: installed or not, and its mark and hold where
/// recorded. The version is not compared: a package manager may install another than planned.
fn package_is(observed: &pm::Observed, name: &str, fact: &PackageFact) -> bool {
    observed.installed.contains_key(name) == fact.installed
        && fact
            .auto
            .is_none_or(|auto| observed.auto.contains(name) == auto)
        && fact
            .held
            .is_none_or(|held| observed.held.contains(name) == held)
}

/// Whether a diagnostic says the action never ran at all, as opposed to failing part way.
fn never_started(code: &str) -> bool {
    matches!(
        code,
        "E_UNKNOWN_PACKAGE"
            | "E_UNKNOWN_UNIT"
            | "E_NETWORK_ROLLED_BACK"
            | "E_NO_RUNTIME"
            | "E_PIN_UNSATISFIABLE"
            | "E_HASH_MISMATCH"
            | "E_REPO_UNREACHABLE"
    )
}

/// One package action: every invocation it holds, in order, each run to completion.
///
/// The transaction checks first that the index can still place every name it is about to ask
/// for. When the plan was made against an index too old to answer that — the case the `i1`
/// refresh exists for — this is where the question is finally settled, before anything is
/// installed rather than after.
fn perform_packages(
    gate: &Gate,
    backend: &dyn Backend,
    packages: &PackageAction,
    steps: &mut Steps<'_>,
) -> Result<(), Diagnostic> {
    // Only a family that refreshes the index as its own action has a window here: its plan may
    // have been derived from an index too old to place a name, and `i1` has just refreshed it.
    // A family that refreshes inside its transaction settled the question at plan time, under
    // the same lock, and asking again would be a second question with no new answer.
    if packages.step == Step::Transaction
        && pm::Shape::of(gate.distro).refresh_is_an_action
        && !packages.install.is_empty()
        && !packages.pinning.private
    {
        let candidates = backend.candidates(&packages.install)?;
        let unknown: Vec<&String> = packages
            .install
            .iter()
            .filter(|name| !candidates.contains_key(*name))
            .collect();
        if let Some(name) = unknown.first() {
            return Err(Diagnostic::new(
                "E_UNKNOWN_PACKAGE",
                format!(
                    "the {} index offers no package called `{name}`, and the index was \
                     refreshed by this switch",
                    gate.distro.name()
                ),
            )
            .hint(
                "nothing was installed or removed; correct the name in the host manifest, or \
                 put it in `[packages] optional`",
            ));
        }
    }
    // An exact removal asks the package manager again, now: an upgrade or an install since the
    // plan can change what a removal takes, and what was planned is the only thing this apply
    // may remove (LD-375). So does an exact install of a name the plan carried past the refresh:
    // what it takes off the machine is known only now (LD-379).
    if let Some(recheck) = &packages.recheck {
        let planned: BTreeSet<String> = packages.remove.iter().cloned().collect();
        let now = backend
            .simulate_pinned(&recheck.install, &recheck.roots, &recheck.pinning)
            .map_err(|error| {
                Diagnostic::new(
                    "E_APPLY",
                    format!(
                        "the package manager no longer agrees to the planned removal ({}); \
                         nothing was removed",
                        error.message
                    ),
                )
            })?;
        if now.removed != planned {
            let list = |set: &BTreeSet<String>| {
                if set.is_empty() {
                    "nothing".to_string()
                } else {
                    set.iter().cloned().collect::<Vec<_>>().join(", ")
                }
            };
            return Err(Diagnostic::new(
                "E_APPLY",
                format!(
                    "the package manager would now remove {} where the plan removed {}; nothing \
                     was removed",
                    list(&now.removed),
                    list(&planned)
                ),
            ));
        }
    }
    let items = |line: &str| backend.item(line);
    if packages.step == Step::Transaction && !packages.install.is_empty() {
        // The download is its own step, entered before this action began, and nothing is
        // installed until it ends: a failure here changes nothing on the host (#694).
        for invocation in &packages.invocations {
            if let Some(download) = backend.download(invocation) {
                pm::run_reporting(&download, steps.sink, &items)?;
            }
        }
        steps.enter(INSTALL, Some(packages.install.len()));
    }
    for invocation in &packages.invocations {
        pm::run_reporting(invocation, steps.sink, &items)?;
    }
    if packages.step == Step::Index {
        backend.refreshed();
    }
    Ok(())
}

/// The packages the lock will record: what it recorded before, plus everything the manifest
/// declares for this machine, minus what this apply removed, and then only those the machine
/// actually shows installed — each with the version **observed after** the transaction, which is
/// the second `dpkg-query` (or `pacman -Q`) OD-15 asks for.
///
/// The declared set is the right input, not this apply's installs. A declared package that was
/// already on the machine when the declaration arrived is still a package Lodi manages: leaving
/// it out would make the lock disagree with `dpkg-query`, and would make it impossible for that
/// package to ever leave the manifest cleanly, since `auto_remove` is bounded by the lock. A
/// package this manifest never declared is never in here, which is what keeps `auto_remove` from
/// touching anything the operator installed for themselves.
fn package_records(
    backend: &dyn Backend,
    plan: &Plan,
    previous: Option<&HostLock>,
) -> Result<(BTreeMap<String, PackageRecord>, Option<pm::Observed>), Diagnostic> {
    let carried: BTreeMap<String, PackageRecord> = previous
        .map(|lock| lock.packages.clone())
        .unwrap_or_default();
    // A plan that did not consider packages says nothing about them and must not forget what
    // the record knows. One that did — a files-only apply of a manifest that declares packages
    // included — records them (LD-375).
    let Some(packages) = &plan.packages else {
        return Ok((carried, None));
    };
    // The managed set, not this apply's installs: a declared package that was already installed
    // is still one Lodi manages, and the lock is the record of the managed set. An exact record
    // is exactly that set; a managed one keeps what it recorded until it leaves (1.1.0's rule).
    let mut names: BTreeSet<String> = packages.managed.iter().cloned().collect();
    if !packages.exact {
        names.extend(carried.keys().cloned());
    }
    for name in &packages.removed {
        names.remove(name);
    }
    // A name the machine does not show installed is not recorded: the lock is a record of what
    // is there, not of what was meant to be. A family whose hold is an option on lodi's own
    // transactions holds nothing on the machine (D11), so the record of it comes from what this
    // apply passes.
    let observed = backend.observe()?;
    // Arch's hold is Lodi's next --ignore, not pacman's machine state. A newly installed pin
    // was not in the pre-transaction ignore vector, but must already be recorded held so the
    // *second* apply is a true no-op (LD-396).
    let held = if pm::Shape::of(backend.distro()).hold_is_machine_state {
        &packages.ignore
    } else {
        &packages.hold
    };
    let records = super::plan::records(&names, &observed, &packages.hold, held);
    Ok((records, Some(observed)))
}

/// Write the record alone, for an apply that changes nothing on the machine and finds the record
/// no longer true of it (LD-375): the packages as the plan read them — or as the record had them,
/// when the plan did not consider packages — and every declared file as the plan found it. A
/// file already there and already right is adopted where it stands (LD-377): nothing of it is
/// written at its path, and its original is kept in the store first when its entry restores it
/// (LD-387, [`keep_originals`]).
///
/// Returns a line for each original that could not be kept.
pub fn record_only(
    gate: &Gate,
    plan: &Plan,
    previous: Option<&HostLock>,
) -> Result<Vec<String>, Diagnostic> {
    let mut lock = HostLock::new(
        crate::util::format_utc(crate::util::now_utc()),
        gate.distro.name().to_string(),
        gate.os.version_id.clone(),
    );
    lock.packages = match (&plan.packages, previous) {
        (Some(packages), _) => packages.record.clone(),
        (None, Some(previous)) => previous.packages.clone(),
        (None, None) => BTreeMap::new(),
    };
    lock.files = super::plan::file_records(plan);
    let lines = keep_originals(gate, plan, &mut lock.files);
    lock.baseline = Some(plan.baseline.clone());
    lock.source = Some(plan.source.clone());
    lock.set_git(plan.git.clone());
    lock.set_services(plan.services.clone());
    lock.set_identities(plan.identities.0.clone(), plan.identities.1.clone());
    lock.set_basics(plan.basics.clone());
    materialise_lock(gate, &lock)?;
    Ok(lines)
}

/// The adoption copies of the files this plan adopts while already equal (LD-387): each taken
/// into the store and named in its record before the record is written, by the one function
/// both ways of writing the record call. Returns a line for each that could not be kept.
fn keep_originals(
    gate: &Gate,
    plan: &Plan,
    files: &mut BTreeMap<String, super::lock::FileRecord>,
) -> Vec<String> {
    let planned: Vec<super::originals::Planned> = plan
        .file_actions()
        .filter(|file| file.adopt)
        .filter_map(|file| super::originals::Planned::from_facts(&file.before))
        .collect();
    super::originals::adopt(&gate.root, files, &planned)
}

/// The lock is written once, at the end, from the plan that was actually performed. It is a
/// **record of what was applied**, never a constraint on a later apply (OD-15).
///
/// The baseline advances with it (LD-378): the packages as the machine has them **after** the
/// transaction, and the files as this apply left them.
fn write_lock(
    gate: &Gate,
    plan: &Plan,
    packages: BTreeMap<String, PackageRecord>,
    observed: Option<&pm::Observed>,
) -> Result<Vec<String>, Diagnostic> {
    let mut lock = HostLock::new(
        crate::util::format_utc(crate::util::now_utc()),
        gate.distro.name().to_string(),
        gate.os.version_id.clone(),
    );
    lock.packages = packages;
    lock.files = super::plan::file_records(plan);
    let lines = keep_originals(gate, plan, &mut lock.files);
    let mut baseline = plan.baseline.clone();
    if let (Some(observed), Some(declared)) = (observed, &plan.packages) {
        // The plan's own baseline already holds every name the last one had that the machine
        // still had, which is what the advance after the transaction starts from.
        let (explicit, holds) = super::reconcile::packages_baseline(
            Some(&plan.baseline),
            &declared.managed,
            observed,
            &declared.hold,
            pm::Shape::of(gate.distro).hold_is_machine_state,
        );
        baseline.explicit = explicit;
        baseline.holds = holds;
    }
    lock.baseline = Some(baseline);
    lock.source = Some(plan.source.clone());
    lock.set_git(plan.git.clone());
    lock.set_services(plan.services.clone());
    lock.set_identities(plan.identities.0.clone(), plan.identities.1.clone());
    lock.set_basics(plan.basics.clone());
    materialise_lock(gate, &lock)?;
    Ok(lines)
}

/// The record, written atomically at `/etc/lodi/host.lock` below the root.
pub fn materialise_lock(gate: &Gate, lock: &HostLock) -> Result<(), Diagnostic> {
    let bytes = lock.to_bytes();
    super::files::materialise(
        &gate.root,
        "/etc/lodi/host.lock",
        &Desired {
            digest: format!("sha256:{}", crate::util::sha256_hex(&bytes)),
            mode: 0o644,
            uid: gate.euid,
            gid: gate.egid,
            owner: gate.euid.to_string(),
            group: gate.egid.to_string(),
            content: Content::Inline(bytes),
        },
    )
}

/// One file action, in the order the journal's brackets describe.
fn perform(gate: &Gate, file: &FileAction) -> Result<(), Diagnostic> {
    let root = &gate.root;
    match file.op {
        Op::Unchanged => Ok(()),
        Op::Keep => Ok(()),
        Op::Remove => super::files::remove(root, &file.path),
        Op::Metadata => {
            let desired = file
                .desired
                .as_ref()
                .expect("a metadata action has a desire");
            // An original whose mode or owner is about to change is kept first, exactly as one
            // whose bytes are (LD-377): the copy carries the mode and owner it had.
            if let Some(backup) = &file.backup {
                super::originals::make_dirs(root, backup)?;
                super::files::copy(root, &file.path, backup)?;
            }
            super::files::metadata(root, &file.path, desired)
        }
        Op::Create | Op::Replace => {
            let desired = file.desired.as_ref().expect("a write action has a desire");
            // The backup is taken and fsynced first, so that a kill between the two is visible
            // as "the old bytes are kept and the target is still the old bytes" — which is
            // neither bracket, and therefore the ambiguity the classifier must report.
            if let Some(backup) = &file.backup {
                super::originals::make_dirs(root, backup)?;
                super::files::copy(root, &file.path, backup)?;
            }
            super::files::materialise(root, &file.path, desired)
        }
        Op::Restore => {
            let desired = file
                .desired
                .as_ref()
                .expect("a restore action has a desire");
            super::files::materialise(root, &file.path, desired)?;
            super::files::remove(
                root,
                file.restore_from.as_deref().expect("a restore source"),
            )
        }
    }
}

const DOWNLOAD: &str = "Download packages";
const INSTALL: &str = "Install packages";

/// The host part's steps, in order (#694): one per run of changing actions of one name, and a
/// download before a transaction that installs. A caller gives these to the step list.
pub fn steps(plan: &Plan) -> Vec<&'static str> {
    let mut steps: Vec<&'static str> = Vec::new();
    for action in plan.changing() {
        let names = if downloads(action) {
            vec![DOWNLOAD, INSTALL]
        } else {
            vec![step_of(action)]
        };
        for name in names {
            if steps.last() != Some(&name) {
                steps.push(name);
            }
        }
    }
    steps
}

/// The step an action belongs to.
fn step_of(action: &Action) -> &'static str {
    match &action.kind {
        Kind::Package(packages) if packages.forced => "Refresh package index",
        Kind::Package(packages) => match packages.step {
            Step::Index => "Refresh package index",
            // A pin's move to another version is an install of that version.
            Step::Transaction
                if packages.sweep
                    || !packages.install.is_empty()
                    || !packages.changed.is_empty() =>
            {
                INSTALL
            }
            Step::Transaction | Step::Removal => "Remove packages",
            Step::Marks => "Package marks",
        },
        Kind::Source(_) => "Package sources",
        Kind::File(_) => "Files",
        Kind::Identity(_) => "Users and groups",
        Kind::Basic(basic) if matches!(basic.key, "cmdline" | "loader" | "parameters") => "Boot",
        Kind::Basic(_) => "System settings",
        Kind::Service(_) => "Services",
    }
}

/// Whether an action is a transaction that installs, which downloads first.
fn downloads(action: &Action) -> bool {
    matches!(&action.kind, Kind::Package(packages)
        if packages.step == Step::Transaction
            && !packages.forced
            && !packages.sweep
            && !(packages.install.is_empty() && packages.changed.is_empty()))
}

fn action_installs(action: &Action) -> usize {
    match &action.kind {
        Kind::Package(packages) => packages.install.len(),
        _ => 0,
    }
}

/// How many items a step of this action is expected to reach, when the plan knows.
fn expected(action: &Action) -> Option<usize> {
    match &action.kind {
        Kind::Package(packages) if packages.step == Step::Removal => Some(packages.remove.len()),
        _ => None,
    }
}

/// The step list as the apply runs: actions of one name share a step.
struct Steps<'a> {
    sink: &'a mut dyn Sink,
    current: Option<&'static str>,
}

impl Steps<'_> {
    fn enter(&mut self, name: &'static str, expected: Option<usize>) {
        if self.current == Some(name) {
            return;
        }
        self.end();
        self.sink.event(progress::Event::Begin {
            name: name.to_string(),
            expected,
        });
        self.current = Some(name);
    }

    fn end(&mut self) {
        if self.current.take().is_some() {
            self.sink.event(progress::Event::Finish {
                count: None,
                detail: None,
            });
        }
    }
}
