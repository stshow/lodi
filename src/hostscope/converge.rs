//! Exact package convergence: `[host] packages = "exact"` (LD-375).
//!
//! An exact apply makes the machine's packages the manifest's, plus the distribution's own base
//! system. The **target** is a function of the manifest and of the machine; the record
//! (`host.lock`) is only a **fence** against changes made by hand, the way a managed file edited
//! by hand is drift.
//!
//! 1. **What may go.** `U` is what the package manager records as explicitly installed, from the
//!    distribution's own repositories, minus the base system the import subtracts
//!    ([`baseline::protected`]). `L` is what the record names. The candidates are
//!    `(U ∪ L) − D`, where `D` is what the manifest declares for this machine, plus every
//!    installed `absent` name. A package from elsewhere — a third-party repository, a package
//!    file, the AUR — is a candidate only when the record names it, because the manifest could
//!    never have put it back. A base-system name taken out of the manifest stays, and is said to.
//!    The machine these are read from is the one the transaction will leave (LD-412): what it
//!    installs counts as installed, with the record the repositories give it, so a declared
//!    metapackage a hand removal took off the machine protects its closure again the moment the
//!    apply puts it back, and nothing in that closure is a candidate.
//! 2. **What goes is the package manager's answer, read before anything moves.** On pacman a
//!    candidate is removable when every package in its `Required By` (as `pacman -Qi` resolves
//!    it, through what packages provide) is itself removable — a fixpoint — and `pacman -Rsp`
//!    then says what `-Rs` of those would take. On apt the exact `apt-get install -s` of the
//!    transaction is simulated; a candidate whose removal would take something outside the
//!    candidates is kept, and the simulation is run again; the packages apt would call "no
//!    longer required" after the change and not before are the orphans the change creates, and
//!    they are removed by name. A candidate something still needs is **demoted** to a
//!    dependency and stays, so that it leaves with the last package that needs it — and so is
//!    one that what the transaction installs needs: on pacman its `Required By` is read with the
//!    arriving packages in it, and on apt it is a candidate apt removes alone but refuses to
//!    remove in the transaction that installs what the manifest declares (LD-412).
//! 3. **What is refused.** A removal that would take a declared name, a base-system package, or a
//!    package from elsewhere the record never named is refused before anything changes — whether
//!    the removal of a candidate would take it, or apt's own transaction would, resolving the
//!    `Conflicts` of something it installs (LD-375 validator repair).
//! 4. **The fence.** A candidate the record never named — installed by hand, or brought into `U`
//!    by a change of the base system — is drift: `W_DRIFT` in the plan, `E_DECLINED` at the
//!    apply unless `--overwrite-drift`. Because the import writes the record, an edit of the
//!    manifest never needs that flag.
//!
//! Nothing here runs a command that changes the machine: every reading is a query or a
//! simulation, and what is decided is returned as names for [`super::plan`] to turn into
//! actions. No unrestricted `apt-get autoremove` and no `pacman -Qdtq` sweep exists anywhere.

use std::collections::{BTreeMap, BTreeSet};

use crate::diag::Diagnostic;

use super::import::{self, baseline};
use super::lock::HostLock;
use super::pm::{self, Backend, Observed, Origin};
use super::safety::Gate;

/// Why a package leaves, is kept, or is demoted: the words the plan prints beside it.
pub const NOT_DECLARED: &str = "not in the manifest";
pub const DRIFTED: &str = "not in the manifest, and installed by hand since lodi last recorded \
                           this machine";
pub const ABSENT: &str = "declared absent";
pub const TAKEN_ALONG: &str = "needed by nothing once the rest is removed";
pub const BASELINE: &str = "baseline; not removed";
pub const REPLACED: &str = "taken off by the package manager to install what the manifest declares";

/// The code `lodi host plan` and `apply` print, without `packages` under `[host]`, for each
/// explicitly installed distribution package the manifest does not declare.
pub const W_UNDECLARED: &str = "W_UNDECLARED";

