//! Pinning a host's packages to the distribution's dated archives (M-Pin, LD-395).
//!
//! The model is the owner's flake model of 2026-09-24: the user-owned host directory is the
//! flake, `host.toml` declares what is pinned and `pins.lock` beside it records what those pins
//! resolved to, as `flake.lock` does. `/etc/lodi/host.lock` stays the machine's record of what
//! was applied (LD-117) and is never read as a pin.
//!
//! - `[host] snapshot` is the file's default pin (D-A): the dated archive at that instant is where
//!   a Lodi transaction installs a missing package from. Nothing already installed moves, and
//!   nothing is held.
//! - `[packages.pin]`, and `[packages.<distro>.pin]` over it, pin one name exactly: a version, or
//!   a date whose dated index names the version. The entry is installed at exactly that version,
//!   held, and read back.
//!
//! Precedence, per name: `[packages.<distro>.pin]`, then `[packages.pin]`, then `[host]
//! snapshot`, then the live archive.
//!
//! This module holds no URL. Every dated URL is rendered from the catalogue's `snapshot_url`
//! (`catalogue/bases/<distro>.toml`), the per-package interface from its `versions_url`, and a
//! declared `[sources]` repository's from the manifest; every byte comes through the verified
//! fetch of `src/fetch.rs`. The stanzas of the private dated source set are rendered by
//! [`super::sourceset`], the one deb822 writer, under `<root>/var/lib/lodi/host/pin/` (D14):
//! `/etc/apt` and `/var/lib/apt/lists` are never written.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::catalogue::{BaseDefinition, ReleaseDefinition, builtin_base, render};
use crate::debian::control::parse_stanzas;
use crate::debian::index::Release;
use crate::debian::version::DebVersion;
use crate::diag::Diagnostic;
use crate::fetch::Fetcher;
use crate::util::{format_utc, parse_utc, sha256_hex, snapshot_id};

use super::safety::Distro;
use super::sourceset::{Entry, Stanza};

pub mod arch;
pub mod fedora;
pub mod keyring;
pub mod verbs;

/// The host directory's pin lock, beside `host.toml`.
pub const FILE: &str = "pins.lock";
/// The format this build writes and reads.
pub const FORMAT: &str = "lodi-host-pins/1";
/// The schema version of [`FORMAT`].
pub const VERSION: u64 = 1;
/// The format of a Fedora host's pins (fk-1, LD-434): each record names one exact build, with
/// fields no earlier build reads. Only a Fedora lock is written in it.
pub const FEDORA_FORMAT: &str = "lodi-host-pins/2";
/// The schema version of [`FEDORA_FORMAT`].
pub const FEDORA_VERSION: u64 = 2;
/// The registry row `pins.lock` is registered as (`src/schema.rs`).
pub const ARTIFACT: &str = "host-pins";
/// The most bytes a `pins.lock` may have before it is refused.
pub const MAX_BYTES: usize = 1024 * 1024;
/// Lodi's own state for pinning, below the root (D14): machine state, never the pin.
pub const STATE: &str = "/var/lib/lodi/host/pin";
/// The private dated source set one apply stages, uses and removes.
pub const STAGE: &str = "/var/lib/lodi/host/pin/stage";
/// The one file of the private source set.
pub const SET_FILE: &str = "lodi-pin.sources";
/// The architecture a Debian or Ubuntu index is read for.
const DEB_ARCH: &str = "amd64";
/// How many hours after a publication the per-package lookup looks for the dated index that
/// first holds it: the measured capture lag of the Ubuntu archive is under an hour (P1).
const CAPTURE_LAG_HOURS: i64 = 2;
/// How many pages of the per-package interface's listing are read at most.
const MAX_PAGES: usize = 10;

// ------------------------------------------------------------------------------ requests ---

/// What one `[packages.pin]` value asks for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Request {
    /// The version the dated archive served at this instant (seconds, floored to the hour).
    Date(i64),
    /// Exactly this version, as the distribution spells it: looked up, never parsed.
    Version(String),
}

impl Request {
    /// `date` or `version`, as `pins.lock` records the policy.
    pub fn policy(&self) -> &'static str {
        match self {
            Request::Date(_) => "date",
            Request::Version(_) => "version",
        }
    }
}

/// Why a `[packages.pin]` value is refused, with the code and the hint it is refused with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
    pub hint: String,
}

/// One `[packages.pin]` value: a UTC instant or a `YYYY-MM-DD` day is a date (floored to the
/// hour, the granularity of the apt dated archives); any other non-empty string on one line is a
/// version. `latest` is not a pin in 1.4, and an empty value or one that breaks a line is a type
/// error.
pub fn parse_value(name: &str, value: &str) -> Result<Request, Refusal> {
    if value == "latest" {
        return Err(Refusal {
            code: "E_UNSUPPORTED",
            message: format!("`{name} = \"latest\"` is not a pin this build reads"),
            hint: format!(
                "remove `{name}` from the pin table to track the live archive (`lodi host unpin \
                 --all` floats every entry); a pin is a version or a date"
            ),
        });
    }
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(Refusal {
            code: "E_TYPE",
            message: format!(
                "the pin of `{name}` must be a version or a date on one line, found {}",
                if value.is_empty() {
                    "an empty string"
                } else {
                    "a string that breaks a line"
                }
            ),
            hint: "write a version such as \"1.07.1-3\", an instant such as \
                   \"2026-09-18T14:00:00Z\" or a day such as \"2026-09-18\""
                .to_string(),
        });
    }
    if let Some(secs) = parse_utc(value) {
        return Ok(Request::Date(floor_hour(secs)));
    }
    if let Some(secs) = parse_day(value) {
        return Ok(Request::Date(secs));
    }
    Ok(Request::Version(value.to_string()))
}

/// The instant floored to the hour: the granularity every apt dated archive is pinned at.
pub fn floor_hour(secs: i64) -> i64 {
    secs - secs.rem_euclid(3600)
}

fn floor_for(distro: Distro, secs: i64) -> i64 {
    if distro == Distro::Arch {
        secs - secs.rem_euclid(86400)
    } else {
        floor_hour(secs)
    }
}

fn ceil_hour(secs: i64) -> i64 {
    let floored = floor_hour(secs);
    if floored == secs {
        secs
    } else {
        floored + 3600
    }
}

/// `YYYY-MM-DD`, read as midnight UTC of that day.
fn parse_day(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    parse_utc(&format!("{value}T00:00:00Z"))
}

/// One pinned name as the manifest declares it for this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    /// The value as written, which `pins.lock` records as `requested`.
    pub requested: String,
    pub request: Request,
}

// ------------------------------------------------------------------------------- bounds ---

/// The release definition of the machine's codename, when the catalogue carries it.
fn release_of<'a>(base: &'a BaseDefinition, codename: &str) -> Option<&'a ReleaseDefinition> {
    base.releases.values().find(|release| {
        release.codename == codename || release.aliases.iter().any(|alias| alias == codename)
    })
}

/// The base definition of an apt distribution.
fn base_of(distro: Distro) -> Result<BaseDefinition, Diagnostic> {
    match builtin_base(distro.name()) {
        Some(Ok(base)) => Ok(base),
        Some(Err(d)) => Err(d),
        None => Err(Diagnostic::new(
            "E_UNSUPPORTED",
            format!("the catalogue carries no base for {}", distro.name()),
        )),
    }
}

