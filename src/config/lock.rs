//! The config's one `lodi.lock` (#705, LD-503, LD-491): at the config root, as `flake.lock`
//! sits beside `flake.nix`, with a host section per `host.toml` and a home section per
//! `home.toml`, each keyed by the manifest's path from the root (`super::Part::key`). A home two
//! hosts share has one section. 2.0 reads no 1.x lock (#688): a file it cannot parse stops with
//! `E_CONFIG` naming it, and a missing file is an empty lock.
//!
//! Two ways to a new lock, both in memory first, so a dry run previews without writing and every
//! lookup happens before any write: [`fill`] adds what is missing and never moves an entry
//! (import, plain switch), and [`refresh`] moves the snapshot and looks the rest up again while a
//! `[packages.pin]` entry stays (`update`, `switch --update`). What is looked up, and from where,
//! is the caller's [`Lookup`]. [`Held::write`] replaces the file atomically, by the user's own
//! process, the file owned by the config folder's owner, under the folder's advisory lock.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::diag::Diagnostic;
use crate::hostscope::pin::{PinRecord, SnapshotRecord};
use crate::lock::{Profile, ToolEntry};

/// The file, at the config root.
pub const FILE: &str = "lodi.lock";
/// The format this build writes and reads; 2.0's schema versions start over (#688).
pub const FORMAT: &str = "lodi-config-lock";
pub const VERSION: u64 = 1;
/// The most bytes a lock may have before it is refused.
const MAX_BYTES: u64 = 16 * 1024 * 1024;

/// The lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Lock {
    pub version: u64,
    pub format: String,
    /// Each host's section, by its `host.toml`'s path from the root.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hosts: BTreeMap<String, HostSection>,
    /// Each home's section, by its `home.toml`'s path from the root.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub homes: BTreeMap<String, HomeSection>,
}

impl Default for Lock {
    fn default() -> Lock {
        Lock {
            version: VERSION,
            format: FORMAT.to_string(),
            hosts: BTreeMap::new(),
            homes: BTreeMap::new(),
        }
    }
}

/// One host: its distribution, the file-level snapshot, each `[packages.pin]` resolution and
/// each `signed_by_url` key's verified `sha256:` digest by source name.
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<String, String>,
}

/// One home: its tools and profiles.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HomeSection {
    pub tools: BTreeMap<String, ToolEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, Profile>,
}

impl Lock {
    /// The canonical bytes: sorted keys, two spaces, a final newline.
    pub fn render(&self) -> String {
        crate::lock::canonical_json(self)
    }
}

/// Which of the two a [`Lookup`] serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Resolve against what is locked: the current snapshot, the current entries.
    Fill,
    /// Resolve afresh: the newest published day, every key fetched and checked again.
    Refresh,
}

/// The caller's lookups: one section as its manifest asks for it now. `current` is what the lock
/// holds, so a lookup can skip what it need not fetch; the rules of what stays are this module's.
pub trait Lookup {
    fn host(
        &mut self,
        key: &str,
        current: Option<&HostSection>,
        mode: Mode,
    ) -> Result<HostSection, Diagnostic>;
    fn home(
        &mut self,
        key: &str,
        current: Option<&HomeSection>,
        mode: Mode,
    ) -> Result<HomeSection, Diagnostic>;
}

/// `lock` with every host and home of `hosts` and `homes` (keys) given an entry for whatever it
/// has none for. An existing entry whose request is unchanged stays exactly as it was; one the
/// manifest no longer asks for goes.
pub fn fill(
    lock: &Lock,
    hosts: &[&str],
    homes: &[&str],
    lookup: &mut dyn Lookup,
) -> Result<Lock, Diagnostic> {
    each(lock, hosts, homes, lookup, Mode::Fill)
}

/// `lock` with every host and home of `hosts` and `homes` looked up again: the snapshot moves
/// (a host without one gets none), keys are recorded afresh, a `[packages.pin]` entry whose
/// request is unchanged stays, and a home keeps what its lookup kept. Other sections stay.
pub fn refresh(
    lock: &Lock,
    hosts: &[&str],
    homes: &[&str],
    lookup: &mut dyn Lookup,
) -> Result<Lock, Diagnostic> {
    each(lock, hosts, homes, lookup, Mode::Refresh)
}

fn each(
    lock: &Lock,
    hosts: &[&str],
    homes: &[&str],
    lookup: &mut dyn Lookup,
    mode: Mode,
) -> Result<Lock, Diagnostic> {
    let mut out = lock.clone();
    let mut done = Vec::new();
    for key in hosts {
        if done.contains(key) {
            continue;
        }
        done.push(*key);
        let current = lock.hosts.get(*key);
        let fresh = lookup.host(key, current, mode)?;
        let merged = match current {
            Some(current) => host(current, fresh, mode),
            None => fresh,
        };
        out.hosts.insert(key.to_string(), merged);
    }
    done.clear();
    for key in homes {
        if done.contains(key) {
            continue;
        }
        done.push(*key);
        let current = lock.homes.get(*key);
        let fresh = lookup.home(key, current, mode)?;
        let merged = match (current, mode) {
            (Some(current), Mode::Fill) => home(current, fresh),
            _ => fresh,
        };
        out.homes.insert(key.to_string(), merged);
    }
    Ok(out)
}

/// One host's merge: `fresh` decides which entries exist, `current` which of them stay.
fn host(current: &HostSection, mut fresh: HostSection, mode: Mode) -> HostSection {
    if mode == Mode::Fill
        && let Some(snapshot) = &current.snapshot
        && fresh.snapshot.as_ref().map(|s| &s.requested) == Some(&snapshot.requested)
    {
        fresh.snapshot = Some(snapshot.clone());
    }
    for (name, pin) in fresh.pins.iter_mut() {
        if let Some(kept) = current.pins.get(name)
            && kept.requested == pin.requested
        {
            *pin = kept.clone();
        }
    }
    if mode == Mode::Fill {
        for (name, key) in fresh.keys.iter_mut() {
            if let Some(kept) = current.keys.get(name) {
                key.clone_from(kept);
            }
        }
    }
    fresh
}