/// What one exact plan decided about packages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Decision {
    /// Every name the package manager will take off the machine, sorted.
    pub removed: Vec<String>,
    /// The names the removal command is given: the rest is what the package manager's own rules
    /// take with them.
    pub roots: Vec<String>,
    /// The printed reason of every name removed or demoted.
    pub reasons: BTreeMap<String, String>,
    /// Explicitly installed names that stay as dependencies.
    pub demote: Vec<String>,
    /// `= package NAME (reason)` lines: what stays, and why.
    pub kept: Vec<String>,
    /// Candidates the record never named.
    pub drift: Vec<String>,
}

/// What the machine is, for the two questions this module asks of it.
struct Reading {
    /// Explicit, from the distribution's own repositories, and not its base system.
    chosen: BTreeSet<String>,
    protected: BTreeSet<String>,
    origins: BTreeMap<String, Origin>,
}

/// Read the machine as the transaction will leave it: `arriving` is what it installs, and
/// `explicit` the names of it the apply leaves explicitly installed. Nothing arriving is the
/// machine as it is.
fn read(
    gate: &Gate,
    backend: &dyn Backend,
    observed: &Observed,
    arriving: &pm::Survey,
    explicit: &BTreeSet<String>,
) -> Result<Reading, Diagnostic> {
    let mut survey = backend.survey()?;
    let origins = backend.origins(&baseline::chosen(observed))?;
    let observed = after_install(observed, &mut survey, arriving, explicit);
    let machine = import::Machine {
        distro: gate.distro,
        codename: gate.os.codename.clone(),
        observed,
        survey,
        origins,
        snaps: 0,
        flatpaks: 0,
        repositories: import::sources::Repositories::default(),
        users: Vec::new(),
        system: Vec::new(),
    };
    let mut protected = baseline::protected(&machine);
    // The network stacks and firewall tools the basics drive stay whatever exact mode says
    // (sd-1).
    // So do the bootloader's packages (bc-1); the running kernel the baseline keeps.
    protected.extend(
        super::basics::STACK
            .iter()
            .chain(super::kernel::LOADERS)
            .filter(|name| machine.observed.installed.contains_key(**name))
            .map(|name| (*name).to_string()),
    );
    let selection = baseline::select(&machine);
    let chosen = selection
        .common
        .into_iter()
        .filter(|name| !protected.contains(name))
        .collect();
    Ok(Reading {
        chosen,
        protected,
        origins: machine.origins,
    })
}

/// The machine as the transaction will leave it (LD-412): every package it installs is
/// installed, with the record the repositories give it; one the manifest asked for is
/// explicitly installed, as the apply marks it, unless `mark_auto` says otherwise; and what that
/// pulls in is a dependency. A package already installed keeps its own record and its mark.
fn after_install(
    observed: &Observed,
    survey: &mut pm::Survey,
    arriving: &pm::Survey,
    explicit: &BTreeSet<String>,
) -> Observed {
    let mut after = observed.clone();
    for (name, depends) in &arriving.depends {
        if observed.installed.contains_key(name) {
            continue;
        }
        after.installed.insert(name.clone(), String::new());
        if !explicit.contains(name) {
            after.auto.insert(name.clone());
        }
        if let Some(priority) = arriving.priority.get(name) {
            survey.priority.insert(name.clone(), *priority);
        }
        if arriving.metapackages.contains(name) {
            survey.metapackages.insert(name.clone());
        }
        if arriving.keep.contains(name) {
            survey.keep.insert(name.clone());
        }
        survey.depends.insert(name.clone(), depends.clone());
    }
    after
}

fn is_distribution(origin: Option<&Origin>) -> bool {
    matches!(
        origin,
        Some(Origin::Base | Origin::Suite(_) | Origin::Unchecked)
    )
}

