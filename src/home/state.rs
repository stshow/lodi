//! The home scope's state record: what Lodi wrote, and what it promised to do when the entry
//! that declared it disappears (M-0.5 T-3, design calls D5, D13 — `LD-101`, `LD-109`).
//!
//! `<data>/home-scope/state.json` is canonical JSON, schema version 3 (below), written atomically
//! through [`crate::home::fsops`] and **last** in an apply, so a state file that exists always
//! describes work that finished. One object per managed path:
//!
//! ```json
//! {
//!   "directories": [".config/git"],
//!   "files": {
//!     ".config/git/ignore": {
//!       "mode": "0644",
//!       "onRemove": "restore",
//!       "origin": { "kind": "content", "sha256": "…" },
//!       "path": ".config/git/ignore",
//!       "sha256": "…"
//!     }
//!   },
//!   "version": 1
//! }
//! ```
//!
//! `sha256` is of the bytes **Lodi wrote**, which is what makes drift detectable: a file whose
//! bytes hash to something else was edited by hand since. `onRemove` is recorded here rather
//! than read from the manifest because the entry that carried it is gone exactly when the
//! behaviour is needed (design call D5), and `backup` names the file in the backup store that
//! `on_remove = "restore"` puts back. `directories` are the parents an apply created; they are
//! recorded so `status` can name them and are **never removed again** (design call D7).
//!
//! **Schema version 2** (M-Home, `docs/design/HOME_PROGRAMS.md` §4.5): a record may also carry
//! `state = "seed"` (a seeded file, never compared again) and an origin of `kind = "program"`
//! with its `module`. Version 1's reader accepts any `kind` string but denies unknown fields, so
//! a 1.0 build would refuse such a record as JSON it did not write; carrying version 2 makes that
//! refusal the one that says so, by the version (`tests/home_file.rs` holds both halves). This
//! builds before 1.5 read 1 and 2 and wrote 2. Builds before 1.12 read 1, 2 and 3 and write 3
//! only after an apply changes something; an empty plan does not rewrite legacy state.
//!
//! **Schema version 4** (hc-1, LD-427) adds `services`, each user unit `[services]` manages with
//! the state it had before Lodi first managed it, and `linger`, true while the lingering Lodi
//! turned on is Lodi's to turn off. A record carries version 4 when it names either, and 3
//! otherwise, so a home without `[services]` writes the bytes it always did.
//!
//! Nothing here is derived from a clock, a pid or a counter (design call D13), so two machines
//! applying the same manifest write the same bytes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::diag::Diagnostic;
use crate::home::backup;
use crate::home::fsops::{self, RelPath, Root};
use crate::home::manifest::{FileEntry, FileState, Origin};
use crate::lock::canonical_json;
use crate::util::sha256_hex;

/// The schema version this build writes for a home without services; it reads 1 to 4
/// (`src/schema.rs`).
pub const STATE_VERSION: u32 = 3;

/// The schema version of a record that names a service or managed lingering.
pub const SERVICES_VERSION: u32 = 4;

/// One user unit `[services]` manages (schema 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceRecord {
    /// `enabled`, `disabled`, or `absent` for a unit that was not there: what the unit goes
    /// back to when its declaration is removed (H2).
    pub before: String,
}

pub const DEFAULT_SOURCE: &str = "~/.config/lodi";

/// The registry row the state file is registered as (`src/schema.rs`).
pub const STATE_ARTIFACT: &str = "home-state";

/// The home scope's own directory below the data root.
pub const HOME_SCOPE_DIR: &str = "home-scope";

/// The state file, relative to the data root.
pub const STATE_FILE: &str = "home-scope/state.json";

/// The apply lock, relative to the data root. `apply` takes it exclusively, `plan` and `status`
/// share it, so a report never sees a half-written state.
pub const LOCK_FILE: &str = "home-scope/.lock";

/// Where a managed file's bytes came from at the apply that wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OriginRecord {
    /// `content`, `source` or `program`.
    pub kind: String,
    /// The sha256 of those bytes.
    pub sha256: String,
    /// The `source` path, relative to the directory `home.toml` is in; absent for `content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The program module that rendered the bytes, for `kind = "program"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
}

