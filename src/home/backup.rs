//! The backup store: the copy Lodi keeps of what was at a path before it took it over
//! (M-0.5 T-3, design calls D5, D13 — `LD-101`, `LD-173`).
//!
//! ```text
//! <data>/home-scope/backups/<sha256 of the relative path>-<16 hex of the content>
//! <data>/home-scope/backups/<sha256 of the relative path>.drift-<16 hex of the content>
//! <data>/home-scope/backups/index.json
//! ```
//!
//! Two rules carry the whole design:
//!
//! 1. **A backup is taken once**, the first time Lodi replaces a file at that path that it did
//!    not write. A later apply never re-backs-up, because by then the file on disk is Lodi's own
//!    output and backing it up again would silently replace the user's original with Lodi's
//!    content — noticed only the day someone needed it (design call D5).
//! 2. **An existing backup is never overwritten.** `--overwrite-drift` keeps the drifted bytes
//!    under a second, `.drift-` name beside it, so confirming a take-back is not destructive
//!    either (design call D6).
//!
//! Names are derived from the path and the content and from nothing else — no timestamp, no pid,
//! no counter (design call D13) — so a test can assert an exact listing and a re-run produces the
//! same names. `index.json` is canonical JSON mapping each name back to the readable path, the
//! hash and the mode the file had, which is what `on_remove = "restore"` puts back.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::diag::Diagnostic;
use crate::home::fsops::{self, RelPath, Root};
use crate::home::state::mode_text;
use crate::lock::canonical_json;
use crate::util::sha256_hex;

/// The one schema version that exists.
pub const INDEX_VERSION: u32 = 1;

/// The registry row the backup index is registered as (`src/schema.rs`).
pub const INDEX_ARTIFACT: &str = "home-backup-index";

/// The backup directory, relative to the data root.
pub const BACKUP_DIR: &str = "home-scope/backups";

/// The index, relative to the data root.
pub const BACKUP_INDEX: &str = "home-scope/backups/index.json";

/// How many hex characters of the content hash a backup's name carries.
const CONTENT_PREFIX: usize = 16;

/// What a backup is a copy of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The unmanaged file that was there before Lodi took the path over.
    Original,
    /// Drifted bytes `--overwrite-drift` took back.
    Drift,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Original => "original",
            Kind::Drift => "drift",
        }
    }
}

/// One row of `index.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupRecord {
    /// The readable path the copy came from, relative to the home root.
    pub path: String,
    /// The sha256 of the copied bytes.
    pub sha256: String,
    /// The mode the file had, as four octal digits: what `restore` puts back.
    pub mode: String,
    /// `original` or `drift`.
    pub kind: String,
}

/// `index.json`: the readable side of the content-derived file names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupIndex {
    pub version: u32,
    /// The Lodi that wrote this index. `#[serde(default)]` because an index written before the
    /// field existed carries the same shape (M-1.0 T-1, design call D5).
    #[serde(default)]
    pub lodi_version: String,
    /// File name -> what it is a copy of, in lexicographic order.
    pub backups: BTreeMap<String, BackupRecord>,
}

impl Default for BackupIndex {
    fn default() -> BackupIndex {
        BackupIndex {
            version: INDEX_VERSION,
            lodi_version: crate::schema::LODI_VERSION.to_string(),
            backups: BTreeMap::new(),
        }
    }
}

impl BackupIndex {
    /// The `original` backup kept for `path`, if there is one. This is the "once" of design
    /// call D5: a path that has one is never backed up again.
    pub fn original_for(&self, path: &str) -> Option<(&String, &BackupRecord)> {
        self.backups
            .iter()
            .find(|(_, record)| record.path == path && record.kind == Kind::Original.name())
    }
}

/// The name a copy of `bytes` from `path` gets.
pub fn name_for(path: &str, bytes: &[u8], kind: Kind) -> String {
    let content = &sha256_hex(bytes)[..CONTENT_PREFIX];
    match kind {
        Kind::Original => format!("{}-{content}", sha256_hex(path.as_bytes())),
        Kind::Drift => format!("{}.drift-{content}", sha256_hex(path.as_bytes())),
    }
}

/// Read `index.json`. An index that is not there is the empty one; reading creates nothing.
pub fn read(data: &Root) -> Result<BackupIndex, Diagnostic> {
    let rel = RelPath::new(BACKUP_INDEX).expect("the index's path is a relative path");
    // Asked of `lstat`: a link here is read, and refused, by `fsops::read` rather than followed.
    if !fsops::lexists(&data.path().join(BACKUP_INDEX)) {
        return Ok(BackupIndex::default());
    }
    let bytes = fsops::read(data, &rel)?;
    let unreadable = |why: String| {
        Diagnostic::new(
            "E_STORE_IO",
            format!("the backup index {BACKUP_INDEX} is not the JSON lodi wrote: {why}"),
        )
        .hint("check the file; the backups themselves are the copies beside it")
    };
    // The schema version is decided from the raw JSON before the record is deserialized
    // (M-1.0 T-1, design calls D3, D18).
    let raw: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| unreadable(e.to_string()))?;
    let artifact = crate::schema::artifact(INDEX_ARTIFACT);
    match crate::schema::version_of(INDEX_ARTIFACT, &raw) {
        Some(v) if artifact.reads_version(v) => {}
        found => {
            let carries = match found {
                Some(v) => format!("is schema version {v}"),
                None => "records no schema version".to_string(),
            };
            let mut d = Diagnostic::new(
                "E_STORE_IO",
                format!(
                    "the backup index {BACKUP_INDEX} {carries}, and this build reads version {}",
                    artifact.read_set()
                ),
            );
            d.notes
                .push(crate::schema::writer_note(INDEX_ARTIFACT, &raw));
            return Err(d.hint("use the lodi that wrote it"));
        }
    }
    let index: BackupIndex = serde_json::from_value(raw).map_err(|e| unreadable(e.to_string()))?;
    for (name, record) in &index.backups {
        if !is_name(name) {
            return Err(unreadable(
                "a backup is named with something that is not a name of the backup store".into(),
            ));
        }
        if !is_digest(&record.sha256) {
            return Err(unreadable(format!(
                "the backup of `{}` records a sha256 that is not 64 lowercase hex digits",
                record.path
            )));
        }
    }
    Ok(index)
}

