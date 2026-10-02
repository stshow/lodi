//! Host-tool environments: `lodi develop -- <command>` and `lodi run <task>` (M-Spike S-3;
//! design `spec/07`, ADR-014, ADR-015, ADR-016).
//!
//! The lifecycle of one invocation, in order:
//!
//! 1. load `./lodi.toml` (hooks, modules and every other unsupported feature are refused here,
//!    exit 3); a `lodi run` task it does not declare is the usage error at once (exit 2);
//! 2. the trust gate: a manifest with task text locks, realizes and runs nothing until that text
//!    is trusted, and one without tasks that declares tools or a base until the manifest is
//!    ([`crate::trust`], LD-359; exit 11), asked on a terminal (LD-496);
//! 3. the lock: a missing `./lodi.lock` is written, and a missing, stale or dropped entry is
//!    resolved alone while every fresh one is kept ([`lock::lock_project`], LD-496); a
//!    `[container]` manifest continues in [`crate::container`] from here;
//! 4. the activation identity `(envhash, project root, profile, runtime)` is compared with the
//!    one the caller is already in (ADR-015): the same identity is re-entered idempotently
//!    (nothing is realized, stacked or re-run); a different host identity is stacked in the
//!    child with `W_NESTED`, or refused with `--no-nest` (`E_NESTED`); another runtime is
//!    refused (`E_NESTED`);
//! 5. realization: every tool's `art-` entry and the `env-` entry, under the shared store lock;
//!    the environment's `bin` names that an executable on the caller's `PATH` also has are
//!    reported with `W_SHADOWS_HOST`, because they run in place of those host commands (LD-359);
//! 6. a live-session root `gcroots/sessions/<pid>` names the entries while the child runs and is
//!    removed when it exits (roots of processes that died are pruned on the next entry);
//! 7. the child runs with the changed environment; Lodi waits, forwards `SIGTERM`/`SIGHUP`, and
//!    exits with the child's status (`128 + n` when it was killed by signal `n`). `SIGINT` and
//!    `SIGQUIT` are not forwarded: a terminal sends them to the whole foreground process group,
//!    so the child already has them, and one sent to Lodi alone leaves the child to its own end.
//!
//! Only the child's environment changes. Lodi's own environment, and therefore the caller's,
//! is never modified.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};

use serde_json::json;

use crate::diag::Diagnostic;
use crate::fetch::Fetcher;
use crate::lock::{self, Failure, LockFile, MANIFEST_FILE, Outcome, canonical_json};
use crate::manifest::ProjectManifest;
use crate::store::{self, Report, Store};
use crate::trust::{Status, Subject, TrustStore};
use crate::util::sha256_tagged;

/// The only profile in this build.
pub const PROFILE: &str = "default";
/// The runtime of this module's environments.
pub const RUNTIME: &str = "host";

/// What to run inside the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// `lodi develop -- <program> <args…>`: the argv is passed on unchanged.
    Develop(Vec<OsString>),
    /// `lodi run <task> <args…>`: `/bin/sh -c <run> <task> <args…>`.
    Run { task: String, args: Vec<OsString> },
}

/// Options of one invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub no_nest: bool,
    /// `--trust` or `LODI_TRUST=1`: authorize this run's task text, or manifest (LD-359), without
    /// recording it.
    pub trust_once: bool,
    /// Ask on the terminal when that is not trusted (only when stdin and stderr are TTYs).
    pub interactive: bool,
}

/// The canonical build plan of a host-tool environment and its hash (design `spec/04` §6).
/// It holds no absolute path and no project name: two projects with the same tools, variables
/// and tasks share one plan, one envhash and one `env-` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub json: serde_json::Value,
    pub envhash: String,
    /// `(label, art entry name)` in plan order (sorted labels).
    pub arts: Vec<(String, String)>,
}

impl Plan {
    /// The `env-` entry name: `env-<first 32 hex of the envhash>`.
    pub fn env_name(&self) -> String {
        format!(
            "env-{}",
            &self.envhash["sha256:".len().."sha256:".len() + 32]
        )
    }
}