/// One managed path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManagedFile {
    pub path: String,
    /// The sha256 of the bytes Lodi wrote. Drift is measured against this.
    pub sha256: String,
    /// The mode Lodi set, as four octal digits.
    pub mode: String,
    pub origin: OriginRecord,
    /// `on_remove` as it stood at the apply that wrote the file (design call D5).
    pub on_remove: String,
    /// The backup store's file name for this path, when one was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
    /// `seed` for a file written once and never managed again (schema version 2); absent for a
    /// managed file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

impl ManagedFile {
    /// The record an apply writes for `entry` after writing `bytes`, carrying `backup` forward.
    pub fn of(entry: &FileEntry, backup: Option<String>) -> ManagedFile {
        let (kind, source, module) = match &entry.origin {
            Origin::Content => ("content", None, None),
            Origin::Source(rel) => ("source", Some(rel.as_str().to_string()), None),
            Origin::Program(name) => ("program", None, Some(name.clone())),
        };
        let origin = OriginRecord {
            kind: kind.into(),
            sha256: sha256_hex(&entry.bytes),
            source,
            module,
        };
        ManagedFile {
            path: entry.path.as_str().to_string(),
            sha256: sha256_hex(&entry.bytes),
            mode: mode_text(entry.mode),
            origin,
            on_remove: entry.on_remove.name().to_string(),
            backup,
            state: (entry.state == FileState::Seed).then(|| "seed".to_string()),
        }
    }

    /// Whether the record is of a seeded file, which is never compared again.
    pub fn is_seed(&self) -> bool {
        self.state.as_deref() == Some("seed")
    }
}

/// The whole record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HomeState {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The Lodi that wrote this state. `#[serde(default)]` because a state file written before
    /// the field existed carries the same shape (M-1.0 T-1, design call D5).
    #[serde(default)]
    pub lodi_version: String,
    /// Keyed by the path, so every walk is in lexicographic order (design call D13).
    pub files: BTreeMap<String, ManagedFile>,
    /// The parent directories an apply created, sorted. They are never removed (D7).
    pub directories: Vec<String>,
    /// Each user unit `[services]` manages, keyed by its name (schema 4).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<String, ServiceRecord>,
    /// Lodi turned lingering on, and turns it off when no service asks for it any more.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub linger: bool,
}

impl Default for HomeState {
    fn default() -> HomeState {
        HomeState {
            version: STATE_VERSION,
            source: None,
            lodi_version: crate::schema::LODI_VERSION.to_string(),
            files: BTreeMap::new(),
            directories: Vec::new(),
            services: BTreeMap::new(),
            linger: false,
        }
    }
}

impl HomeState {
    pub fn source(&self) -> &str {
        self.source.as_deref().unwrap_or(DEFAULT_SOURCE)
    }
}

/// A mode as the four octal digits the state file and the plan's column use.
pub fn mode_text(mode: u32) -> String {
    format!("{:04o}", mode & 0o7777)
}

/// Read `<data>/home-scope/state.json`. A state file that is not there is the empty state: the
/// first apply of a machine has never written one, and reading creates nothing.
pub fn read(data: &Root) -> Result<HomeState, Diagnostic> {
    let rel = RelPath::new(STATE_FILE).expect("the state file's path is a relative path");
    // Asked of `lstat`: a link here is read, and refused, by `fsops::read` rather than followed.
    if !fsops::lexists(&data.path().join(STATE_FILE)) {
        return Ok(HomeState::default());
    }
    let bytes = fsops::read(data, &rel)?;
    let unreadable = |why: String| {
        Diagnostic::new(
            "E_STORE_IO",
            format!("the home scope's state file {STATE_FILE} is not the JSON lodi wrote: {why}"),
        )
        .hint("check the file, or remove it to start from an unmanaged home")
    };
    // The schema version is decided from the raw JSON before the record is deserialized, and a
    // version outside the read-set keeps this file's own code (M-1.0 T-1, design calls D3, D18).
    let raw: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| unreadable(e.to_string()))?;
    let artifact = crate::schema::artifact(STATE_ARTIFACT);
    match crate::schema::version_of(STATE_ARTIFACT, &raw) {
        Some(v) if artifact.reads_version(v) => {}
        found => {
            let carries = match found {
                Some(v) => format!("is schema version {v}"),
                None => "records no schema version".to_string(),
            };
            let mut d = Diagnostic::new(
                "E_STORE_IO",
                format!(
                    "the home scope's state file {STATE_FILE} {carries}, and this build reads \
                     version {}",
                    artifact.read_set()
                ),
            );
            d.notes
                .push(crate::schema::writer_note(STATE_ARTIFACT, &raw));
            return Err(d.hint("use the lodi that wrote it"));
        }
    }
    let state: HomeState = serde_json::from_value(raw).map_err(|e| unreadable(e.to_string()))?;
    check(&state).map_err(unreadable)?;
    Ok(state)
}

