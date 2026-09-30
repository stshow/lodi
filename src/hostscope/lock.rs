//! `/etc/lodi/host.lock`: the **record of what was applied**, not a pin (OD-15, ADR-013, design
//! call D5/LD-117).
//!
//! There is no `lodi host lock` command and no resolution step: `plan` and `apply` read the lock
//! when it is there and work without it when it is not. What it records is what makes two other
//! promises keepable — drift is a comparison against the digest Lodi last wrote, and
//! `auto_remove` is bounded to the packages Lodi itself installed.
//!
//! Canonical JSON, keys sorted by the field order below, two-space indent, trailing newline, so
//! that an unchanged apply writes unchanged bytes. It is written atomically at the end of a
//! successful apply; a failure leaves the previous record in place.
//!
//! # Schema 2: the baseline a reconcile merges from (LD-378)
//!
//! `lodi-host-lock/2` adds two fields to version 1 and changes none. [`Baseline`] is Lodi's record
//! of the machine at the last sync — an import, a writing reconcile or an apply: the explicitly
//! installed names from the distribution, the apt holds, and the digest of each captured or
//! managed file. It is the **base** of the three-way merge `lodi host import` makes over an
//! existing manifest ([`super::reconcile`]); without it a name the person deleted would come back
//! at the next reconcile. `source` is the directory, inside the root, the manifest of that sync
//! was read from. A version-1 record has neither field, and a reconcile over it reports and writes
//! nothing.
//!
//! # Schema 3: the git revision a URL apply read (LD-401)
//!
//! `lodi-host-lock/3` adds [`GitRecord`] and nothing else, and only a record of a host read from a
//! git URL is written as version 3: a directory's record stays version 2, byte for byte. `source`
//! is then the canonical URL. This build reads versions 1 to 3; a 1.4 build refuses a version-3
//! record as a version it does not read.
//!
//! # Schema 4: the declared services and the managed accounts and groups (sc-1, LD-418; su-1,
//! LD-419)
//!
//! `lodi-host-lock/4` adds `services`, each unit `[services]` declared at the last apply and its
//! state, and `users` and `groups`, the names a `host.toml` declared at the last apply, and
//! nothing else; a git record may come with it. Only a record that names a service, an account
//! or a group is written as version 4, so any other record stays version 2 or 3, byte for byte.
//! A unit the record names and the manifest no longer declares goes back to its preset. An
//! account or group name that leaves the manifest is said to be no longer managed and leaves
//! the record, and the account or group itself is never touched.
//!
//! # Schema 5: the OS basics (sd-1, LD-420)
//!
//! `lodi-host-lock/5` adds `basics`: for each of `hostname`, `timezone`, `locale` and `keymap`
//! the host declared at the last apply, its `value` and what it `was` before lodi first set it,
//! which an apply puts back once the declaration is gone; `firewall`, the digest of lodi's table;
//! and `network`, the interfaces lodi's own network files configure. Only a record that holds a
//! basic is written as version 5, so every other record keeps its version byte for byte. This
//! build reads versions 1 to 5.
//!
//! bc-1 (LD-423) adds two kinds of basic to version 5: `kernel.parameters`, the parameters the
//! loader was last written with, and `sysctl.NAME`, each sysctl value with what it `was`.
//!
//! bl-1 (LD-424) adds `boot.loader`, the declared loader with the one it replaced as its `was`,
//! and `boot.timeout` and `boot.default`, each as lodi last wrote it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::diag::Diagnostic;

/// The lock schema this build writes for a host directory. It reads versions 1 to 3
/// (`src/schema.rs`).
pub const VERSION: u64 = 2;
/// The explicitly versioned schema name this build writes; no compatibility beyond the read-set
/// is promised.
pub const FORMAT: &str = "lodi-host-lock/2";
/// The schema of a record that carries a [`GitRecord`] (LD-401).
pub const GIT_VERSION: u64 = 3;
/// The schema of a record that names a declared service (sc-1, LD-418) or managed accounts or
/// groups (su-1, LD-419).
pub const SERVICES_VERSION: u64 = 4;
/// The schema of a record that holds an OS basic (sd-1, LD-420).
pub const BASICS_VERSION: u64 = 5;
/// The directory, inside the root, the in-place manifest is read from: the `source` a sync of
/// `/etc/lodi/host.toml` records.
pub const IN_PLACE_SOURCE: &str = "/etc/lodi";

