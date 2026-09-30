//! The repository's one `lodi.lock` (DONE D4; M-Flake F-23, F-24, LD-416): what every host and
//! every home of a repository resolved to, in one file at the repository's root, as `flake.lock`
//! sits beside `flake.nix`.
//!
//! - **Where.** The directory a `SOURCE` names holds it — a single host or a directory of hosts —
//!   and it is never found by searching upward. `/etc/lodi`, a URL's machine record and a
//!   directory holding a project's `lodi.toml` are not repository roots.
//! - **What.** A host section, keyed by the host directory's path from the root (`.` for the root
//!   itself), is 1.4's `pins.lock` without its version and format: distribution, release,
//!   architecture, the file-level snapshot and every pin, plus the verified SHA-256 of each
//!   `signed_by_url` key (Q3). A home section, keyed by the home's directory
//!   (`laptop/home/alice`), is 1.4's `home.lock`: the tools, with the manifest hash, profiles
//!   and the writer they were resolved by.
//! - **Bytes.** Canonical JSON (sorted keys, two spaces, `\n`), its own format and schema
//!   version, unknown fields denied, and nothing of the machine that wrote it.
//! - **Migration.** With no root lock, 1.4's per-folder `pins.lock` and `home.lock` are read
//!   unchanged. Every writer ([`Writer`]) moves every one of them into the root lock unchanged,
//!   replaces the root lock atomically, and only then removes them, reporting each file. An old
//!   file equal to its directory's section is a move cut short, which the next writer finishes by
//!   removing it; one that differs is `E_LOCK_STALE` naming both (#296).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::diag::Diagnostic;
use crate::hostscope::pin::{self, PinRecord, PinsLock, SnapshotRecord};
use crate::hostscope::source::{self, Host};
use crate::lock::{LockFile, Profile, ToolEntry};

/// The file, at the repository's root.
pub const FILE: &str = "lodi.lock";
/// The format this build writes and reads.
pub const FORMAT: &str = "lodi-repository-lock/1";
/// The schema version of [`FORMAT`].
pub const VERSION: u64 = 1;
/// The format of a root lock that holds a Fedora host (fk-1, LD-434), whose pin records carry
/// fields no earlier build reads. A root lock with no Fedora host is written in [`FORMAT`].
pub const FEDORA_FORMAT: &str = "lodi-repository-lock/2";
/// The schema version of [`FEDORA_FORMAT`].
pub const FEDORA_VERSION: u64 = 2;
/// The registry row it is registered as (`src/schema.rs`).
pub const ARTIFACT: &str = "repository-lock";
/// The most bytes a root lock may have before it is refused.
pub const MAX_BYTES: usize = 16 * 1024 * 1024;

/// The root lock.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootLock {
    pub version: u64,
    pub format: String,
    /// Each host directory's section, by its path from the root.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hosts: BTreeMap<String, HostSection>,
    /// Each home directory's section, by its path from the root.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub homes: BTreeMap<String, HomeSection>,
}

/// One host: 1.4's `pins.lock` without its version and format, and the locked key digests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostSection {
    pub distro: String,
    pub release: String,
    pub arch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotRecord>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pins: BTreeMap<String, PinRecord>,
    /// `[sources.NAME] signed_by_url`'s verified SHA-256, `sha256:<hex>`, by source name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<String, String>,
}

/// One home: 1.4's `home.lock` without its version, format and (always absent) base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HomeSection {
    pub generated_by: String,
    pub manifest_hash: String,
    pub tools: BTreeMap<String, ToolEntry>,
    pub profiles: BTreeMap<String, Profile>,
}

impl RootLock {
    pub fn new() -> RootLock {
        RootLock {
            version: VERSION,
            format: FORMAT.to_string(),
            hosts: BTreeMap::new(),
            homes: BTreeMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty() && self.homes.is_empty()
    }
}

impl HostSection {
    pub fn from_pins(lock: PinsLock) -> HostSection {
        HostSection {
            distro: lock.distro,
            release: lock.release,
            arch: lock.arch,
            snapshot: lock.snapshot,
            pins: lock.pins,
            keys: BTreeMap::new(),
        }
    }

    /// The section as the pin lock the host plan reads.
    pub fn to_pins(&self) -> PinsLock {
        PinsLock {
            version: pin::VERSION,
            format: pin::FORMAT.to_string(),
            distro: self.distro.clone(),
            release: self.release.clone(),
            arch: self.arch.clone(),
            snapshot: self.snapshot.clone(),
            pins: self.pins.clone(),
        }
    }

