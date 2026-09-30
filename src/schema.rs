//! The on-disk schema registry: every structured file this build serializes, the schema version
//! it writes, the versions it reads, and the field that records which Lodi wrote it (M-1.0 T-1,
//! design calls D1, D3, D4, D5, D15).
//!
//! "Lodi reads every schema it has ever written, or refuses naming the version that can" is a
//! property of the whole tree, so it is stated in **one** place. [`ARTIFACTS`] is a closed table
//! with one row per file; `tests/schema.rs` reads `src/**/*.rs`, collects every type that derives
//! `Serialize`, and fails when one is not named by a row, so a new on-disk artifact cannot be
//! added without registering it (design call D4).
//!
//! No schema version is bumped at 1.0 (D1) and the locks keep the historical `format` string
//! `lodi-spike-lock/1` (D2): what 1.0 owes its users is not a new number but a survivable one.
//! Where a row's file lacked a numeric version or a writer field it is **added**, behind
//! `#[serde(default)]` on the typed records, so a file an earlier release wrote still reads (D5);
//! `tests/fixtures/schemas/<version>/` holds real artifacts written by each released version and
//! is what proves it (D15).
//!
//! Nothing here upgrades a file on read. A file outside its artifact's read-set is refused with
//! that artifact's own code, before the rest of it is deserialized (D18).

use serde::Serialize;

use crate::lock::canonical_json;

/// The version of Lodi this build is, as the bare string a `lodiVersion` field carries.
pub const LODI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The value a `generatedBy` field carries: `lodi <version>`.
pub fn generated_by() -> String {
    format!("lodi {LODI_VERSION}")
}

/// One registered on-disk artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Artifact {
    /// The stable identifier used by this table, `docs/SCHEMAS.md` and the specimen fixtures.
    pub kind: &'static str,
    /// The file name or path pattern, relative to [`Artifact::root`].
    pub path: &'static str,
    /// Which root the path is relative to, in words.
    pub root: &'static str,
    /// The schema version this build writes.
    pub writes: u64,
    /// The schema versions this build reads, in order.
    pub reads: &'static [u64],
    /// The JSON field carrying the schema version.
    pub version_field: &'static str,
    /// The JSON field carrying the version of Lodi that wrote the file.
    pub writer_field: &'static str,
    /// Every `Serialize`-deriving type in `src/**` that is part of this document, as
    /// `module::Type`. A value-shaped record (one built as a `serde_json::Value`) names none,
    /// and an artifact that shares another row's types names none either — [`Artifact::shares`]
    /// says which row holds them.
    pub types: &'static [&'static str],
    /// The `kind` of the row whose types and reader this artifact shares, when it has one.
    pub shares: Option<&'static str>,
}

impl Artifact {
    /// Whether this build reads schema version `version` of this artifact.
    pub fn reads_version(&self, version: u64) -> bool {
        self.reads.contains(&version)
    }

