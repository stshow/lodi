//! The per-user store (M-Spike S-3; design `spec/03`, ADR-005 as amended by ADR-016).
//!
//! ```text
//! $LODI_HOME/                     default $XDG_DATA_HOME/lodi, else ~/.local/share/lodi
//!   .layout.json                  the store's layout version and the lodi that wrote it; an
//!                                 absent marker is layout 0 ([`crate::layout`], M-1.0 T-2)
//!   store/.lock                   flock: shared while entries are realized or rooted (a future
//!                                 collector takes it exclusively; none exists, LD-19)
//!   store/.locks/<entry>.lock     per-entry flock: one realization of a key at a time
//!   store/.meta/<entry>.json      sidecar; `complete: true` is written last
//!   store/tmp/<pid>-<n>-<entry>/  staging on the same file system (atomic rename)
//!   store/art-<h32>-<name>-<ver>/ an extracted tool artifact, read-only
//!   store/env-<h32>/              a host-tool environment (env.json, tree/bin), read-only
//!   cache/dl/<sha256>             verified downloads, named by their full SHA-256
//!   cache/shell/<h32>.lock        ad-hoc `lodi shell` resolutions, named by the request
//!   cache/api/<h32>.json          kept GitHub API answers and their ETags, named by the request
//!                                 (0700 directory, 0600 entries; [`crate::fetch::api_cache`], LD-404)
//!   gcroots/sessions/<pid>        live-session roots (ADR-016)
//!   logs/build-<img>.log          the last container image build of that image (S-4)
//! ```
//!
//! Container images live in Podman's storage, not here; `store/.meta/img-<h32>.json` records
//! each image Lodi built and verified ([`crate::container`]).
//!
//! An entry's *identity* is the hash that names it (an artifact's file SHA-256, an
//! environment's envhash); its *content hash* is the NAR hash of the published tree
//! ([`crate::nar`]), recorded as `treeHash` in the sidecar. An entry is complete only when its
//! directory exists **and** its sidecar says `complete: true`; anything else is the residue of
//! an interrupted realization and is replaced, never used.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::archive::{self, Limits};
use crate::diag::Diagnostic;
use crate::fetch::{FetchError, Fetcher};
use crate::lock::{ToolEntry, canonical_json};
use crate::util::{format_utc, now_utc, untag_sha256};

/// Largest artifact download when the lock records no size.
pub const MAX_ARTIFACT_BYTES: u64 = 2 << 30;
/// The modification time given to every published file (`spec/03` §5 step 4).
pub const NORMALIZED_MTIME: i64 = 1;

/// Points inside a realization where tests inject a failure or an abrupt stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    /// The verified download has been written to the cache.
    Downloaded,
    /// The tree is complete and hashed in staging, not yet renamed.
    Staged,
    /// The tree has been renamed into place; the sidecar is not yet written.
    Renamed,
}

/// A hook called at each [`Point`]; an error aborts the realization there.
pub type Hook<'a> = &'a dyn Fn(Point, &str) -> io::Result<()>;

fn no_hook(_: Point, _: &str) -> io::Result<()> {
    Ok(())
}

/// The sidecar of a store entry (`spec/03` §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Meta {
    /// The sidecar's schema version. It is `#[serde(default)]` because releases through 0.3.0
    /// wrote this record without it, in exactly this shape: a sidecar with no `version` **is**
    /// version 1 (M-1.0 T-1, design call D5).
    #[serde(default = "sidecar_version")]
    pub version: u64,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    /// The hash that names the entry.
    pub identity: String,
    /// The NAR hash of the published tree.
    pub tree_hash: String,
    pub created: String,
    pub references: Vec<String>,
    pub complete: bool,
    pub lodi_version: String,
}

/// The registry row a store entry's sidecar is registered as.
pub const SIDECAR_ARTIFACT: &str = "store-sidecar";
/// The registry row a live session root is registered as.
pub const SESSION_ROOT_ARTIFACT: &str = "session-root";