/// Every digest and backup name the state carries has the shape this build writes, or the state
/// is not one Lodi wrote (LD-361): the values are shortened for a warning and joined to a path
/// later, and nothing downstream has to cope with one that is not lowercase hex.
fn check(state: &HomeState) -> Result<(), String> {
    if (state.version == SERVICES_VERSION) == (state.services.is_empty() && !state.linger) {
        return Err("services, lingering and schema version 4 come together".into());
    }
    for (unit, record) in &state.services {
        if !crate::hostscope::manifest::is_unit_name(unit)
            || !["enabled", "disabled", "absent"].contains(&record.before.as_str())
        {
            return Err(format!("the service `{unit}` is not a record lodi wrote"));
        }
    }
    for (key, file) in &state.files {
        if !backup::is_digest(&file.sha256) || !backup::is_digest(&file.origin.sha256) {
            return Err(format!(
                "`{key}` records a sha256 that is not 64 lowercase hex digits"
            ));
        }
        if file
            .backup
            .as_deref()
            .is_some_and(|name| !backup::is_name(name))
        {
            return Err(format!(
                "`{key}` names a backup that is not a name of the backup store"
            ));
        }
    }
    Ok(())
}

/// Write the state atomically, last in an apply. The bytes are canonical JSON, so an apply that
/// changed nothing rewrites exactly what was there.
pub fn write(data: &Root, state: &HomeState) -> Result<(), Diagnostic> {
    let rel = RelPath::new(STATE_FILE).expect("the state file's path is a relative path");
    // Whatever version was read, what is written is the version this build writes.
    let mut state = state.clone();
    state.version = if state.services.is_empty() && !state.linger {
        STATE_VERSION
    } else {
        SERVICES_VERSION
    };
    if state.source.is_none() {
        state.source = Some(DEFAULT_SOURCE.to_string());
    }
    // The record holds digests of the user's private originals: owner-only, in an owner-only
    // directory, whatever the umask (LD-361).
    let dir = RelPath::new(HOME_SCOPE_DIR).expect("the scope directory is a relative path");
    fsops::mkdir_private(data, &dir)?;
    fsops::write(data, &rel, canonical_json(&state).as_bytes(), 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mode_is_four_octal_digits() {
        assert_eq!(mode_text(0o644), "0644");
        assert_eq!(mode_text(0o600), "0600");
        assert_eq!(mode_text(0o40755 & 0o7777), "0755");
    }

    #[test]
    fn the_state_round_trips_through_canonical_json() {
        let mut state = HomeState::default();
        state.files.insert(
            ".inputrc".into(),
            ManagedFile {
                path: ".inputrc".into(),
                sha256: sha256_hex(b"x"),
                mode: "0600".into(),
                origin: OriginRecord {
                    kind: "content".into(),
                    sha256: sha256_hex(b"x"),
                    source: None,
                    module: None,
                },
                on_remove: "restore".into(),
                backup: None,
                state: None,
            },
        );
        assert_eq!(state.lodi_version, crate::schema::LODI_VERSION);
        let text = canonical_json(&state);
        // Canonical JSON sorts keys, so the record is diffable and stable (design call D13).
        assert!(text.starts_with("{\n  \"directories\": []"), "{text}");
        assert!(text.ends_with("\n"), "{text}");
        assert!(!text.contains("\"backup\""), "an absent backup is omitted");
        let back: HomeState = serde_json::from_str(&text).unwrap();
        assert_eq!(back, state);
    }
}
