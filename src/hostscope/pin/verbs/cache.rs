//! The versions cache of `lodi pin NAME` (V2, design call D11): one file per name at
//! `$LODI_HOME/cache/versions/<distro>/<name>.json`, holding the instant it was fetched and what
//! the archive offered then. A file younger than one day is used with zero requests; an older
//! one is refetched; a file that does not read back as exactly this name's record is refused and
//! refetched, never repaired — like `api-cache` (LD-404), an entry this build does not read is
//! fetched again, never refused. With no usable file and no network the lookup is
//! `E_REPO_UNREACHABLE` (exit 4): a stale answer is never served in place of a fetch.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::diag::Diagnostic;

/// How long a fetched answer is used before it is fetched again.
pub const MAX_AGE: i64 = 86_400;

/// The schema version this build writes and reads (`lodi::schema`'s `versions-cache` row).
const VERSION: u64 = 1;

/// One version an archive offers and the instant it arrived there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arrival {
    pub arrived: i64,
    pub version: String,
}

/// Where a lookup's answer came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A cache file younger than [`MAX_AGE`]: no request was made.
    Cached,
    /// One fetch, now written to the cache.
    Fetched,
}

#[derive(Debug)]
pub struct Lookup {
    pub rows: Vec<Arrival>,
    pub source: Source,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    distro: String,
    fetched: i64,
    #[serde(rename = "lodiVersion")]
    lodi_version: String,
    name: String,
    version: u64,
    versions: Vec<Arrival>,
}

/// The cache of one distribution under one `LODI_HOME`.
pub struct VersionsCache {
    dir: PathBuf,
    distro: String,
}

impl VersionsCache {
    pub fn new(lodi_home: &Path, distro: &str) -> VersionsCache {
        VersionsCache {
            dir: lodi_home.join("cache").join("versions").join(distro),
            distro: distro.to_string(),
        }
    }

    /// The file for `name`; `None` for a string that is not a package name, which therefore
    /// never becomes a path.
    pub fn path(&self, name: &str) -> Option<PathBuf> {
        crate::hostscope::manifest::is_package_name(name)
            .then(|| self.dir.join(format!("{name}.json")))
    }

    /// What the archive offers for `name` at `now`: the cache file when it is younger than a
    /// day, otherwise one call of `fetch`, whose answer replaces the file.
    pub fn lookup(
        &self,
        name: &str,
        now: i64,
        fetch: impl FnOnce() -> Result<Vec<Arrival>, Diagnostic>,
    ) -> Result<Lookup, Diagnostic> {
        let path = self.path(name).ok_or_else(|| {
            Diagnostic::new(
                "E_UNKNOWN_PACKAGE",
                format!("`{name}` is not a package name"),
            )
        })?;
        if let Some(rows) = self.read(&path, name, now) {
            return Ok(Lookup {
                rows,
                source: Source::Cached,
            });
        }
        let rows = fetch().map_err(|e| unreachable(name, &self.distro, e))?;
        self.write(&path, name, now, &rows)?;
        Ok(Lookup {
            rows,
            source: Source::Fetched,
        })
    }

    /// The rows of a fresh, well-formed file for exactly this name; `None` for anything else.
    fn read(&self, path: &Path, name: &str, now: i64) -> Option<Vec<Arrival>> {
        let bytes = fs::read(path).ok()?;
        let file: File = serde_json::from_slice(&bytes).ok()?;
        let age = now.checked_sub(file.fetched)?;
        (file.version == VERSION
            && file.name == name
            && file.distro == self.distro
            && (0..MAX_AGE).contains(&age))
        .then_some(file.versions)
    }

    fn write(&self, path: &Path, name: &str, now: i64, rows: &[Arrival]) -> Result<(), Diagnostic> {
        let file = File {
            distro: self.distro.clone(),
            fetched: now,
            lodi_version: crate::schema::LODI_VERSION.to_string(),
            name: name.to_string(),
            version: VERSION,
            versions: rows.to_vec(),
        };
        let mut bytes = serde_json::to_vec_pretty(&file).expect("the cache file serializes");
        bytes.push(b'\n');
        let io =
            |e: std::io::Error| Diagnostic::new("E_STORE_IO", format!("{}: {e}", path.display()));
        fs::create_dir_all(&self.dir).map_err(io)?;
        crate::lock::write_atomic(path, &bytes).map_err(io)
    }
}

/// A failed fetch with no usable cache. An integrity refusal keeps its own code; anything else
/// is the archive being out of reach.
fn unreachable(name: &str, distro: &str, e: Diagnostic) -> Diagnostic {
    if matches!(e.code, "E_HASH_MISMATCH" | "E_NO_CHECKSUM") {
        return e;
    }
    let mut d = Diagnostic::new(
        "E_REPO_UNREACHABLE",
        format!(
            "cannot read which versions of `{name}` the {distro} archive offers, and no cached answer younger than a day exists"
        ),
    );
    d.notes.push(format!("{}: {}", e.code, e.message));
    d.hint("run it again once the archive is reachable")
}