/// The `W_UNDECLARED` lines of a manifest that does not say `packages` under `[host]`: one per
/// explicitly installed distribution package it neither declares nor records.
pub fn undeclared(
    gate: &Gate,
    backend: &dyn Backend,
    observed: &Observed,
    declared: &BTreeSet<String>,
    lock: Option<&HostLock>,
) -> Result<Vec<String>, Diagnostic> {
    let reading = read(
        gate,
        backend,
        observed,
        &pm::Survey::default(),
        &BTreeSet::new(),
    )?;
    Ok(reading
        .chosen
        .iter()
        .filter(|name| !declared.contains(*name))
        .filter(|name| !lock.is_some_and(|lock| lock.packages.contains_key(*name)))
        .map(|name| {
            format!(
                "{W_UNDECLARED}: {name} is explicitly installed and host.toml does not declare \
                 it, so apply leaves it; add packages = \"exact\" under [host] to have apply \
                 remove what host.toml does not declare, or declare {name}"
            )
        })
        .collect())
}

/// Decide an exact plan's package removals. `declared` is `D`; `absent` the installed names the
/// manifest declares absent; `install` what the transaction installs, which apt simulates with
/// the removals because it solves them together, and which the machine is read as having
/// (LD-412); `mark_auto` the names the apply marks as dependencies rather than explicit.
#[allow(clippy::too_many_arguments)]
pub fn decide(
    gate: &Gate,
    backend: &dyn Backend,
    observed: &Observed,
    lock: Option<&HostLock>,
    declared: &BTreeSet<String>,
    absent: &[String],
    install: &[String],
    mark_auto: &[String],
) -> Result<Decision, Diagnostic> {
    let arriving = backend.arriving(install)?;
    let explicit: BTreeSet<String> = install
        .iter()
        .filter(|name| !mark_auto.contains(*name))
        .cloned()
        .collect();
    let reading = read(gate, backend, observed, &arriving, &explicit)?;
    // What each package the transaction installs will need once it is installed, by name.
    let mut needs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (name, depends) in &arriving.depends {
        if observed.installed.contains_key(name) {
            continue;
        }
        for dependency in depends {
            needs
                .entry(dependency.clone())
                .or_default()
                .insert(name.clone());
        }
    }
    let recorded: BTreeSet<String> = lock
        .map(|lock| {
            lock.packages
                .keys()
                .filter(|name| observed.installed.contains_key(*name))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let absent: BTreeSet<String> = absent.iter().cloned().collect();

    let mut decision = Decision::default();
    let mut candidates: BTreeSet<String> = BTreeSet::new();
    for name in reading.chosen.union(&recorded) {
        if declared.contains(name) || absent.contains(name) {
            continue;
        }
        if reading.protected.contains(name) {
            decision.kept.push(format!("= package {name} ({BASELINE})"));
            continue;
        }
        if recorded.contains(name) {
            decision.reasons.insert(name.clone(), NOT_DECLARED.into());
        } else {
            decision.drift.push(name.clone());
            decision.reasons.insert(name.clone(), DRIFTED.into());
        }
        candidates.insert(name.clone());
    }
    for name in &absent {
        decision.reasons.insert(name.clone(), ABSENT.into());
        candidates.insert(name.clone());
    }

    let shape = pm::Shape::of(gate.distro);
    let (roots, simulation) = if shape.removals_are_a_second_action {
        by_requirement(backend, &candidates, &needs, &mut decision)?
    } else {
        by_simulation(
            backend,
            &candidates,
            install,
            &needs,
            &mut decision,
            |name| protection(name, declared, &reading, &recorded, gate),
        )?
    };

    // What the package manager would take, checked against what must stay.
    let taken: Vec<String> = simulation
        .removed
        .iter()
        .filter(|name| !candidates.contains(*name))
        .cloned()
        .collect();
    let origins = backend.origins(
        &taken
            .iter()
            .filter(|name| !reading.origins.contains_key(*name))
            .cloned()
            .collect::<Vec<_>>(),
    )?;
    let mut refused: Vec<String> = Vec::new();
    for name in &simulation.removed {
        if absent.contains(name) {
            continue;
        }
        let why = if declared.contains(name) {
            Some("the manifest declares it".to_string())
        } else if reading.protected.contains(name) {
            Some(format!(
                "it is part of {}'s base system",
                gate.distro.name()
            ))
        } else if !recorded.contains(name)
            && !reading.chosen.contains(name)
            && !is_distribution(reading.origins.get(name).or(origins.get(name)))
        {
            Some(format!(
                "lodi never recorded it and it is not from {}'s own repositories",
                gate.distro.name()
            ))
        } else {
            None
        };
        if let Some(why) = why {
            refused.push(format!("{name} ({why})"));
        }
    }
    if !refused.is_empty() && roots.is_empty() {
        // Nothing is being removed: the package manager's own transaction reaches it, to install
        // something that cannot stand beside it.
        return Err(Diagnostic::new(
            "E_DECLINED",
            format!(
                "installing {} would remove {}; nothing was changed",
                install.join(", "),
                refused.join(", ")
            ),
        )
        .hint(
            "the package manager removes an installed package that conflicts with one it \
             installs; take the new line out, or declare the conflicting package absent",
        ));
    }
    if !refused.is_empty() {
        return Err(Diagnostic::new(
            "E_DECLINED",
            format!(
                "removing {} would also remove {}; nothing was changed",
                roots.join(", "),
                refused.join(", ")
            ),
        )
        .hint(
            "the package manager takes a dependency along when nothing else needs it; put the \
             removed line back, or declare what still needs it",
        ));
    }
    for name in &simulation.removed {
        decision
            .reasons
            .entry(name.clone())
            .or_insert_with(|| TAKEN_ALONG.to_string());
    }
    decision.removed = simulation.removed.into_iter().collect();
    decision.roots = roots;
    let fated: BTreeSet<&String> = decision
        .removed
        .iter()
        .chain(decision.demote.iter())
        .collect();
    decision.drift.retain(|name| fated.contains(name));
    Ok(decision)
}

/// Why a name must not be removed, when it must not: what an apt orphan is checked against
/// before it is offered for removal at all.
fn protection(
    name: &str,
    declared: &BTreeSet<String>,
    reading: &Reading,
    recorded: &BTreeSet<String>,
    gate: &Gate,
) -> Option<String> {
    if declared.contains(name) {
        Some("declared".to_string())
    } else if reading.protected.contains(name) {
        Some("part of the base system".to_string())
    } else if !recorded.contains(name)
        && !reading.chosen.contains(name)
        && reading
            .origins
            .get(name)
            .is_some_and(|origin| !is_distribution(Some(origin)))
    {
        Some(format!(
            "not from {}'s own repositories and never recorded",
            gate.distro.name()
        ))
    } else {
        None
    }
}

/// pacman: the removable set is the fixpoint of the candidates whose every requirer is itself
/// removable; the rest are demoted. A package the transaction installs is a requirer too, and
/// never a removable one (LD-412). `pacman -Rsp` of that set is the answer.
fn by_requirement(
    backend: &dyn Backend,
    candidates: &BTreeSet<String>,
    needs: &BTreeMap<String, BTreeSet<String>>,
    decision: &mut Decision,
) -> Result<(Vec<String>, pm::Simulation), Diagnostic> {
    if candidates.is_empty() {
        return Ok((Vec::new(), pm::Simulation::default()));
    }
    let mut required_by = backend.required_by()?;
    for (name, by) in needs {
        required_by
            .entry(name.clone())
            .or_default()
            .extend(by.iter().cloned());
    }
    let empty = BTreeSet::new();
    let mut removable = candidates.clone();
    loop {
        let stays: Vec<String> = removable
            .iter()
            .filter(|name| {
                !required_by
                    .get(*name)
                    .unwrap_or(&empty)
                    .iter()
                    .all(|by| removable.contains(by))
            })
            .cloned()
            .collect();
        if stays.is_empty() {
            break;
        }
        for name in stays {
            removable.remove(&name);
        }
    }
    for name in candidates.difference(&removable) {
        let needed: Vec<String> = required_by
            .get(name)
            .unwrap_or(&empty)
            .iter()
            .filter(|by| !removable.contains(*by))
            .cloned()
            .collect();
        demote(decision, name, &needed);
    }
    let roots: Vec<String> = removable.into_iter().collect();
    let simulation = backend.simulate(&[], &roots)?;
    Ok((roots, simulation))
}

/// apt: simulate the exact transaction; a candidate whose removal reaches outside the candidates
/// is demoted and the simulation run again; then the orphans the change creates are added by
/// name, unless they must stay.
///
/// What the installs take off the machine on their own — apt resolves a `Conflicts` by removing —
/// is read first, from the transaction with no removal in it. It is part of every answer, is not
/// what a candidate reaches, and is held to what must stay by [`decide`] like any other removal
/// (LD-375 validator repair).
///
/// apt refuses the transaction outright — "held broken packages" — when a removal would break
/// what it installs. A candidate apt removes alone but will not remove in the transaction that
/// installs what the manifest declares is one that transaction needs, and is demoted: `needs`
/// names what needs it where the arriving records say, and otherwise the installs do (LD-412).
fn by_simulation(
    backend: &dyn Backend,
    candidates: &BTreeSet<String>,
    install: &[String],
    needs: &BTreeMap<String, BTreeSet<String>>,
    decision: &mut Decision,
    protect: impl Fn(&str) -> Option<String>,
) -> Result<(Vec<String>, pm::Simulation), Diagnostic> {
    if candidates.is_empty() && install.is_empty() {
        return Ok((Vec::new(), pm::Simulation::default()));
    }
    let installing = backend.simulate(install, &[])?;
    let replaced = installing.removed.clone();
    for name in &replaced {
        decision.reasons.insert(name.clone(), REPLACED.to_string());
    }
    let needed_by = |name: &String| -> Vec<String> {
        match needs.get(name) {
            Some(by) if !by.is_empty() => by.iter().cloned().collect(),
            _ => install.to_vec(),
        }
    };
    let mut remaining = candidates.clone();
    let simulation;
    loop {
        if remaining.is_empty() {
            return Ok((Vec::new(), installing));
        }
        let roots: Vec<String> = remaining.iter().cloned().collect();
        let reached: BTreeSet<String> = remaining.union(&replaced).cloned().collect();
        let answer = match backend.simulate(install, &roots) {
            Ok(answer) if answer.removed.is_subset(&reached) => {
                simulation = answer;
                break;
            }
            answer => answer,
        };
        // Which of them reaches outside, or is needed by what is installed: each asked alone.
        let mut demoted = false;
        for name in roots {
            let one = std::slice::from_ref(&name);
            let outside: Vec<String> = match backend.simulate(install, one) {
                Ok(alone) => alone
                    .removed
                    .iter()
                    .filter(|other| !remaining.contains(*other) && !replaced.contains(*other))
                    .cloned()
                    .collect(),
                Err(refused) => {
                    if install.is_empty() || backend.simulate(&[], one).is_err() {
                        return Err(refused);
                    }
                    needed_by(&name)
                }
            };
            if !outside.is_empty() {
                demote(decision, &name, &outside);
                remaining.remove(&name);
                demoted = true;
            }
        }
        if !demoted {
            // Only together do they reach outside, or does apt refuse them. Nothing is guessed:
            // all of them stay.
            let outside: Vec<String> = match answer {
                Ok(answer) => answer.removed.difference(&reached).cloned().collect(),
                Err(refused) => {
                    let all: Vec<String> = remaining.iter().cloned().collect();
                    if backend.simulate(&[], &all).is_err() {
                        return Err(refused);
                    }
                    install.to_vec()
                }
            };
            for name in std::mem::take(&mut remaining) {
                demote(decision, &name, &outside);
            }
        }
    }
    let before = installing.orphans;
    let mut roots: BTreeSet<String> = remaining.clone();
    for orphan in simulation.orphans.difference(&before) {
        match protect(orphan) {
            Some(why) => decision.kept.push(format!(
                "= package {orphan} (no longer needed, but {why}; not removed)"
            )),
            None => {
                roots.insert(orphan.clone());
            }
        }
    }
    let roots: Vec<String> = roots.into_iter().collect();
    let simulation = if roots.len() == remaining.len() {
        simulation
    } else {
        backend.simulate(install, &roots)?
    };
    Ok((roots, simulation))
}

fn demote(decision: &mut Decision, name: &str, needed_by: &[String]) {
    decision.demote.push(name.to_string());
    decision.reasons.insert(
        name.to_string(),
        format!(
            "a dependency now: needed by {}; stays",
            needed_by.join(", ")
        ),
    );
}