fn sidecar_version() -> u64 {
    crate::schema::artifact(SIDECAR_ARTIFACT).writes
}

/// What one realization did, for reporting and for tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Entries published by this call.
    pub created: Vec<String>,
    /// Bytes downloaded by this call.
    pub downloaded: u64,
    /// Cached downloads that failed verification and were discarded.
    pub cache_discarded: Vec<String>,
    /// Incomplete entries (a directory without a complete sidecar) that were replaced.
    pub incomplete_replaced: Vec<String>,
}

/// What a publication names: the entry, its type, the hash that names it, and the entries it
/// refers to (the collector's edges, `spec/03` §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: &'static str,
    pub identity: String,
    pub references: Vec<String>,
}

/// An exclusive or shared `flock` held until drop.
pub struct FileLock {
    file: fs::File,
}

impl FileLock {
    /// Take the lock at `path`, waiting for it, as a [`Diagnostic`] rather than an
    /// [`io::Error`]. The home scope's `<data>/home-scope/.lock` is taken through this, so there
    /// is **one** `flock` implementation in the tree rather than a second copy beside it
    /// (M-0.5 T-3, `LD-146`). The file is created if it is not there; it is never read or
    /// written, only locked, and the lock is released when the value is dropped.
    pub fn at(path: &Path, exclusive: bool) -> Result<FileLock, Diagnostic> {
        FileLock::take(path, exclusive).map_err(|e| store_io(path.display(), e))
    }

    /// [`FileLock::at`] without waiting: `Ok(None)` means another process holds it, which is
    /// what lets a command say `waiting for the home lock` once and then block.
    pub fn try_at(path: &Path, exclusive: bool) -> Result<Option<FileLock>, Diagnostic> {
        FileLock::try_take(path, exclusive).map_err(|e| store_io(path.display(), e))
    }

    fn take(path: &Path, exclusive: bool) -> io::Result<FileLock> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let op = if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        };
        FileLock::flock(file, op)
    }

    /// Take the lock only if it is free. `Ok(None)` means another process holds it.
    fn try_take(path: &Path, exclusive: bool) -> io::Result<Option<FileLock>> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let op = if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        } | libc::LOCK_NB;
        match FileLock::flock(file, op) {
            Ok(lock) => Ok(Some(lock)),
            Err(e)
                if e.raw_os_error() == Some(libc::EWOULDBLOCK)
                    || e.kind() == io::ErrorKind::WouldBlock =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    fn flock(file: fs::File, op: libc::c_int) -> io::Result<FileLock> {
        loop {
            // SAFETY: flock on a file descriptor this struct owns.
            if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
                return Ok(FileLock { file });
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: unlocking the descriptor this struct owns; closing would unlock as well.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn store_io(what: impl std::fmt::Display, e: io::Error) -> Diagnostic {
    Diagnostic::new("E_STORE_IO", format!("{what}: {e}"))
}

/// Whether process `pid` exists (`kill(pid, 0)`; `EPERM` means it exists).
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks for existence and permission.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Make a published (read-only) tree writable again and remove it.
pub fn remove_tree(path: &Path) -> io::Result<()> {
    fn unlock(path: &Path) -> io::Result<()> {
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_dir() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            for entry in fs::read_dir(path)? {
                unlock(&entry?.path())?;
            }
        }
        Ok(())
    }
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
        Ok(m) if !m.file_type().is_dir() => return fs::remove_file(path),
        Ok(_) => {}
    }
    unlock(path)?;
    fs::remove_dir_all(path)
}

fn set_mtime(path: &Path) -> io::Result<()> {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("path contains NUL"))?;
    let t = libc::timespec {
        tv_sec: NORMALIZED_MTIME as libc::time_t,
        tv_nsec: 0,
    };
    let times = [t, t];
    // SAFETY: a valid C string and two timespecs; AT_SYMLINK_NOFOLLOW sets a link's own time.
    let rc = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Create the one directory `path` at mode 0700, set explicitly after the creation so the umask
/// plays no part; its parent must exist. An existing `path` is `AlreadyExists` and keeps its mode.
fn private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

/// Read-only modes and normalized mtimes, bottom-up (`spec/03` §5 step 4). The top directory
/// keeps its write bit: `rename(2)` needs it to move a directory, so [`Store::publish`] makes it
/// read-only after the rename. Durability (step 5) is [`sync_file_system`], once per tree.
fn seal(path: &Path, top: bool) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let kind = meta.file_type();
    if kind.is_dir() {
        for entry in fs::read_dir(path)? {
            seal(&entry?.path(), false)?;
        }
        set_mtime(path)?;
        if !top {
            fs::set_permissions(path, fs::Permissions::from_mode(0o555))?;
        }
    } else if kind.is_file() {
        let exec = meta.permissions().mode() & 0o100 != 0;
        set_mtime(path)?;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if exec { 0o555 } else { 0o444 }),
        )?;
    } else if kind.is_symlink() {
        set_mtime(path)?;
    } else {
        return Err(io::Error::other(format!(
            "{} is not a file, directory or symlink",
            path.display()
        )));
    }
    Ok(())
}

