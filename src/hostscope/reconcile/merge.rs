//! The reconcile rules as **one pure function** (LD-378): [`merge`] takes the base, the manifest
//! and the machine as values and returns what changes in the manifest. It reads no file, runs no
//! command and knows nothing of TOML text; the caller turns its answer into edits.
//!
//! Names are merged as set membership, which cannot conflict: each of base, ours and theirs says
//! yes or no. Three things can: an apt hold (a hold is a version as well as a yes), a name the
//! manifest keeps off the machine that was installed by hand, a file's bytes, and a file's mode,
//! owner and group. A conflict keeps the manifest's side and is named by one `W_RECONCILE` line;
//! nothing is guessed.
//!
//! A file's bytes and its metadata are merged apart. The base of its bytes is the baseline's
//! digest; the base of its mode, owner and group is the lock's own record of the file (`files`,
//! a field of schema 1 and 2), so a metadata change needs no new baseline member and the schema
//! stays 2 (validator-repair-1).
//!
//! | Change | Result |
//! |---|---|
//! | installed outside Lodi from the distribution (or auto → explicit) | added to `common` |
//! | installed outside Lodi from a third party, a package file or the AUR | a `NOT CAPTURED` line |
//! | removed outside Lodi (or explicit → auto) | taken out of every list |
//! | a line deleted and never applied | stays deleted |
//! | a line added and never applied | kept |
//! | a hold changed on both sides differently | conflict, the manifest's side |
//! | an `absent` or per-distribution `remove` name installed by hand | conflict, the manifest's side |
//! | a file's bytes changed on both sides differently | conflict, the manifest's side |
//! | a file's bytes changed only on the machine | named, never copied (si-1) |
//! | a file's mode, owner or group changed only on the machine | named, never copied (si-1) |
//! | a file's mode, owner or group changed on both sides differently | conflict, the manifest's side |
//! | a file edited only in the manifest, bytes or metadata | kept |
//! | a declared file missing from the machine | reported, and its declaration kept |
//! | a declared name now installed from a third party | kept, with a warning |

use std::collections::{BTreeMap, BTreeSet};

use crate::hostscope::lock::Baseline;

/// The warning code of every reconcile line: a conflict, or a decision the person should see.
pub const W_RECONCILE: &str = "W_RECONCILE";

/// The manifest's side: what it declares, in the terms the merge compares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ours {
    /// What the manifest installs on this machine: `common` and the `add` lists, less the
    /// `remove` lists of this distribution and architecture.
    pub declared: BTreeSet<String>,
    /// `absent`, and this machine's per-distribution and per-architecture `remove` lists: the
    /// names the manifest keeps off it.
    pub kept_off: BTreeSet<String>,
    /// Every name any list names, `absent` and `remove` excepted: what a removal takes out.
    pub listed: BTreeSet<String>,
    pub hold: BTreeSet<String>,
    pub mark_auto: BTreeSet<String>,
    /// Each declared, present file.
    pub files: BTreeMap<String, OurFile>,
    /// Every path `[files]` names, in any state.
    pub file_paths: BTreeSet<String>,
}

/// One declared file, as the manifest has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OurFile {
    /// The `sha256:` digest of the bytes the manifest declares, `None` when its `source` cannot
    /// be read.
    pub digest: Option<String>,
    /// `content` inline in the manifest rather than a `source` beside it.
    pub inline: bool,
    /// The mode, owner and group it declares.
    pub meta: Meta,
}

/// A file's mode, owner and group, the owner and group as the ids the root's own `passwd` and
/// `group` give them, so that `root` and `0` are the same owner. `None` is a name the root does
/// not know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub mode: u32,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

/// The machine's side, as an import reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Theirs {
    /// Explicitly installed from the distribution's own repositories, less its base system: what
    /// an import declares.
    pub chosen: BTreeSet<String>,
    /// Every explicitly installed name, from anywhere.
    pub explicit: BTreeSet<String>,
    /// Explicitly installed from elsewhere, with the words that say from where.
    pub elsewhere: BTreeMap<String, String>,
    /// The held names the manifest could declare, with their versions; `None` on a family whose
    /// hold is not machine state, where the manifest alone decides.
    pub holds: Option<BTreeMap<String, String>>,
    /// Each path of interest's digest on the machine, `None` when it is not there.
    pub files: BTreeMap<String, Option<String>>,
    /// Each path of interest's mode, owner and group on the machine, when it is there.
    pub meta: BTreeMap<String, Meta>,
    /// The configuration files the capture took, by path.
    pub captured: BTreeSet<String>,
}