pub fn plan(manifest: &ProjectManifest, lock: &LockFile) -> Result<Plan, Diagnostic> {
    plan_for(manifest, lock, RUNTIME, None)
}

/// The plan of an environment of `mode` (`host`, or `container` with the image identity in
/// `base`, [`crate::container`]). A host plan has no `base` key.
pub fn plan_for(
    manifest: &ProjectManifest,
    lock: &LockFile,
    mode: &str,
    base: Option<serde_json::Value>,
) -> Result<Plan, Diagnostic> {
    let mut packages = Vec::new();
    let mut arts = Vec::new();
    for (label, tool) in &lock.packages {
        let art = store::art_name(tool)?;
        packages.push(json!({
            "label": label,
            "provider": tool.provider,
            "version": tool.version,
            "artifacts": tool.artifacts.iter().map(|a| a.sha256.clone()).collect::<Vec<_>>(),
            "entry": art,
            "spec": { "bin": tool.spec.bin, "path": tool.spec.path, "env": tool.spec.env },
        }));
        arts.push((label.clone(), art));
    }
    let tasks: BTreeMap<&String, serde_json::Value> = manifest
        .tasks
        .iter()
        .map(|(name, task)| (name, json!({ "run": task.run })))
        .collect();
    let mut value = json!({
        "schema": 1,
        "format": "lodi-spike-plan/1",
        "mode": mode,
        "profile": PROFILE,
        "arch": "x86_64",
        "packages": packages,
        "env": manifest.env,
        "tasks": tasks,
        "lodiMajor": 0,
    });
    if let Some(base) = base {
        value["base"] = base;
    }
    let envhash = sha256_tagged(canonical_json(&value).as_bytes());
    Ok(Plan {
        json: value,
        envhash,
        arts,
    })
}

/// The registry row the realized environment plan is registered as (`src/schema.rs`).
pub const PLAN_ARTIFACT: &str = "environment-plan";

/// The activation identity of ADR-015.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    pub envhash: String,
    pub project_root: PathBuf,
    pub profile: String,
    pub runtime: String,
}

impl Activation {
    /// `sha256:<hex>` of the canonical identity, exported as `LODI_ACTIVATION`.
    pub fn id(&self) -> String {
        sha256_tagged(
            canonical_json(&json!({
                "envhash": self.envhash,
                "projectRoot": self.project_root.to_string_lossy(),
                "profile": self.profile,
                "runtime": self.runtime,
            }))
            .as_bytes(),
        )
    }

    pub fn record(&self) -> serde_json::Value {
        json!({
            "id": self.id(),
            "envhash": self.envhash,
            "projectRoot": self.project_root.to_string_lossy(),
            "profile": self.profile,
            "runtime": self.runtime,
        })
    }
}

/// How an entry relates to the environment the caller is already in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nesting {
    /// Not inside any Lodi environment.
    Fresh,
    /// Inside this very activation: re-entry is idempotent.
    Same,
    /// Inside another host activation: stacked in the child (`W_NESTED`).
    Stacked,
}

/// Decide [`Nesting`] from the caller's `LODI_MODE` and `LODI_ACTIVATION`.
pub fn nesting(
    env: &BTreeMap<OsString, OsString>,
    activation: &Activation,
    no_nest: bool,
) -> Result<Nesting, Diagnostic> {
    let get = |k: &str| {
        env.get(OsStr::new(k))
            .map(|v| v.to_string_lossy().into_owned())
    };
    let mode = get("LODI_MODE");
    let outer = get("LODI_ACTIVATION");
    if let Some(mode) = &mode
        && mode != RUNTIME
    {
        return Err(Diagnostic::new(
            "E_NESTED",
            format!(
                "cannot enter a {RUNTIME} environment from inside a {mode} environment; \
                 cross-runtime nesting is not supported"
            ),
        )
        .hint("leave the outer environment first"));
    }
    match outer {
        None if mode.is_none() => Ok(Nesting::Fresh),
        Some(id) if id == activation.id() => Ok(Nesting::Same),
        _ if no_nest => Err(Diagnostic::new(
            "E_NESTED",
            format!(
                "already inside another environment (project {}); --no-nest refuses to stack",
                get("LODI_PROJECT_ROOT").unwrap_or_else(|| "unknown".into())
            ),
        )),
        _ => Ok(Nesting::Stacked),
    }
}