    /// Whether the section records nothing but its machine.
    pub fn is_empty(&self) -> bool {
        self.snapshot.is_none() && self.pins.is_empty() && self.keys.is_empty()
    }
}

impl HomeSection {
    pub fn from_lock(lock: LockFile) -> HomeSection {
        HomeSection {
            generated_by: lock.generated_by,
            manifest_hash: lock.manifest_hash,
            tools: lock.packages,
            profiles: lock.profiles,
        }
    }

    /// The section as the tool lock the home scope reads.
    pub fn to_lock(&self) -> LockFile {
        LockFile {
            version: crate::lock::LOCK_VERSION,
            format: crate::lock::LOCK_FORMAT.to_string(),
            generated_by: self.generated_by.clone(),
            manifest_hash: self.manifest_hash.clone(),
            base: None,
            packages: self.tools.clone(),
            profiles: self.profiles.clone(),
        }
    }
}

/// The schema version and format `lock` is written in: [`FEDORA_FORMAT`] when it holds a Fedora
/// host, else [`FORMAT`].
pub fn format_of(lock: &RootLock) -> (u64, &'static str) {
    if lock.hosts.values().any(|host| host.distro == "fedora") {
        (FEDORA_VERSION, FEDORA_FORMAT)
    } else {
        (VERSION, FORMAT)
    }
}

/// The bytes of `lock`: canonical JSON, a pure function of its argument.
pub fn render(lock: &RootLock) -> String {
    let (version, format) = format_of(lock);
    let mut lock = lock.clone();
    lock.version = version;
    lock.format = format.to_string();
    crate::lock::canonical_json(&lock)
}

fn unreadable(shown: &str, why: String) -> Diagnostic {
    Diagnostic::new(
        "E_LOCK_VERSION",
        format!("{shown} is not a repository lock this build reads: {why}"),
    )
    .hint("use the lodi that wrote it, or move it aside and run `lodi update`")
}

/// Parse `bytes` as the root lock `shown`. A project's `lodi.lock` is refused by name, and so is
/// a later format; the version is decided from the raw JSON before anything is deserialized.
pub fn parse(bytes: &[u8], shown: &str) -> Result<RootLock, Diagnostic> {
    let raw: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| unreadable(shown, e.to_string()))?;
    let format = raw.get("format").and_then(serde_json::Value::as_str);
    if format == Some(crate::lock::LOCK_FORMAT) {
        return Err(unreadable(
            shown,
            format!(
                "it is a project lock ({}, written by `lodi lock` for `lodi develop`), not a \
                 repository lock ({FORMAT})",
                crate::lock::LOCK_FORMAT
            ),
        ));
    }
    let artifact = crate::schema::artifact(ARTIFACT);
    let found = crate::schema::version_of(ARTIFACT, &raw);
    match found {
        Some(version)
            if artifact.reads_version(version)
                && matches!(format, Some(FORMAT | FEDORA_FORMAT)) => {}
        _ => {
            return Err(unreadable(
                shown,
                format!(
                    "it is {} version {}, and this build reads {FORMAT} ({})",
                    format.unwrap_or("of no format"),
                    found.map_or_else(|| "none".to_string(), |v| v.to_string()),
                    crate::schema::note_for(None)
                ),
            ));
        }
    }
    let lock: RootLock =
        serde_json::from_value(raw).map_err(|e| unreadable(shown, e.to_string()))?;
    let (version, format) = format_of(&lock);
    if (lock.version, lock.format.as_str()) != (version, format) {
        return Err(unreadable(
            shown,
            format!(
                "it is {} version {}, and a lock {} a fedora host is {format} version {version}",
                lock.format,
                lock.version,
                if version == FEDORA_VERSION {
                    "with"
                } else {
                    "without"
                }
            ),
        ));
    }
    for (key, home) in &lock.homes {
        crate::lock::validate(&home.to_lock())
            .map_err(|why| unreadable(shown, format!("home {key}: {why}")))?;
    }
    for key in lock.hosts.keys().chain(lock.homes.keys()) {
        if !valid_key(key) {
            return Err(unreadable(
                shown,
                format!("`{key}` is not a directory below it"),
            ));
        }
    }
    Ok(lock)
}

/// A key is `.` or a relative path of safe components.
fn valid_key(key: &str) -> bool {
    key == "." || key.split('/').all(source::safe_component)
}

