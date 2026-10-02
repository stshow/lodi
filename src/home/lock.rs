//! A home's tool lock: the project lock schema (M-0.5 T-4, LD-105/LD-108), held in the config's
//! `lodi.lock` and never written by the home scope (LD-528). Reading delegates to
//! [`crate::lock`].

use crate::diag::Diagnostic;
use crate::lock::{self, LockFile, LockProblem};
use crate::tools::ToolSet;

/// What a home's lock is: its section of the config's `lodi.lock`.
const SECTION: &str = "the home's section of lodi.lock";

/// The home-specific rendering of a lock refusal. It keeps the shared codes and lock-version
/// wording, while a stale lock names the only command that is allowed to refresh it.
pub fn diagnostic(problem: &LockProblem) -> Diagnostic {
    match problem {
        LockProblem::Version { .. } | LockProblem::ConfigLock => {
            problem.diagnostic(lock::LOCK_FILE)
        }
        LockProblem::Missing => {
            Diagnostic::new("E_LOCK_STALE", format!("{SECTION} does not exist"))
                .hint("run lodi update to re-resolve it")
        }
        LockProblem::Invalid(why) => {
            Diagnostic::new("E_LOCK_STALE", format!("{SECTION} is invalid: {why}"))
                .hint("run lodi update to re-resolve it")
        }
        LockProblem::Stale(entries) => {
            let mut d = Diagnostic::new(
                "E_LOCK_STALE",
                format!("{SECTION} is out of date with the manifest"),
            );
            d.notes.extend(entries.iter().cloned());
            d.hint("run lodi update to re-resolve it")
        }
    }
}

/// The exact project freshness rule, over the shared tool declarations only.
pub fn staleness(tools: &ToolSet, lock: &LockFile) -> Vec<String> {
    lock::tools_staleness(tools, lock)
}