/// Expand `$NAME` in a manifest `[env]` value from `env` (unset is empty), as `sh` would for
/// that one form. Nothing else is interpreted: no command substitution, no `${…}` (a literal
/// `${` written as `$${` stays literal), no quoting; a `$` not followed by a name is kept.
pub fn expand(value: &str, env: &BTreeMap<OsString, OsString>) -> OsString {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let starts_name = |b: u8| b.is_ascii_alphabetic() || b == b'_';
        if bytes[i] == b'$' && i + 1 < bytes.len() && starts_name(bytes[i + 1]) {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let name = OsStr::from_bytes(&bytes[i + 1..j]);
            if let Some(v) = env.get(name) {
                out.extend_from_slice(v.as_bytes());
            }
            i = j;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    OsString::from_vec(out)
}

/// The child's environment: the caller's, plus the activation (spec/07 §3, in this order):
/// `LODI_*`, `PATH` = `<env>/tree/bin` before the previous `PATH`, each tool's `spec.env`
/// (`${self.path}` = its store path, `${self.version}` = its version; `@{self.path}` is the
/// same thing under the catalogue's older spelling), then the manifest's `[env]` in
/// declaration order.
pub fn child_environment(
    parent: &BTreeMap<OsString, OsString>,
    store: &Store,
    plan: &Plan,
    lock: &LockFile,
    manifest: &ProjectManifest,
    activation: &Activation,
    env_name_for_humans: &str,
) -> Result<BTreeMap<OsString, OsString>, Diagnostic> {
    let mut env = parent.clone();
    let set = |env: &mut BTreeMap<OsString, OsString>, k: &str, v: OsString| {
        env.insert(OsString::from(k), v);
    };
    let env_path = store.entry_path(&plan.env_name());
    let previous = parent.get(OsStr::new("PATH")).cloned().unwrap_or_default();
    set(&mut env, "LODI_ENV", env_path.clone().into_os_string());
    set(&mut env, "LODI_ENV_HASH", plan.envhash.clone().into());
    set(&mut env, "LODI_ENV_NAME", env_name_for_humans.into());
    set(&mut env, "LODI_PROFILE", activation.profile.clone().into());
    set(
        &mut env,
        "LODI_PROJECT_ROOT",
        activation.project_root.clone().into_os_string(),
    );
    set(&mut env, "LODI_MODE", activation.runtime.clone().into());
    set(&mut env, "LODI_ACTIVATION", activation.id().into());
    set(&mut env, "LODI_ORIG_PATH", previous.clone());
    let mut path = env_path.join("tree/bin").into_os_string();
    if !previous.is_empty() {
        path.push(":");
        path.push(&previous);
    }
    set(&mut env, "PATH", path);
    for (label, art) in &plan.arts {
        let tool = &lock.packages[label];
        let art_path = store.entry_path(art);
        for (k, v) in &tool.spec.env {
            let path = art_path.to_string_lossy();
            let v = v
                .replace("@{self.path}", &path)
                .replace("${self.path}", &path)
                .replace("${self.version}", &tool.version);
            if v.contains("@{") || v.contains("${self.") {
                return Err(Diagnostic::new(
                    "E_RECIPE_CTX",
                    format!("tool `{label}` sets {k} with an unknown placeholder"),
                ));
            }
            set(&mut env, k, v.into());
        }
    }
    for (k, v) in &manifest.env {
        let value = expand(v, &env);
        env.insert(OsString::from(k), value);
    }
    Ok(env)
}

/// Realize every tool and the `env-` entry of `plan`; warnings (bin collisions) are returned.
pub fn realize(
    store: &Store,
    fetcher: &dyn Fetcher,
    lock: &LockFile,
    plan: &Plan,
    report: &mut Report,
) -> Result<Vec<String>, Diagnostic> {
    for (label, _) in &plan.arts {
        store.realize_tool(fetcher, &lock.packages[label], report, None)?;
    }
    let mut warnings = Vec::new();
    let references: Vec<String> = plan.arts.iter().map(|(_, a)| a.clone()).collect();
    store.publish(
        store::Entry {
            name: plan.env_name(),
            kind: "env",
            identity: plan.envhash.clone(),
            references,
        },
        report,
        &|_, _| Ok(()),
        |staging, _, _| {
            let io = |p: &Path, e: io::Error| {
                Diagnostic::new("E_STORE_IO", format!("{}: {e}", p.display()))
            };
            let bin = staging.join("tree/bin");
            fs::create_dir_all(&bin).map_err(|e| io(&bin, e))?;
            // The plan's own bytes name the store entry (`envhash`), so the registry's writer
            // field is added to the **document** and never to the value that is hashed: an
            // environment must not move because the lodi that realized it changed (M-1.0 T-1).
            fs::write(
                staging.join("env.json"),
                crate::schema::stamped_json(PLAN_ARTIFACT, &plan.json),
            )
            .map_err(|e| io(staging, e))?;
            // spec/07 §2: later packages win a name collision, with W_BIN_CONFLICT.
            let mut owner: BTreeMap<OsString, String> = BTreeMap::new();
            for (label, art) in &plan.arts {
                for dir in &lock.packages[label].spec.path {
                    let source = store.entry_path(art).join(dir);
                    let Ok(entries) = fs::read_dir(&source) else {
                        continue;
                    };
                    // Only executables go on PATH. A recipe whose artifact puts its binary at
                    // the root (`path = ["."]`, the shape most single-binary release tarballs
                    // have) otherwise puts that artifact's README and licence on PATH too, and
                    // makes two such tools collide on those names (M-0.4 T-3, LD placed there).
                    let mut names: Vec<OsString> = entries
                        .flatten()
                        .filter(|e| {
                            e.file_type().is_ok_and(|t| !t.is_dir())
                                && fs::metadata(e.path())
                                    .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
                        })
                        .map(|e| e.file_name())
                        .collect();
                    names.sort();
                    for name in names {
                        let link = bin.join(&name);
                        if let Some(previous) = owner.insert(name.clone(), label.clone()) {
                            warnings.push(format!(
                                "lodi: warning W_BIN_CONFLICT: `{}` is provided by {previous} and \
                                 {label}; {label} wins",
                                name.to_string_lossy()
                            ));
                            fs::remove_file(&link).map_err(|e| io(&link, e))?;
                        }
                        let target = Path::new("../../..").join(art).join(dir).join(&name);
                        std::os::unix::fs::symlink(&target, &link).map_err(|e| io(&link, e))?;
                    }
                }
            }
            Ok(())
        },
    )?;
    Ok(warnings)
}

/// The names that are an executable file in an environment's `bin` and that some directory of
/// `path`, the caller's `PATH` at entry, also provides as an executable file: host commands the
/// environment runs in their place. An entry of `bin` that is not an executable file runs
/// nothing, so it shadows nothing. Sorted, each once.
pub fn shadowed_host_commands(bin: &Path, path: &OsStr) -> Vec<String> {
    let Ok(entries) = fs::read_dir(bin) else {
        return Vec::new();
    };
    let dirs: Vec<PathBuf> = std::env::split_paths(path)
        .filter(|dir| dir.as_path() != bin)
        .collect();
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name())
        .filter(|name| {
            is_executable_file(&bin.join(name))
                && dirs.iter().any(|dir| is_executable_file(&dir.join(name)))
        })
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Whether `path`, followed through symbolic links, is a regular file with an execute bit: what
/// a `PATH` lookup would run.
fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The one `W_SHADOWS_HOST` line for an environment whose `bin` shadows host commands, or
/// `None` when it shadows none (LD-359).
pub fn shadow_warning(bin: &Path, path: Option<&OsStr>) -> Option<String> {
    let names = shadowed_host_commands(bin, path?);
    if names.is_empty() {
        return None;
    }
    Some(format!(
        "lodi: warning W_SHADOWS_HOST: this environment runs its own {} in place of the \
         host command{} of the same name on PATH",
        names
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", "),
        if names.len() == 1 { "" } else { "s" }
    ))
}