/// `dir` below `root`, as a key: `.` for the root itself.
fn key_of(root: &Path, dir: &Path) -> Option<String> {
    let rel = dir.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    let key = if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    };
    valid_key(&key).then_some(key)
}

/// `key` joined with `more`, `.` taken as the root.
fn join_key(key: &str, more: &str) -> String {
    if key == "." {
        more.to_string()
    } else {
        format!("{key}/{more}")
    }
}

fn overlap(root: &Path, old: &Path) -> Diagnostic {
    Diagnostic::new(
        "E_LOCK_STALE",
        format!(
            "{} and {} both lock {}: a repository has one lock",
            root.display(),
            old.display(),
            old.parent().unwrap_or(old).display()
        ),
    )
    .hint(format!(
        "keep the one you mean: remove {} if the root lock is current, or remove its section from \
         {} and run `lodi update`",
        old.display(),
        root.display()
    ))
}

/// The repository a host directory was chosen from: the directory its `SOURCE` named, read with
/// the host's own trust walk. `None` for `/etc/lodi` and for a project directory.
pub struct Repository {
    pub dir: PathBuf,
    reader: Host,
}

impl Repository {
    pub fn of(host: &Host) -> Option<Repository> {
        if host.in_place {
            return None;
        }
        let named = host.named.as_ref()?;
        if !host.dir.starts_with(named) || named.join(crate::lock::MANIFEST_FILE).exists() {
            return None;
        }
        Some(Repository {
            dir: named.clone(),
            reader: Host {
                dir: named.clone(),
                anchor: host.anchor.clone(),
                source: host.source.clone(),
                in_place: false,
                owners: host.owners.clone(),
                named: None,
            },
        })
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(FILE)
    }

    pub fn key(&self, dir: &Path) -> Option<String> {
        key_of(&self.dir, dir)
    }

    /// The root lock, read once through the host's trust walk; `None` when there is none.
    pub fn read(&self) -> Result<Option<RootLock>, Diagnostic> {
        let Some(bytes) = source::read_file(&self.reader, FILE, MAX_BYTES)? else {
            return Ok(None);
        };
        if bytes.len() > MAX_BYTES {
            return Err(unreadable(
                &self.path().display().to_string(),
                format!("it exceeds {MAX_BYTES} bytes"),
            ));
        }
        parse(&bytes, &self.path().display().to_string()).map(Some)
    }
}

/// What a host's plan reads of its pins: its section of the root lock, else its own 1.4
/// `pins.lock` (`old`, already read), with the key digests the section locks.
pub struct HostRead {
    pub pins: Option<PinsLock>,
    pub keys: BTreeMap<String, String>,
}

pub fn read_host(host: &Host, old: Option<PinsLock>) -> Result<HostRead, Diagnostic> {
    let plain = |pins| HostRead {
        pins,
        keys: BTreeMap::new(),
    };
    let Some(repo) = Repository::of(host) else {
        return Ok(plain(old));
    };
    let Some(root) = repo.read()? else {
        return Ok(plain(old));
    };
    let Some(section) = repo
        .key(&host.dir)
        .and_then(|key| root.hosts.get(&key).cloned())
    else {
        return Ok(plain(old));
    };
    if old.is_some_and(|old| old != section.to_pins()) {
        return Err(overlap(&repo.path(), &host.dir.join(pin::FILE)));
    }
    Ok(HostRead {
        pins: Some(section.to_pins()),
        keys: section.keys,
    })
}

/// What a home reads of its tools: its section of the root lock, else its own 1.4 `home.lock`
/// (`old`, already read). `host` is the host the home was chosen through; `home` its directory.
pub fn read_home(
    host: &Host,
    home: &Path,
    old: Option<LockFile>,
) -> Result<Option<LockFile>, Diagnostic> {
    let Some(repo) = Repository::of(host) else {
        return Ok(old);
    };
    let Some(key) = repo.key(home).filter(|key| key != ".") else {
        return Ok(old);
    };
    let Some(root) = repo.read()? else {
        return Ok(old);
    };
    let Some(section) = root.homes.get(&key) else {
        return Ok(old);
    };
    if old.is_some_and(|old| old != section.to_lock()) {
        return Err(overlap(
            &repo.path(),
            &home.join(crate::lock::HOME_LOCK_FILE),
        ));
    }
    Ok(Some(section.to_lock()))
}

/// Where a home's tool lock is written when its home lives in a repository: the root and the
/// home's key in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeTarget {
    pub root: PathBuf,
    pub key: String,
}