/// What a reconcile changes in the manifest, and what it says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Names to add to `common`, sorted.
    pub adopt: Vec<String>,
    /// Names to take out of every list, sorted.
    pub remove: Vec<String>,
    pub hold_add: Vec<String>,
    pub hold_remove: Vec<String>,
    /// Declared paths whose bytes are captured again from the machine.
    pub recapture: Vec<String>,
    /// Declared paths whose mode, owner and group are taken again from the machine; a path whose
    /// bytes are captured again may be here too.
    pub metadata: Vec<String>,
    /// Captured paths to declare for the first time.
    pub declare: Vec<String>,
    /// How many of the person's own edits the machine disagrees with and were kept.
    pub kept: usize,
    /// One `W_RECONCILE` line per conflict: the manifest's side was kept.
    pub conflicts: Vec<String>,
    /// `W_RECONCILE` lines that are not conflicts: a missing file, an origin that moved, a
    /// change that is reported and not captured.
    pub notes: Vec<String>,
}

impl Outcome {
    /// How many names or files it adds to the manifest.
    pub fn added(&self) -> usize {
        let metadata_only = self
            .metadata
            .iter()
            .filter(|path| !self.recapture.contains(path))
            .count();
        self.adopt.len()
            + self.hold_add.len()
            + self.declare.len()
            + self.recapture.len()
            + metadata_only
    }

    /// How many it takes out of it.
    pub fn removed(&self) -> usize {
        self.remove.len() + self.hold_remove.len()
    }

    /// Whether the manifest changes at all.
    pub fn changes_manifest(&self) -> bool {
        self.added() + self.removed() > 0
    }
}

/// The three-way merge. `base` is the baseline of the last sync, `recorded` the mode, owner and
/// group the lock records for each file at that sync, `ours` the manifest, `theirs` the machine
/// now.
pub fn merge(
    base: &Baseline,
    recorded: &BTreeMap<String, Meta>,
    ours: &Ours,
    theirs: &Theirs,
) -> Outcome {
    let mut out = Outcome::default();
    packages(base, ours, theirs, &mut out);
    holds(base, ours, theirs, &mut out);
    files(base, recorded, ours, theirs, &mut out);
    out
}

fn packages(base: &Baseline, ours: &Ours, theirs: &Theirs, out: &mut Outcome) {
    let mut names: BTreeSet<&String> = BTreeSet::new();
    names.extend(&base.explicit);
    names.extend(&ours.declared);
    names.extend(&ours.listed);
    names.extend(&ours.kept_off);
    names.extend(&theirs.explicit);
    for name in names {
        let in_base = base.explicit.contains(name);
        let declared = ours.declared.contains(name);
        let on_machine = theirs.explicit.contains(name);
        if !in_base && theirs.chosen.contains(name) && !declared {
            if ours.kept_off.contains(name) {
                out.conflicts.push(format!(
                    "{W_RECONCILE}: package {name} was installed by hand, and host.toml keeps it \
                     off this machine (absent, or a remove list); host.toml is kept, and the next \
                     apply removes it"
                ));
            } else {
                // Installed outside Lodi from the distribution, or marked explicit by hand.
                out.adopt.push(name.clone());
            }
            continue;
        }
        if in_base && !on_machine {
            // Removed outside Lodi, or marked as a dependency by hand.
            if ours.listed.contains(name) {
                out.remove.push(name.clone());
            }
            continue;
        }
        if declared
            && on_machine
            && let Some(origin) = theirs.elsewhere.get(name)
        {
            out.notes.push(format!(
                "{W_RECONCILE}: package {name} is declared, and this machine now has it {origin}; \
                 the declaration is kept"
            ));
            continue;
        }
        if in_base && !declared && on_machine {
            // The owner's `bat`: a line deleted and never applied stays deleted.
            out.kept += 1;
        } else if !in_base && declared && !on_machine && !ours.mark_auto.contains(name) {
            // A line added and never applied is kept.
            out.kept += 1;
        }
    }
}