static CHILD: AtomicI32 = AtomicI32::new(0);
/// A forwarded signal that arrived before the child's pid was known: the child may already be
/// running (spawn returns only after its `exec`), so it is sent as soon as the pid is stored.
static PENDING: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward(signal: libc::c_int) {
    let pid = CHILD.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: kill is async-signal-safe.
        unsafe { libc::kill(pid, signal) };
    } else {
        PENDING.store(signal, Ordering::SeqCst);
    }
}

extern "C" fn ignore(_: libc::c_int) {}

/// Install `handler` for `signal`; returns the previous action. Handlers (unlike ignored
/// dispositions) are reset to the default in the child by `exec`.
fn install(signal: libc::c_int, handler: extern "C" fn(libc::c_int)) -> libc::sigaction {
    // SAFETY: plain sigaction calls with zero-initialized structures.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(signal, &action, &mut old);
        old
    }
}

fn restore(signal: libc::c_int, old: &libc::sigaction) {
    // SAFETY: restoring an action returned by sigaction.
    unsafe { libc::sigaction(signal, old, std::ptr::null_mut()) };
}

/// Run `argv` with `env` in `cwd`, wait, and return the exit status to report: the child's
/// code, `128 + n` for a signal, 127 when the program is not found, 126 when it cannot run.
pub fn spawn_and_wait(argv: &[OsString], env: &BTreeMap<OsString, OsString>, cwd: &Path) -> u8 {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .env_clear()
        .envs(env)
        .current_dir(cwd);
    let forwarded = [libc::SIGTERM, libc::SIGHUP];
    let terminal = [libc::SIGINT, libc::SIGQUIT];
    let mut saved = Vec::new();
    PENDING.store(0, Ordering::SeqCst);
    for s in forwarded {
        saved.push((s, install(s, forward)));
    }
    // The terminal delivers SIGINT/SIGQUIT to the child's process group itself; Lodi only has
    // to survive them so that it can report the child's status and clean up.
    for s in terminal {
        saved.push((s, install(s, ignore)));
    }
    let status = match command.spawn() {
        Ok(mut child) => {
            CHILD.store(child.id() as i32, Ordering::SeqCst);
            let pending = PENDING.swap(0, Ordering::SeqCst);
            if pending != 0 {
                // SAFETY: a signal to the child this function started and has not yet waited for.
                unsafe { libc::kill(child.id() as i32, pending) };
            }
            let status = child.wait();
            CHILD.store(0, Ordering::SeqCst);
            match status {
                Ok(s) => match (s.code(), s.signal()) {
                    (Some(code), _) => code as u8,
                    (None, Some(signal)) => 128u8.wrapping_add(signal as u8),
                    _ => 1,
                },
                Err(e) => {
                    eprintln!("lodi: error E_ENTER: waiting for the command failed: {e}");
                    1
                }
            }
        }
        Err(e) => {
            let shown = argv[0].to_string_lossy();
            if e.kind() == io::ErrorKind::NotFound {
                eprintln!("lodi: {shown}: command not found in the environment");
                127
            } else {
                eprintln!("lodi: {shown}: cannot run: {e}");
                126
            }
        }
    };
    for (s, old) in saved.iter().rev() {
        restore(*s, old);
    }
    status
}

