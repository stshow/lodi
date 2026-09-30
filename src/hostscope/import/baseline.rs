//! Baseline subtraction: from everything installed, to the short list a person actually chose.
//!
//! The rule is one rule with two family-shaped inputs, and both inputs are readings of the
//! machine itself — never a downloaded image manifest and never a table shipped in the binary
//! (design call D3, LD-271):
//!
//! 1. the **candidate set** is `Observed::installed − Observed::auto`: what the package manager
//!    itself records as explicitly wanted. On apt that is what is not `apt-mark showauto`, on
//!    pacman what `-Qe` lists. It needs no new process (`CURRENT_STATE.md`);
//! 2. a name the distribution itself calls `required`, `important` or `standard` is the
//!    installer's, not the person's, and leaves (apt; pacman states no priority);
//! 3. an installed **metapackage** of the distribution's own — a section of `metapackages` on
//!    apt, `base` on pacman — is **kept**, and the whole `Depends`/`Recommends` closure it
//!    reaches is dropped, because declaring the metapackage already declares all of it;
//! 4. the family's kernels and firmware survive the subtraction whatever else it takes
//!    ([`pm::Survey::keep`]), because a machine restored without them does not boot;
//! 5. what is left is classified by the **origin of its installed version** ([`pm::Origin`]):
//!    a name the distribution's own repositories serve is `[packages] common`, and everything
//!    else — a package from an enabled third-party repository or a PPA, one installed from a
//!    package file or from a source the machine no longer has, an AUR or hand-built package —
//!    is named under its class in the comment block and **never** declared (D4, LD-272).
//!
//! Rule 5 was "the enabled repositories offer a candidate for it" until R-2. On apt that test
//! was true of everything installed, because `apt-cache policy` counts `/var/lib/dpkg/status`
//! as a source, so every Docker, PPA and hand-installed package was declared and every restore
//! of such a file stopped at `E_UNKNOWN_PACKAGE` on the new machine. The decision record
//! supersedes the old rule by name.

use std::collections::{BTreeMap, BTreeSet};

use super::super::pm;
use super::super::safety::Distro;
use super::Machine;

/// One name and the label its class prints beside it: a third-party source's own label, or
/// the suite a declared name's installed version really came from.
pub type Labelled = (String, String);

/// What one machine's readings come to: the lists the emitter writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// `[packages] common`, sorted.
    pub common: Vec<String>,
    /// `[packages] hold`, sorted: the names of [`pm::Observed::held`] that `common` carries, so
    /// that the emitted file holds only what it also declares. Empty on a family whose hold is
    /// not machine state, which the emitter says in a comment rather than omitting silently.
    pub hold: Vec<String>,
    /// Declared, and installed here from one of the distribution's own suites that a fresh
    /// install does not enable (`bookworm-backports`). The name is in `common`; the emitted
    /// file says beside it that the **version** a fresh machine gets will be another one.
    pub from_other_suite: Vec<Labelled>,
    /// Class A: chosen, survived the subtraction, and installed from an enabled repository that
    /// is not the distribution's own — a third-party repository or a PPA, by its label.
    pub third_party: Vec<Labelled>,
    /// Class B: chosen, survived the subtraction, and its installed version is known to the
    /// local package database alone — a package file installed by hand, or a source the machine
    /// no longer has.
    pub local_only: Vec<String>,
    /// Class C: chosen, survived the subtraction, and the package manager itself says it came
    /// from no repository it knows (`pacman -Qqem`: an AUR build, a local build).
    pub foreign: Vec<String>,
    /// True when the machine has no package index at all ([`pm::Origin::Unchecked`]), so no
    /// name's origin could be checked and every one of them is declared as read. It is a
    /// property of the machine and not of a name: the backend answers `Unchecked` for all of
    /// them or for none.
    pub unchecked: bool,
}

impl Selection {
    /// How many names the emitted file names but does not declare — the number the counts line
    /// at the top of the `NOT CAPTURED` block states, and the number of `W_UNCAPTURED` lines an
    /// import prints for packages.
    pub fn not_captured(&self) -> usize {
        self.third_party.len() + self.local_only.len() + self.foreign.len()
    }

    /// One `W_UNCAPTURED` line per left-out package, in the order the emitted file names them:
    /// the classes in the file's order, and each class's names sorted. A machine with no
    /// package index leaves nothing out and warns once, first, with its own code.
    ///
    /// A warning never changes an exit status (`docs/ERRORS.md`): an import whose every chosen
    /// name is off-base still exits 0, having said so once per name.
    pub fn warnings(&self, distro: Distro) -> Vec<String> {
        let mut out = Vec::new();
        if self.unchecked {
            out.push(format!(
                "{}: this machine has no package index, so where the declared names came \
                 from was not checked: every name is declared as read. Fetch the index and \
                 import again to have the names no base repository serves named instead.",
                W_UNCHECKED
            ));
        }
        for (name, label) in &self.third_party {
            out.push(warning(
                name,
                &format!(
                    "it is installed from {label}, which is not {}'s own repository",
                    distro.name()
                ),
            ));
        }
        for name in &self.local_only {
            out.push(warning(
                name,
                "its installed version is known only to this machine's own package \
                 database: a package file installed by hand, or a source this machine no \
                 longer has",
            ));
        }
        for name in &self.foreign {
            out.push(warning(
                name,
                "the package manager itself says it came from no repository it knows",
            ));
        }
        out
    }
}