/// The format name each version this build reads carries.
fn format_of(version: u64) -> String {
    format!("lodi-host-lock/{version}")
}
/// The registry row this record is registered as (`src/schema.rs`, M-1.0 T-1).
pub const ARTIFACT: &str = "host-lock";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostLock {
    pub version: u64,
    pub format: String,
    pub generated_by: String,
    /// When the apply that wrote this record finished, RFC 3339 UTC.
    pub applied_at: String,
    /// `ID` from the root's `os-release` at that time.
    pub distro: String,
    /// `VERSION_ID`, or `rolling`.
    pub distro_version: String,
    /// The packages **Lodi** installed, with the versions observed after the transaction. It is
    /// a record: a package installed by hand is not in it and `auto_remove` never touches it.
    pub packages: BTreeMap<String, PackageRecord>,
    /// Each managed file, by its absolute path inside the root.
    pub files: BTreeMap<String, FileRecord>,
    /// The machine at the last sync: the base of a reconcile (schema 2). A version-1 record has
    /// none, and is read with none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<Baseline>,
    /// The directory inside the root the manifest of the last sync was read from (schema 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The git revision the manifest of the last sync was read from, when it was a URL (schema 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitRecord>,
    /// Each unit `[services]` declared at the last apply, `enabled` or `disabled` (schema 4).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<String, String>,
    /// The accounts `[users]` declared at the last apply (schema 4).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub users: BTreeSet<String>,
    /// The groups `[groups]` declared at the last apply (schema 4).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub groups: BTreeSet<String>,
    /// The OS basics the last apply set, by name (schema 5).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub basics: BTreeMap<String, BasicRecord>,
}

/// One OS basic as the last apply left it (sd-1): its value, and for a `[system]` basic the value
/// the machine had before lodi first set it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BasicRecord {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub was: Option<String>,
}

/// What a URL apply locked (LD-401): the canonical URL, the ref asked for, the commit it named
/// and the NAR SHA-256 of that commit's tree. A re-apply uses `rev` and asks nobody.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitRecord {
    pub url: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub rev: String,
    /// `sha256:<hex>`.
    pub nar_hash: String,
}