pub(crate) fn fail(d: Diagnostic) -> Failure {
    Failure::one(d)
}

/// The trust gate (ADR-014, LD-359). Returns only when the manifest's task text, or for a
/// manifest without tasks the tools and base it declares, may run.
/// Public so that `tests/diagnostics.rs` can pin `E_TRUST_REQUIRED`'s code, status and hint
/// without a lock and without the network (M-0.3 T-1).
pub fn trust_gate(
    root: &Path,
    manifest: &ProjectManifest,
    options: &Options,
) -> Result<(), Failure> {
    let manifest_path = root.join(MANIFEST_FILE);
    let subject = trust_subject(&manifest_path, manifest)?;
    let store = TrustStore::from_env().map_err(fail)?;
    let status = store.status(&manifest_path, &subject).map_err(fail)?;
    let noun = subject.noun();
    match status {
        Status::NothingToTrust | Status::Trusted => return Ok(()),
        Status::Changed => match subject {
            Subject::Manifest(_) => eprintln!(
                "lodi: warning W_TRUST_CHANGED: {} changed since it was trusted",
                manifest_path.display()
            ),
            _ => eprintln!(
                "lodi: warning W_TRUST_CHANGED: the task text of {} changed since it was trusted",
                manifest_path.display()
            ),
        },
        Status::Unknown => {}
    }
    let hash = subject.hash().unwrap_or_default();
    if options.trust_once {
        let what = match subject {
            Subject::Manifest(_) => "manifest",
            _ => "script text",
        };
        eprintln!(
            "lodi: trusted for this run only ({what} {hash}); {}",
            subject.boundary()
        );
        return Ok(());
    }
    if options.interactive {
        eprintln!("{}", subject.describe(&manifest_path));
        eprint!("Allow this {noun} to run? [y/N] ");
        let _ = io::stderr().flush();
        let mut answer = String::new();
        let _ = io::stdin().lock().read_line(&mut answer);
        if matches!(answer.trim(), "y" | "Y" | "yes") {
            store.trust(&manifest_path, &subject).map_err(fail)?;
            return Ok(());
        }
        return Err(fail(Diagnostic::new(
            "E_DECLINED",
            format!("the {noun} was not trusted; nothing was locked, realized or run"),
        )));
    }
    let declares = match subject {
        Subject::Manifest(_) => "declares tools or a base and",
        _ => "declares task text that",
    };
    Err(fail(
        Diagnostic::new(
            "E_TRUST_REQUIRED",
            format!(
                "{} {declares} has not been trusted{}; nothing was realized or run",
                manifest_path.display(),
                if status == Status::Changed {
                    " in its current form"
                } else {
                    ""
                }
            ),
        )
        .hint("pass --trust to allow it for this run"),
    ))
}