impl HomeTarget {
    /// The target of the home `home` chosen through `host`, when there is a repository.
    pub fn of(host: &Host, home: &Path) -> Option<HomeTarget> {
        let repo = Repository::of(host)?;
        let key = repo.key(home).filter(|key| key != ".")?;
        Some(HomeTarget {
            root: repo.dir,
            key,
        })
    }

    /// Write this home's section (or remove it), migrating every 1.4 lock of the repository, as
    /// the user the home belongs to; the other sections are carried unchanged.
    pub fn write(&self, lock: Option<&LockFile>) -> Result<Vec<String>, Diagnostic> {
        settle(&self.root, &[(self.key.clone(), lock.cloned())], &[])
    }
}

/// One write of the root lock of the repository at `root`: each home section of `homes` set (or
/// removed), with every 1.4 lock moved in. With no home to set it writes only to remove one of
/// `olds` that a move cut short left beside its section (#296), and otherwise nothing; what a
/// writer would refuse there is left to the next writer to name.
pub fn settle(
    root: &Path,
    homes: &[(String, Option<LockFile>)],
    olds: &[PathBuf],
) -> Result<Vec<String>, Diagnostic> {
    if homes.is_empty() {
        let cut_short = olds.iter().any(|old| old.exists())
            && Writer::open(root).is_ok_and(|writer| {
                writer
                    .old
                    .iter()
                    .any(|old| old.moved && olds.contains(&old.path))
            });
        if !cut_short {
            return Ok(Vec::new());
        }
    }
    let _held = crate::hostscope::pin::verbs::dirlock::HostDirLock::take(root)?;
    let mut writer = Writer::open(root)?;
    for (key, lock) in homes {
        writer.set_home(key, lock.clone().map(HomeSection::from_lock));
    }
    Ok(writer.commit(None, || Ok(()))?.lines())
}

/// One 1.4 lock a writer moves into the root lock.
#[derive(Debug, Clone)]
struct Old {
    path: PathBuf,
    /// Its section was already in the root lock: a move cut short, only to be removed.
    moved: bool,
}

/// A writer of the root lock: the lock as it will be, every 1.4 lock of the repository already
/// moved into it, and what was there before, so that a failed write can put it back.
pub struct Writer {
    pub dir: PathBuf,
    pub lock: RootLock,
    before: Option<Vec<u8>>,
    old: Vec<Old>,
}

/// What a writer did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    pub path: PathBuf,
    pub lock: bool,
    pub manifest: bool,
    pub migrated: Vec<PathBuf>,
}

impl Committed {
    /// One line per migrated file, then the root lock when it changed.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .migrated
            .iter()
            .map(|old| format!("migrated {} into {}", old.display(), self.path.display()))
            .collect();
        if self.lock {
            let verb = if self.path.exists() {
                "wrote"
            } else {
                "removed"
            };
            lines.push(format!("{verb} {}", self.path.display()));
        }
        lines
    }
}

fn io_at(path: &Path) -> impl Fn(io::Error) -> Diagnostic + '_ {
    move |e| Diagnostic::new("E_STORE_IO", format!("{}: {e}", path.display()))
}

/// A regular file's bytes, `None` when absent; a link is refused.
fn read_plain(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, Diagnostic> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_at(path)(e)),
        Ok(meta) if !meta.is_file() => {
            return Err(Diagnostic::new(
                "E_PATH_ESCAPE",
                format!("{} is not a regular file", path.display()),
            ));
        }
        Ok(_) => {}
    }
    let bytes = fs::read(path).map_err(io_at(path))?;
    if bytes.len() > limit {
        return Err(Diagnostic::new(
            "E_LOCK_STALE",
            format!("{} exceeds {limit} bytes", path.display()),
        ));
    }
    Ok(Some(bytes))
}

fn is_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// The host directories of the repository at `root` with their keys: the root itself when it is
/// a host, and each directory beside it that holds a `host.toml`.
fn host_dirs(root: &Path) -> Result<Vec<(String, PathBuf)>, Diagnostic> {
    let mut out = Vec::new();
    if root.join("host.toml").exists() || root.join(pin::FILE).exists() {
        out.push((".".to_string(), root.to_path_buf()));
    }
    let mut entries: Vec<_> = fs::read_dir(root)
        .map_err(io_at(root))?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    for name in entries {
        let dir = root.join(&name);
        if source::safe_component(&name)
            && name != "home"
            && is_dir(&dir)
            && (dir.join("host.toml").exists() || dir.join(pin::FILE).exists())
        {
            out.push((name, dir));
        }
    }
    Ok(out)
}