/// The two bounds of a dated request, decided **without a request** (`E_SNAPSHOT_TOO_OLD`,
/// `E_SNAPSHOT_FUTURE`, exit 4), by the rule `src/debian/base.rs` already applies to a container
/// base. The floor is the catalogue's `snapshot_min` for the machine's release; a release the
/// catalogue does not carry has no floor Lodi can state, and the archive itself answers.
pub fn check_bounds(
    distro: Distro,
    codename: &str,
    what: &str,
    instant: i64,
    now: i64,
) -> Result<(), Diagnostic> {
    let latest = if distro == Distro::Arch {
        crate::arch::base::latest_published_day(now)
    } else {
        now
    };
    if instant > latest {
        return Err(if distro == Distro::Arch {
            Diagnostic::new("E_SNAPSHOT_FUTURE", format!(
                "{what} {} is inside today's UTC day or later; the latest published Arch day is {}",
                format_utc(instant), format_utc(latest)
            )).hint("pin the latest published day or earlier; nothing was fetched")
        } else {
            Diagnostic::new("E_SNAPSHOT_FUTURE", format!(
                "{what} {} is in the future for this machine", format_utc(instant)
            )).hint("pin an instant that has passed, or correct the machine's clock; nothing was fetched")
        });
    }
    let base = base_of(distro)?;
    let release_name = if distro == Distro::Arch {
        "rolling"
    } else {
        codename
    };
    if let Some(release) = release_of(&base, release_name)
        && instant < release.snapshot_min
    {
        return Err(Diagnostic::new(
            "E_SNAPSHOT_TOO_OLD",
            format!(
                "{what} {} is before {}, the earliest the dated {} archive serves for {}",
                format_utc(instant),
                format_utc(release.snapshot_min),
                distro.name(),
                release.codename
            ),
        )
        .hint("pin a later instant; nothing was fetched"));
    }
    Ok(())
}

// ----------------------------------------------------------------------------- pins.lock ---

/// `pins.lock`: what the host directory's pins resolved to (`lodi-host-pins/1`). It holds no
/// machine name, user, absolute path, binary version or wall-clock time: the same requests
/// resolved against the same archives give the same bytes on any machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PinsLock {
    pub version: u64,
    pub format: String,
    /// The distribution, its release codename and Lodi's architecture name the pins are for.
    pub distro: String,
    pub release: String,
    pub arch: String,
    /// The file-level snapshot, with the digests of the dated index files it resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotRecord>,
    /// Each pinned name's resolution, by name.
    pub pins: BTreeMap<String, PinRecord>,
}

/// The file-level snapshot as it was resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotRecord {
    /// `[host] snapshot` as written.
    pub requested: String,
    /// The dated archive instant it resolved to, `YYYY-MM-DDTHH:MM:SSZ`.
    pub instant: String,
    /// `repository/dists/suite/Release` of each dated index file, and its `sha256:` digest.
    pub indexes: BTreeMap<String, String>,
}

/// One pinned name's resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PinRecord {
    /// `date` or `version`.
    pub policy: String,
    /// The `[packages.pin]` value as written.
    pub requested: String,
    /// The dated archive instant the version was read from; absent for a declared `[sources]`
    /// repository, which has no dated archive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    pub version: String,
    /// `sha256:<hex>` of the package file, as the index lists it.
    pub sha256: String,
    /// The package file, relative to the archive.
    pub filename: String,
    /// The repository by name (`debian`, `debian-security`, `ubuntu`, or a `[sources]` name);
    /// its URL is rebuilt from the catalogue or the manifest, never recorded. On Fedora, the dnf
    /// repository that served the build when it was pinned.
    pub repository: String,
    /// A Fedora build's epoch, release and architecture; `version` is then its VERSION alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    /// A Fedora build's signed Koji copy, over HTTPS: where an apply fetches exactly these bytes
    /// when no configured repository serves them any more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl PinsLock {
    /// An empty lock for this machine: [`FEDORA_FORMAT`] on Fedora, [`FORMAT`] elsewhere.
    pub fn new(distro: &str, release: &str, arch: &str) -> PinsLock {
        let (version, format) = format_for(distro);
        PinsLock {
            version,
            format: format.to_string(),
            distro: distro.to_string(),
            release: release.to_string(),
            arch: arch.to_string(),
            snapshot: None,
            pins: BTreeMap::new(),
        }
    }
}

/// The schema version and format a pin lock of `distro` is written in.
pub fn format_for(distro: &str) -> (u64, &'static str) {
    if distro == Distro::Fedora.name() {
        (FEDORA_VERSION, FEDORA_FORMAT)
    } else {
        (VERSION, FORMAT)
    }
}

/// The bytes of `lock`: JSON, keys sorted, two-space indent, `\n`, one trailing newline. It is a
/// pure function of its argument, and the only way this build writes the format (the pin verbs
/// of M-Pin's verbs lane call it).
pub fn render_lock(lock: &PinsLock) -> String {
    crate::lock::canonical_json(lock)
}

/// Parse `bytes` as a `pins.lock` named `shown`. A later format is `E_LOCK_VERSION` naming the
/// file, decided from the raw JSON before the record is deserialized.
pub fn parse_lock(bytes: &[u8], shown: &str) -> Result<PinsLock, Diagnostic> {
    let unreadable = |why: String| {
        Diagnostic::new(
            "E_LOCK_VERSION",
            format!("{shown} is not a pin lock this build reads: {why}"),
        )
        .hint("use the lodi that wrote it, or move it aside: a pin it does not record is resolved again")
    };
    let raw: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| unreadable(e.to_string()))?;
    let artifact = crate::schema::artifact(ARTIFACT);
    let found = crate::schema::version_of(ARTIFACT, &raw);
    let format = raw.get("format").and_then(serde_json::Value::as_str);
    let distro = raw.get("distro").and_then(serde_json::Value::as_str);
    match found {
        Some(version)
            if artifact.reads_version(version)
                && distro.is_some_and(|d| format_for(d) == (version, format.unwrap_or(""))) => {}
        _ => {
            return Err(unreadable(format!(
                "it is {} version {}, and this build reads {FORMAT} ({})",
                format.unwrap_or("of no format"),
                found.map_or_else(|| "none".to_string(), |v| v.to_string()),
                crate::schema::note_for(None)
            )));
        }
    }
    serde_json::from_value(raw).map_err(|e| unreadable(e.to_string()))
}

/// Read `pins.lock` from the host directory, once, through W3's trust walk and trust rule
/// ([`super::source::read_file`]): never through a link, only as a regular file, owned by the
/// directory's owners and writable by nobody else. `None` when there is none.
pub fn read_lock(host: &super::source::Host) -> Result<Option<PinsLock>, Diagnostic> {
    let Some(bytes) = super::source::read_file(host, FILE, MAX_BYTES)? else {
        return Ok(None);
    };
    parse_lock(&bytes, &host.dir.join(FILE).display().to_string()).map(Some)
}

// ---------------------------------------------------------------------------- resolution ---