/// The trust subject of `manifest`, whose file is `manifest_path`. The file is read again only
/// when the subject is the file itself: a manifest with tasks is trusted for its task text.
fn trust_subject(manifest_path: &Path, manifest: &ProjectManifest) -> Result<Subject, Failure> {
    if !manifest.tasks.is_empty() {
        return Ok(Subject::of(manifest, &[]));
    }
    let bytes = fs::read(manifest_path).map_err(|e| {
        fail(Diagnostic::new(
            "E_NO_MANIFEST",
            format!("cannot read {}: {e}", manifest_path.display()),
        ))
    })?;
    Ok(Subject::of(manifest, &bytes))
}

/// The argv an action runs: a `develop` argv unchanged, a task as
/// `/bin/sh -c <run> <task> <args…>`.
fn argv_of(action: &Action, manifest: &ProjectManifest) -> Vec<OsString> {
    match action {
        Action::Develop(argv) => argv.clone(),
        Action::Run { task, args } => {
            let mut v: Vec<OsString> = vec![
                "/bin/sh".into(),
                "-c".into(),
                manifest.tasks[task].run.clone().into(),
                task.into(),
            ];
            v.extend(args.iter().cloned());
            v
        }
    }
}

/// The usage error of `lodi run TASK` for a name `manifest` has no task for, in the shape of
/// every other failure: the `lodi: error` prefix, then the tasks there are as a note. A usage
/// error carries no code (`spec/10` looks a name up as a task, then as an environment binary;
/// this build has only tasks).
pub fn unknown_task(manifest: &ProjectManifest, task: &str) -> String {
    let known: Vec<&str> = manifest.tasks.keys().map(String::as_str).collect();
    let tasks = if known.is_empty() {
        "it declares no tasks".to_string()
    } else {
        format!("its tasks: {}", known.join(", "))
    };
    format!("lodi: error: {MANIFEST_FILE} has no task `{task}`\n   = {tasks}")
}

