//! `[services]`: the systemd units a host declares enabled or disabled (sc-1, LD-418).
//!
//! A unit declared `enabled` is enabled and running, and one declared `disabled` is disabled and
//! stopped; the plan runs `systemctl enable --now` or `disable --now` for each that is not, after
//! the packages and the files. A unit the last apply declared and the manifest no longer does
//! goes back to its preset with `systemctl preset`, and nothing else: a unit never declared is
//! never read or touched. The record of what was declared is `host.lock`'s schema 4.
//!
//! `systemctl` is resolved and run like a package manager ([`super::pm::Invocation`]): by bare
//! name, with the environment cleared, and every unit after `--`. Under `--root DIR` each call
//! carries `--root=DIR`, so that `systemctl` reads and writes that root's unit files and never
//! the running system's.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::diag::Diagnostic;

use super::lock::HostLock;
use super::manifest::HostManifest;
use super::pm::{Invocation, noninteractive_env};
use super::safety::{self, Gate};

/// The one wording a unit waiting for this apply's packages is printed with.
pub const DEFERRED: &str = "after the package transaction";

/// What one service line does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Enable,
    Disable,
    Preset,
    Unchanged,
}

/// One declared, or no longer declared, unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceAction {
    pub unit: String,
    pub change: Change,
    /// The command it runs; `None` for a unit already as declared.
    pub invocation: Option<Invocation>,
    /// The unit was not on the machine when the plan was made and this apply installs packages:
    /// it is looked for again after the transaction, before any service changes.
    pub deferred: bool,
    /// The unit file's state as the plan saw it.
    pub state: String,
}

impl ServiceAction {
    pub fn line(&self) -> String {
        let verb = match self.change {
            Change::Enable => "+",
            Change::Disable => "-",
            Change::Preset => "~",
            Change::Unchanged => "=",
        };
        let detail = match &self.invocation {
            Some(invocation) if self.deferred => {
                format!("{}; {DEFERRED}", invocation.command_line())
            }
            Some(invocation) => invocation.command_line(),
            None => self.state.clone(),
        };
        format!("{verb} service {} ({detail})", self.unit)
    }

    pub fn kind_name(&self) -> &'static str {
        match self.change {
            Change::Enable => "service.enable",
            Change::Disable => "service.disable",
            Change::Preset => "service.preset",
            Change::Unchanged => "service.unchanged",
        }
    }
}

/// `systemctl`, resolved for one root.
pub struct Systemctl {
    path: PathBuf,
    root: Option<String>,
}

impl Systemctl {
    pub fn find(gate: &Gate) -> Result<Systemctl, Diagnostic> {
        let path = match safety::resolve_program("systemctl", gate.system_root, gate.euid) {
            Ok(Some(path)) => path,
            Ok(None) => {
                return Err(Diagnostic::new(
                    "E_NO_RUNTIME",
                    format!(
                        "`systemctl` is not on {}, and host.toml declares [services]",
                        safety::searched(gate.system_root)
                    ),
                )
                .hint("[services] needs a machine that runs systemd; nothing was changed"));
            }
            Err(why) => return Err(safety::untrusted_runtime("systemctl", &why)),
        };
        Ok(Systemctl {
            path,
            root: (!gate.system_root).then(|| format!("--root={}", gate.root.display())),
        })
    }

    /// `systemctl [--root=DIR] ARGS... -- UNIT`.
    pub fn invocation(&self, args: &[&str], unit: &str) -> Invocation {
        let argv: Vec<String> = self
            .root
            .iter()
            .cloned()
            .chain(args.iter().map(|arg| (*arg).to_string()))
            .chain(["--".to_string(), unit.to_string()])
            .collect();
        Invocation::resolved("systemctl", &self.path, &argv, noninteractive_env())
    }

    /// The unit file's state and whether the unit runs, or `None` when the machine has no such
    /// unit. `is-enabled` and `is-active` answer with a word and an exit status that means
    /// something other than failure, so the word is what is read.
    pub fn observe(&self, unit: &str) -> Result<Option<(String, bool)>, Diagnostic> {
        let state = self.word(&["is-enabled"], unit)?;
        if state.is_empty() || state == "not-found" {
            return Ok(None);
        }
        let active = self.word(&["is-active"], unit)?;
        let running = matches!(
            active.as_str(),
            "active" | "activating" | "reloading" | "refreshing"
        );
        Ok(Some((state, running)))
    }