fn holds(base: &Baseline, ours: &Ours, theirs: &Theirs, out: &mut Outcome) {
    let Some(machine) = &theirs.holds else {
        return;
    };
    let mut names: BTreeSet<&String> = BTreeSet::new();
    names.extend(base.holds.keys());
    names.extend(&ours.hold);
    names.extend(machine.keys());
    for name in names {
        let was = base.holds.get(name);
        let ours_held = ours.hold.contains(name);
        let now = machine.get(name);
        let ours_changed = ours_held != was.is_some();
        let theirs_changed = now != was;
        if !theirs_changed {
            if ours_changed {
                out.kept += 1;
            }
            continue;
        }
        if !ours_changed {
            match (ours_held, now.is_some()) {
                (false, true) => out.hold_add.push(name.clone()),
                // A name removed from the machine leaves `hold` with its other lists.
                (true, false) if !out.remove.contains(name) => {
                    out.hold_remove.push(name.clone());
                }
                _ => {}
            }
            continue;
        }
        if ours_held != now.is_some() {
            out.conflicts.push(format!(
                "{W_RECONCILE}: the hold on {name} changed in host.toml and on this machine, \
                 differently; host.toml is kept"
            ));
        }
    }
}

fn files(
    base: &Baseline,
    recorded: &BTreeMap<String, Meta>,
    ours: &Ours,
    theirs: &Theirs,
    out: &mut Outcome,
) {
    for (path, file) in &ours.files {
        let now = theirs.files.get(path).cloned().flatten();
        let Some(now) = now else {
            out.notes.push(format!(
                "{W_RECONCILE}: {path} is declared and missing from this machine; the \
                 declaration is kept"
            ));
            continue;
        };
        let Some(declared) = &file.digest else {
            out.notes.push(format!(
                "{W_RECONCILE}: {path}'s source cannot be read; the declaration is kept"
            ));
            continue;
        };
        let mut bytes_taken = false;
        if *declared != now {
            let was = base.files.get(path);
            if was == Some(declared) {
                if file.inline {
                    out.notes.push(format!(
                        "{W_RECONCILE}: {path} changed on this machine, and host.toml declares \
                         its content inline; it is reported and not captured"
                    ));
                } else {
                    out.recapture.push(path.clone());
                    bytes_taken = true;
                }
            } else if was == Some(&now) || was.is_none() {
                out.kept += 1;
            } else {
                out.conflicts.push(format!(
                    "{W_RECONCILE}: {path} changed on this machine and in host.toml, \
                     differently; host.toml is kept"
                ));
            }
        }
        let Some(machine) = theirs.meta.get(path) else {
            continue;
        };
        match recorded.get(path) {
            // No record of the metadata: bytes captured again bring the machine's with them.
            None => {
                if bytes_taken && *machine != file.meta {
                    out.metadata.push(path.clone());
                }
            }
            Some(_) if *machine == file.meta => {}
            // The machine alone changed it.
            Some(was) if file.meta == *was => out.metadata.push(path.clone()),
            // The manifest alone changed it.
            Some(was) if machine == was => out.kept += 1,
            Some(_) => out.conflicts.push(format!(
                "{W_RECONCILE}: {path}'s mode, owner or group changed on this machine and in \
                 host.toml, differently; host.toml is kept"
            )),
        }
    }
    for path in &theirs.captured {
        if ours.file_paths.contains(path) {
            continue;
        }
        if base.files.contains_key(path) {
            // A declaration the person deleted stays deleted.
            out.kept += 1;
        } else {
            out.declare.push(path.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The merge with no recorded metadata: the rows that are not about a file's metadata.
    fn merge(base: &Baseline, ours: &Ours, theirs: &Theirs) -> Outcome {
        super::merge(base, &BTreeMap::new(), ours, theirs)
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(ToString::to_string).collect()
    }

    fn base(explicit: &[&str]) -> Baseline {
        Baseline {
            explicit: set(explicit),
            ..Baseline::default()
        }
    }

    fn ours(declared: &[&str]) -> Ours {
        Ours {
            declared: set(declared),
            listed: set(declared),
            ..Ours::default()
        }
    }

    fn theirs(chosen: &[&str]) -> Theirs {
        Theirs {
            chosen: set(chosen),
            explicit: set(chosen),
            holds: Some(BTreeMap::new()),
            ..Theirs::default()
        }
    }

    fn held(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    }

    const META: Meta = Meta {
        mode: 0o644,
        uid: Some(0),
        gid: Some(0),
    };

    fn file(digest: &str) -> OurFile {
        OurFile {
            digest: Some(digest.to_string()),
            inline: false,
            meta: META,
        }
    }

    fn with_file(mut ours: Ours, path: &str, digest: &str) -> Ours {
        ours.files.insert(path.to_string(), file(digest));
        ours.file_paths.insert(path.to_string());
        ours
    }

    fn machine_file(mut theirs: Theirs, path: &str, digest: Option<&str>) -> Theirs {
        theirs
            .files
            .insert(path.to_string(), digest.map(ToString::to_string));
        if digest.is_some() {
            theirs.meta.insert(path.to_string(), META);
        }
        theirs
    }

    fn base_file(mut base: Baseline, path: &str, digest: &str) -> Baseline {
        base.files.insert(path.to_string(), digest.to_string());
        base
    }

    // One row per line of the handoff's table.

    #[test]
    fn installed_outside_lodi_from_the_distribution_is_added_to_common() {
        let out = merge(&base(&["jq"]), &ours(&["jq"]), &theirs(&["jq", "zip"]));
        assert_eq!(out.adopt, ["zip"]);
        assert!(out.remove.is_empty() && out.conflicts.is_empty());
    }

    #[test]
    fn installed_from_a_third_party_a_package_file_or_the_aur_is_not_captured() {
        let mut machine = theirs(&["jq"]);
        for name in ["vendor-tool", "local-deb", "aur-build"] {
            machine.explicit.insert(name.to_string());
            machine
                .elsewhere
                .insert(name.to_string(), "from elsewhere".to_string());
        }
        let out = merge(&base(&["jq"]), &ours(&["jq"]), &machine);
        assert!(out.adopt.is_empty(), "{out:?}");
        assert!(!out.changes_manifest(), "{out:?}");
    }

    #[test]
    fn removed_outside_lodi_is_taken_out_of_every_list() {
        let mut manifest = ours(&["jq", "tree"]);
        manifest.hold.insert("tree".into());
        manifest.mark_auto.insert("tree".into());
        manifest.listed.insert("opt-only".into());
        let mut was = base(&["jq", "tree", "opt-only"]);
        was.holds = held(&[("tree", "1.0")]);
        let out = merge(&was, &manifest, &theirs(&["jq"]));
        assert_eq!(out.remove, ["opt-only", "tree"]);
        // The hold goes with the name, not as a second change.
        assert!(out.hold_remove.is_empty(), "{out:?}");
        assert!(out.conflicts.is_empty(), "{out:?}");
    }

    #[test]
    fn a_deleted_line_that_was_never_applied_stays_deleted() {
        let out = merge(
            &base(&["jq", "bat"]),
            &ours(&["jq"]),
            &theirs(&["jq", "bat"]),
        );
        assert!(out.adopt.is_empty(), "{out:?}");
        assert_eq!(out.kept, 1);
    }

    #[test]
    fn a_line_added_and_never_applied_is_kept() {
        let out = merge(&base(&["jq"]), &ours(&["jq", "htop"]), &theirs(&["jq"]));
        assert!(out.remove.is_empty(), "{out:?}");
        assert_eq!(out.kept, 1);
    }

    #[test]
    fn explicit_to_auto_is_a_removal() {
        // `tree` is still installed, as a dependency: it has left the explicit set.
        let out = merge(
            &base(&["jq", "tree"]),
            &ours(&["jq", "tree"]),
            &theirs(&["jq"]),
        );
        assert_eq!(out.remove, ["tree"]);
    }

    #[test]
    fn auto_to_explicit_is_an_adoption() {
        let out = merge(&base(&["jq"]), &ours(&["jq"]), &theirs(&["jq", "libfoo"]));
        assert_eq!(out.adopt, ["libfoo"]);
    }

    #[test]
    fn a_hold_changed_on_both_sides_differently_is_a_conflict_that_keeps_the_manifest() {
        let mut was = base(&["jq"]);
        was.holds = held(&[("jq", "1.0")]);
        let manifest = ours(&["jq"]); // the person released the hold
        let mut machine = theirs(&["jq"]);
        machine.holds = Some(held(&[("jq", "2.0")])); // upgraded and held again by hand
        let out = merge(&was, &manifest, &machine);
        assert_eq!(out.conflicts.len(), 1, "{out:?}");
        assert!(out.conflicts[0].starts_with("W_RECONCILE: the hold on jq"));
        assert!(out.hold_add.is_empty() && out.hold_remove.is_empty());
    }

    #[test]
    fn a_hold_changed_on_one_side_takes_that_side() {
        let mut machine = theirs(&["jq", "tree"]);
        machine.holds = Some(held(&[("jq", "1.0")]));
        let mut manifest = ours(&["jq", "tree"]);
        manifest.hold.insert("tree".into());
        let mut was = base(&["jq", "tree"]);
        was.holds = held(&[("tree", "1.0")]);
        let out = merge(&was, &manifest, &machine);
        assert_eq!(out.hold_add, ["jq"]);
        assert_eq!(out.hold_remove, ["tree"]);
        // On a family whose hold is not machine state the manifest alone decides.
        machine.holds = None;
        let out = merge(&was, &manifest, &machine);
        assert!(out.hold_add.is_empty() && out.hold_remove.is_empty());
    }

    #[test]
    fn an_absent_or_removed_name_installed_by_hand_is_a_conflict() {
        let mut manifest = ours(&["jq"]);
        manifest.kept_off = set(&["telnet", "nano"]);
        let out = merge(
            &base(&["jq"]),
            &manifest,
            &theirs(&["jq", "telnet", "nano"]),
        );
        assert!(out.adopt.is_empty(), "{out:?}");
        assert_eq!(out.conflicts.len(), 2, "{out:?}");
        assert!(
            out.conflicts
                .iter()
                .all(|c| c.starts_with("W_RECONCILE: package"))
        );
    }

    #[test]
    fn file_bytes_changed_on_both_sides_differently_is_a_conflict() {
        let was = base_file(base(&[]), "/etc/app.conf", "sha256:a");
        let manifest = with_file(ours(&[]), "/etc/app.conf", "sha256:b");
        let machine = machine_file(theirs(&[]), "/etc/app.conf", Some("sha256:c"));
        let out = merge(&was, &manifest, &machine);
        assert_eq!(out.conflicts.len(), 1, "{out:?}");
        assert!(out.recapture.is_empty());
    }

    #[test]
    fn a_file_changed_only_on_the_machine_is_captured_again() {
        let was = base_file(base(&[]), "/etc/app.conf", "sha256:a");
        let manifest = with_file(ours(&[]), "/etc/app.conf", "sha256:a");
        let machine = machine_file(theirs(&[]), "/etc/app.conf", Some("sha256:c"));
        let out = merge(&was, &manifest, &machine);
        assert_eq!(out.recapture, ["/etc/app.conf"]);
        assert!(out.conflicts.is_empty());
    }

    #[test]
    fn a_files_metadata_changed_only_on_the_machine_is_taken_and_only_in_the_manifest_is_kept() {
        let path = "/etc/app.conf";
        let was = base_file(base(&[]), path, "sha256:a");
        let recorded = BTreeMap::from([(path.to_string(), META)]);
        let other = Meta {
            mode: 0o600,
            uid: Some(1000),
            gid: Some(1000),
        };
        // The machine alone, the bytes the same: its metadata is taken, its bytes are not.
        let manifest = with_file(ours(&[]), path, "sha256:a");
        let mut machine = machine_file(theirs(&[]), path, Some("sha256:a"));
        machine.meta.insert(path.to_string(), other);
        let out = super::merge(&was, &recorded, &manifest, &machine);
        assert_eq!(out.metadata, [path]);
        assert!(
            out.recapture.is_empty() && out.conflicts.is_empty(),
            "{out:?}"
        );
        assert_eq!(out.added(), 1);
        // With the bytes too, it is one change, not two.
        let machine_bytes = {
            let mut m = machine.clone();
            m.files.insert(path.to_string(), Some("sha256:c".into()));
            m
        };
        let out = super::merge(&was, &recorded, &manifest, &machine_bytes);
        assert_eq!(
            (out.recapture.len(), out.metadata.len(), out.added()),
            (1, 1, 1)
        );
        // The manifest alone: kept, and so are its metadata when the machine's bytes change.
        let mut edited = manifest.clone();
        edited.files.get_mut(path).unwrap().meta = other;
        let same = machine_file(theirs(&[]), path, Some("sha256:c"));
        let out = super::merge(&was, &recorded, &edited, &same);
        assert_eq!(out.recapture, [path]);
        assert!(
            out.metadata.is_empty() && out.conflicts.is_empty(),
            "{out:?}"
        );
        assert_eq!(out.kept, 1);
        // Both, differently: a conflict that keeps the manifest.
        let mut third = machine.clone();
        third.meta.insert(
            path.to_string(),
            Meta {
                mode: 0o640,
                ..other
            },
        );
        let out = super::merge(&was, &recorded, &edited, &third);
        assert!(out.metadata.is_empty(), "{out:?}");
        assert_eq!(out.conflicts.len(), 1, "{out:?}");
        assert!(out.conflicts[0].contains("mode, owner or group changed"));
        // Both, the same way: nothing to do.
        let out = super::merge(&was, &recorded, &edited, &machine);
        assert_eq!(out, Outcome::default());
    }

    #[test]
    fn a_file_edited_only_in_the_manifest_keeps_its_edit() {
        let was = base_file(base(&[]), "/etc/app.conf", "sha256:a");
        let manifest = with_file(ours(&[]), "/etc/app.conf", "sha256:b");
        let machine = machine_file(theirs(&[]), "/etc/app.conf", Some("sha256:a"));
        let out = merge(&was, &manifest, &machine);
        assert!(out.recapture.is_empty() && out.conflicts.is_empty());
        assert_eq!(out.kept, 1);
    }

    #[test]
    fn a_declared_file_missing_from_the_machine_is_reported_and_kept() {
        let was = base_file(base(&[]), "/etc/app.conf", "sha256:a");
        let manifest = with_file(ours(&[]), "/etc/app.conf", "sha256:a");
        let machine = machine_file(theirs(&[]), "/etc/app.conf", None);
        let out = merge(&was, &manifest, &machine);
        assert!(!out.changes_manifest(), "{out:?}");
        assert_eq!(out.notes.len(), 1);
        assert!(out.notes[0].contains("/etc/app.conf is declared and missing"));
    }

    #[test]
    fn a_distribution_to_third_party_origin_change_keeps_the_declaration_with_a_warning() {
        let mut machine = theirs(&[]);
        machine.explicit.insert("jq".into());
        machine.elsewhere.insert(
            "jq".into(),
            "from a repository that is not debian's own".into(),
        );
        let out = merge(&base(&["jq"]), &ours(&["jq"]), &machine);
        assert!(!out.changes_manifest(), "{out:?}");
        assert_eq!(out.notes.len(), 1);
        assert!(out.notes[0].starts_with("W_RECONCILE: package jq is declared"));
    }

    #[test]
    fn a_newly_changed_configuration_file_is_declared_and_a_deleted_declaration_stays_deleted() {
        let mut machine = theirs(&[]);
        machine.captured = set(&["/etc/new.conf", "/etc/gone.conf"]);
        let was = base_file(base(&[]), "/etc/gone.conf", "sha256:a");
        let out = merge(&was, &ours(&[]), &machine);
        assert_eq!(out.declare, ["/etc/new.conf"]);
        assert_eq!(out.kept, 1);
    }

    #[test]
    fn an_unchanged_machine_changes_nothing() {
        let was = base_file(base(&["jq"]), "/etc/app.conf", "sha256:a");
        let manifest = with_file(ours(&["jq"]), "/etc/app.conf", "sha256:a");
        let machine = machine_file(theirs(&["jq"]), "/etc/app.conf", Some("sha256:a"));
        let out = merge(&was, &manifest, &machine);
        assert_eq!(out, Outcome::default());
    }
}
