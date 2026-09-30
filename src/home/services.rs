//! `[services]` of `home.toml`: the user's own systemd units (hc-1, LD-427).
//!
//! A unit declared with `enable = true` is enabled and running, and one with `enable = false` is
//! disabled and stopped, through `systemctl --user` run **as the home's user**: `lodi home apply`
//! is that user already, and `sudo lodi apply` runs each home in a process that dropped to it
//! for good. Nothing here ever talks to a root-owned user manager, so a root process with
//! services to plan is refused.
//!
//! Each unit Lodi manages is recorded in the home state with the state it had before Lodi first
//! managed it; a unit whose declaration is removed goes back to that state, and nothing else: a
//! unit never declared is never read or touched (H2). An inline `unit` is one more managed file
//! of the manifest, below `.config/systemd/user`, and exists only when the user writes one (H3).
//! Lingering is turned on only while a service asks for it with `linger = true`, and turned off
//! again only when Lodi was the one that turned it on (H1).
//!
//! `systemctl` and `loginctl` are resolved like the host scope's programs
//! ([`crate::hostscope::safety::resolve_program`]): from the fixed `PATH` when the home belongs
//! to the running system under `sudo lodi apply`, else from the caller's own `PATH`, which is
//! the user's. They run with the environment cleared, the fixed `PATH`, the user's own runtime
//! directory (and session bus, where there is one), and every unit after `--`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::diag::Diagnostic;
use crate::home::manifest::HomeManifest;
use crate::home::state::{HomeState, ServiceRecord};
use crate::hostscope::pm::{self, Invocation, noninteractive_env};
use crate::hostscope::safety;

/// How the home's process reaches its user manager, and who changes lingering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Context {
    /// Resolve the programs in the fixed `PATH` and use `/run/user/<uid>`: a home of the running
    /// system that `sudo lodi apply` dropped to.
    pub fixed_path: bool,
    /// Root runs `loginctl` for this home (`sudo lodi apply`): it turned lingering on before the
    /// home ran, and turns it off after the home asks for that in [`Services::linger_off`].
    pub linger_by_root: bool,
    /// Root turned lingering on for this home just before it ran.
    pub linger_enabled_by_root: bool,
}

/// What one service line does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Enable,
    Disable,
    /// A unit no longer declared, going back to the state it had before Lodi managed it.
    Restore,
    Unchanged,
}

/// One declared, or no longer declared, user unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub unit: String,
    pub change: Change,
    /// The command it runs; a declared unit runs it after the files, its own included.
    pub invocation: Option<Invocation>,
    /// The unit file's state as the plan saw it.
    pub state: String,
}

impl Step {
    pub fn line(&self) -> String {
        let verb = match self.change {
            Change::Enable => "+",
            Change::Disable => "-",
            Change::Restore => "~",
            Change::Unchanged => "=",
        };
        let detail = match &self.invocation {
            Some(invocation) => invocation.command_line(),
            None => self.state.clone(),
        };
        format!("{verb} service {} ({detail})", self.unit)
    }
}

/// The services part of a home plan. Empty for a home without `[services]` and without a record
/// of one, so such a home plans and prints exactly as before.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Services {
    /// Units no longer declared, restored before the files change, in unit order.
    pub restores: Vec<Step>,
    /// Declared units, enabled or disabled after the files, in unit order.
    pub steps: Vec<Step>,
    /// `systemctl --user daemon-reload`, when this apply writes or removes a unit file.
    pub reload: Option<Invocation>,
    /// `loginctl enable-linger` or `disable-linger`, and whether it enables.
    pub linger: Option<(bool, Invocation)>,
    /// Root turned lingering on for this home ([`Context::linger_enabled_by_root`]).
    pub linger_done: Option<Invocation>,
    /// Lingering is on and declared: the plan's `=` line.
    pub linger_kept: bool,
    /// What a converged apply records.
    pub records: BTreeMap<String, ServiceRecord>,
    pub linger_managed: bool,
}

impl Services {
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        if let Some(invocation) = &self.linger_done {
            lines.push(format!("+ linger ({})", invocation.command_line()));
        }
        if let Some((true, invocation)) = &self.linger {
            lines.push(format!("+ linger ({})", invocation.command_line()));
        }
        if self.linger_kept {
            lines.push("= linger (on)".into());
        }
        lines.extend(self.restores.iter().chain(&self.steps).map(Step::line));
        if let Some((false, invocation)) = &self.linger {
            lines.push(format!("- linger ({})", invocation.command_line()));
        }
        lines
    }

    /// Whether an apply would change nothing.
    pub fn is_empty(&self) -> bool {
        self.linger.is_none()
            && self.linger_done.is_none()
            && self
                .restores
                .iter()
                .chain(&self.steps)
                .all(|step| step.invocation.is_none())
    }

    /// Lingering this home's process leaves to root to turn off.
    pub fn linger_off(&self) -> bool {
        matches!(self.linger, Some((false, _)))
    }
}

