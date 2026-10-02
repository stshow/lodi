//! The three-way merge `lodi import` runs over a host file it wrote before (LD-378, LD-519).
//!
//! The **base** is the baseline the record keeps at the last sync — an import or an apply;
//! **ours** is the host file; **theirs** is the machine now, read exactly as an import reads
//! it. [`merge::merge`] decides, [`edit::apply`] makes the decision in the person's own file,
//! and [`diff::unified`] shows it. Names and digests are compared, never TOML text.
//!
//! The baseline is advanced by the import and by every apply ([`packages_baseline`],
//! [`files_baseline`]); without that, a name the person deleted after an import adopted it
//! would be adopted again by the next one.

pub mod diff;
pub mod edit;
pub mod merge;

use std::collections::{BTreeMap, BTreeSet};

use super::lock::{Baseline, FileRecord};
use super::pm::Observed;

/// The package half of the baseline a sync leaves: of the names the last baseline had and the
/// names the manifest declares, those the machine now has explicitly installed; and of the holds
/// the last baseline had and the manifest declares, those the machine now holds, at the version
/// it has. A family whose hold is not machine state records no hold.
pub fn packages_baseline(
    previous: Option<&Baseline>,
    declared: &[String],
    observed: &Observed,
    hold: &[String],
    hold_is_state: bool,
) -> (BTreeSet<String>, BTreeMap<String, String>) {
    let explicit =
        |name: &String| observed.installed.contains_key(name) && !observed.auto.contains(name);
    let mut names: BTreeSet<String> = previous
        .map(|b| b.explicit.iter().cloned().collect())
        .unwrap_or_default();
    names.extend(declared.iter().cloned());
    names.retain(explicit);
    let mut holds = BTreeMap::new();
    if hold_is_state {
        let mut held: BTreeSet<&String> = previous
            .map(|b| b.holds.keys().collect())
            .unwrap_or_default();
        held.extend(hold);
        for name in held {
            if observed.held.contains(name)
                && let Some(version) = observed.installed.get(name)
            {
                holds.insert(name.clone(), version.clone());
            }
        }
    }
    (names, holds)
}

/// The file half: each declared file as the record has it, and each path the last baseline had
/// that is no longer declared, carried as it was.
pub fn files_baseline(
    previous: Option<&Baseline>,
    records: &BTreeMap<String, FileRecord>,
) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = previous.map(|b| b.files.clone()).unwrap_or_default();
    for (path, record) in records {
        out.insert(path.clone(), record.digest.clone());
    }
    out
}