/// `lodi develop -- …` and `lodi run …` for the project in `root` (the current directory).
/// Returns the exit status to report.
pub fn enter(
    root: &Path,
    action: &Action,
    options: &Options,
    fetcher: &dyn Fetcher,
) -> Result<u8, Failure> {
    let root = root.canonicalize().map_err(|e| {
        fail(Diagnostic::new(
            "E_NO_MANIFEST",
            format!("{}: {e}", root.display()),
        ))
    })?;
    let (manifest, _) = lock::load_manifest(&root)?;
    // An unknown task name is known as soon as the manifest is read, so it is reported before
    // trust is asked or anything is locked for a command that would not run anyway.
    if let Action::Run { task, .. } = action
        && !manifest.tasks.contains_key(task)
    {
        eprintln!("{}", unknown_task(&manifest, task));
        return Ok(crate::diag::EXIT_USAGE);
    }
    // Trust comes first: locking fetches metadata from what the manifest declares (LD-496).
    trust_gate(&root, &manifest, options)?;
    let lock = lock_by_itself(&root, fetcher)?;
    let argv = match argv_of(action, &manifest) {
        // `lodi develop` with no `-- COMMAND` is the interactive entry (`spec/10-cli` §3, D3,
        // LD-48): the project's own environment with a shell in it instead of a command.
        argv if argv.is_empty() => {
            crate::shell::interactive_argv(manifest.is_container_mode()).map_err(fail)?
        }
        argv => argv,
    };
    if manifest.is_container_mode() {
        return crate::container::enter(&root, &manifest, &lock, &argv, options, fetcher);
    }
    enter_host(&root, &manifest, &lock, &argv, options, fetcher)
}

/// The project's lock, written first when it is missing or lacks an entry (LD-496): one line on
/// standard error names what was locked, so standard output stays the command's.
fn lock_by_itself(root: &Path, fetcher: &dyn Fetcher) -> Result<LockFile, Failure> {
    match lock::lock_project(root, fetcher, crate::util::now_utc())? {
        Outcome::UpToDate(lock) => Ok(lock),
        Outcome::Written { lock, resolved } => {
            eprintln!("lodi: {}", lock::wrote(&lock, &resolved));
            Ok(lock)
        }
    }
}