/// One pinned name, resolved: from its `pins.lock` record, or in memory by this plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub name: String,
    pub requested: String,
    pub policy: &'static str,
    pub version: String,
    pub sha256: String,
    pub filename: String,
    pub repository: String,
    /// The dated archive instant, absent for a declared `[sources]` repository.
    pub snapshot: Option<i64>,
    /// `pins.lock` records it, with this request.
    pub recorded: bool,
    /// A Fedora build's signed Koji copy ([`fedora`]); `None` elsewhere.
    pub source: Option<String>,
}

impl Resolved {
    /// Where the version came from, in the one wording every plan line uses.
    pub fn origin(&self) -> String {
        match self.snapshot {
            Some(instant) => format!("{} at {}", self.repository, format_utc(instant)),
            None if self.source.is_some() => self.repository.clone(),
            None => format!("source {}", self.repository),
        }
    }

    /// The record `pins.lock` would hold for it.
    pub fn record(&self) -> PinRecord {
        if self.source.is_some() {
            return fedora::record(self);
        }
        PinRecord {
            policy: self.policy.to_string(),
            requested: self.requested.clone(),
            snapshot: self.snapshot.map(format_utc),
            version: self.version.clone(),
            sha256: self.sha256.clone(),
            filename: self.filename.clone(),
            repository: self.repository.clone(),
            epoch: None,
            release: None,
            arch: None,
            source: None,
        }
    }
}

/// The file-level snapshot of a plan: the instant, and the digests of its dated index files when
/// `pins.lock` records them for this request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub requested: String,
    pub instant: i64,
    pub indexes: BTreeMap<String, String>,
    pub recorded: bool,
}

/// Everything a plan resolved about pins, carried unchanged to the apply that performs it: an
/// apply never resolves again (LD-395).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution {
    pub snapshot: Option<Snapshot>,
    pub pins: BTreeMap<String, Resolved>,
    /// The dated index digests this plan fetched, by instant: what an apply checks the index it
    /// downloads against when `pins.lock` records none.
    pub fetched: BTreeMap<i64, BTreeMap<String, String>>,
}

impl Resolution {
    /// The `pins.lock` these resolutions make: what the pin verbs write through [`render_lock`].
    pub fn lock(&self, distro: &str, release: &str, arch: &str) -> PinsLock {
        let mut lock = PinsLock::new(distro, release, arch);
        lock.snapshot = self.snapshot.as_ref().map(|snapshot| SnapshotRecord {
            requested: snapshot.requested.clone(),
            instant: format_utc(snapshot.instant),
            indexes: if snapshot.indexes.is_empty() {
                self.fetched
                    .get(&snapshot.instant)
                    .cloned()
                    .unwrap_or_default()
            } else {
                snapshot.indexes.clone()
            },
        });
        lock.pins = self
            .pins
            .iter()
            .map(|(name, resolved)| (name.clone(), resolved.record()))
            .collect();
        lock
    }
}

/// One package as a dated or current index lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub version: String,
    pub sha256: Option<String>,
    pub filename: String,
    pub repository: String,
}

/// An index read through the verified fetch: every package it lists by name, and the digest of
/// each `Release` file it was verified against.
#[derive(Debug, Clone, Default)]
pub struct Index {
    pub packages: BTreeMap<String, Vec<Listed>>,
    pub releases: BTreeMap<String, String>,
}

/// The dated repositories of the base for `codename` at `instant`: each repository's rendered URL,
/// suites and components, from the catalogue alone. A release the catalogue does not carry takes
/// every repository of the base with its suites rendered for that codename.
pub fn dated_repositories(
    distro: Distro,
    codename: &str,
    instant: i64,
) -> Result<Vec<crate::distro::Repository>, Diagnostic> {
    let base = base_of(distro)?;
    if distro == Distro::Arch {
        let release = release_of(&base, "rolling").ok_or_else(|| {
            Diagnostic::new("E_UNSUPPORTED", "the arch base has no rolling release")
        })?;
        return crate::arch::base::repositories(&base, release, instant, "x86_64");
    }
    let names: Vec<String> = match release_of(&base, codename) {
        Some(release) => release.repositories.clone(),
        None => base.repositories.keys().cloned().collect(),
    };
    let invalid = |e: String| Diagnostic::new("E_RECIPE_INVALID", format!("{}: {e}", base.file));
    let id = snapshot_id(instant);
    let mut out = Vec::new();
    for name in names {
        let repo = &base.repositories[&name];
        out.push(crate::distro::Repository {
            name: name.clone(),
            url: render(&repo.snapshot_url, &[("snapshot_id", &id)]).map_err(invalid)?,
            suites: repo
                .suites
                .iter()
                .map(|s| render(s, &[("release", codename)]))
                .collect::<Result<_, _>>()
                .map_err(invalid)?,
            components: repo.components.clone(),
        });
    }
    Ok(out)
}

/// Read one repository's indexes through the verified fetch: each suite's `Release`, then each
/// component's `Packages.xz`, held to the size and SHA-256 the `Release` lists for it
/// (`E_HASH_MISMATCH`, nothing kept). A flat suite (`./`) is read from beside the URI.
pub fn read_index(
    fetcher: &dyn Fetcher,
    repository: &crate::distro::Repository,
    into: &mut Index,
) -> Result<(), Diagnostic> {
    for suite in &repository.suites {
        let flat = suite.ends_with('/');
        let dir = if flat {
            format!("{}{}", repository.url, suite.trim_start_matches("./"))
        } else {
            format!("{}dists/{suite}/", repository.url)
        };
        let release_url = format!("{dir}Release");
        let release_bytes = fetcher
            .get(&release_url)
            .map_err(crate::debian::base::repo_error)?;
        let release_text = String::from_utf8(release_bytes.clone()).map_err(|_| {
            Diagnostic::new(
                "E_REPO_UNREACHABLE",
                format!("{release_url} is not UTF-8 text"),
            )
        })?;
        let sums = release_sums(&release_text)
            .map_err(|e| Diagnostic::new("E_REPO_UNREACHABLE", format!("{release_url}: {e}")))?;
        let key = if flat {
            format!(
                "{}/{}Release",
                repository.name,
                suite.trim_start_matches("./")
            )
        } else {
            format!("{}/dists/{suite}/Release", repository.name)
        };
        into.releases
            .insert(key, format!("sha256:{}", sha256_hex(&release_bytes)));
        let components: Vec<String> = if flat {
            vec![String::new()]
        } else {
            repository.components.clone()
        };
        for component in components {
            let stem = if flat {
                "Packages".to_string()
            } else {
                format!("{component}/binary-{DEB_ARCH}/Packages")
            };
            // The first form the Release lists: a distribution's own archive publishes `.xz`, a
            // third-party repository often only `.gz` or the plain file.
            let Some((relative, (expected, size))) = [".xz", ".gz", ""]
                .iter()
                .map(|suffix| format!("{stem}{suffix}"))
                .find_map(|relative| sums.get(&relative).cloned().map(|sum| (relative, sum)))
            else {
                // A component this suite does not carry (a security suite without `universe`)
                // has nothing to read.
                continue;
            };
            let url = format!("{dir}{relative}");
            let compressed = fetcher.get(&url).map_err(crate::debian::base::repo_error)?;
            let actual = sha256_hex(&compressed);
            if actual != expected || compressed.len() as u64 != size {
                return Err(Diagnostic::new(
                    "E_HASH_MISMATCH",
                    format!(
                        "{url}: its Release lists sha256:{expected} ({size} bytes), got \
                         sha256:{actual} ({} bytes)",
                        compressed.len()
                    ),
                )
                .hint(
                    "the index does not match its Release file; nothing was recorded or installed",
                ));
            }
            let plain = if relative.ends_with(".xz") {
                crate::debian::base::xz_decompress(&compressed)
                    .map_err(|e| Diagnostic::new("E_HASH_MISMATCH", format!("{url}: {e}")))?
            } else if relative.ends_with(".gz") {
                let mut out = Vec::new();
                std::io::Read::read_to_end(
                    &mut flate2::read::GzDecoder::new(compressed.as_slice()),
                    &mut out,
                )
                .map_err(|e| Diagnostic::new("E_HASH_MISMATCH", format!("{url}: {e}")))?;
                out
            } else {
                compressed
            };
            let text = String::from_utf8(plain).map_err(|_| {
                Diagnostic::new("E_REPO_UNREACHABLE", format!("{url} is not UTF-8"))
            })?;
            for stanza in parse_stanzas(&text)
                .map_err(|e| Diagnostic::new("E_REPO_UNREACHABLE", format!("{url}: {e}")))?
            {
                let (Some(name), Some(version), Some(filename)) = (
                    stanza.get("Package"),
                    stanza.get("Version"),
                    stanza.get("Filename"),
                ) else {
                    continue;
                };
                into.packages
                    .entry(name.to_string())
                    .or_default()
                    .push(Listed {
                        version: version.to_string(),
                        sha256: stanza
                            .get("SHA256")
                            .filter(|hex| crate::debian::index::is_sha256_hex(hex))
                            .map(str::to_string),
                        filename: filename.to_string(),
                        repository: repository.name.clone(),
                    });
            }
        }
    }
    Ok(())
}