/// The code the import prints once when the machine has no package index at all. It is its own
/// code and not `W_UNCAPTURED`, because nothing was left out: everything was declared, and what
/// is missing is the check, not the name (`docs/ERRORS.md`).
pub const W_UNCHECKED: &str = "W_UNCHECKED_ORIGIN";

/// One package's `W_UNCAPTURED` line, in the shape `files.rs` already prints for a refused
/// configuration file: the code, what was left out, and why in one sentence.
fn warning(name: &str, why: &str) -> String {
    format!(
        "{}: {name} is not declared: {why}",
        super::files::W_UNCAPTURED
    )
}

/// The candidate set: what the package manager records as explicitly wanted, sorted.
pub fn chosen(observed: &pm::Observed) -> Vec<String> {
    observed
        .installed
        .keys()
        .filter(|name| !observed.auto.contains(*name))
        .cloned()
        .collect()
}

/// Apply the five rules above to one machine's readings.
pub fn select(machine: &Machine) -> Selection {
    let chosen: BTreeSet<String> = chosen(&machine.observed).into_iter().collect();
    let survey = &machine.survey;

    // The metapackages this machine has and this person chose. One that is installed only
    // because something else pulled it in is not a declaration anybody made.
    let seeds: BTreeSet<&String> = survey
        .metapackages
        .iter()
        .filter(|name| chosen.contains(*name))
        .collect();
    let covered = closure(&seeds, &survey.depends);

    let mut selection = Selection::default();
    for name in &chosen {
        // A kernel and a metapackage survive every subtraction: the first because a machine
        // without it does not boot, the second because it is the declaration that stands for the
        // closure being dropped.
        let unconditional = survey.keep.contains(name) || seeds.contains(name);
        let subtracted = survey
            .priority
            .get(name)
            .is_some_and(|priority| priority.is_baseline())
            || covered.contains(name);
        if subtracted && !unconditional {
            continue;
        }
        // `pacman -Qqem` is the one family's own answer to "this came from no repository I
        // know", and it is read before the origin table because no `-Si` record exists for
        // such a name to classify.
        if survey.foreign.contains(name) {
            selection.foreign.push(name.clone());
            continue;
        }
        match machine.origins.get(name) {
            Some(pm::Origin::Base) => selection.common.push(name.clone()),
            Some(pm::Origin::Suite(suite)) => {
                selection.common.push(name.clone());
                selection
                    .from_other_suite
                    .push((name.clone(), suite.clone()));
            }
            Some(pm::Origin::ThirdParty(label)) => {
                selection.third_party.push((name.clone(), label.clone()));
            }
            // No index: the machine could not be asked where anything came from, so the name
            // is declared as read and the emitted file says above the list that it was not
            // checked. Leaving it out instead would emit an empty file for a cloud image.
            Some(pm::Origin::Unchecked) => {
                selection.common.push(name.clone());
                selection.unchecked = true;
            }
            // A name the backend could say nothing about is the local-only answer, which is
            // what "known to this machine's database and to no repository" means.
            Some(pm::Origin::LocalOnly) | None => selection.local_only.push(name.clone()),
        }
    }
    selection.hold = selection
        .common
        .iter()
        .filter(|name| machine.observed.held.contains(*name))
        .cloned()
        .collect();
    selection
}

/// The distribution's own base system on this machine, as the import subtracts it: every name
/// the distribution itself calls `required`, `important` or `standard`, every chosen
/// metapackage of its own and everything that metapackage reaches, and the kernels and
/// firmware (LD-375). An exact apply never removes one of these because a line left the
/// manifest, and refuses a removal that would take one with it.
pub fn protected(machine: &Machine) -> BTreeSet<String> {
    let chosen: BTreeSet<String> = chosen(&machine.observed).into_iter().collect();
    let survey = &machine.survey;
    let seeds: BTreeSet<&String> = survey
        .metapackages
        .iter()
        .filter(|name| chosen.contains(*name))
        .collect();
    let mut out = closure(&seeds, &survey.depends);
    out.extend(seeds.into_iter().cloned());
    out.extend(survey.keep.iter().cloned());
    out.extend(
        survey
            .priority
            .iter()
            .filter(|(_, priority)| priority.is_baseline())
            .map(|(name, _)| name.clone()),
    );
    out.retain(|name| machine.observed.installed.contains_key(name));
    out
}

/// Every name the seeds reach through `depends`, transitively, **excluding the seeds**.
///
/// It is a breadth-first walk over a map that is finite and already in memory, and a name is
/// enqueued at most once, so a dependency cycle — which both families allow — terminates.
fn closure(
    seeds: &BTreeSet<&String>,
    depends: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeSet<String> {
    let mut reached: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<String> = seeds
        .iter()
        .filter_map(|seed| depends.get(*seed))
        .flatten()
        .cloned()
        .collect();
    while let Some(name) = queue.pop() {
        if seeds.iter().any(|seed| **seed == name) || !reached.insert(name.clone()) {
            continue;
        }
        if let Some(next) = depends.get(&name) {
            queue.extend(next.iter().cloned());
        }
    }
    reached
}