/// The homes of one host directory with their logins, sorted.
fn homes_below(host: &Path) -> Vec<(String, PathBuf)> {
    let homes = host.join("home");
    if !is_dir(&homes) {
        return Vec::new();
    }
    let mut out: Vec<(String, PathBuf)> = fs::read_dir(&homes)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| source::safe_component(name))
                .map(|name| (name.clone(), homes.join(name)))
                .filter(|(_, dir)| is_dir(dir))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

impl Writer {
    /// Read the root lock of the repository at `dir` and move every 1.4 lock of it in, unchanged;
    /// an old file equal to its directory's section is only removed, and one that differs is
    /// `E_LOCK_STALE` naming both. Nothing is written until [`Writer::commit`].
    pub fn open(dir: &Path) -> Result<Writer, Diagnostic> {
        if dir.join(crate::lock::MANIFEST_FILE).exists() {
            return Err(Diagnostic::new(
                "E_CONFIG",
                format!(
                    "{} holds a project's lodi.toml, so its lodi.lock is the project's, not a \
                     repository lock",
                    dir.display()
                ),
            )
            .hint("keep the hosts of a repository in a directory of their own"));
        }
        let path = dir.join(FILE);
        let before = read_plain(&path, MAX_BYTES)?;
        let mut lock = match &before {
            Some(bytes) => parse(bytes, &path.display().to_string())?,
            None => RootLock::new(),
        };
        let mut old = Vec::new();
        for (key, host) in host_dirs(dir)? {
            let file = host.join(pin::FILE);
            if let Some(bytes) = read_plain(&file, pin::MAX_BYTES)? {
                let shown = file.display().to_string();
                let moved = lock.hosts.get(&key).map(HostSection::to_pins);
                if moved.is_some() && pin::parse_lock(&bytes, &shown).ok() != moved {
                    return Err(overlap(&path, &file));
                }
                if moved.is_none() {
                    let pins = pin::parse_lock(&bytes, &shown)?;
                    lock.hosts.insert(key.clone(), HostSection::from_pins(pins));
                }
                old.push(Old {
                    path: file,
                    moved: moved.is_some(),
                });
            }
            for (login, home) in homes_below(&host) {
                let file = home.join(crate::lock::HOME_LOCK_FILE);
                let Some(bytes) = read_plain(&file, crate::hostscope::MAX_SOURCE_BYTES)? else {
                    continue;
                };
                let home_key = join_key(&key, &format!("home/{login}"));
                let moved = lock.homes.get(&home_key).map(HomeSection::to_lock);
                if moved.is_some() && crate::lock::parse_lock(&bytes).ok() != moved {
                    return Err(overlap(&path, &file));
                }
                if moved.is_none() {
                    let parsed = crate::lock::parse_lock(&bytes)
                        .map_err(|problem| problem.diagnostic(&file.display().to_string()))?;
                    lock.homes.insert(home_key, HomeSection::from_lock(parsed));
                }
                old.push(Old {
                    path: file,
                    moved: moved.is_some(),
                });
            }
        }
        Ok(Writer {
            dir: dir.to_path_buf(),
            lock,
            before,
            old,
        })
    }

    /// The section of the host directory `host`, if the lock has one.
    pub fn host(&self, host: &Path) -> Option<&HostSection> {
        key_of(&self.dir, host).and_then(|key| self.lock.hosts.get(&key))
    }

    /// Set (or, with `None`, remove) the section of the host directory `host`.
    pub fn set_host_dir(
        &mut self,
        host: &Path,
        section: Option<HostSection>,
    ) -> Result<(), Diagnostic> {
        let key = key_of(&self.dir, host).ok_or_else(|| {
            Diagnostic::new(
                "E_PATH_ESCAPE",
                format!("{} is not below {}", host.display(), self.dir.display()),
            )
        })?;
        match section {
            Some(section) => {
                self.lock.hosts.insert(key, section);
            }
            None => {
                self.lock.hosts.remove(&key);
            }
        }
        Ok(())
    }

    pub fn set_home(&mut self, key: &str, section: Option<HomeSection>) {
        match section {
            Some(section) => {
                self.lock.homes.insert(key.to_string(), section);
            }
            None => {
                self.lock.homes.remove(key);
            }
        }
    }