/// The digest of every dated `Release` file of the base at `instant`, read through the verified
/// fetch and nothing else: what `pins.lock` records for a file-level snapshot (`lodi host pin
/// --all`, M-Pin's verbs lane), keyed `repository/dists/suite/Release`.
pub fn index_digests(
    fetcher: &dyn Fetcher,
    distro: Distro,
    codename: &str,
    instant: i64,
) -> Result<BTreeMap<String, String>, Diagnostic> {
    let mut out = BTreeMap::new();
    if distro == Distro::Arch {
        for repository in dated_repositories(distro, codename, instant)? {
            let (index, _) = crate::arch::base::fetch_index(fetcher, &repository)?;
            out.insert(
                format!("{}.db", repository.name),
                format!("sha256:{}", index.packages_sha256),
            );
        }
        return Ok(out);
    }
    for repository in dated_repositories(distro, codename, instant)? {
        for suite in &repository.suites {
            let url = format!("{}dists/{suite}/Release", repository.url);
            let bytes = fetcher.get(&url).map_err(crate::debian::base::repo_error)?;
            Release::parse(&String::from_utf8_lossy(&bytes))
                .map_err(|e| Diagnostic::new("E_REPO_UNREACHABLE", format!("{url}: {e}")))?;
            out.insert(
                format!("{}/dists/{suite}/Release", repository.name),
                format!("sha256:{}", sha256_hex(&bytes)),
            );
        }
    }
    Ok(out)
}

/// The `SHA256` lines of a `Release` file: path → (hex digest, size). Nothing else of the file
/// is read, so a third-party repository's `Release` without a `Codename` still reads.
fn release_sums(text: &str) -> Result<BTreeMap<String, (String, u64)>, String> {
    let stanzas = parse_stanzas(text)?;
    let [stanza] = stanzas.as_slice() else {
        return Err(format!(
            "expected one stanza in a Release file, found {}",
            stanzas.len()
        ));
    };
    let mut out = BTreeMap::new();
    for line in stanza.get("SHA256").unwrap_or_default().lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let [hash, size, path] = parts.as_slice() {
            if !crate::debian::index::is_sha256_hex(hash) {
                return Err(format!("malformed SHA-256 `{hash}` in Release"));
            }
            let size = size
                .parse::<u64>()
                .map_err(|_| format!("malformed size `{size}` in Release"))?;
            out.insert((*path).to_string(), ((*hash).to_string(), size));
        }
    }
    Ok(out)
}

/// The dated index of the base at `instant`, read once per plan and kept.
fn dated_index<'a>(
    cache: &'a mut BTreeMap<i64, Index>,
    fetcher: &dyn Fetcher,
    distro: Distro,
    codename: &str,
    instant: i64,
) -> Result<&'a Index, Diagnostic> {
    if let std::collections::btree_map::Entry::Vacant(slot) = cache.entry(instant) {
        let mut index = Index::default();
        for repository in dated_repositories(distro, codename, instant)? {
            if distro == Distro::Arch {
                let (locked, packages) = crate::arch::base::fetch_index(fetcher, &repository)
                    .map_err(|e| if e.message.contains("%SHA256SUM%") {
                        Diagnostic::new("E_NO_CHECKSUM", format!("{}: {}", repository.name, e.message))
                            .hint("this dated Arch database has no SHA-256 for a package; nothing is pinned")
                    } else { e })?;
                index.releases.insert(
                    format!("{}.db", repository.name),
                    format!("sha256:{}", locked.packages_sha256),
                );
                for package in packages {
                    index
                        .packages
                        .entry(package.name)
                        .or_default()
                        .push(Listed {
                            version: package.version.to_string(),
                            sha256: Some(package.sha256),
                            filename: package.filename,
                            repository: repository.name.clone(),
                        });
                }
            } else {
                read_index(fetcher, &repository, &mut index)?;
            }
        }
        slot.insert(index);
    }
    Ok(&cache[&instant])
}

/// The highest version of `name` an index lists, dpkg's ordering deciding.
fn newest(listed: &[Listed], distro: Distro) -> Option<&Listed> {
    listed
        .iter()
        .max_by(|a, b| compare_for(distro, &a.version, &b.version))
}

/// Compare an Arch pin with pacman's version ordering, not dpkg's unrelated grammar.
pub fn compare_for(distro: Distro, a: &str, b: &str) -> std::cmp::Ordering {
    if distro == Distro::Fedora {
        return super::pm::dnf::compare_evr(a, b);
    }
    if distro == Distro::Arch {
        return match (
            crate::arch::version::PacmanVersion::parse(a),
            crate::arch::version::PacmanVersion::parse(b),
        ) {
            (Ok(a), Ok(b)) => a.cmp(&b),
            _ => a.cmp(b),
        };
    }
    compare(a, b)
}

/// dpkg's ordering of two versions; a version dpkg cannot read orders by its text.
pub fn compare(a: &str, b: &str) -> std::cmp::Ordering {
    match (DebVersion::parse(a), DebVersion::parse(b)) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        _ => a.cmp(b),
    }
}

