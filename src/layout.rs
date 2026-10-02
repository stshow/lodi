//! The store's layout marker (design calls D7, D8, D18; 2.0's clean break, #710 story 8).
//!
//! The per-user store ([`crate::store`]) records its shape in `$LODI_HOME/.layout.json`: the
//! numeric layout version, the lodi that wrote it and the UTC time it was written.
//!
//! Three rules hold the whole mechanism together:
//!
//! 1. **A store without the 2.0 marker is no 2.0 store.** No marker, a marker that is not JSON or
//!    records no number, or a layout below [`CURRENT`] (a 1.x store) is never read, migrated or
//!    refused: the first command that writes into it starts it fresh by writing the 2.0 marker.
//! 2. **The marker is written under the exclusive store lock, once.** A store that already
//!    records [`CURRENT`] is left alone — not a byte, not an mtime — and two processes that meet
//!    serialize on `store/.lock` rather than racing.
//! 3. **A layout from a newer lodi is a refusal, never a repair**: `E_STORE_VERSION` at exit 6
//!    (D8), naming the layout found, the layout this build knows and the lodi that wrote the
//!    marker. Nothing below the store is opened and nothing is written.
//!
//! [`start`] is called from [`crate::store::Store::open_for_write`] and from nowhere else: the
//! one point where a store root is opened by a command that will write into it, before any entry
//! is realized. A read-only command never writes the marker: `lodi gc --dry-run` opens the store
//! with [`crate::store::Store::open`] instead, and `lodi search` does not open the store at all.

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

/// The store layout this build writes and works with: 2.0's.
pub const CURRENT: u64 = 2;

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
    /// The version of lodi that wrote the marker.
    pub lodi_version: String,
    /// When it was written, UTC.
    pub written: String,
}

/// What [`start`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The store already recorded [`CURRENT`]: nothing was written, not even an mtime.
    Current,
    /// The store had no 2.0 marker; it was written, and nothing else.
    Fresh,
}

fn store_io(path: &Path, why: impl std::fmt::Display) -> Diagnostic {
    Diagnostic::new("E_STORE_IO", format!("{}: {why}", path.display()))
}

/// The layout of the store at `home`, reading the marker and **writing nothing at all**:
/// `Some(CURRENT)`, or `None` for a store without the 2.0 marker (rule 1).
///
/// A marker from a newer lodi is `E_STORE_VERSION` (exit 6, design call D8); a marker that
/// cannot be read for an I/O reason is `E_STORE_IO` (exit 6).
pub fn read(home: &Path) -> Result<Option<u64>, Diagnostic> {
    let path = home.join(MARKER);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(store_io(&path, e)),
    };
    let Ok(raw) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(None);
    };
    match crate::schema::version_of(LAYOUT_ARTIFACT, &raw) {
        Some(CURRENT) => Ok(Some(CURRENT)),
        Some(layout) if layout > CURRENT => Err(newer(home, layout, &raw)),
        _ => Ok(None),
    }
}

/// `E_STORE_VERSION` for a marker of `layout`, from a newer lodi. The layout is decided from the
/// raw JSON before anything else in the document is believed (design call D18), and the refusal
/// names the lodi that wrote the file — or says plainly that the file records none (D3).
fn newer(home: &Path, layout: u64, raw: &serde_json::Value) -> Diagnostic {
    let artifact = crate::schema::artifact(LAYOUT_ARTIFACT);
    let mut d = Diagnostic::new(
        "E_STORE_VERSION",
        format!(
            "the store at {} has layout version {layout}; this lodi knows layout version {}",
            home.display(),
            artifact.read_set()
        ),
    );
    d.notes
        .push(crate::schema::writer_note(LAYOUT_ARTIFACT, raw));
    d.hint("install a newer lodi to use this store; nothing in it was read, changed or removed")
}

/// Give a store without the 2.0 marker that marker, once (rule 1).
///
/// Called from [`Store::open_for_write`] and nowhere else. The marker is read first, without a
/// lock: a store that is already current is the common case, and making every `lodi develop` or
/// `lodi shell` take the **exclusive** store lock would serialize entries that deliberately
/// share the store today. When the marker may be missing the exclusive lock is taken — waiting,
/// with the one line on standard error the collector already prints — and the marker is re-read
/// under it, so a process that waited for another one finds the work done and writes nothing.
pub fn start(store: &Store) -> Result<Outcome, Diagnostic> {
    if read(store.home())?.is_some() {
        return Ok(Outcome::Current);
    }
    let _lock = match store.try_exclusive_lock()? {
        Some(lock) => lock,
        None => {
            eprintln!("lodi: waiting for the store lock");
            store.exclusive_lock()?
        }
    };
    if read(store.home())?.is_some() {
        return Ok(Outcome::Current);
    }
    write_marker(store.home())?;
    Ok(Outcome::Fresh)
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
        assert_eq!(
            crate::schema::version_of(LAYOUT_ARTIFACT, &raw),
            Some(CURRENT)
        );
        assert_eq!(
            crate::schema::writer_of(LAYOUT_ARTIFACT, &raw).as_deref(),
            Some("9.9.9")
        );
    }
}