    /// Write the root lock, call `between` (a fault seam; `|| Ok(())` otherwise), then replace
    /// `manifest` with its text when given and different — all or nothing: a failure after the
    /// root lock was replaced puts its old bytes back, and the 1.4 locks are removed only once
    /// both are written.
    pub fn commit(
        self,
        manifest: Option<(&Path, &str)>,
        between: impl FnOnce() -> io::Result<()>,
    ) -> Result<Committed, Diagnostic> {
        let path = self.dir.join(FILE);
        let bytes = (!self.lock.is_empty()).then(|| render(&self.lock).into_bytes());
        let lock_changed = bytes != self.before;
        let restore = |before: &Option<Vec<u8>>| match before {
            Some(old) => {
                let _ = replace(&path, old, 0o644);
            }
            None => {
                let _ = fs::remove_file(&path);
            }
        };
        if lock_changed {
            match &bytes {
                Some(bytes) => replace(&path, bytes, 0o644).map_err(io_at(&path))?,
                None => fs::remove_file(&path).map_err(io_at(&path))?,
            }
        }
        if let Err(e) = between() {
            if lock_changed {
                restore(&self.before);
            }
            return Err(io_at(&path)(e));
        }
        let mut manifest_changed = false;
        if let Some((file, text)) = manifest {
            let current = fs::read(file).map_err(io_at(file));
            let differs = match current {
                Ok(current) => current != text.as_bytes(),
                Err(d) => {
                    if lock_changed {
                        restore(&self.before);
                    }
                    return Err(d);
                }
            };
            if differs {
                if let Err(e) = replace(file, text.as_bytes(), 0o644) {
                    if lock_changed {
                        restore(&self.before);
                    }
                    return Err(io_at(file)(e));
                }
                manifest_changed = true;
            }
        }
        let mut migrated = Vec::new();
        for old in self.old {
            fs::remove_file(&old.path).map_err(io_at(&old.path))?;
            migrated.push(old.path);
        }
        Ok(Committed {
            path,
            lock: lock_changed,
            manifest: manifest_changed,
            migrated,
        })
    }
}

/// Replace `path` by `bytes` through a synced temporary file and a rename, keeping the mode,
/// owner and group of what it replaces (`default_mode` for a new file); a link is refused.
fn replace(path: &Path, bytes: &[u8], default_mode: u32) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let old = match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(io::Error::other(
                "is a symbolic link; refusing to replace it",
            ));
        }
        Ok(m) => Some(m),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    crate::lock::write_atomic_with(path, bytes, |tmp| {
        let mode = old.as_ref().map_or(default_mode, |m| m.mode() & 0o7777);
        fs::set_permissions(tmp, fs::Permissions::from_mode(mode))?;
        // A new file belongs to the repository's owner, as the file it replaces did: root
        // writes no lock of its own into a user's repository (F-25).
        let owner = match &old {
            Some(m) => Some((m.uid(), m.gid())),
            // SAFETY: geteuid cannot fail and takes no arguments.
            None if unsafe { libc::geteuid() } == 0 => path
                .parent()
                .and_then(|dir| fs::metadata(dir).ok())
                .map(|m| (m.uid(), m.gid())),
            None => None,
        };
        if let Some((uid, gid)) = owner {
            let here = fs::metadata(tmp)?;
            if (here.uid(), here.gid()) != (uid, gid) {
                std::os::unix::fs::lchown(tmp, Some(uid), Some(gid))?;
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_paths_below_the_root() {
        let root = Path::new("/r");
        assert_eq!(key_of(root, Path::new("/r")).as_deref(), Some("."));
        assert_eq!(key_of(root, Path::new("/r/box")).as_deref(), Some("box"));
        assert_eq!(
            key_of(root, Path::new("/r/box/home/al")).as_deref(),
            Some("box/home/al")
        );
        assert_eq!(key_of(root, Path::new("/elsewhere")), None);
        assert_eq!(join_key(".", "home/al"), "home/al");
        assert!(!valid_key("../x") && !valid_key("") && valid_key("."));
    }

    #[test]
    fn a_project_lock_is_refused_by_name() {
        let project = format!(
            "{{\"version\":1,\"format\":\"{}\"}}",
            crate::lock::LOCK_FORMAT
        );
        let refused = parse(project.as_bytes(), "x/lodi.lock").unwrap_err();
        assert_eq!(refused.code, "E_LOCK_VERSION");
        assert!(refused.message.contains("project lock"));
        let empty = render(&RootLock::new());
        assert_eq!(parse(empty.as_bytes(), "x").unwrap(), RootLock::new());
        let unknown = empty.replace("\"format\"", "\"extra\": 1,\n  \"format\"");
        assert!(parse(unknown.as_bytes(), "x").is_err());
    }
}