/// `sha256:<hex>` of a listed package, or `E_NO_CHECKSUM` (exit 5): a version the index serves
/// with no digest is never installed on the strength of its name.
fn digest_of(name: &str, listed: &Listed, place: &str) -> Result<String, Diagnostic> {
    match &listed.sha256 {
        Some(hex) => Ok(format!("sha256:{hex}")),
        None => Err(Diagnostic::new(
            "E_NO_CHECKSUM",
            format!("{place} serves {name} {} with no SHA-256", listed.version),
        )
        .hint("lodi pins nothing it cannot verify; pin another version or date")),
    }
}

/// The nearest versions to `wanted` among `versions`, closest in dpkg's order first: the one
/// just below and the one just above, then the next ones out.
pub fn nearest_versions(wanted: &str, versions: &[String]) -> Vec<String> {
    let mut sorted: Vec<String> = versions.to_vec();
    sorted.sort_by(|a, b| compare(a, b));
    sorted.dedup();
    let at = sorted.partition_point(|v| compare(v, wanted) == std::cmp::Ordering::Less);
    let mut out = Vec::new();
    let (mut below, mut above) = (at, at);
    while out.len() < 3 && (below > 0 || above < sorted.len()) {
        if below > 0 {
            below -= 1;
            out.push(sorted[below].clone());
        }
        if out.len() < 3 && above < sorted.len() {
            out.push(sorted[above].clone());
            above += 1;
        }
    }
    out
}

fn no_match(name: &str, wanted: &str, place: &str, versions: &[String]) -> Diagnostic {
    let nearest = nearest_versions(wanted, versions);
    Diagnostic::new("E_NO_MATCH", format!("{place} has no {name} {wanted}")).hint(
        if nearest.is_empty() {
            format!("{place} lists no version of {name}; pin a date or a version that exists")
        } else {
            format!("the nearest versions are {}", nearest.join(", "))
        },
    )
}

/// What resolving needs to know of the machine and the manifest.
pub struct Context<'a> {
    pub distro: Distro,
    pub codename: &'a str,
    pub arch: &'a str,
    /// The manifest's declared `[sources]`, which a version pin is looked up in first.
    pub sources: &'a BTreeMap<String, super::manifest::SourceEntry>,
    pub now: i64,
}

/// Resolve every pinned name and the file-level snapshot. A request `lock` records, with the same
/// `requested`, is taken from the record and makes no request at all; anything else is resolved
/// here, in memory, through `fetcher` — which is asked for only when something is unrecorded.
/// The bounds of every dated request are decided first, with no request.
pub fn resolve(
    cx: &Context,
    snapshot: Option<&str>,
    pins: &BTreeMap<String, Declared>,
    lock: Option<&PinsLock>,
    fetcher: &mut dyn FnMut() -> Result<Box<dyn Fetcher>, Diagnostic>,
) -> Result<Resolution, Diagnostic> {
    if cx.distro == Distro::Fedora {
        return fedora::resolve(cx.codename, snapshot, pins, lock);
    }
    // A lock for another distribution, release or architecture records nothing for this machine.
    let lock = lock.filter(|lock| {
        lock.distro == cx.distro.name() && lock.release == cx.codename && lock.arch == cx.arch
    });
    let mut out = Resolution::default();
    if let Some(requested) = snapshot {
        let instant = floor_for(
            cx.distro,
            parse_utc(requested).ok_or_else(|| {
                Diagnostic::new(
                    "E_TYPE",
                    format!("[host] snapshot \"{requested}\" is not an RFC 3339 UTC timestamp"),
                )
            })?,
        );
        check_bounds(cx.distro, cx.codename, "[host] snapshot", instant, cx.now)?;
        let record = lock
            .and_then(|lock| lock.snapshot.as_ref())
            .filter(|record| {
                record.requested == requested && parse_utc(&record.instant) == Some(instant)
            });
        out.snapshot = Some(Snapshot {
            requested: requested.to_string(),
            instant,
            indexes: record.map(|r| r.indexes.clone()).unwrap_or_default(),
            recorded: record.is_some(),
        });
    }
    for (name, declared) in pins {
        if let Request::Date(instant) = declared.request {
            check_bounds(
                cx.distro,
                cx.codename,
                &format!("the pin of {name}"),
                floor_for(cx.distro, instant),
                cx.now,
            )?;
        }
    }
    let mut own: Option<Box<dyn Fetcher>> = None;
    let mut indexes: BTreeMap<i64, Index> = BTreeMap::new();
    let mut sources: Option<Vec<(String, Index)>> = None;
    for (name, declared) in pins {
        if let Some(record) = lock.and_then(|lock| lock.pins.get(name))
            && record.requested == declared.requested
            && record.policy == declared.request.policy()
        {
            out.pins.insert(
                name.clone(),
                Resolved {
                    name: name.clone(),
                    requested: record.requested.clone(),
                    policy: declared.request.policy(),
                    version: record.version.clone(),
                    sha256: record.sha256.clone(),
                    filename: record.filename.clone(),
                    repository: record.repository.clone(),
                    snapshot: record.snapshot.as_deref().and_then(parse_utc),
                    recorded: true,
                    source: None,
                },
            );
            continue;
        }
        if own.is_none() {
            own = Some(fetcher()?);
        }
        let fetcher = own.as_deref().expect("a fetcher");
        if sources.is_none() {
            sources = Some(if cx.distro == Distro::Arch {
                Vec::new()
            } else {
                source_indexes(fetcher, cx)?
            });
        }
        let from_source = sources
            .as_ref()
            .expect("the sources were read")
            .iter()
            .find(|(_, index)| index.packages.contains_key(name));
        let resolved = match (&declared.request, from_source) {
            (Request::Date(_), Some((source, _))) => {
                return Err(Diagnostic::new(
                    "E_UNSUPPORTED",
                    format!(
                        "{name} comes from the declared source {source}, which has no dated \
                         archive, so it cannot be pinned to a date"
                    ),
                )
                .hint(format!(
                    "pin {name} to a version its repository offers now; a date pin reaches the \
                     distribution's own dated archive only"
                )));
            }
            (Request::Version(version), Some((source, index))) => {
                let listed = &index.packages[name];
                let found = listed
                    .iter()
                    .find(|l| &l.version == version)
                    .ok_or_else(|| {
                        no_match(
                            name,
                            version,
                            &format!("the declared source {source}"),
                            &listed.iter().map(|l| l.version.clone()).collect::<Vec<_>>(),
                        )
                    })?;
                Resolved {
                    name: name.clone(),
                    requested: declared.requested.clone(),
                    policy: "version",
                    version: found.version.clone(),
                    sha256: digest_of(name, found, &format!("the declared source {source}"))?,
                    filename: found.filename.clone(),
                    repository: source.clone(),
                    snapshot: None,
                    recorded: false,
                    source: None,
                }
            }
            (Request::Date(instant), None) => {
                let instant = floor_for(cx.distro, *instant);
                let index = dated_index(&mut indexes, fetcher, cx.distro, cx.codename, instant)?;
                let place = format!(
                    "the dated {} archive at {}",
                    cx.distro.name(),
                    format_utc(instant)
                );
                let listed = index
                    .packages
                    .get(name)
                    .and_then(|l| newest(l, cx.distro))
                    .ok_or_else(|| {
                        Diagnostic::new("E_NO_MATCH", format!("{place} has no package {name}"))
                            .hint(format!("pin {name} to a later date, or check the name"))
                    })?;
                Resolved {
                    name: name.clone(),
                    requested: declared.requested.clone(),
                    policy: "date",
                    version: listed.version.clone(),
                    sha256: digest_of(name, listed, &place)?,
                    filename: listed.filename.clone(),
                    repository: listed.repository.clone(),
                    snapshot: Some(instant),
                    recorded: false,
                    source: None,
                }
            }
            (Request::Version(version), None) => resolve_version(
                fetcher,
                cx,
                &mut indexes,
                name,
                &declared.requested,
                version,
            )?,
        };
        out.pins.insert(name.clone(), resolved);
    }
    for (instant, index) in &indexes {
        out.fetched.insert(*instant, index.releases.clone());
    }
    Ok(out)
}

