//! `<config>/home.lock`: the project lock schema, parser and canonical bytes, with the home
//! scope's filenames and containment-safe writes (M-0.5 T-4, LD-105/LD-108).
//!
//! Reading delegates to [`crate::lock`]. Writing and removal go through [`crate::home::fsops`],
//! because the configuration directory is a home-scope root and no sibling module may mutate it
//! directly.

use crate::diag::Diagnostic;
use crate::home::fsops::{self, RelPath};
use crate::lock::{self, HOME_LOCK_FILE, LockFile, LockProblem};
use crate::roots::Roots;
use crate::tools::ToolSet;

fn path() -> RelPath {
    RelPath::new(HOME_LOCK_FILE).expect("the home lock is directly below the configuration root")
}

/// Read and validate `<config>/home.lock` through the project lock's parser.
pub fn read(roots: &Roots) -> Result<LockFile, LockProblem> {
    lock::read_home_lock(roots.config())
}

/// The home-specific rendering of a lock refusal. It keeps the shared codes and lock-version
/// wording, while a stale lock names the only command that is allowed to refresh it.
pub fn diagnostic(problem: &LockProblem) -> Diagnostic {
    match problem {
        LockProblem::Version { .. } => problem.diagnostic(HOME_LOCK_FILE),
        LockProblem::Missing => {
            Diagnostic::new("E_LOCK_STALE", format!("{HOME_LOCK_FILE} does not exist"))
                .hint("run lodi home apply without --locked to re-resolve")
        }
        LockProblem::Invalid(why) => Diagnostic::new(
            "E_LOCK_STALE",
            format!("{HOME_LOCK_FILE} is invalid: {why}"),
        )
        .hint("run lodi home apply without --locked to re-resolve"),
        LockProblem::Stale(entries) => {
            let mut d = Diagnostic::new(
                "E_LOCK_STALE",
                format!("{HOME_LOCK_FILE} is out of date with the manifest"),
            );
            d.notes.extend(entries.iter().cloned());
            d.hint("run lodi home apply without --locked to re-resolve")
        }
    }
}

/// The exact project freshness rule, over the shared tool declarations only.
pub fn staleness(tools: &ToolSet, lock: &LockFile) -> Vec<String> {
    lock::tools_staleness(tools, lock)
}

/// Write canonical lock bytes atomically through the configuration root's type wall.
pub fn write(roots: &Roots, lock: &LockFile) -> Result<(), Diagnostic> {
    fsops::write(
        &roots.config_root(),
        &path(),
        lock.to_canonical_json().as_bytes(),
        0o644,
    )
}

/// Remove an obsolete lock when an unlocked resolution sees no `[tools]`. A missing lock is
/// already the desired state.
pub fn remove(roots: &Roots) -> Result<(), Diagnostic> {
    fsops::remove(&roots.config_root(), &path())
}