/// The host-tool half of [`enter`], and of [`crate::shell`]'s host mode: realize the plan of
/// `manifest`/`lock`, root a live session at `gcroots/sessions/<pid>` and run `argv` in `root`
/// with the environment of the activation. The shared store lock is released before the child
/// starts, so a shell never holds it for its life (T-2's "must not").
pub fn enter_host(
    root: &Path,
    manifest: &ProjectManifest,
    lock: &LockFile,
    argv: &[OsString],
    options: &Options,
    fetcher: &dyn Fetcher,
) -> Result<u8, Failure> {
    let root = root.to_path_buf();
    let plan = plan(manifest, lock).map_err(fail)?;
    let activation = Activation {
        envhash: plan.envhash.clone(),
        project_root: root.clone(),
        profile: PROFILE.into(),
        runtime: RUNTIME.into(),
    };
    let parent: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    let nesting = nesting(&parent, &activation, options.no_nest).map_err(fail)?;

    if nesting == Nesting::Same {
        // Re-entry into the active identity: nothing is realized, stacked or re-run.
        return Ok(spawn_and_wait(argv, &parent, &root));
    }
    if nesting == Nesting::Stacked {
        eprintln!(
            "lodi: warning W_NESTED: entering {} inside another environment; it is stacked in \
             the child and the outer one is unchanged when it exits",
            root.display()
        );
    }

    let store = Store::open_for_write(&Store::home_from_env().map_err(fail)?).map_err(fail)?;
    store.prune_sessions();
    store.prune_staging();
    let shared = store.shared_lock().map_err(fail)?;
    let mut report = Report::default();
    let warnings = realize(&store, fetcher, lock, &plan, &mut report).map_err(fail)?;
    for w in warnings {
        eprintln!("{w}");
    }
    if let Some(w) = shadow_warning(
        &store.entry_path(&plan.env_name()).join("tree/bin"),
        parent.get(OsStr::new("PATH")).map(OsString::as_os_str),
    ) {
        eprintln!("{w}");
    }
    if !report.created.is_empty() {
        eprintln!(
            "lodi: realized {} ({} bytes downloaded)",
            report.created.join(", "),
            report.downloaded
        );
    }
    let name = manifest
        .environment_name(&root)
        .unwrap_or_else(|| "project".into());
    let env = child_environment(&parent, &store, &plan, lock, manifest, &activation, &name)
        .map_err(fail)?;
    let mut entries = vec![plan.env_name()];
    entries.extend(plan.arts.iter().map(|(_, a)| a.clone()));
    let pid = std::process::id();
    let root_file = store
        .add_session_root(
            pid,
            &json!({
                "pid": pid,
                "activation": activation.record(),
                "entries": entries,
            }),
        )
        .map_err(fail)?;
    drop(shared);
    let status = spawn_and_wait(argv, &env, &root);
    let _ = fs::remove_file(&root_file);
    Ok(status)
}

/// Whether stdin and stderr are both terminals (the trust prompt is only asked then).
pub fn interactive() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<OsString, OsString> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    #[test]
    fn expansion_handles_only_dollar_name() {
        let e = env(&[("HOME", "/h"), ("A_1", "x")]);
        assert_eq!(expand("$HOME/.cache", &e), "/h/.cache");
        assert_eq!(expand("$A_1$A_1-$UNSET.", &e), "xx-.");
        assert_eq!(
            expand("$(rm -rf x) `id` ${HOME} $ $1", &e),
            "$(rm -rf x) `id` ${HOME} $ $1"
        );
    }

    #[test]
    fn nesting_rules() {
        let a = Activation {
            envhash: "sha256:a".into(),
            project_root: "/p".into(),
            profile: PROFILE.into(),
            runtime: RUNTIME.into(),
        };
        assert_eq!(nesting(&env(&[]), &a, false), Ok(Nesting::Fresh));
        let same = env(&[("LODI_MODE", "host"), ("LODI_ACTIVATION", &a.id())]);
        assert_eq!(nesting(&same, &a, true), Ok(Nesting::Same));
        let other = env(&[("LODI_MODE", "host"), ("LODI_ACTIVATION", "sha256:b")]);
        assert_eq!(nesting(&other, &a, false), Ok(Nesting::Stacked));
        assert_eq!(nesting(&other, &a, true).unwrap_err().code, "E_NESTED");
        let container = env(&[("LODI_MODE", "container"), ("LODI_ACTIVATION", "sha256:c")]);
        assert_eq!(nesting(&container, &a, false).unwrap_err().code, "E_NESTED");
    }

    #[test]
    fn the_activation_identity_includes_every_component() {
        let a = Activation {
            envhash: "sha256:a".into(),
            project_root: "/p".into(),
            profile: PROFILE.into(),
            runtime: RUNTIME.into(),
        };
        let mut b = a.clone();
        b.project_root = "/q".into();
        assert_ne!(a.id(), b.id());
        let mut c = a.clone();
        c.envhash = "sha256:b".into();
        assert_ne!(a.id(), c.id());
    }
}
