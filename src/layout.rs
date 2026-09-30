//! The store's layout version and its once-only migration (M-1.0 T-2, design calls D7, D8, D18).
//!
//! The per-user store ([`crate::store`]) is the one thing Lodi writes that had no version of any
//! kind: its shape was described in a module comment and nothing on disk recorded which shape a
//! given store was. This module gives it `$LODI_HOME/.layout.json` — the numeric layout version,
//! the Lodi that wrote it and the UTC time it was written — and the migration that puts it there.
//!
//! Three rules hold the whole mechanism together:
//!
//! 1. **A store with no marker is layout [`UNMARKED`]**, which is every store any released Lodi
//!    has ever created. Its layout is not inferred from what is present: guessing about a
//!    directory you are about to write into is how stores get corrupted (D7).
//! 2. **The migration runs under the exclusive store lock and is idempotent.** A store that
//!    already records [`CURRENT`] is left alone — not a byte, not an mtime — and two processes
//!    that meet serialize on `store/.lock` rather than racing.
//! 3. **A layout this build does not know is a refusal, never a repair**: `E_STORE_VERSION` at
//!    exit 6 (D8), naming the layout found, the layout this build knows and the Lodi that wrote
//!    the marker. Nothing below the store is opened and nothing is written.
//!
//! [`migrate`] is called from [`crate::store::Store::open_for_write`] and from nowhere else: the
//! one point where a store root is opened by a command that will write into it, before any entry
//! is realized. A read-only command never migrates and never creates the marker:
//! `lodi gc --dry-run` opens the store with [`crate::store::Store::open`] instead and predicts
//! against the layout it finds, and `lodi search`, `lodi info` and `lodi home status` do not open
//! the store at all.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::diag::Diagnostic;
use crate::lock::{canonical_json, write_atomic_with};
use crate::store::Store;
use crate::util::{format_utc, now_utc};

/// The registry row of the marker in [`crate::schema::ARTIFACTS`].
pub const LAYOUT_ARTIFACT: &str = "store-layout";

/// The marker's name at the root of the store: `$LODI_HOME/.layout.json`.
pub const MARKER: &str = ".layout.json";

/// The store layout this build writes and works with.
pub const CURRENT: u64 = 1;

/// The layout of a store with no marker — every store any released Lodi has created.
pub const UNMARKED: u64 = 0;

/// `$LODI_HOME/.layout.json` (design call D7): what shape the store is, and who said so.
///
/// It is the one record in this tree deliberately **not** declared `deny_unknown_fields`
/// (`LD-233`): its whole job is to be read by a build that does not know
/// what wrote it, so a later build that adds a field while keeping the layout number must stay
/// readable here. What this build does not know it ignores; what it must not misread — the
/// layout number — is read from the raw JSON before the document is deserialized at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Marker {
    /// The store's layout version. This is also the document's registered schema version: the
    /// marker says what shape the store is and nothing else, so the two cannot disagree.
    pub layout: u64,
    /// The version of Lodi that wrote the marker.
    pub lodi_version: String,
    /// When it was written, UTC.
    pub written: String,
}

/// What [`migrate`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The store already recorded [`CURRENT`]: nothing was written, not even an mtime.
    Current,
    /// The store was migrated from this layout to [`CURRENT`].
    Migrated { from: u64 },
}

/// One ordered layout step.
///
/// Every step must be **re-runnable**: it may be reached again by a process that was interrupted
/// part way through the previous attempt, because the marker is written last. The 0 → 1 step is
/// additive and does nothing at all, which is the easiest possible case of that rule;
/// `docs/SCHEMAS.md` states it for the steps that come later.
pub struct Step {
    /// The layout this step starts from.
    pub from: u64,
    /// The layout it produces.
    pub to: u64,
    /// Whether the step only adds. A step that moves, renames or removes anything is **not**
    /// additive and `docs/SCHEMAS.md` must say what it does before it ships.
    pub additive: bool,
    /// What the step does, in words, for `docs/SCHEMAS.md` and the record.
    pub what: &'static str,
    run: fn(&Store) -> Result<(), Diagnostic>,
}

/// The ordered steps this build knows, lowest first.
///
/// At 1.0 there is exactly one, and it is empty: the layout of a 0.x store already **is** the
/// layout 1.0 wants, so the step records the marker and touches nothing else. Nothing is moved,
/// renamed or removed.
pub const STEPS: &[Step] = &[Step {
    from: UNMARKED,
    to: 1,
    additive: true,
    what: "record the layout marker; a 0.x store already has the shape 1.0 wants, so nothing is \
           moved, renamed or removed",
    run: |_| Ok(()),
}];

/// The ordered steps from `layout` to [`CURRENT`], or `None` when this build has no path from it
/// — a layout from the future, or one no step starts at.
pub fn steps_from(layout: u64) -> Option<&'static [Step]> {
    if layout == CURRENT {
        return Some(&[]);
    }
    let start = STEPS.iter().position(|s| s.from == layout)?;
    let rest = &STEPS[start..];
    let mut at = layout;
    for step in rest {
        if step.from != at {
            return None;
        }
        at = step.to;
    }
    (at == CURRENT).then_some(rest)
}

fn store_io(path: &Path, why: impl std::fmt::Display) -> Diagnostic {
    Diagnostic::new("E_STORE_IO", format!("{}: {why}", path.display()))
}