/// The user's `systemctl --user` and `loginctl`, resolved for one process.
struct Manager {
    systemctl: PathBuf,
    env: Vec<(String, String)>,
    uid: u32,
    fixed_path: bool,
}

impl Manager {
    fn find(context: &Context) -> Result<Manager, Diagnostic> {
        // SAFETY: geteuid cannot fail and takes no arguments.
        let uid = unsafe { libc::geteuid() };
        if uid == 0 {
            return Err(Diagnostic::new(
                "E_CONFIG",
                "[services] runs as the home's own user, and this process is root",
            )
            .hint(
                "declare user services in the home of the user they run as; nothing was changed",
            ));
        }
        let runtime = match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
            Some(dir) if !context.fixed_path && dir.is_absolute() => dir,
            _ => PathBuf::from(format!("/run/user/{uid}")),
        };
        // `systemctl --user` talks to the manager's own socket first; a session bus is not
        // there on every distribution (Debian 12 ships none by default).
        let socket = runtime.join("systemd/private");
        if !crate::home::fsops::lexists(&socket) {
            return Err(Diagnostic::new(
                "E_NO_RUNTIME",
                format!(
                    "the systemd user manager of uid {uid} is not running: {} is not there",
                    socket.display()
                ),
            )
            .hint(
                "log in as that user, or set linger = true on a service and run sudo lodi apply; \
                 nothing was changed",
            ));
        }
        let mut env = noninteractive_env();
        env.push(("XDG_RUNTIME_DIR".into(), runtime.display().to_string()));
        let bus = runtime.join("bus");
        if crate::home::fsops::lexists(&bus) {
            env.push((
                "DBUS_SESSION_BUS_ADDRESS".into(),
                format!("unix:path={}", bus.display()),
            ));
        }
        Ok(Manager {
            systemctl: resolve("systemctl", context.fixed_path, uid)?,
            env,
            uid,
            fixed_path: context.fixed_path,
        })
    }

    /// `systemctl --user ARGS... -- UNIT`.
    fn invocation(&self, args: &[&str], unit: &str) -> Invocation {
        let argv: Vec<String> = ["--user"]
            .iter()
            .chain(args)
            .chain(&["--", unit])
            .map(|arg| (*arg).to_string())
            .collect();
        Invocation::resolved("systemctl", &self.systemctl, &argv, self.env.clone())
    }

    /// The unit file's state and whether the unit runs, or `None` when there is no such unit.
    fn observe(&self, unit: &str) -> Result<Option<(String, bool)>, Diagnostic> {
        let state = word(&self.invocation(&["is-enabled"], unit))?;
        if state.is_empty() || state == "not-found" {
            return Ok(None);
        }
        let active = word(&self.invocation(&["is-active"], unit))?;
        let running = matches!(
            active.as_str(),
            "active" | "activating" | "reloading" | "refreshing"
        );
        Ok(Some((state, running)))
    }

    /// `loginctl ARGS... UID`, with the manager's environment.
    fn loginctl(&self, args: &[&str]) -> Result<Invocation, Diagnostic> {
        let path = resolve("loginctl", self.fixed_path, self.uid)?;
        let argv: Vec<String> = args
            .iter()
            .map(|arg| (*arg).to_string())
            .chain([self.uid.to_string()])
            .collect();
        Ok(Invocation::resolved(
            "loginctl",
            &path,
            &argv,
            self.env.clone(),
        ))
    }

    fn lingering(&self) -> Result<bool, Diagnostic> {
        Ok(word(&self.loginctl(&["show-user", "--property=Linger", "--value"])?)? == "yes")
    }
}

fn resolve(program: &str, fixed_path: bool, uid: u32) -> Result<PathBuf, Diagnostic> {
    match safety::resolve_program(program, fixed_path, uid) {
        Ok(Some(path)) => Ok(path),
        Ok(None) => Err(Diagnostic::new(
            "E_NO_RUNTIME",
            format!(
                "`{program}` is not on {}, and home.toml declares [services]",
                if fixed_path {
                    safety::FIXED_PATH.to_string()
                } else {
                    "PATH".to_string()
                }
            ),
        )
        .hint("[services] needs a machine that runs systemd; nothing was changed")),
        Err(why) => Err(safety::untrusted_runtime(program, &why)),
    }
}