/// What the machine was at the last sync (LD-378): the base of the three-way merge a reconcile
/// makes. Names and digests only, never file contents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Baseline {
    /// The explicitly installed names the sync saw, from the distribution's own repositories,
    /// and the declared names the machine had explicitly installed.
    pub explicit: std::collections::BTreeSet<String>,
    /// The held names the manifest could declare, with the version each was held at. Empty on a
    /// family whose hold is not machine state.
    pub holds: BTreeMap<String, String>,
    /// Each captured or managed file, by its absolute path inside the root, and the `sha256:`
    /// digest of its bytes on the machine at the sync.
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageRecord {
    /// The version observed **after** the transaction, never a constraint and never a pin.
    pub version: String,
    /// `manual` or `auto`, as the apply set it.
    pub mark: String,
    pub held: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileRecord {
    /// `sha256:` of the bytes Lodi last wrote. Drift is a comparison against this.
    pub digest: String,
    /// Octal, as written.
    pub mode: String,
    pub owner: String,
    pub group: String,
    /// The backup of the unmanaged file this entry replaced, relative to the root; absent when
    /// there was nothing to keep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
    /// What to do when the declaration leaves the manifest.
    #[serde(default = "default_on_remove")]
    pub on_remove: String,
}

fn default_on_remove() -> String {
    "restore".to_string()
}

impl HostLock {
    pub fn new(applied_at: String, distro: String, distro_version: String) -> HostLock {
        HostLock {
            version: VERSION,
            format: FORMAT.to_string(),
            generated_by: crate::schema::generated_by(),
            applied_at,
            distro,
            distro_version,
            packages: BTreeMap::new(),
            files: BTreeMap::new(),
            baseline: None,
            source: None,
            git: None,
            services: BTreeMap::new(),
            users: BTreeSet::new(),
            groups: BTreeSet::new(),
            basics: BTreeMap::new(),
        }
    }

    /// Record the OS basics, and the schema that carries them: version 5 when there is one.
    pub fn set_basics(&mut self, basics: BTreeMap<String, BasicRecord>) {
        self.basics = basics;
        self.settle();
    }

    /// Record `git`, and the schema that carries it: version 3 with a git record, the directory
    /// schema without one.
    pub fn set_git(&mut self, git: Option<GitRecord>) {
        self.git = git;
        self.settle();
    }

    /// Record the declared services, and the schema that carries them: version 4 when there is
    /// one, else as [`HostLock::set_git`] decides.
    pub fn set_services(&mut self, services: BTreeMap<String, String>) {
        self.services = services;
        self.settle();
    }

    /// Record the managed accounts and groups, and the schema that carries them.
    pub fn set_identities(&mut self, users: BTreeSet<String>, groups: BTreeSet<String>) {
        (self.users, self.groups) = (users, groups);
        self.settle();
    }

    /// Version 4 with declared services or managed accounts or groups, else 3 with a git
    /// record, else the directory schema: a record carries the lowest version that holds what
    /// it records.
    fn settle(&mut self) {
        self.version = if !self.basics.is_empty() {
            BASICS_VERSION
        } else if !self.services.is_empty() || !self.users.is_empty() || !self.groups.is_empty() {
            SERVICES_VERSION
        } else if self.git.is_some() {
            GIT_VERSION
        } else {
            VERSION
        };
        self.format = format_of(self.version);
    }

    /// Whether this record carries a base a reconcile may merge from for the manifest read from
    /// `source`: a schema-2 record whose last sync read that same directory.
    pub fn base_for(&self, source: &str) -> Option<&Baseline> {
        if self.version < 2 || self.source.as_deref() != Some(source) {
            return None;
        }
        self.baseline.as_ref()
    }

    /// The canonical bytes of this record.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut text = serde_json::to_string_pretty(self).expect("the host lock serializes");
        text.push('\n');
        text.into_bytes()
    }

    /// Read the lock at `path`. A lock that is not there is `Ok(None)`: `plan` and `apply` work
    /// without one. A lock that is there and unreadable is an error rather than a fresh start,
    /// because silently forgetting what was applied is how drift becomes invisible.
    pub fn read(path: &Path) -> Result<Option<HostLock>, Diagnostic> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(Diagnostic::new(
                    "E_STORE_IO",
                    format!("cannot read {}: {e}", path.display()),
                )
                .hint("the host lock records what the last apply did; fix its permissions"));
            }
        };
        let unreadable = |why: String| {
            Diagnostic::new(
                "E_LOCK_VERSION",
                format!(
                    "{} is not a host lock this build reads: {why}",
                    path.display()
                ),
            )
            .hint("use the lodi that wrote it, or move it aside and apply again")
        };
        // The schema version and the format name are decided from the raw JSON **before** the
        // record is deserialized, so a file from another release is refused as a version, never
        // as a missing field (M-1.0 T-1, design calls D3, D18).
        let raw: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| unreadable(e.to_string()))?;
        let artifact = crate::schema::artifact(ARTIFACT);
        let found = crate::schema::version_of(ARTIFACT, &raw);
        let format = raw.get("format").and_then(serde_json::Value::as_str);
        let expected = found.map(format_of);
        if !found.is_some_and(|v| artifact.reads_version(v)) || format != expected.as_deref() {
            let carries = match (format, found) {
                (Some(f), Some(v)) => format!("is {f} version {v}"),
                (Some(f), None) => format!("is {f} and records no schema version"),
                (None, Some(v)) => format!("is version {v} and names no format"),
                (None, None) => "names neither a format nor a schema version".to_string(),
            };
            let mut d = Diagnostic::new(
                "E_LOCK_VERSION",
                format!(
                    "{} {carries}; this build reads lodi-host-lock version {}",
                    path.display(),
                    artifact.read_set()
                ),
            );
            d.notes.push(crate::schema::writer_note(ARTIFACT, &raw));
            return Err(d.hint("use the lodi that wrote it, or move it aside and apply again"));
        }
        let lock: HostLock = serde_json::from_value(raw).map_err(|e| unreadable(e.to_string()))?;
        // Version 1 has no baseline and no source: a version-1 file that carries either is not a
        // file any release wrote.
        if lock.version < 2 && (lock.baseline.is_some() || lock.source.is_some()) {
            return Err(unreadable(
                "a version-1 record carries a field version 2 introduced".to_string(),
            ));
        }
        if (lock.version < GIT_VERSION && lock.git.is_some())
            || (lock.version == GIT_VERSION && lock.git.is_none())
        {
            return Err(unreadable(
                "a git record and schema version 3 come together or not at all".to_string(),
            ));
        }
        let identities =
            !(lock.services.is_empty() && lock.users.is_empty() && lock.groups.is_empty());
        if (lock.version < SERVICES_VERSION && identities)
            || (lock.version == SERVICES_VERSION && !identities)
        {
            return Err(unreadable(
                "declared services, managed accounts or groups and schema version 4 come together \
                 or not at all"
                    .to_string(),
            ));
        }
        if (lock.version == BASICS_VERSION) == lock.basics.is_empty() {
            return Err(unreadable(
                "OS basics and schema version 5 come together or not at all".to_string(),
            ));
        }
        // What the record keeps of a basic reaches its tool's argv when the declaration goes,
        // so it is held to the grammar the manifest holds it to.
        if let Some((key, _)) = lock.basics.iter().find(|(key, record)| match key.as_str() {
            "firewall" => !record.value.starts_with("sha256:") || record.was.is_some(),
            "network" => {
                record.was.is_some()
                    || !record
                        .value
                        .split(' ')
                        .all(super::manifest::is_interface_name)
            }
            super::kernel::PARAMETERS_KEY => {
                record.was.is_some() || !record.value.split(' ').all(super::manifest::is_parameter)
            }
            key if key.starts_with(super::kernel::SYSCTL_KEY) => {
                !super::manifest::is_sysctl_name(&key[super::kernel::SYSCTL_KEY.len()..])
                    || !super::manifest::is_sysctl_value(&record.value)
                    || !record
                        .was
                        .as_deref()
                        .is_some_and(super::manifest::is_sysctl_value)
            }
            super::boot::LOADER_KEY => {
                let loader = |name: &str| super::manifest::Loader::parse(name).is_some();
                !loader(&record.value) || !record.was.as_deref().is_some_and(loader)
            }
            super::boot::TIMEOUT_KEY => {
                record.was.is_some() || !record.value.parse::<u32>().is_ok_and(|n| n <= 600)
            }
            super::boot::DEFAULT_KEY => {
                record.was.is_some() || !super::manifest::is_boot_default(&record.value)
            }
            _ => {
                !super::manifest::is_basic(key, &record.value)
                    || record
                        .was
                        .as_ref()
                        .is_some_and(|was| !super::manifest::is_basic(key, was))
            }
        }) {
            return Err(unreadable(format!(
                "it records the basic {key:?} as nothing lodi sets"
            )));
        }
        // A recorded name is only ever printed, never run; it is still held to the grammar a
        // manifest name is held to.
        if let Some(name) = lock
            .users
            .iter()
            .chain(&lock.groups)
            .find(|name| !super::manifest::is_identity_name(name))
        {
            return Err(unreadable(format!(
                "it records {name:?} as an account or group, which is not such a name"
            )));
        }
        if let Some((unit, state)) = lock.services.iter().find(|(unit, state)| {
            !super::manifest::is_unit_name(unit)
                || !["enabled", "disabled"].contains(&state.as_str())
        }) {
            return Err(unreadable(format!(
                "it records {unit:?} as {state:?}, which is not a declared service"
            )));
        }
        // A recorded package name reaches a package manager's argv when `auto_remove` removes
        // it, so it is held to the grammar a manifest name is held to before anything uses it.
        if let Some(name) = lock
            .packages
            .keys()
            .chain(
                lock.baseline
                    .iter()
                    .flat_map(|b| b.explicit.iter().chain(b.holds.keys())),
            )
            .find(|name| !super::manifest::is_package_name(name))
        {
            return Err(unreadable(format!(
                "it records {name:?} as a package, which is not a package name"
            )));
        }
        Ok(Some(lock))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HostLock {
        let mut lock = HostLock::new("2026-09-21T00:00:00Z".into(), "debian".into(), "12".into());
        lock.packages.insert(
            "git".into(),
            PackageRecord {
                version: "1:2.39.5-0+deb12u2".into(),
                mark: "manual".into(),
                held: false,
            },
        );
        lock.files.insert(
            "/etc/lodi-example.conf".into(),
            FileRecord {
                digest: "sha256:00".into(),
                mode: "0644".into(),
                owner: "root".into(),
                group: "root".into(),
                backup: None,
                on_remove: "restore".into(),
            },
        );
        lock
    }

    #[test]
    fn the_record_round_trips_byte_for_byte() {
        let lock = sample();
        let bytes = lock.to_bytes();
        let back: HostLock = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, lock);
        assert_eq!(back.to_bytes(), bytes);
        assert!(bytes.ends_with(b"\n"));
    }

    /// A package key that is not a package name — one that would read as an option, or carries
    /// a path or a space — is refused as a lock this build does not read, before anything uses it.
    #[test]
    fn a_package_key_is_held_to_the_package_name_grammar() {
        let dir = std::path::Path::new(env!("OUT_DIR")).join("hostscope-lock-keys");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("host.lock");
        fs::write(&path, sample().to_bytes()).unwrap();
        assert_eq!(HostLock::read(&path).unwrap(), Some(sample()));
        for bad in ["-oAPT::Get::x", "--root", "a b", "../x", "", "x/y"] {
            let mut lock = sample();
            let record = lock.packages["git"].clone();
            lock.packages.insert(bad.to_string(), record);
            fs::write(&path, lock.to_bytes()).unwrap();
            let error = HostLock::read(&path).expect_err(bad);
            assert_eq!(error.code, "E_LOCK_VERSION", "{bad:?}: {}", error.message);
            assert!(
                error.message.contains("which is not a package name"),
                "{bad:?}: {}",
                error.message
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn it_records_versions_and_never_a_constraint() {
        let text = String::from_utf8(sample().to_bytes()).unwrap();
        assert!(text.contains("1:2.39.5-0+deb12u2"), "{text}");
        for pin in [">=", "<=", "~>", "constraint", "snapshot"] {
            assert!(!text.contains(pin), "the record carries {pin}: {text}");
        }
    }
}