/// `spec/03` §5 step 5: make every byte written below `path` durable before it is published.
/// One `syncfs(2)` on the store's file system replaces an `fsync` per file (measured on the
/// spike host: about 3 ms per file, over 30 s for the 10,000 files of Python and Node; LD-19).
fn sync_file_system(path: &Path) -> io::Result<()> {
    let dir = fs::File::open(path)?;
    // SAFETY: syncfs on a descriptor owned by `dir`.
    if unsafe { libc::syncfs(dir.as_raw_fd()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn sync_dir(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

/// A lower-case file-name fragment: `[a-z0-9._+-]`, at most 64 characters (`spec/03` §1).
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._+-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect()
}

/// The identity of a locked tool's artifacts: the one artifact's tagged SHA-256, or, for
/// several, `sha256:` of their tagged hashes in recipe order, one per line. A tool with one
/// artifact keeps exactly the identity earlier builds recorded.
pub fn artifacts_identity(tool: &ToolEntry) -> Result<String, Diagnostic> {
    if tool
        .artifacts
        .iter()
        .any(|artifact| !artifact.exclude.is_empty())
    {
        let mut bytes = b"lodi-artifact-exclusions-v1\0".to_vec();
        for artifact in &tool.artifacts {
            bytes.extend_from_slice(&(artifact.sha256.len() as u64).to_be_bytes());
            bytes.extend_from_slice(artifact.sha256.as_bytes());
            bytes.extend_from_slice(&(artifact.exclude.len() as u64).to_be_bytes());
            for path in &artifact.exclude {
                bytes.extend_from_slice(&(path.len() as u64).to_be_bytes());
                bytes.extend_from_slice(path.as_bytes());
            }
        }
        return Ok(format!("sha256:{}", crate::util::sha256_hex(&bytes)));
    }
    match tool.artifacts.as_slice() {
        [] => Err(Diagnostic::new(
            "E_RECIPE_CTX",
            format!("tool `{}` has no artifacts", tool.request.name),
        )),
        [one] => Ok(one.sha256.clone()),
        many => {
            let mut text = String::new();
            for artifact in many {
                text.push_str(&artifact.sha256);
                text.push('\n');
            }
            Ok(format!(
                "sha256:{}",
                crate::util::sha256_hex(text.as_bytes())
            ))
        }
    }
}

/// The store entry name of a locked tool: `art-<first 32 hex of the artifact SHA-256>-<recipe
/// name>-<version>`. Several artifacts hash their tagged hashes in order, so the name is stable
/// across runs and changes when any artifact does (M-0.4 T-2).
pub fn art_name(tool: &ToolEntry) -> Result<String, Diagnostic> {
    let identity = artifacts_identity(tool)?;
    let hex = untag_sha256(&identity).ok_or_else(|| {
        Diagnostic::new(
            "E_HASH_MISMATCH",
            format!("tool `{}` has a malformed artifact hash", tool.request.name),
        )
    })?;
    Ok(format!(
        "art-{}-{}-{}",
        &hex[..32],
        sanitize(&tool.request.name),
        sanitize(&tool.version)
    ))
}

static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The per-user store rooted at `$LODI_HOME`.
#[derive(Debug, Clone)]
pub struct Store {
    home: PathBuf,
}

impl Store {
    /// `LODI_HOME`, else `$XDG_DATA_HOME/lodi`, else `$HOME/.local/share/lodi`. The variables
    /// are the ones `crate::roots::capture` read; this precedence is its `store_root`
    /// projection, unchanged since S-3 (`LD-184`).
    pub fn home_from_env() -> Result<PathBuf, Diagnostic> {
        crate::roots::capture().store_root()
    }

    /// Open (creating its directories) the store at `home`, which must be absolute.
    pub fn open(home: &Path) -> Result<Store, Diagnostic> {
        if !home.is_absolute() {
            return Err(Diagnostic::new(
                "E_STORE_PERM",
                format!("the store location `{}` is not absolute", home.display()),
            ));
        }
        for dir in [
            "store/.locks",
            "store/.meta",
            "cache/dl",
            "cache/shell",
            "gcroots/sessions",
            "logs",
        ] {
            let path = home.join(dir);
            fs::create_dir_all(&path).map_err(|e| {
                Diagnostic::new(
                    "E_STORE_PERM",
                    format!("cannot create {}: {e}", path.display()),
                )
            })?;
        }
        // Staging is where a tree is written before it is sealed: its parent is created owner-only
        // at an explicit mode, never the umask's (LD-361). An existing one — made by another
        // process opening the same store at the same moment included — is left as it is.
        let tmp = home.join("store/tmp");
        match private_dir(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && tmp.is_dir() => {}
            Err(e) => {
                return Err(Diagnostic::new(
                    "E_STORE_PERM",
                    format!("cannot create {}: {e}", tmp.display()),
                ));
            }
        }
        Ok(Store {
            home: home.to_path_buf(),
        })
    }

    /// [`Store::open`] for a command that will **write** into the store: a store without the
    /// 2.0 marker is given it once, before any entry is realized, and is never migrated.
    ///
    /// This is the only place [`crate::layout::start`] is called from. The layout is read
    /// **before** [`Store::open`] creates a directory, so a store written by a newer Lodi is
    /// `E_STORE_VERSION` without this build having opened or written anything below it (design
    /// call D8). A read-only command uses [`Store::open`] and never writes the marker.
    pub fn open_for_write(home: &Path) -> Result<Store, Diagnostic> {
        crate::layout::read(home)?;
        let store = Store::open(home)?;
        crate::layout::start(&store)?;
        Ok(store)
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn entry_path(&self, name: &str) -> PathBuf {
        self.home.join("store").join(name)
    }

    /// The sidecar path of entry `name` (`store/.meta/<name>.json`).
    pub fn meta_path(&self, name: &str) -> PathBuf {
        self.home.join("store/.meta").join(format!("{name}.json"))
    }

    pub fn download_path(&self, sha256_hex: &str) -> PathBuf {
        self.home.join("cache/dl").join(sha256_hex)
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.home.join("gcroots/sessions")
    }

    /// The shared store lock (`spec/03` §5 step 1), held while entries are realized and rooted.
    pub fn shared_lock(&self) -> Result<FileLock, Diagnostic> {
        let path = self.home.join("store/.lock");
        FileLock::take(&path, false).map_err(|e| store_io(path.display(), e))
    }

    /// The **exclusive** store lock (`spec/03` §7 step 1), held by the collector while it
    /// sweeps: it waits for every realization and every entry in progress to finish.
    pub fn exclusive_lock(&self) -> Result<FileLock, Diagnostic> {
        let path = self.home.join("store/.lock");
        FileLock::take(&path, true).map_err(|e| store_io(path.display(), e))
    }

    /// The exclusive store lock if it is free right now, so the collector can say that it is
    /// waiting before it blocks. `Ok(None)` means someone else holds it.
    pub fn try_exclusive_lock(&self) -> Result<Option<FileLock>, Diagnostic> {
        let path = self.home.join("store/.lock");
        FileLock::try_take(&path, true).map_err(|e| store_io(path.display(), e))
    }

    /// The exclusive per-entry lock `store/.locks/<name>.lock`.
    pub fn key_lock(&self, name: &str) -> Result<FileLock, Diagnostic> {
        let path = self.home.join("store/.locks").join(format!("{name}.lock"));
        FileLock::take(&path, true).map_err(|e| store_io(path.display(), e))
    }

    /// The sidecar of `name`, if the entry is complete: its directory exists and the sidecar
    /// parses, names it and says `complete: true`.
    pub fn complete(&self, name: &str) -> Option<Meta> {
        // A sidecar whose schema version this build does not read is not a usable sidecar, so
        // the entry is incomplete and is realized again rather than trusted (M-1.0 T-1, D18).
        let meta: Meta = crate::schema::parse_registered(
            SIDECAR_ARTIFACT,
            &fs::read(self.meta_path(name)).ok()?,
        )?;
        let dir = fs::symlink_metadata(self.entry_path(name)).ok()?;
        (meta.complete && meta.name == name && dir.file_type().is_dir()).then_some(meta)
    }

    /// Remove staging directories and partial downloads left by processes that no longer
    /// exist (`store/tmp/<pid>-…`, `cache/dl/.<pid>-….part`).
    pub fn prune_staging(&self) {
        for (dir, prefix) in [("store/tmp", ""), ("cache/dl", ".")] {
            let Ok(entries) = fs::read_dir(self.home.join(dir)) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(rest) = name.strip_prefix(prefix) else {
                    continue;
                };
                if prefix == "." && !rest.ends_with(".part") {
                    continue;
                }
                let pid = rest.split('-').next().and_then(|p| p.parse::<i32>().ok());
                if pid.is_some_and(|p| !pid_alive(p)) {
                    let _ = remove_tree(&entry.path());
                }
            }
        }
    }

    fn staging(&self, name: &str) -> PathBuf {
        let n = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
        self.home
            .join("store/tmp")
            .join(format!("{}-{n}-{name}", std::process::id()))
    }

    /// Publish entry `name` (`spec/03` §5): under the entry's lock, return the complete entry if
    /// there is one; otherwise discard any incomplete residue, let `build` fill a fresh staging
    /// directory, seal and hash it, rename it into place and write the sidecar last.
    pub fn publish(
        &self,
        entry: Entry,
        report: &mut Report,
        hook: Hook,
        build: impl FnOnce(&Path, &mut Report, Hook) -> Result<(), Diagnostic>,
    ) -> Result<Meta, Diagnostic> {
        let name = entry.name.as_str();
        let _key = self.key_lock(name)?;
        if let Some(meta) = self.complete(name) {
            return Ok(meta);
        }
        let target = self.entry_path(name);
        if fs::symlink_metadata(&target).is_ok() || self.meta_path(name).exists() {
            let _ = fs::remove_file(self.meta_path(name));
            remove_tree(&target).map_err(|e| store_io(target.display(), e))?;
            report.incomplete_replaced.push(name.to_string());
        }
        let staging = self.staging(name);
        let result = (|| {
            // The staging directory exists, owner-only at an explicit mode, before `build` writes
            // a byte into it; a residue of that name from a process that reused this pid is
            // discarded first rather than built into (LD-361).
            if fs::symlink_metadata(&staging).is_ok() {
                remove_tree(&staging).map_err(|e| store_io(staging.display(), e))?;
            }
            private_dir(&staging).map_err(|e| store_io(staging.display(), e))?;
            build(&staging, report, hook)?;
            let tree_hash = crate::nar::hash(&staging).map_err(|e| match e {
                crate::nar::NarError::Unsupported(p) => Diagnostic::new(
                    "E_TREE_UNSUPPORTED",
                    format!("{} is a special file", p.display()),
                ),
                other => Diagnostic::new("E_STORE_IO", other.to_string()),
            })?;
            seal(&staging, true).map_err(|e| store_io(staging.display(), e))?;
            sync_file_system(&staging).map_err(|e| store_io(staging.display(), e))?;
            hook(Point::Staged, name).map_err(|e| store_io("interrupted", e))?;
            fs::rename(&staging, &target).map_err(|e| store_io(target.display(), e))?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o555))
                .and_then(|()| set_mtime(&target))
                .map_err(|e| store_io(target.display(), e))?;
            sync_dir(&self.home.join("store")).map_err(|e| store_io("store", e))?;
            hook(Point::Renamed, name).map_err(|e| store_io("interrupted", e))?;
            let meta = Meta {
                version: sidecar_version(),
                name: name.to_string(),
                kind: entry.kind.to_string(),
                identity: entry.identity.clone(),
                tree_hash,
                created: format_utc(now_utc()),
                references: entry.references.clone(),
                complete: true,
                lodi_version: env!("CARGO_PKG_VERSION").to_string(),
            };
            crate::lock::write_atomic(&self.meta_path(name), canonical_json(&meta).as_bytes())
                .map_err(|e| store_io(self.meta_path(name).display(), e))?;
            Ok(meta)
        })();
        match result {
            Ok(meta) => {
                report.created.push(name.to_string());
                Ok(meta)
            }
            Err(d) => {
                let _ = remove_tree(&staging);
                // A tree renamed into place without its sidecar is not complete; remove it now
                // rather than leave it for the next realization.
                if self.complete(name).is_none() && fs::symlink_metadata(&target).is_ok() {
                    let _ = remove_tree(&target);
                }
                Err(d)
            }
        }
    }

    /// The verified download of `url` in `cache/dl/<sha256>`. A cached file is re-hashed before
    /// use and discarded if it does not match; a new download is hashed while it streams into
    /// a temporary file and renamed into the cache only when size and SHA-256 match.
    pub fn fetch_verified(
        &self,
        fetcher: &dyn Fetcher,
        url: &str,
        sha256: &str,
        size: Option<u64>,
        report: &mut Report,
    ) -> Result<PathBuf, Diagnostic> {
        let hex = untag_sha256(sha256).ok_or_else(|| {
            Diagnostic::new("E_HASH_MISMATCH", format!("malformed hash `{sha256}`"))
        })?;
        let path = self.download_path(hex);
        if path.exists() {
            match hash_file(&path) {
                Ok((actual, len)) if actual == hex && size.is_none_or(|s| s == len) => {
                    return Ok(path);
                }
                _ => {
                    fs::remove_file(&path).map_err(|e| store_io(path.display(), e))?;
                    report.cache_discarded.push(hex.to_string());
                }
            }
        }
        let n = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .home
            .join("cache/dl")
            .join(format!(".{}-{n}.part", std::process::id()));
        let result = (|| {
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(|e| store_io(tmp.display(), e))?;
            let mut writer = HashingWriter {
                inner: io::BufWriter::with_capacity(1 << 20, file),
                hasher: Sha256::new(),
                len: 0,
            };
            // A size the lock states narrows the ceiling; it never raises it (M-1.0 S-1).
            let limit = size.map_or(MAX_ARTIFACT_BYTES, |s| s.min(MAX_ARTIFACT_BYTES));
            fetcher
                .download(url, &mut writer, limit)
                .map_err(|e| match e {
                    FetchError::Insecure(u) => {
                        Diagnostic::new("E_INSECURE_URL", format!("{u} is not an HTTPS URL"))
                    }
                    e @ FetchError::InsecureRedirect(..) => {
                        Diagnostic::new("E_INSECURE_URL", e.to_string())
                    }
                    other => Diagnostic::new("E_FETCH", other.to_string()),
                })?;
            let actual: String = writer
                .hasher
                .clone()
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let len = writer.len;
            let file = writer
                .inner
                .into_inner()
                .map_err(|e| store_io(tmp.display(), e.into_error()))?;
            report.downloaded += len;
            if actual != hex || size.is_some_and(|s| s != len) {
                return Err(Diagnostic::new(
                    "E_HASH_MISMATCH",
                    format!("{url} does not match the lock"),
                )
                .hint(format!(
                    "expected sha256:{hex}{}, got sha256:{actual} ({len} bytes); nothing was stored",
                    size.map_or(String::new(), |s| format!(" ({s} bytes)"))
                )));
            }
            file.sync_all().map_err(|e| store_io(tmp.display(), e))?;
            fs::rename(&tmp, &path).map_err(|e| store_io(path.display(), e))?;
            sync_dir(&self.home.join("cache/dl")).map_err(|e| store_io("cache", e))?;
            Ok(path.clone())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// Realize a locked tool as its `art-` entry: verified download, bounded extraction with
    /// the lock's `strip_components` and `subdir`, every `spec.bin` present, then [`publish`].
    ///
    /// [`publish`]: Store::publish
    pub fn realize_tool(
        &self,
        fetcher: &dyn Fetcher,
        tool: &ToolEntry,
        report: &mut Report,
        hook: Option<Hook>,
    ) -> Result<(String, Meta), Diagnostic> {
        let hook = hook.unwrap_or(&no_hook);
        let name = art_name(tool)?;
        let identity = artifacts_identity(tool)?;
        let meta = self.publish(
            Entry {
                name: name.clone(),
                kind: "art",
                identity,
                references: Vec::new(),
            },
            report,
            hook,
            |staging, report, hook| {
                // Several artifacts are realized in the order the declaration lists them, over
                // one another into one staging tree, so the entry's tree hash covers the merged
                // result (`spec/01` §3.8). Two artifacts that write the same path are refused.
                for artifact in &tool.artifacts {
                    let file = self.fetch_verified(
                        fetcher,
                        &artifact.url,
                        &artifact.sha256,
                        artifact.size,
                        report,
                    )?;
                    hook(Point::Downloaded, &name).map_err(|e| store_io("interrupted", e))?;
                    if artifact.format == "binary" {
                        archive::install_binary(&file, &tool.request.name, staging)?;
                    } else {
                        archive::extract_with_excludes(
                            &file,
                            &artifact.format,
                            artifact.strip_components,
                            &artifact.subdir,
                            archive::Excludes {
                                paths: &artifact.exclude,
                                recipe_file: &tool.recipe.path,
                            },
                            staging,
                            &Limits::default(),
                        )?;
                    }
                }
                for bin in &tool.spec.bin {
                    let path = staging.join(bin);
                    let ok = fs::metadata(&path)
                        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o100 != 0);
                    if !ok {
                        return Err(Diagnostic::new(
                            "E_BIN_MISSING",
                            format!(
                                "`{bin}` of {} {} is not an executable file in its artifact",
                                tool.request.name, tool.version
                            ),
                        ));
                    }
                }
                Ok(())
            },
        )?;
        Ok((name, meta))
    }

    /// Write the live-session root `gcroots/sessions/<pid>` naming `entries` (ADR-016). It is
    /// replaced atomically (temporary file and rename) but not synced: a root only matters while
    /// its process lives, and after a crash that process is gone and its root is pruned.
    pub fn add_session_root(
        &self,
        pid: u32,
        record: &serde_json::Value,
    ) -> Result<PathBuf, Diagnostic> {
        let path = self.sessions_dir().join(pid.to_string());
        let tmp = self.sessions_dir().join(format!(".{pid}.tmp"));
        // The session root is value-shaped, so its registered schema version and writer field
        // are stamped here rather than derived from a typed record (M-1.0 T-1, design call D5).
        fs::write(
            &tmp,
            crate::schema::stamped_json(SESSION_ROOT_ARTIFACT, record),
        )
        .and_then(|()| fs::rename(&tmp, &path))
        .map_err(|e| {
            let _ = fs::remove_file(&tmp);
            store_io(path.display(), e)
        })?;
        Ok(path)
    }

    /// Remove session roots whose process no longer exists; returns the pruned pids.
    pub fn prune_sessions(&self) -> Vec<i32> {
        let mut pruned = Vec::new();
        let Ok(entries) = fs::read_dir(self.sessions_dir()) else {
            return pruned;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<i32>().ok())
            else {
                continue;
            };
            if !pid_alive(pid) && fs::remove_file(entry.path()).is_ok() {
                pruned.push(pid);
            }
        }
        pruned
    }
}

struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    len: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A download tried again starts over (LD-386): the partial file is emptied and the digest and
/// length begin again, so what is verified and renamed into the cache is one attempt's bytes.
impl crate::fetch::Sink for HashingWriter<io::BufWriter<fs::File>> {
    fn restart(&mut self) -> io::Result<()> {
        self.inner.flush()?;
        crate::fetch::Sink::restart(self.inner.get_mut())?;
        self.hasher = Sha256::new();
        self.len = 0;
        Ok(())
    }
}

/// Hex SHA-256 and length of a file.
pub fn hash_file(path: &Path) -> io::Result<(String, u64)> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut len = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        len += n as u64;
    }
    Ok((
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        len,
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::lock::{ArtifactEntry, RecipeRef, ToolEntry, ToolRequest, ToolSpec};

    fn identity_tool(exclude: Vec<String>) -> ToolEntry {
        ToolEntry {
            request: ToolRequest {
                constraint: "1".into(),
                name: "tool".into(),
                provider: "binary".into(),
                declaration: None,
            },
            provider: "binary".into(),
            version: "1".into(),
            tag: None,
            recipe: RecipeRef {
                input: "builtin".into(),
                path: "tool.toml".into(),
                sha256: format!("sha256:{}", "b".repeat(64)),
            },
            artifacts: vec![ArtifactEntry {
                url: "https://example.invalid/tool.tar.gz".into(),
                sha256: format!("sha256:{}", "a".repeat(64)),
                size: None,
                format: "tar.gz".into(),
                strip_components: 1,
                subdir: String::new(),
                exclude,
            }],
            spec: ToolSpec {
                arch: "x86_64".into(),
                bin: vec![],
                path: vec![],
                env: BTreeMap::new(),
            },
        }
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize("Python 3.12+x/y"), "python-3.12+x-y");
        assert_eq!(sanitize(&"a".repeat(100)).len(), 64);
    }

    #[test]
    fn this_process_is_alive_and_pid_zero_is_not() {
        assert!(pid_alive(std::process::id() as i32));
        assert!(!pid_alive(0));
        assert!(!pid_alive(-1));
    }

    #[test]
    fn exclusions_move_identity_but_empty_lists_keep_the_old_identity() {
        let old = format!("sha256:{}", "a".repeat(64));
        assert_eq!(artifacts_identity(&identity_tool(vec![])).unwrap(), old);
        assert_ne!(
            artifacts_identity(&identity_tool(vec!["manifest.in".into()])).unwrap(),
            old
        );
    }
}