    /// The read-set as the refusal messages spell it: `1`, or `1, 2`.
    pub fn read_set(&self) -> String {
        self.reads
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Every structured state or metadata file this build serializes onto disk.
///
/// This excludes what is not a versioned Lodi record: user-authored manifests, the raw bytes of
/// a backup, downloaded artifacts, build logs and generated shell scripts.
pub const ARTIFACTS: &[Artifact] = &[
    Artifact {
        kind: "project-lock",
        path: "lodi.lock",
        root: "the project directory",
        writes: 1,
        reads: &[1],
        version_field: "version",
        writer_field: "generatedBy",
        types: &[
            "lock::ArtifactEntry",
            "lock::BaseEntry",
            "lock::BaseOptions",
            "lock::Closure",
            "lock::ClosureEntry",
            "lock::FileRef",
            "lock::IndexEntry",
            "lock::LockFile",
            "lock::Profile",
            "lock::RecipeRef",
            "lock::RepositoryEntry",
            "lock::RootfsEntry",
            "lock::SizedRef",
            "lock::ToolEntry",
            "lock::ToolRequest",
            "lock::ToolSpec",
        ],
        shares: None,
    },
    Artifact {
        kind: "home-lock",
        path: "lodi/home.lock",
        root: "the configuration root",
        writes: 1,
        reads: &[1],
        version_field: "version",
        writer_field: "generatedBy",
        types: &[],
        shares: Some("project-lock"),
    },
    Artifact {
        kind: "shell-lock",
        path: "cache/shell/<h32>.lock",
        root: "the store root",
        writes: 1,
        reads: &[1],
        version_field: "version",
        writer_field: "generatedBy",
        types: &[],
        shares: Some("project-lock"),
    },
    Artifact {
        // A cache, like `shell-lock`: an entry this build does not read is ignored and fetched
        // again, never refused (LD-404).
        kind: "api-cache",
        path: "cache/api/<h32>.json",
        root: "the store root",
        writes: 1,
        reads: &[1],
        version_field: "version",
        writer_field: "lodiVersion",
        types: &["fetch::api_cache::CachedResponse"],
        shares: None,
    },
    Artifact {
        // A cache, like `api-cache`: what an archive offers for one name, used for a day by
        // `lodi host versions` (M-Pin D11); an entry this build does not read is fetched again.
        kind: "versions-cache",
        path: "cache/versions/<distro>/<name>.json",
        root: "the store root",
        writes: 1,
        reads: &[1],
        version_field: "version",
        writer_field: "lodiVersion",
        types: &[
            "hostscope::pin::verbs::cache::Arrival",
            "hostscope::pin::verbs::cache::File",
        ],
        shares: None,
    },
    Artifact {
        kind: "home-state",
        path: "home-scope/state.json",
        root: "the data root",
        // Version 4 only for a record that names a user service or managed lingering (hc-1,
        // LD-427); a home without `[services]` is written as version 3, unchanged.
        writes: 4,
        reads: &[1, 2, 3, 4],
        version_field: "version",
        writer_field: "lodiVersion",
        types: &[
            "home::state::HomeState",
            "home::state::ManagedFile",
            "home::state::OriginRecord",
            "home::state::ServiceRecord",
        ],
        shares: None,
    },
    Artifact {
        kind: "home-backup-index",
        path: "home-scope/backups/index.json",
        root: "the data root",
        writes: 1,
        reads: &[1],
        version_field: "version",
        writer_field: "lodiVersion",
        types: &["home::backup::BackupIndex", "home::backup::BackupRecord"],
        shares: None,
    },
    Artifact {
        kind: "home-gc-root",
        path: "gcroots/home",
        root: "the store root",
        writes: 1,
        reads: &[1],
        version_field: "version",
        writer_field: "lodiVersion",
        types: &[],
        shares: None,
    },
    Artifact {
        kind: "session-root",
        path: "gcroots/sessions/<pid>",
        root: "the store root",
        writes: 1,
        reads: &[1],
        version_field: "version",
        writer_field: "lodiVersion",
        types: &[],
        shares: None,
    },
    Artifact {
        kind: "host-lock",
        path: "etc/lodi/host.lock",
        root: "the host root",
        // Version 5 only for a record that holds an OS basic (sd-1, LD-420), else version 4 only
        // for a record that names a declared service (sc-1, LD-418) or managed accounts or groups
        // (su-1, LD-419), else version 3 only for a record of a host read from a git URL; a
        // directory's record is written as version 2, unchanged (LD-401).
        writes: 5,
        reads: &[1, 2, 3, 4, 5],
        version_field: "version",
        writer_field: "generatedBy",
        types: &[
            "hostscope::lock::Baseline",
            "hostscope::lock::BasicRecord",
            "hostscope::lock::FileRecord",
            "hostscope::lock::GitRecord",
            "hostscope::lock::HostLock",
            "hostscope::lock::PackageRecord",
        ],
        shares: None,
    },
    Artifact {
        // The host directory's pin lock (M-Pin, LD-395): beside `host.toml`, user-owned and
        // committable. It deliberately records no writer, so that the same pins resolved by two
        // builds on two machines give the same bytes; a refusal says it names none.
        kind: "host-pins",
        path: "<host directory>/pins.lock",
        root: "the host directory",
        // Version 2 only for a Fedora host, whose records name one exact build (fk-1, LD-434);
        // every other host's lock is written as version 1, unchanged.
        writes: 2,
        reads: &[1, 2],
        version_field: "version",
        writer_field: "generatedBy",
        types: &[
            "hostscope::pin::PinRecord",
            "hostscope::pin::PinsLock",
            "hostscope::pin::SnapshotRecord",
        ],
        shares: None,
    },
    Artifact {
        // The repository's one lock (M-Flake F-23, LD-416): at the root a `SOURCE` names, user-
        // owned and committable. Host sections are 1.4's `pins.lock` bodies and home sections
        // 1.4's `home.lock` bodies, moved in unchanged. Like `pins.lock` it records no writer of
        // its own, so that equal inputs give equal bytes on any machine.
        kind: "repository-lock",
        path: "<repository>/lodi.lock",
        root: "the repository",
        // Version 2 only when it holds a Fedora host (fk-1, LD-434); a lock with none is written
        // as version 1, byte for byte as before.
        writes: 2,
        reads: &[1, 2],
        version_field: "version",
        writer_field: "generatedBy",
        types: &[
            "flakelock::HomeSection",
            "flakelock::HostSection",
            "flakelock::RootLock",
        ],
        shares: None,
    },
    Artifact {
        kind: "host-journal",
        path: "var/lib/lodi/host/journal/<id>.json",
        root: "the host root",
        writes: 1,
        reads: &[1],
        version_field: "version",
        writer_field: "lodiVersion",
        types: &[
            "hostscope::journal::Entry",
            "hostscope::journal::Event",
            "hostscope::journal::Header",
            "hostscope::plan::Facts",
            "hostscope::plan::PackageFact",
        ],
        shares: None,
    },
    Artifact {
        kind: "store-sidecar",
        path: "store/.meta/<entry>.json",
        root: "the store root",
        writes: 1,
        reads: &[1],
        version_field: "version",
        writer_field: "lodiVersion",
        types: &["store::Meta"],
        shares: None,
    },
    Artifact {
        kind: "image-record",
        path: "store/.meta/img-<h32>.json",
        root: "the store root",
        writes: 1,
        reads: &[1],
        version_field: "version",
        writer_field: "lodiVersion",
        types: &["container::ImageMeta"],
        shares: None,
    },
    Artifact {
        kind: "environment-plan",
        path: "store/env-<h32>/env.json",
        root: "the store root",
        writes: 1,
        reads: &[1],
        // The realized plan has always spelled its schema version `schema`, beside the
        // `format` string `lodi-spike-plan/1`. Renaming the field would move every byte of a
        // document whose hash names the store entry, so the registry records the name that is
        // there (D1, D2).
        version_field: "schema",
        writer_field: "lodiVersion",
        types: &[],
        shares: None,
    },
    Artifact {
        kind: "store-layout",
        path: ".layout.json",
        root: "the store root",
        writes: 1,
        reads: &[1],
        // The store's layout version *is* this document's schema version: the marker records
        // what shape the store is and nothing else, so a second number could only disagree with
        // the first one (M-1.0 T-2, design call D7). It is therefore the one row whose read-set
        // is also the set of store layouts this build works with, and the one artifact where a
        // version *below* the read-set is migrated — once, additively, under the exclusive store
        // lock — rather than refused (`src/layout.rs`; every other row obeys D18 unchanged).
        version_field: "layout",
        writer_field: "lodiVersion",
        types: &["layout::Marker"],
        shares: None,
    },
    Artifact {
        kind: "trust-store",
        path: "lodi/trust.json",
        root: "the configuration root",
        writes: 1,
        reads: &[1],
        version_field: "version",
        writer_field: "lodiVersion",
        types: &["trust::TrustEntry", "trust::TrustFile"],
        shares: None,
    },
];

/// The registered artifact called `kind`.
///
/// # Panics
///
/// Panics when `kind` is not a registered artifact: every caller names a literal from
/// [`ARTIFACTS`], so a miss is a programming error, never input.
pub fn artifact(kind: &str) -> &'static Artifact {
    ARTIFACTS
        .iter()
        .find(|a| a.kind == kind)
        .unwrap_or_else(|| panic!("`{kind}` is not a registered on-disk artifact"))
}

/// The Lodi that wrote `raw`, read from the raw JSON **before** the document is deserialized
/// (design call D3). `lodi 0.3.0` and `0.3.0` are both read as `0.3.0`; anything absent, empty
/// or not a string is `None`, and nothing is guessed from it.
pub fn writer_of(kind: &str, raw: &serde_json::Value) -> Option<String> {
    let field = artifact(kind).writer_field;
    let text = raw.get(field)?.as_str()?.trim();
    let text = match text.strip_prefix("lodi ") {
        Some(rest) => rest.trim(),
        // The bare word with nothing after it names no version, so it is no writer at all.
        None if text == "lodi" => "",
        None => text,
    };
    (!text.is_empty()).then(|| text.to_string())
}

/// The note every schema refusal carries: which Lodi wrote the file, or that it does not say.
///
/// A file whose writer field is absent, empty or unreadable names **no** version: a guessed
/// version printed as an instruction is worse than no instruction (design call D3).
pub fn writer_note(kind: &str, raw: &serde_json::Value) -> String {
    note_for(writer_of(kind, raw).as_deref())
}

/// [`writer_note`] for a writer that has already been read out of the raw JSON.
pub fn note_for(writer: Option<&str>) -> String {
    match writer {
        Some(version) => format!("written by lodi {version}"),
        None => "this file does not record which lodi wrote it".to_string(),
    }
}

/// The schema version `raw` declares, or `None` when the field is absent or is not a number.
pub fn version_of(kind: &str, raw: &serde_json::Value) -> Option<u64> {
    raw.get(artifact(kind).version_field)?.as_u64()
}

/// The canonical JSON of `record` with this build's schema version and writer field added, for a
/// value-shaped artifact that has no typed record of its own (the GC roots and the realized
/// environment plan).
///
/// The fields are added to the **document**, never to the value an identity is hashed from: an
/// environment's name must not move because the version of Lodi that realized it did.
pub fn stamped_json<T: Serialize>(kind: &str, record: &T) -> String {
    let mut value = serde_json::to_value(record).expect("an on-disk record serializes");
    stamp(kind, &mut value);
    canonical_json(&value)
}

/// Parse `bytes` as `T` after deciding the registered schema version from the raw JSON.
///
/// `None` when the bytes are not JSON, carry a version outside the artifact's read-set, or do
/// not deserialize. It is for the records whose existing behaviour treats an unreadable file as
/// *absent* rather than as an error — a store entry's sidecar and an image record, where an
/// entry with no usable sidecar is simply not a complete entry. Nothing is upgraded on read and
/// a file from a newer schema is never partly believed (design call D18).
pub fn parse_registered<T: serde::de::DeserializeOwned>(kind: &str, bytes: &[u8]) -> Option<T> {
    let raw: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let version = version_of(kind, &raw).unwrap_or(artifact(kind).writes);
    artifact(kind).reads_version(version).then_some(())?;
    serde_json::from_value(raw).ok()
}

/// Add this build's schema version and writer field to `value` in place.
pub fn stamp(kind: &str, value: &mut serde_json::Value) {
    let artifact = artifact(kind);
    let Some(object) = value.as_object_mut() else {
        return;
    };
    object.insert(
        artifact.version_field.to_string(),
        serde_json::Value::from(artifact.writes),
    );
    object.insert(
        artifact.writer_field.to_string(),
        serde_json::Value::from(LODI_VERSION),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_is_consistent_and_named_once() {
        let mut kinds: Vec<&str> = ARTIFACTS.iter().map(|a| a.kind).collect();
        kinds.sort_unstable();
        let unique = kinds.len();
        kinds.dedup();
        assert_eq!(kinds.len(), unique, "two rows share a kind");
        for a in ARTIFACTS {
            assert!(
                a.reads_version(a.writes),
                "{}: writes what it cannot read",
                a.kind
            );
            assert!(!a.version_field.is_empty() && !a.writer_field.is_empty());
            if let Some(shared) = a.shares {
                assert!(a.types.is_empty(), "{}: a sharing row owns no type", a.kind);
                assert!(!artifact(shared).types.is_empty());
            }
        }
    }

    #[test]
    fn a_writer_is_read_in_both_spellings_and_never_guessed() {
        let generated = serde_json::json!({ "generatedBy": "lodi 0.2.0" });
        assert_eq!(
            writer_of("project-lock", &generated).as_deref(),
            Some("0.2.0")
        );
        assert_eq!(
            writer_note("project-lock", &generated),
            "written by lodi 0.2.0"
        );
        let bare = serde_json::json!({ "lodiVersion": "0.3.0" });
        assert_eq!(writer_of("store-sidecar", &bare).as_deref(), Some("0.3.0"));
        for absent in [
            serde_json::json!({}),
            serde_json::json!({ "lodiVersion": "" }),
            serde_json::json!({ "lodiVersion": 3 }),
            serde_json::json!({ "lodiVersion": "lodi  " }),
        ] {
            assert_eq!(writer_of("store-sidecar", &absent), None, "{absent}");
            assert_eq!(
                writer_note("store-sidecar", &absent),
                "this file does not record which lodi wrote it"
            );
        }
    }

    #[test]
    fn stamping_adds_the_registered_fields() {
        let text = stamped_json("session-root", &serde_json::json!({ "entries": ["a"] }));
        let back: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(version_of("session-root", &back), Some(1));
        assert_eq!(
            writer_of("session-root", &back).as_deref(),
            Some(LODI_VERSION)
        );
        assert!(text.ends_with('\n'));
    }
}