/// Every declared `[sources]` repository's current index, in name order: what a version pin is
/// looked up in first (LD-395).
fn source_indexes(fetcher: &dyn Fetcher, cx: &Context) -> Result<Vec<(String, Index)>, Diagnostic> {
    let mut out = Vec::new();
    for (name, source) in cx.sources {
        let mut index = Index::default();
        for uri in &source.uris {
            let url = if uri.ends_with('/') {
                uri.clone()
            } else {
                format!("{uri}/")
            };
            read_index(
                fetcher,
                &crate::distro::Repository {
                    name: name.clone(),
                    url,
                    suites: source.suites.clone(),
                    components: source.components.clone(),
                },
                &mut index,
            )?;
        }
        out.push((name.clone(), index));
    }
    Ok(out)
}

/// Whether the machine's release has a per-package interface this build verifies versions
/// through (`versions_url`, Ubuntu's): where `lodi host versions` lists every published version
/// and a version is a pin. Decided from the catalogue, with no request.
pub fn per_package_interface(cx: &Context) -> Result<bool, Diagnostic> {
    let base = base_of(cx.distro)?;
    Ok(release_of(&base, cx.codename).is_some_and(|r| r.versions_url.is_some()))
}

/// Every publication of `name` the release's per-package interface (`versions_url`) lists,
/// following the interface's own next page a bounded number of times; `None`, with no request,
/// for a release that has no such interface.
fn interface_entries(
    fetcher: &dyn Fetcher,
    cx: &Context,
    name: &str,
) -> Result<Option<Vec<serde_json::Value>>, Diagnostic> {
    let base = base_of(cx.distro)?;
    let Some(template) = release_of(&base, cx.codename).and_then(|r| r.versions_url.clone()) else {
        return Ok(None);
    };
    let encode = |text: &str| -> String {
        text.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    let url = render(
        &template,
        &[
            ("name", &encode(name)),
            ("release", cx.codename),
            ("deb_arch", DEB_ARCH),
        ],
    )
    .map_err(|e| Diagnostic::new("E_RECIPE_INVALID", format!("{}: {e}", base.file)))?;
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut next = Some(url);
    let mut pages = 0;
    while let Some(page) = next.take() {
        pages += 1;
        let answer: serde_json::Value = serde_json::from_slice(
            &fetcher
                .get(&page)
                .map_err(crate::debian::base::repo_error)?,
        )
        .map_err(|e| Diagnostic::new("E_REPO_UNREACHABLE", format!("{page}: {e}")))?;
        entries.extend(answer["entries"].as_array().cloned().unwrap_or_default());
        next = answer["next_collection_link"]
            .as_str()
            .filter(|_| pages < MAX_PAGES)
            .map(str::to_string);
    }
    Ok(Some(entries))
}

/// Whether a publication is in the release's own pockets, not `-proposed` or `-backports`.
fn own_pocket(entry: &serde_json::Value) -> bool {
    matches!(
        entry["pocket"].as_str(),
        Some("Release" | "Updates" | "Security")
    )
}

/// Each version of `name` the release's own pockets published, with the instant it was first
/// published there: what `lodi host versions NAME` lists (LD-397). `None`, with no request, on a
/// release with no verifiable per-package interface (Debian and Arch, P1).
pub fn published_versions(
    fetcher: &dyn Fetcher,
    cx: &Context,
    name: &str,
) -> Result<Option<Vec<(String, i64)>>, Diagnostic> {
    let Some(entries) = interface_entries(fetcher, cx, name)? else {
        return Ok(None);
    };
    let mut first: BTreeMap<String, i64> = BTreeMap::new();
    for entry in entries.iter().filter(|entry| own_pocket(entry)) {
        let (Some(version), Some(at)) = (
            entry["binary_package_version"].as_str(),
            entry["date_published"].as_str().and_then(published_at),
        ) else {
            continue;
        };
        let slot = first.entry(version.to_string()).or_insert(at);
        *slot = (*slot).min(at);
    }
    Ok(Some(first.into_iter().collect()))
}

/// A version pin on the distribution's own package: the release's per-package interface
/// (`versions_url`) names the instant the version was published, and the dated index at that
/// hour — or within the measured capture lag after it — must list it with a SHA-256. A release
/// whose family has no verifiable per-package interface refuses a version, naming the date form.
fn resolve_version(
    fetcher: &dyn Fetcher,
    cx: &Context,
    indexes: &mut BTreeMap<i64, Index>,
    name: &str,
    requested: &str,
    version: &str,
) -> Result<Resolved, Diagnostic> {
    // Every publication of the name: the version is looked for among them, and the others are
    // the nearest candidates.
    let Some(entries) = interface_entries(fetcher, cx, name)? else {
        return Err(Diagnostic::new(
            "E_UNSUPPORTED",
            format!(
                "{name} = \"{version}\": {} {} has no per-package interface this build can verify, \
                 so a version is not a pin here",
                cx.distro.name(),
                cx.codename
            ),
        )
        .hint(format!(
            "pin {name} to a date instead (`{name} = \"YYYY-MM-DD\"`): the version the dated \
             archive served that day is recorded and installed"
        )));
    };
    let mut published: Vec<(i64, String)> = entries
        .iter()
        .filter(|entry| entry["binary_package_version"].as_str() == Some(version))
        .filter(|entry| own_pocket(entry))
        .filter_map(|entry| {
            let at = published_at(entry["date_published"].as_str()?)?;
            Some((at, entry["self_link"].as_str()?.to_string()))
        })
        .collect();
    published.sort();
    let Some((at, link)) = published.first().cloned() else {
        // Not published in the release's own pockets at that version: name what is.
        let versions: Vec<String> = entries
            .iter()
            .filter(|entry| own_pocket(entry))
            .filter_map(|entry| entry["binary_package_version"].as_str().map(str::to_string))
            .collect();
        return Err(no_match(
            name,
            version,
            &format!("the {} {} archive", cx.distro.name(), cx.codename),
            &versions,
        ));
    };
    // The interface's own answer carries the file's SHA-256; the dated index must agree with it.
    let files_url = format!("{link}?ws.op=binaryFileUrls&include_meta=true");
    let files: serde_json::Value = serde_json::from_slice(
        &fetcher
            .get(&files_url)
            .map_err(crate::debian::base::repo_error)?,
    )
    .map_err(|e| Diagnostic::new("E_REPO_UNREACHABLE", format!("{files_url}: {e}")))?;
    let vouched: BTreeSet<String> = files
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|file| file["sha256"].as_str())
        .filter(|hex| crate::debian::index::is_sha256_hex(hex))
        .map(|hex| format!("sha256:{hex}"))
        .collect();
    if vouched.is_empty() {
        return Err(Diagnostic::new(
            "E_NO_CHECKSUM",
            format!("the per-package interface lists {name} {version} with no SHA-256"),
        )
        .hint("lodi pins nothing it cannot verify; pin a date instead"));
    }
    let first = ceil_hour(at);
    for lag in 0..=CAPTURE_LAG_HOURS {
        let instant = first + lag * 3600;
        if instant > cx.now {
            break;
        }
        let index = dated_index(indexes, fetcher, cx.distro, cx.codename, instant)?;
        let place = format!(
            "the dated {} archive at {}",
            cx.distro.name(),
            format_utc(instant)
        );
        if let Some(listed) = index
            .packages
            .get(name)
            .and_then(|l| l.iter().find(|l| l.version == version))
        {
            let sha256 = digest_of(name, listed, &place)?;
            if !vouched.contains(&sha256) {
                return Err(Diagnostic::new(
                    "E_HASH_MISMATCH",
                    format!(
                        "{place} lists {name} {version} as {sha256}, which the per-package \
                         interface does not vouch for"
                    ),
                )
                .hint(
                    "the archive and its publisher disagree; nothing was recorded or installed",
                ));
            }
            return Ok(Resolved {
                name: name.to_string(),
                requested: requested.to_string(),
                policy: "version",
                version: version.to_string(),
                sha256,
                filename: listed.filename.clone(),
                repository: listed.repository.clone(),
                snapshot: Some(instant),
                recorded: false,
                source: None,
            });
        }
    }
    Err(Diagnostic::new(
        "E_NO_MATCH",
        format!(
            "{name} {version} was published at {}, and the dated {} archive does not list it \
             within {CAPTURE_LAG_HOURS} hours after",
            format_utc(at),
            cx.distro.name()
        ),
    )
    .hint(format!("pin {name} to a later date instead")))
}