/// One home's fill: a tool whose request is unchanged keeps its entry.
fn home(current: &HomeSection, mut fresh: HomeSection) -> HomeSection {
    for (name, tool) in fresh.tools.iter_mut() {
        if let Some(kept) = current.tools.get(name)
            && kept.request == tool.request
        {
            *tool = kept.clone();
        }
    }
    for (name, profile) in fresh.profiles.iter_mut() {
        if let Some(kept) = current.profiles.get(name) {
            *profile = kept.clone();
        }
    }
    fresh
}

/// The lock of the config at `root`; a missing file is an empty lock.
pub fn read(root: &Path) -> Result<Lock, Diagnostic> {
    read_file(&root.join(FILE))
}

/// The lock at `path`, as [`read`] reads a config's.
pub fn read_file(path: &Path) -> Result<Lock, Diagnostic> {
    let path = path.to_path_buf();
    let unusable = |why: String| {
        Diagnostic::new(
            "E_CONFIG",
            format!("{} cannot be used: {why}", path.display()),
        )
        .hint(format!(
            "lodi 2.0 reads only its own lock: remove or fix {}, then run lodi switch or \
             lodi update to write it again",
            path.display()
        ))
    };
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Lock::default()),
        Err(e) => return Err(unusable(e.to_string())),
    };
    if !meta.is_file() {
        return Err(unusable("it is not a regular file".into()));
    }
    if meta.len() > MAX_BYTES {
        return Err(unusable(format!("it is over {MAX_BYTES} bytes")));
    }
    let bytes = fs::read(&path).map_err(|e| unusable(e.to_string()))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| unusable(format!("it is not JSON ({e})")))?;
    let format = value.get("format").and_then(serde_json::Value::as_str);
    let version = value.get("version").and_then(serde_json::Value::as_u64);
    if format != Some(FORMAT) || version != Some(VERSION) {
        return Err(unusable(format!(
            "it is not a {FORMAT} version {VERSION} lock (a 1.x lock, or a project's)"
        )));
    }
    serde_json::from_value(value).map_err(|e| unusable(e.to_string()))
}

/// The config folder's advisory lock, held by the one writer.
#[derive(Debug)]
pub struct Held {
    root: PathBuf,
    _lock: crate::hostscope::pin::verbs::dirlock::HostDirLock,
}

/// Take the advisory lock on the config folder `root` without waiting: a second writer stops
/// with `E_SYSTEM_BUSY`.
pub fn hold(root: &Path) -> Result<Held, Diagnostic> {
    let lock =
        crate::hostscope::pin::verbs::dirlock::HostDirLock::take(root).map_err(|mut d| {
            if d.code == "E_SYSTEM_BUSY" {
                d.message = format!("another lodi is changing {}", root.display());
            }
            d
        })?;
    Ok(Held {
        root: root.to_path_buf(),
        _lock: lock,
    })
}

/// [`hold`], waiting for another lodi that holds it instead of stopping: the line `waiting for`
/// once, then a blocking lock that is let go and [`hold`] taken again, until it is.
pub fn wait(root: &Path) -> Result<Held, Diagnostic> {
    let mut said = false;
    loop {
        match hold(root) {
            Err(d) if d.code == "E_SYSTEM_BUSY" => {}
            other => return other,
        }
        if !said {
            eprintln!(
                "lodi: waiting for another lodi to finish with {}",
                root.display()
            );
            said = true;
        }
        let dir = fs::File::open(root)
            .map_err(|e| Diagnostic::new("E_STORE_IO", format!("{}: {e}", root.display())))?;
        // SAFETY: flock on a descriptor owned here; closing it lets the lock go again.
        unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&dir), libc::LOCK_EX) };
    }
}

impl Held {
    /// [`read`], under the lock.
    pub fn read(&self) -> Result<Lock, Diagnostic> {
        read(&self.root)
    }

    /// Replace the lock by `lock` through a synced temporary file and a rename: `Ok(false)` when
    /// the file already holds exactly these bytes. Mode 0644 for a new file. Under `sudo` or
    /// `doas` (`user.sudo`) the file is given to the config folder's owner before the rename.
    pub fn write(&self, lock: &Lock, user: &super::User) -> Result<bool, Diagnostic> {
        let path = self.root.join(FILE);
        let failed = |e: io::Error| {
            Diagnostic::new(
                "E_STORE_IO",
                format!("cannot write {}: {e}", path.display()),
            )
        };
        let bytes = lock.render();
        let old = match fs::symlink_metadata(&path) {
            Ok(meta) if !meta.is_file() => {
                return Err(failed(io::Error::other(
                    "it is not a regular file; nothing was written",
                )));
            }
            Ok(meta) => Some(meta),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(failed(e)),
        };
        if old.is_some() && fs::read(&path).is_ok_and(|now| now == bytes.as_bytes()) {
            return Ok(false);
        }
        let folder = fs::metadata(&self.root).map_err(failed)?;
        crate::lock::write_atomic_with(&path, bytes.as_bytes(), |tmp| {
            let mode = old.as_ref().map_or(0o644, |m| m.mode() & 0o7777);
            fs::set_permissions(tmp, fs::Permissions::from_mode(mode))?;
            if user.sudo {
                std::os::unix::fs::lchown(tmp, Some(folder.uid()), Some(folder.gid()))?;
            }
            Ok(())
        })
        .map_err(failed)?;
        Ok(true)
    }
}