/// The first word a reading command printed. `is-enabled`, `is-active` and `show-user` answer
/// with a word and an exit status that means something other than failure.
fn word(invocation: &Invocation) -> Result<String, Diagnostic> {
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

fn problem(unit: &str, state: Option<&str>) -> String {
    match state {
        None => format!("no user unit {unit}"),
        Some(state) => format!("{unit} is {state}, which systemctl cannot enable or disable"),
    }
}

/// Plan the services of `manifest` against `state`. `written` are the unit files this apply
/// writes or removes, by unit name. Reads only; runs nothing at all for a home with no
/// services declared or recorded.
pub fn plan(
    manifest: &HomeManifest,
    state: &HomeState,
    written: &[String],
    context: &Context,
) -> Result<Services, Diagnostic> {
    let mut out = Services::default();
    if manifest.services.is_empty()
        && state.services.is_empty()
        && !manifest.linger
        && !state.linger
    {
        return Ok(out);
    }
    let manager = Manager::find(context)?;
    let mut problems = Vec::new();
    for (unit, &enable) in &manifest.services {
        let (args, change): (&[&str], Change) = if enable {
            (&["enable", "--now"], Change::Enable)
        } else {
            (&["disable", "--now"], Change::Disable)
        };
        let (change, word) = match manager.observe(unit)? {
            None if written.contains(unit) => (change, "absent".to_string()),
            None => {
                problems.push(problem(unit, None));
                continue;
            }
            Some((word, _)) if word != "enabled" && word != "disabled" => {
                problems.push(problem(unit, Some(&word)));
                continue;
            }
            Some((word, running)) if (word == "enabled") == enable && running == enable => {
                (Change::Unchanged, word)
            }
            Some((word, _)) => (change, word),
        };
        let record = state.services.get(unit).cloned().unwrap_or(ServiceRecord {
            before: word.clone(),
        });
        out.records.insert(unit.clone(), record);
        out.steps.push(Step {
            unit: unit.clone(),
            invocation: (change != Change::Unchanged).then(|| manager.invocation(args, unit)),
            change,
            state: word,
        });
    }
    if !problems.is_empty() {
        return Err(Diagnostic::new("E_UNKNOWN_UNIT", problems.join("; ")).hint(
            "nothing changed; correct the name, write the unit inline with `unit`, or install \
             the program that ships it",
        ));
    }
    for (unit, record) in &state.services {
        if manifest.services.contains_key(unit) {
            continue;
        }
        // A unit gone along with its declaration has nothing to go back to.
        let Some((word, running)) = manager.observe(unit)? else {
            continue;
        };
        let enable = record.before == "enabled";
        let at_rest = (word == "enabled") == enable && running == enable;
        let args: &[&str] = if enable {
            &["enable", "--now"]
        } else {
            &["disable", "--now"]
        };
        out.restores.push(Step {
            unit: unit.clone(),
            change: if at_rest {
                Change::Unchanged
            } else {
                Change::Restore
            },
            invocation: (!at_rest).then(|| manager.invocation(args, unit)),
            state: word,
        });
    }
    if !written.is_empty() {
        let argv = ["--user".to_string(), "daemon-reload".to_string()];
        out.reload = Some(Invocation::resolved(
            "systemctl",
            &manager.systemctl,
            &argv,
            manager.env.clone(),
        ));
    }
    plan_linger(&manager, manifest, state, context, &mut out)?;
    Ok(out)
}

/// H1: lingering on while a service asks for it; off again only when Lodi turned it on.
fn plan_linger(
    manager: &Manager,
    manifest: &HomeManifest,
    state: &HomeState,
    context: &Context,
    out: &mut Services,
) -> Result<(), Diagnostic> {
    if !manifest.linger && !state.linger {
        return Ok(());
    }
    let on = manager.lingering()?;
    if manifest.linger {
        if context.linger_enabled_by_root {
            out.linger_done = Some(manager.loginctl(&["enable-linger"])?);
            out.linger_managed = true;
        } else if on {
            out.linger_kept = true;
            out.linger_managed = state.linger;
        } else {
            out.linger = Some((true, manager.loginctl(&["enable-linger"])?));
            out.linger_managed = true;
        }
    } else if on {
        out.linger = Some((false, manager.loginctl(&["disable-linger"])?));
    }
    Ok(())
}

/// Run the steps before the files change: lingering on, then the units going back.
pub fn apply_before(services: &Services) -> Result<(), Diagnostic> {
    if let Some((true, invocation)) = &services.linger {
        pm::run(invocation)?;
    }
    for step in &services.restores {
        if let Some(invocation) = &step.invocation {
            pm::run(invocation)?;
        }
    }
    Ok(())
}

/// Run the steps after the files changed: the reload, the declared units, lingering off unless
/// root turns it off.
pub fn apply_after(services: &Services, context: &Context) -> Result<(), Diagnostic> {
    if let Some(invocation) = &services.reload {
        pm::run(invocation)?;
    }
    for step in &services.steps {
        if let Some(invocation) = &step.invocation {
            pm::run(invocation)?;
        }
    }
    if let Some((false, invocation)) = &services.linger
        && !context.linger_by_root
    {
        pm::run(invocation)?;
    }
    Ok(())
}