/// Launchpad's `date_published` (`2024-04-13T07:08:21.761140+00:00`), in seconds.
fn published_at(text: &str) -> Option<i64> {
    let (head, _) = text
        .split_once('.')
        .unwrap_or((text.trim_end_matches("+00:00"), ""));
    let head = head.trim_end_matches("+00:00");
    parse_utc(&format!("{head}Z"))
}

// ---------------------------------------------------------------------- the private set ---

/// The base repository an enabled machine entry lists, when it lists one: its URI's last path
/// component is the archive the catalogue's `snapshot_url` names (`debian`, `debian-security`,
/// `ubuntu`), one of its suites is one the catalogue renders for this release, and one of its
/// components is one the catalogue names. A third-party repository that happens to use the
/// release's codename as a suite (a vendor's `…/linux/debian bookworm stable`) names no catalogue
/// component and is not one. A URI that is a local mirror list (`mirror+file:`, as Debian's
/// cloud image writes both of its archives) is read as the addresses `mirrors` says it names.
fn base_repository_of(
    entry: &Entry,
    repositories: &[crate::distro::Repository],
    archives: &BTreeMap<String, String>,
    mirrors: &BTreeMap<String, Vec<String>>,
) -> Option<String> {
    let archive = |uri: &str| {
        uri.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string()
    };
    repositories
        .iter()
        .find(|repo| {
            let wanted = archives
                .get(&repo.name)
                .map(String::as_str)
                .unwrap_or_default();
            entry
                .uris
                .iter()
                .flat_map(|uri| {
                    mirrors
                        .get(uri)
                        .map_or(std::slice::from_ref(uri), Vec::as_slice)
                })
                .any(|uri| archive(uri) == wanted)
                && entry.suites.iter().any(|s| repo.suites.contains(s))
                && entry.components.iter().any(|c| repo.components.contains(c))
        })
        .map(|repo| repo.name.clone())
}

/// The archive name each base repository's `snapshot_url` serves: the path component just before
/// `${snapshot_id}`.
fn archive_names(distro: Distro) -> Result<BTreeMap<String, String>, Diagnostic> {
    let base = base_of(distro)?;
    Ok(base
        .repositories
        .iter()
        .map(|(name, repo)| {
            let head = repo
                .snapshot_url
                .split("${snapshot_id}")
                .next()
                .unwrap_or_default();
            let archive = head
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string();
            (name.clone(), archive)
        })
        .collect())
}

/// The stanzas of the private source set one apply reads (P4, D-A):
///
/// - with a file-level snapshot, one dated stanza per base repository the machine's own stanzas
///   list, directly or through a local mirror list `mirrors` reads (the suites and components they
///   list, the catalogue's where the machine lists none;
///   `Signed-By` the keyring the machine's stanza names, and none added by Lodi), in place of
///   those stanzas; without one, the machine's own stanzas as they are;
/// - one dated stanza per base repository for each other instant a per-entry pin was resolved
///   at, beside them;
/// - every other enabled stanza of the machine — a declared `[sources]` repository included — as
///   it is, because the file-level snapshot does not reach it.
///
/// Each stanza comes with whether it is dated: only a dated one is rendered with
/// `Check-Valid-Until: no` ([`super::sourceset::render_pin_set`]), because a dated `Release`
/// file's `Valid-Until` has passed by construction (P1 measured seven days after its `Date`).
pub fn private_stanzas(
    distro: Distro,
    codename: &str,
    machine: &[Entry],
    mirrors: &BTreeMap<String, Vec<String>>,
    snapshot: Option<i64>,
    instants: &BTreeSet<i64>,
) -> Result<Vec<(Stanza, bool)>, Diagnostic> {
    let archives = archive_names(distro)?;
    let catalogue = dated_repositories(distro, codename, 0)?;
    let mut out = Vec::new();
    // What each base repository looks like on this machine, merged over its stanzas.
    let mut listed: BTreeMap<String, Stanza> = BTreeMap::new();
    for entry in machine {
        if entry.inline_key || entry.trusted {
            return Err(Diagnostic::new(
                "E_UNSUPPORTED",
                format!(
                    "{} line {}: a stanza with an inline key or `trusted` cannot be carried into \
                     lodi's private source set",
                    entry.file, entry.line
                ),
            )
            .hint(
                "give that repository a keyring file with Signed-By; lodi adds no trust of its own",
            ));
        }
        let stanza = Stanza {
            types: entry.types.clone(),
            uris: entry.uris.clone(),
            suites: entry.suites.clone(),
            components: entry.components.clone(),
            architectures: entry.architectures.clone(),
            signed_by: entry.signed_by.clone().unwrap_or_default(),
        };
        match base_repository_of(entry, &catalogue, &archives, mirrors) {
            Some(name) => {
                let merged = listed.entry(name).or_insert_with(|| Stanza {
                    types: vec!["deb".to_string()],
                    signed_by: stanza.signed_by.clone(),
                    ..Stanza::default()
                });
                for suite in &stanza.suites {
                    if !merged.suites.contains(suite) {
                        merged.suites.push(suite.clone());
                    }
                }
                for component in &stanza.components {
                    if !merged.components.contains(component) {
                        merged.components.push(component.clone());
                    }
                }
                if snapshot.is_none() {
                    out.push((stanza, false));
                }
            }
            None => out.push((stanza, false)),
        }
    }
    let dated = |instant: i64| -> Result<Vec<(Stanza, bool)>, Diagnostic> {
        let mut stanzas = Vec::new();
        for repo in dated_repositories(distro, codename, instant)? {
            let (suites, components, signed_by) = match listed.get(&repo.name) {
                Some(on_machine) => (
                    on_machine.suites.clone(),
                    on_machine.components.clone(),
                    on_machine.signed_by.clone(),
                ),
                None => (repo.suites.clone(), repo.components.clone(), String::new()),
            };
            stanzas.push((
                Stanza {
                    types: vec!["deb".to_string()],
                    uris: vec![repo.url],
                    suites,
                    components,
                    architectures: Vec::new(),
                    signed_by,
                },
                true,
            ));
        }
        Ok(stanzas)
    };
    let mut every: BTreeSet<i64> = instants.clone();
    if let Some(instant) = snapshot {
        every.insert(instant);
    }
    let mut dated_out = Vec::new();
    for instant in every {
        dated_out.extend(dated(instant)?);
    }
    dated_out.extend(out);
    Ok(dated_out)
}