/// The layout of the store at `home`, reading the marker and **writing nothing at all**.
///
/// An absent marker is [`UNMARKED`]. A marker whose layout version this build has no path from
/// is `E_STORE_VERSION` (exit 6, design call D8); one that is not JSON, or cannot be read, is
/// `E_STORE_IO` (exit 6). Unknown input is a refusal, never a silent default: a store that will
/// not say what shape it is, is not a store this build writes into.
pub fn read(home: &Path) -> Result<u64, Diagnostic> {
    let path = home.join(MARKER);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(UNMARKED),
        Err(e) => return Err(store_io(&path, e)),
    };
    let raw: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| store_io(&path, format!("the store layout marker is not JSON: {e}")))?;
    let artifact = crate::schema::artifact(LAYOUT_ARTIFACT);
    let found = crate::schema::version_of(LAYOUT_ARTIFACT, &raw);
    if let Some(layout) = found
        && steps_from(layout).is_some()
    {
        return Ok(layout);
    }
    // The layout is decided from the raw JSON before anything else in the document is believed
    // (design call D18), and the refusal names the Lodi that wrote the file — or says plainly
    // that the file records none (D3).
    let carries = match found {
        Some(layout) => format!("has layout version {layout}"),
        None => format!("records no numeric `{}`", artifact.version_field),
    };
    let mut d = Diagnostic::new(
        "E_STORE_VERSION",
        format!(
            "the store at {} {carries}; this lodi knows layout version {}",
            home.display(),
            artifact.read_set()
        ),
    );
    d.notes
        .push(crate::schema::writer_note(LAYOUT_ARTIFACT, &raw));
    Err(d
        .hint("install a newer lodi to use this store; nothing in it was read, changed or removed"))
}

/// Bring the store to [`CURRENT`], once (design call D7).
///
/// Called from [`Store::open_for_write`] and nowhere else. The marker is read first, without a
/// lock: a store that is already current is the common case, and making every `lodi develop` or
/// `lodi shell` take the **exclusive** store lock would serialize entries that deliberately
/// share the store today. When a migration may be due the exclusive lock is taken — waiting,
/// with the one line on standard error the collector already prints — and the marker is re-read
/// under it, so a process that waited for another one's migration finds the work done and writes
/// nothing.
pub fn migrate(store: &Store) -> Result<Outcome, Diagnostic> {
    if read(store.home())? == CURRENT {
        return Ok(Outcome::Current);
    }
    let _lock = match store.try_exclusive_lock()? {
        Some(lock) => lock,
        None => {
            eprintln!("lodi: waiting for the store lock");
            store.exclusive_lock()?
        }
    };
    let from = read(store.home())?;
    if from == CURRENT {
        return Ok(Outcome::Current);
    }
    let steps = steps_from(from).ok_or_else(|| {
        // `read` refuses a layout with no path, so this cannot be reached from a file; it would
        // be a hole in STEPS, which the unit tests below pin.
        Diagnostic::new(
            "E_STORE_VERSION",
            format!(
                "the store at {} is layout version {from}, which this build cannot migrate",
                store.home().display()
            ),
        )
    })?;
    for step in steps {
        (step.run)(store)?;
    }
    write_marker(store.home())?;
    Ok(Outcome::Migrated { from })
}

/// Write the marker for [`CURRENT`]: 0644, canonical JSON, atomically.
fn write_marker(home: &Path) -> Result<(), Diagnostic> {
    let path = home.join(MARKER);
    let marker = Marker {
        layout: CURRENT,
        lodi_version: crate::schema::LODI_VERSION.to_string(),
        written: format_utc(now_utc()),
    };
    write_atomic_with(&path, canonical_json(&marker).as_bytes(), |tmp| {
        fs::set_permissions(tmp, fs::Permissions::from_mode(0o644))
    })
    .map_err(|e| store_io(&path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_step_table_is_a_contiguous_chain_from_unmarked_to_current() {
        assert_eq!(STEPS.first().map(|s| s.from), Some(UNMARKED));
        assert_eq!(STEPS.last().map(|s| s.to), Some(CURRENT));
        let mut at = UNMARKED;
        for step in STEPS {
            assert_eq!(step.from, at, "a hole in STEPS at {at}");
            assert!(step.to > step.from, "a step must move forward");
            assert!(!step.what.is_empty(), "a step says what it does");
            at = step.to;
        }
        assert_eq!(at, CURRENT);
        assert_eq!(steps_from(CURRENT).map(<[Step]>::len), Some(0));
        assert_eq!(steps_from(UNMARKED).map(<[Step]>::len), Some(STEPS.len()));
        assert!(steps_from(CURRENT + 1).is_none());
    }

    #[test]
    fn the_only_step_of_this_build_is_additive() {
        for step in STEPS {
            assert!(
                step.additive,
                "layout step {} -> {} moves or removes something; docs/SCHEMAS.md must say so \
                 and it must be re-runnable",
                step.from, step.to
            );
        }
    }

    #[test]
    fn the_marker_is_registered_and_writes_what_it_reads() {
        let artifact = crate::schema::artifact(LAYOUT_ARTIFACT);
        assert_eq!(artifact.writes, CURRENT);
        assert!(artifact.reads_version(CURRENT));
        assert_eq!(artifact.version_field, "layout");
        let marker = Marker {
            layout: CURRENT,
            lodi_version: "9.9.9".into(),
            written: "2026-09-21T00:00:00Z".into(),
        };
        let raw: serde_json::Value =
            serde_json::from_str(&canonical_json(&marker)).expect("the marker is JSON");
        assert_eq!(crate::schema::version_of(LAYOUT_ARTIFACT, &raw), Some(1));
        assert_eq!(
            crate::schema::writer_of(LAYOUT_ARTIFACT, &raw).as_deref(),
            Some("9.9.9")
        );
    }
}