    fn word(&self, args: &[&str], unit: &str) -> Result<String, Diagnostic> {
        let invocation = self.invocation(args, unit);
        let output = invocation.command().output().map_err(|e| {
            Diagnostic::new(
                "E_APPLY",
                format!("cannot run {}: {e}", invocation.path.display()),
            )
        })?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string())
    }
}

/// The refusal of a declared unit the machine does not have, or cannot enable or disable.
fn unknown(problems: &[String], done: &str) -> Diagnostic {
    Diagnostic::new("E_UNKNOWN_UNIT", problems.join("; ")).hint(format!(
        "{done}; correct the name, or declare the package that ships it"
    ))
}

fn problem(unit: &str, state: Option<&str>) -> String {
    match state {
        None => format!("no unit {unit} on this machine"),
        Some(state) => format!("{unit} is {state}, which systemctl cannot enable or disable"),
    }
}

/// Plan the services: one action per declared unit and per unit the record declared and the
/// manifest no longer does, in unit order. Returns the record a converged apply writes.
///
/// A declared unit the machine does not have stops the plan with `E_UNKNOWN_UNIT`, unless
/// `installs` says this apply installs packages first: then it is looked for after them.
pub fn plan(
    gate: &Gate,
    manifest: &HostManifest,
    lock: Option<&HostLock>,
    installs: bool,
    out: &mut Vec<ServiceAction>,
) -> Result<BTreeMap<String, String>, Diagnostic> {
    let recorded = lock.map(|lock| &lock.services);
    let removed: Vec<&String> = recorded
        .into_iter()
        .flat_map(|services| services.keys())
        .filter(|unit| !manifest.services.contains_key(*unit))
        .collect();
    if manifest.services.is_empty() && removed.is_empty() {
        return Ok(BTreeMap::new());
    }
    let systemctl = Systemctl::find(gate)?;
    let mut actions = BTreeMap::new();
    let mut problems = Vec::new();
    for (unit, &enabled) in &manifest.services {
        let (args, change): (&[&str], Change) = if enabled {
            (&["enable", "--now"], Change::Enable)
        } else {
            (&["disable", "--now"], Change::Disable)
        };
        let (change, state, deferred) = match systemctl.observe(unit)? {
            None if installs => (change, String::new(), true),
            None => {
                problems.push(problem(unit, None));
                continue;
            }
            Some((state, _)) if state != "enabled" && state != "disabled" => {
                problems.push(problem(unit, Some(&state)));
                continue;
            }
            Some((state, running)) if (state == "enabled") == enabled && running == enabled => {
                (Change::Unchanged, state, false)
            }
            Some((state, _)) => (change, state, false),
        };
        let invocation = (change != Change::Unchanged).then(|| systemctl.invocation(args, unit));
        actions.insert(
            unit.clone(),
            ServiceAction {
                unit: unit.clone(),
                change,
                invocation,
                deferred,
                state,
            },
        );
    }
    if !problems.is_empty() {
        return Err(unknown(&problems, "nothing changed"));
    }
    // A unit gone from the machine along with its declaration has nothing to go back to.
    for unit in removed {
        if let Some((state, _)) = systemctl.observe(unit)? {
            actions.insert(
                unit.clone(),
                ServiceAction {
                    unit: unit.clone(),
                    change: Change::Preset,
                    invocation: Some(systemctl.invocation(&["preset"], unit)),
                    deferred: false,
                    state,
                },
            );
        }
    }
    out.extend(actions.into_values());
    Ok(manifest
        .services
        .iter()
        .map(|(unit, &enabled)| {
            let state = if enabled { "enabled" } else { "disabled" };
            (unit.clone(), state.to_string())
        })
        .collect())
}

/// Before the first service changes, every unit the plan waited for is looked for again: the
/// packages this apply installed are there now, and a unit none of them brought stops the apply
/// with `E_UNKNOWN_UNIT` and no service changed.
pub fn recheck<'a>(
    gate: &Gate,
    deferred: impl Iterator<Item = &'a ServiceAction>,
) -> Result<(), Diagnostic> {
    let mut deferred = deferred.peekable();
    if deferred.peek().is_none() {
        return Ok(());
    }
    let systemctl = Systemctl::find(gate)?;
    let mut problems = Vec::new();
    for action in deferred {
        match systemctl.observe(&action.unit)? {
            None => problems.push(problem(&action.unit, None)),
            Some((state, _)) if state != "enabled" && state != "disabled" => {
                problems.push(problem(&action.unit, Some(&state)));
            }
            Some(_) => {}
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(unknown(
        &problems,
        "the packages this switch installed stay, and no service changed",
    ))
}