/// The options that point one apt invocation at the private source set staged below the root:
/// its own source list and source parts, its own lists, and no binary cache, so that nothing
/// under `/etc/apt`, `/var/lib/apt/lists` or `/var/cache/apt` is read as a source or written.
/// The paths are inside the root, which apt's own `RootDir` prefixes under `--root`.
pub fn private_options() -> Vec<String> {
    let mut out = Vec::new();
    for (key, value) in [
        ("Dir::Etc::SourceList", format!("{STAGE}/sources.list")),
        ("Dir::Etc::SourceParts", format!("{STAGE}/sources.list.d")),
        ("Dir::State::Lists", format!("{STAGE}/lists")),
        ("Dir::Cache::pkgcache", String::new()),
        ("Dir::Cache::srcpkgcache", String::new()),
    ] {
        out.push("-o".to_string());
        out.push(format!("{key}={value}"));
    }
    out
}

/// The name apt gives the list file of one index it downloaded (`URItoFileName`): the URI
/// without its scheme, every `/` a `_`.
pub fn list_file_name(uri: &str, suite: &str, file: &str) -> String {
    let bare = uri.split_once("://").map_or(uri, |(_, rest)| rest);
    let bare = bare.trim_end_matches('/');
    format!("{bare}/dists/{suite}/{file}").replace('/', "_")
}

/// The signed body of a clearsigned `InRelease`, dash-unescaped: the dated `Release` byte for
/// byte, as P1 measured on both families. `None` for anything else.
pub fn signed_body(bytes: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let rest = text.strip_prefix("-----BEGIN PGP SIGNED MESSAGE-----\n")?;
    let (_, body) = rest.split_once("\n\n")?;
    let end = body.find("\n-----BEGIN PGP SIGNATURE-----")?;
    let body = &body[..=end];
    let unescaped: Vec<&str> = body
        .split('\n')
        .map(|line| line.strip_prefix("- ").unwrap_or(line))
        .collect();
    Some(unescaped.join("\n").into_bytes())
}

/// Check the index apt downloaded into the private lists against the digests `expected` holds
/// (`repository/dists/suite/Release` → `sha256:`), before anything is installed: a difference is
/// `E_HASH_MISMATCH`, and so is a recorded index apt did not download.
pub fn check_downloaded(
    lists: &Path,
    distro: Distro,
    codename: &str,
    instant: i64,
    expected: &BTreeMap<String, String>,
) -> Result<(), Diagnostic> {
    for repo in dated_repositories(distro, codename, instant)? {
        for suite in &repo.suites {
            let key = format!("{}/dists/{suite}/Release", repo.name);
            let Some(want) = expected.get(&key) else {
                continue;
            };
            let signed = lists.join(list_file_name(&repo.url, suite, "InRelease"));
            let plain = lists.join(list_file_name(&repo.url, suite, "Release"));
            let got = match std::fs::read(&signed) {
                Ok(bytes) => {
                    signed_body(&bytes).map(|body| format!("sha256:{}", sha256_hex(&body)))
                }
                Err(_) => std::fs::read(&plain)
                    .ok()
                    .map(|bytes| format!("sha256:{}", sha256_hex(&bytes))),
            };
            if got.as_deref() != Some(want.as_str()) {
                return Err(Diagnostic::new(
                    "E_HASH_MISMATCH",
                    format!(
                        "the dated index {key} at {} that apt downloaded is {}, and pins.lock \
                         records {want}",
                        format_utc(instant),
                        got.as_deref().unwrap_or("missing")
                    ),
                )
                .hint(
                    "nothing was installed; the archive no longer serves what was pinned, or \
                       something between here and it changed it",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 1_788_134_400; // 2026-08-31T00:00:00Z

    #[test]
    fn a_value_is_a_date_or_a_version_and_latest_is_refused() {
        assert_eq!(
            parse_value("x", "2026-08-31T00:59:59Z"),
            Ok(Request::Date(T)),
            "an instant is floored to the hour"
        );
        assert_eq!(parse_value("x", "2026-08-31"), Ok(Request::Date(T)));
        assert_eq!(
            parse_value("x", "1:2.39.5-0+deb12u2"),
            Ok(Request::Version("1:2.39.5-0+deb12u2".into()))
        );
        assert_eq!(
            parse_value("x", "latest").unwrap_err().code,
            "E_UNSUPPORTED"
        );
        assert_eq!(parse_value("x", "").unwrap_err().code, "E_TYPE");
        assert_eq!(parse_value("x", "1.0\n2.0").unwrap_err().code, "E_TYPE");
    }

    #[test]
    fn nearest_versions_are_the_neighbours_in_dpkg_order() {
        let listed: Vec<String> = ["1.0-1", "1.0-3", "2.0-1", "0.9-1"]
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            nearest_versions("1.0-2", &listed),
            ["1.0-1", "1.0-3", "0.9-1"]
        );
        assert!(nearest_versions("1.0-2", &[]).is_empty());
    }

    #[test]
    fn an_apt_list_file_is_named_the_way_apt_names_it() {
        assert_eq!(
            list_file_name(
                &format!(
                    "{}{}",
                    "https:", "//snapshot.debian.org/archive/debian/20260831T000000Z/"
                ),
                "bookworm",
                "InRelease"
            ),
            "snapshot.debian.org_archive_debian_20260831T000000Z_dists_bookworm_InRelease"
        );
    }

    #[test]
    fn the_signed_body_of_an_inrelease_is_its_release() {
        let signed = b"-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA512\n\nOrigin: Debian\n- -dash\n-----BEGIN PGP SIGNATURE-----\nx\n-----END PGP SIGNATURE-----\n";
        assert_eq!(
            signed_body(signed).unwrap(),
            b"Origin: Debian\n-dash\n".to_vec()
        );
        assert!(signed_body(b"Origin: Debian\n").is_none());
    }

    #[test]
    fn launchpad_instants_read_to_the_second() {
        assert_eq!(
            published_at("2026-08-31T00:59:59.761140+00:00"),
            Some(T + 3599)
        );
        assert_eq!(published_at("2026-08-31T00:00:00+00:00"), Some(T));
    }
}