/// Whether `text` is a sha256 as this build writes one: 64 lowercase hex digits.
pub fn is_digest(text: &str) -> bool {
    text.len() == 64 && is_hex(text)
}

/// Whether `text` is a name [`name_for`] can produce, of either kind.
pub fn is_name(text: &str) -> bool {
    let Some((path, content)) = text.split_once(".drift-").or_else(|| text.split_once('-')) else {
        return false;
    };
    is_digest(path) && content.len() == CONTENT_PREFIX && is_hex(content)
}

fn is_hex(text: &str) -> bool {
    text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Copy `bytes` into the store as a backup of `path` and record it, then write the index. An
/// existing file of that name is **left exactly as it is**: the name is derived from the content,
/// so a second copy of the same bytes is the same bytes. Returns the name.
pub fn keep(
    data: &Root,
    index: &mut BackupIndex,
    path: &str,
    bytes: &[u8],
    mode: u32,
    kind: Kind,
) -> Result<String, Diagnostic> {
    let name = name_for(path, bytes, kind);
    let rel = RelPath::new(&format!("{BACKUP_DIR}/{name}"))
        .expect("a backup's name is hex and a path this module built");
    // "Already kept" is asked of `lstat` through the same component checks as a write: a link
    // at the name is refused, never followed, so the index never records a copy that was not
    // written (LD-361).
    fsops::mkdir_private(data, &backup_dir())?;
    if !fsops::is_file(data, &rel)? {
        fsops::write(data, &rel, bytes, 0o600)?;
    }
    index.backups.insert(
        name.clone(),
        BackupRecord {
            path: path.to_string(),
            sha256: sha256_hex(bytes),
            mode: mode_text(mode),
            kind: kind.name().to_string(),
        },
    );
    write_index(data, index)?;
    Ok(name)
}

/// The bytes of the backup called `name`.
pub fn bytes_of(data: &Root, name: &str) -> Result<Vec<u8>, Diagnostic> {
    let rel = RelPath::new(&format!("{BACKUP_DIR}/{name}")).map_err(|_| {
        Diagnostic::new(
            "E_STORE_IO",
            format!("the state names a backup `{name}` that is not a name of the backup store"),
        )
    })?;
    fsops::read(data, &rel)
}

/// Write `index.json` atomically.
pub fn write_index(data: &Root, index: &BackupIndex) -> Result<(), Diagnostic> {
    let rel = RelPath::new(BACKUP_INDEX).expect("the index's path is a relative path");
    fsops::mkdir_private(data, &backup_dir())?;
    fsops::write(data, &rel, canonical_json(index).as_bytes(), 0o600)
}

fn backup_dir() -> RelPath {
    RelPath::new(BACKUP_DIR).expect("the backup directory is a relative path")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_the_path_and_the_content_and_nothing_else() {
        let first = name_for(".inputrc", b"old\n", Kind::Original);
        assert_eq!(first, name_for(".inputrc", b"old\n", Kind::Original));
        assert_eq!(first.len(), 64 + 1 + CONTENT_PREFIX);
        assert!(first.starts_with(&sha256_hex(b".inputrc")));
        // Different content, different name; different path, different name.
        assert_ne!(first, name_for(".inputrc", b"new\n", Kind::Original));
        assert_ne!(first, name_for(".bashrc", b"old\n", Kind::Original));
        // A drift copy never collides with the original it must not replace.
        let drift = name_for(".inputrc", b"old\n", Kind::Drift);
        assert_ne!(first, drift);
        assert!(drift.contains(".drift-"));
    }

    #[test]
    fn only_the_shapes_this_build_writes_are_digests_and_names() {
        let digest = sha256_hex(b"x");
        assert!(is_digest(&digest));
        assert!(!is_digest(&digest.to_uppercase()));
        assert!(!is_digest(&digest[..63]));
        assert!(!is_digest(&"€".repeat(21)));
        for kind in [Kind::Original, Kind::Drift] {
            assert!(is_name(&name_for(".inputrc", b"old\n", kind)));
        }
        for bad in [
            "",
            "-",
            "a-1",
            &format!("{digest}-"),
            &"€".repeat(27),
            "../index.json",
        ] {
            assert!(!is_name(bad), "{bad}");
        }
    }

    #[test]
    fn the_index_finds_the_one_original_of_a_path() {
        let mut index = BackupIndex::default();
        index.backups.insert(
            "a-1".into(),
            BackupRecord {
                path: ".inputrc".into(),
                sha256: "x".into(),
                mode: "0600".into(),
                kind: "drift".into(),
            },
        );
        assert!(index.original_for(".inputrc").is_none());
        index.backups.insert(
            "a-2".into(),
            BackupRecord {
                path: ".inputrc".into(),
                sha256: "y".into(),
                mode: "0644".into(),
                kind: "original".into(),
            },
        );
        let (name, record) = index.original_for(".inputrc").unwrap();
        assert_eq!(name, "a-2");
        assert_eq!(record.mode, "0644");
    }
}
