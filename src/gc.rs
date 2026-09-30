//! `lodi gc` — the store collector (M-0.3 T-3; design `spec/03` §7, design calls D6…D9).
//!
//! The order is `spec/03` §7 exactly:
//!
//! 1. take `store/.lock` **exclusively**, waiting (one line on stderr if it is held);
//! 2. prune `gcroots/sessions/<pid>` whose pid is not alive;
//! 3. the **live set** is what the persistent `gcroots/home` root and every surviving session
//!    root name — their `entries`, and for a container session the `img-<h32>` record it names
//!    (ADR-016, LD-110 adds the home root without exempting any entry by name);
//! 4. **mark** transitively over each sidecar's `references`; a reference with neither a
//!    directory nor a sidecar is the warning `W_MISSING_REF` and is ignored;
//! 5. **sweep** every unmarked `store/<entry>` with its sidecar, every sidecar whose entry is
//!    gone, `store/tmp/<name>` older than 24 h whose creator is gone, and every `img-` record
//!    whose image is gone from Podman's storage;
//! 6. **cache**: `cache/dl/<sha256>` that no surviving sidecar names **and** that is older than
//!    `--keep-days` (default 7), and `cache/shell/<h32>.lock` whose session root is gone;
//! 7. print one line per class and a total on stdout.
//!
//! The default **collects**; `--dry-run` **predicts** and changes nothing (design call D6,
//! LD-51). That is the opposite default from `scripts/reap-lodi.py`, and deliberately so: the
//! reaper acts on machine-wide resources it did not create and must prove ownership of, while
//! `lodi gc` removes only what Lodi itself put inside `$LODI_HOME`. The reaper is a developer
//! tool of this repository, not a product feature; `lodi gc` never calls it and never reads its
//! configuration.
//!
//! Nothing outside `$LODI_HOME` is removed unless `--images` is given, and then only Podman
//! images whose tag is **exactly** `localhost/lodi-env:<32 lower-case hex>` (design call D8,
//! LD-53, applying the ownership rule of LD-31/LD-32 unchanged). Removal never follows a
//! symlink out of the store and never crosses a mount.

use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::container::{ContainerEntry, ImageEntry, Podman};
use crate::diag::Diagnostic;
use crate::store::{Meta, Store, pid_alive};

/// Downloads no surviving sidecar names are evicted once they are this old (`spec/03` §10).
pub const DEFAULT_KEEP_DAYS: u64 = 7;
/// A staging directory in `store/tmp` is swept once it is this old (`spec/03` §7 step 5).
pub const STAGING_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// The one repository whose exact tags `--images` may remove.
pub const IMAGE_PREFIX: &str = "localhost/lodi-env:";
/// The build bases a Lodi image build starts from: never this command's business.
pub const BASE_PREFIX: &str = "localhost/lodi-tmp-base:";

/// What one `lodi gc` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Predict and change nothing.
    pub dry_run: bool,
    /// Evict an unreferenced download only once it is older than this many days.
    pub keep_days: u64,
    /// Also remove the unused `localhost/lodi-env:<h32>` images of Podman's storage.
    pub images: bool,
    /// Name every object, not only the per-class counts.
    pub verbose: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            dry_run: false,
            keep_days: DEFAULT_KEEP_DAYS,
            images: false,
            verbose: false,
        }
    }
}

/// The classes `spec/03` §7 sweeps, in the order they are printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    SessionRoot,
    Entry,
    Record,
    Staging,
    Download,
    ShellResolution,
}

impl Class {
    /// The order of the summary block: one line per class, always, even at zero.
    pub const ALL: [Class; 6] = [
        Class::SessionRoot,
        Class::Entry,
        Class::Record,
        Class::Staging,
        Class::Download,
        Class::ShellResolution,
    ];

    pub fn singular(self) -> &'static str {
        match self {
            Class::SessionRoot => "session root",
            Class::Entry => "entry",
            Class::Record => "image record",
            Class::Staging => "staging directory",
            Class::Download => "download",
            Class::ShellResolution => "shell resolution",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Class::SessionRoot => "session roots",
            Class::Entry => "entries",
            Class::Record => "image records",
            Class::Staging => "staging directories",
            Class::Download => "downloads",
            Class::ShellResolution => "shell resolutions",
        }
    }
}

/// One object the sweep removes, with the bytes it accounts for and why it is unused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removal {
    pub class: Class,
    /// What the object is called in its class (an entry name, a pid, a digest).
    pub name: String,
    /// Every path this removal unlinks, in the order it unlinks them.
    pub paths: Vec<PathBuf>,
    pub bytes: u64,
    pub why: String,
}

/// One image of Podman's storage the informational block names (design call D8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRow {
    pub tag: String,
    pub id: String,
    /// `true` when the tag is Lodi's own and nothing proves it is still in use.
    pub reclaimable: bool,
    pub why: String,
}

/// What a run did, or — with `--dry-run` — what it would have done.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub removals: Vec<Removal>,
    pub images: Vec<ImageRow>,
    /// `true` once Podman has been asked (only ever when an `img-` record exists).
    pub images_inspected: bool,
    /// `$LODI_HOME` does not exist: there is nothing to collect and nothing was created.
    pub nothing: bool,
    /// Warnings already written to stderr, kept for tests.
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn count(&self, class: Class) -> usize {
        self.removals.iter().filter(|r| r.class == class).count()
    }

    /// The bytes inside `$LODI_HOME` this run frees.
    pub fn freed(&self) -> u64 {
        self.removals.iter().map(|r| r.bytes).sum()
    }
}

fn store_io(path: &Path, e: io::Error) -> Diagnostic {
    Diagnostic::new("E_STORE_IO", format!("{}: {e}", path.display()))
}

fn store_perm(path: &Path, what: &str) -> Diagnostic {
    Diagnostic::new("E_STORE_PERM", format!("{} {what}", path.display()))
        .hint("make it writable, or set LODI_HOME to a store you own")
}

/// Whether the current user may write inside `path` (`access(2)`, `W_OK | X_OK`).
fn writable(path: &Path) -> bool {
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a valid C string; `access` only reports, it changes nothing.
    unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

/// The apparent size of `path` and everything below it, counted the way `du -sb` counts it
/// (**measured**, GNU coreutils 9.x on ext4): every file and symlink contributes its own
/// `st_size` and a directory contributes only what is inside it. Nothing is ever followed, so
/// a symlink counts as the bytes of its target's name and never as the target.
fn apparent_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.file_type().is_dir() {
        return meta.size();
    }
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| apparent_size(&entry.path()))
        .sum()
}

/// Remove `path` without ever following a symlink out of the store and without ever crossing a
/// mount: every directory descended into must be on device `dev`, and a symlink is unlinked,
/// never walked. A published tree is read-only, so directories are made writable first.
fn remove_guarded(path: &Path, dev: u64) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !meta.file_type().is_dir() {
        // A symlink, a file, a socket: unlinked as it stands. `remove_file` never follows.
        return fs::remove_file(path);
    }
    if meta.dev() != dev {
        return Err(io::Error::other(format!(
            "{} is on another file system; nothing below a mount point is removed",
            path.display()
        )));
    }
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    for entry in fs::read_dir(path)? {
        remove_guarded(&entry?.path(), dev)?;
    }
    fs::remove_dir(path)
}

fn age_of(path: &Path, now: SystemTime) -> Option<Duration> {
    let modified = fs::symlink_metadata(path).ok()?.modified().ok()?;
    now.duration_since(modified).ok()
}

fn is_h32(text: &str) -> bool {
    text.len() == 32
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_sha256_name(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Read `store/.meta/<name>.json` whatever its `complete` flag says; the sweep has to see
/// incomplete sidecars too.
fn read_meta(store: &Store, name: &str) -> Option<Meta> {
    serde_json::from_slice(&fs::read(store.meta_path(name)).ok()?).ok()
}

fn dir_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// The live set of one root: the entries it names, and the `img-` record when this is a container
/// session. `None` means the record could not be read, which makes the whole run conservative.
fn root_names(path: &Path) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let mut names = Vec::new();
    for entry in value.get("entries")?.as_array()? {
        names.push(entry.as_str()?.to_string());
    }
    if let Some(image) = value.get("image")
        && let Some(name) = image.get("name").and_then(|n| n.as_str())
    {
        names.push(name.to_string());
    }
    Some(names)
}

/// `lodi gc` with the store `$LODI_HOME` names. Warnings go to stderr as they are found; the
/// lines the command prints on stdout are returned.
pub fn run(options: &Options) -> Result<Vec<String>, Diagnostic> {
    let home = Store::home_from_env()?;
    let plan = collect(&home, options)?;
    Ok(render(&plan, options))
}

/// Collect (or, with `--dry-run`, predict) in the store at `home`.
pub fn collect(home: &Path, options: &Options) -> Result<Plan, Diagnostic> {
    let mut plan = Plan::default();
    match fs::metadata(home) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            plan.nothing = true;
            return Ok(plan);
        }
        Err(e) => return Err(store_io(home, e)),
        Ok(m) if !m.is_dir() => return Err(store_perm(home, "is not a directory")),
        Ok(_) => {}
    }
    if !writable(home) {
        return Err(store_perm(home, "is not writable"));
    }
    for relative in [
        "store",
        "store/.meta",
        "store/tmp",
        "cache/dl",
        "cache/shell",
    ] {
        let path = home.join(relative);
        if path.is_dir() && !writable(&path) {
            return Err(store_perm(&path, "is not writable"));
        }
    }
    // A collector sweeps a store it must understand, so a collection opens it for writing and
    // migrates the layout once (M-1.0 T-2); `--dry-run` is read-only and does neither — it reads
    // the marker if it is there and predicts against the layout it finds.
    let store = if options.dry_run {
        Store::open(home)?
    } else {
        Store::open_for_write(home)?
    };
    let dev = fs::metadata(home.join("store"))
        .map_err(|e| store_io(&home.join("store"), e))?
        .dev();

    // 1. The exclusive store lock, waiting, with one line on stderr when it is held.
    let _lock = match store.try_exclusive_lock()? {
        Some(lock) => lock,
        None => {
            eprintln!("lodi: waiting for the store lock");
            store.exclusive_lock()?
        }
    };

    let now = SystemTime::now();

    // 2. Dead session roots, and 3. the persistent home root plus the live set of the surviving
    // sessions. `session_live` stays separate because an ad-hoc shell resolution is a session
    // cache: a home environment that happens to use the same artifacts must not keep it.
    let mut live: BTreeSet<String> = BTreeSet::new();
    let mut session_live: BTreeSet<String> = BTreeSet::new();
    let mut unreadable_root = false;
    let mut unreadable_session_root = false;
    let home_root = home.join(crate::home::tools::HOME_GC_ROOT);
    if fs::symlink_metadata(&home_root).is_ok() {
        match root_names(&home_root) {
            Some(names) => live.extend(names),
            None => {
                unreadable_root = true;
                plan.warn(format!(
                    "W_ROOT_UNREADABLE: the home root {} cannot be read; nothing in the store is \
                     swept this run",
                    home_root.display()
                ));
            }
        }
    }
    for name in dir_names(&store.sessions_dir()) {
        let path = store.sessions_dir().join(&name);
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        if !pid_alive(pid) {
            plan.removals.push(Removal {
                class: Class::SessionRoot,
                name,
                bytes: apparent_size(&path),
                paths: vec![path],
                why: "its process is gone".into(),
            });
            continue;
        }
        match root_names(&path) {
            Some(names) => {
                live.extend(names.iter().cloned());
                session_live.extend(names);
            }
            None => {
                unreadable_root = true;
                unreadable_session_root = true;
                plan.warn(format!(
                    "W_ROOT_UNREADABLE: the live session root {} cannot be read; \
                     nothing in the store is swept this run",
                    path.display()
                ));
            }
        }
    }

    let meta_dir = home.join("store/.meta");
    let store_dir = home.join("store");
    let entry_names: BTreeSet<String> = dir_names(&store_dir)
        .into_iter()
        .filter(|n| !n.starts_with('.') && n != "tmp")
        .chain(
            dir_names(&meta_dir)
                .into_iter()
                .filter_map(|n| n.strip_suffix(".json").map(str::to_string)),
        )
        .collect();
    if unreadable_root {
        live.extend(entry_names.iter().cloned());
    }
    if unreadable_session_root {
        session_live.extend(entry_names.iter().cloned());
    }

    // 4. Mark transitively over each sidecar's `references`.
    let mut marked: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = live.iter().cloned().collect();
    while let Some(name) = queue.pop_front() {
        if !marked.insert(name.clone()) {
            continue;
        }
        match read_meta(&store, &name) {
            Some(meta) => queue.extend(meta.references),
            None if !store.entry_path(&name).exists() => plan.warn(format!(
                "W_MISSING_REF: `{name}` is named by a home root, a live session or a sidecar \
                 and is not in the store; it is ignored"
            )),
            None => {}
        }
    }

    // 5. The store sweep. Roots decide, not age: an entry is swept when nothing marks it.
    let mut surviving: Vec<Meta> = Vec::new();
    for name in &entry_names {
        if name.starts_with("img-") {
            continue;
        }
        let dir = store.entry_path(name);
        let sidecar = store.meta_path(name);
        if marked.contains(name) {
            if let Some(meta) = read_meta(&store, name) {
                surviving.push(meta);
            }
            continue;
        }
        let why = match (dir.exists(), store.complete(name).is_some()) {
            (false, _) => "its entry is gone",
            (true, false) => "it is incomplete: no sidecar says it is complete",
            (true, true) => "no root names it",
        };
        let paths: Vec<PathBuf> = [dir, sidecar]
            .into_iter()
            .filter(|p| fs::symlink_metadata(p).is_ok())
            .collect();
        plan.removals.push(Removal {
            class: Class::Entry,
            name: name.clone(),
            bytes: paths.iter().map(|p| apparent_size(p)).sum(),
            paths,
            why: why.into(),
        });
    }

    // `store/tmp/<name>` older than 24 h. A directory whose creator is still running is left
    // alone whatever its age: Podman's own temporary copies live there (LD-25).
    for name in dir_names(&store_dir.join("tmp")) {
        let path = store_dir.join("tmp").join(&name);
        let alive = name
            .split('-')
            .next()
            .and_then(|p| p.parse::<i32>().ok())
            .is_some_and(pid_alive);
        if alive || age_of(&path, now).is_none_or(|age| age < STAGING_MAX_AGE) {
            continue;
        }
        plan.removals.push(Removal {
            class: Class::Staging,
            name,
            bytes: apparent_size(&path),
            paths: vec![path],
            why: "it is staging older than 24 h and its process is gone".into(),
        });
    }

    // 6. The caches. A download survives while a surviving sidecar names its digest, and,
    // beyond that, until it is older than --keep-days.
    let referenced: BTreeSet<String> = surviving
        .iter()
        .filter_map(|m| crate::util::untag_sha256(&m.identity).map(str::to_string))
        .collect();
    let keep = Duration::from_secs(options.keep_days * 24 * 60 * 60);
    for name in dir_names(&home.join("cache/dl")) {
        if !is_sha256_name(&name) || referenced.contains(&name) {
            continue;
        }
        let path = home.join("cache/dl").join(&name);
        if age_of(&path, now).is_none_or(|age| age < keep) {
            continue;
        }
        plan.removals.push(Removal {
            class: Class::Download,
            name,
            bytes: apparent_size(&path),
            paths: vec![path],
            why: format!(
                "no entry names it and it is older than {} days",
                options.keep_days
            ),
        });
    }

    // An ad-hoc `lodi shell` resolution lives as long as a session root names the entries it
    // resolves to; after that it is a cache nothing roots (design call D7).
    for name in dir_names(&home.join("cache/shell")) {
        if !name.ends_with(".lock") {
            continue;
        }
        let path = home.join("cache/shell").join(&name);
        if shell_resolution_is_live(&path, &session_live) {
            continue;
        }
        plan.removals.push(Removal {
            class: Class::ShellResolution,
            name,
            bytes: apparent_size(&path),
            paths: vec![path],
            why: "no live session entered it".into(),
        });
    }

    plan.removals
        .sort_by(|a, b| (a.class, &a.name, &a.paths).cmp(&(b.class, &b.name, &b.paths)));

    // Podman is asked only when this store actually holds an `img-` record, and never
    // otherwise: a store with no container environment never starts a container runtime.
    let records: Vec<String> = entry_names
        .iter()
        .filter(|n| n.starts_with("img-"))
        .cloned()
        .collect();
    if !records.is_empty() {
        inspect_images(&store, &records, &marked, options, &mut plan)?;
    }

    if !options.dry_run {
        apply(&plan, dev)?;
    }
    Ok(plan)
}

impl Plan {
    fn warn(&mut self, text: String) {
        eprintln!("lodi: warning {text}");
        self.warnings.push(text);
    }
}

/// Whether a cached `lodi shell` resolution is the one a live session entered: every store
/// entry the resolution names is in the live set, and it names at least one.
fn shell_resolution_is_live(path: &Path, live: &BTreeSet<String>) -> bool {
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    let Ok(lock) = crate::lock::parse_lock(&bytes) else {
        return false;
    };
    let mut any = false;
    for tool in lock.packages.values() {
        let Ok(name) = crate::store::art_name(tool) else {
            return false;
        };
        any = true;
        if !live.contains(&name) {
            return false;
        }
    }
    any
}

/// The image block of design call D8: every `localhost/lodi-env:<h32>` and every build base of
/// Podman's storage, with what keeps it or the `podman rmi` that would reclaim it. Also decides
/// which `img-` records are stale (their image is gone from Podman's storage).
fn inspect_images(
    store: &Store,
    records: &[String],
    marked: &BTreeSet<String>,
    options: &Options,
    plan: &mut Plan,
) -> Result<(), Diagnostic> {
    let podman = match Podman::find(std::env::var_os("PATH").as_deref()) {
        Ok(p) => p,
        Err(_) => {
            plan.warn(
                "W_NO_RUNTIME: `podman` is not on PATH, so the environment images and the \
                 image records of this store were not inspected"
                    .into(),
            );
            return Ok(());
        }
    };
    let images = podman.list_images()?;
    let containers = podman.list_containers()?;
    let live_tags: BTreeSet<String> = records
        .iter()
        .filter(|name| marked.contains(*name))
        .filter_map(|name| crate::container::image_record(store, name).map(|m| m.tag))
        .collect();
    plan.images = classify_images(&images, &containers, &live_tags);
    plan.images_inspected = true;

    // An `img-` record whose image is gone from Podman's storage records nothing: it is swept
    // like any other residue. With --images the records of the tags this run reclaims go too.
    let present: BTreeSet<&str> = images.iter().map(|i| i.tag.as_str()).collect();
    let reclaimed: BTreeSet<&str> = if options.images {
        plan.images
            .iter()
            .filter(|r| r.reclaimable)
            .map(|r| r.tag.as_str())
            .collect()
    } else {
        BTreeSet::new()
    };
    for name in records {
        if marked.contains(name) {
            continue;
        }
        let tag = crate::container::image_record(store, name).map(|m| m.tag);
        let why = match tag.as_deref() {
            Some(tag) if reclaimed.contains(tag) => "the image it records is reclaimed by --images",
            Some(tag) if present.contains(tag) => continue,
            Some(_) => "the image it records is gone from Podman's storage",
            None => "it is not a record this build can read",
        };
        let path = store.meta_path(name);
        plan.removals.push(Removal {
            class: Class::Record,
            name: name.clone(),
            bytes: apparent_size(&path),
            paths: vec![path],
            why: why.into(),
        });
    }
    plan.removals
        .sort_by(|a, b| (a.class, &a.name, &a.paths).cmp(&(b.class, &b.name, &b.paths)));

    if options.images && !options.dry_run {
        remove_images(&podman, plan)?;
    }
    Ok(())
}

/// The ownership rule of LD-31 with LD-32, applied unchanged: only the **exact** tag Lodi's own
/// code writes is Lodi's, a container in any state keeps its image, and a build base is never
/// this command's business. Everything with another name is not listed at all.
pub fn classify_images(
    images: &[ImageEntry],
    containers: &[ContainerEntry],
    live_tags: &BTreeSet<String>,
) -> Vec<ImageRow> {
    let mut rows = Vec::new();
    for image in images {
        let Some(rest) = image.tag.strip_prefix(IMAGE_PREFIX) else {
            if image.tag.starts_with(BASE_PREFIX) {
                rows.push(ImageRow {
                    tag: image.tag.clone(),
                    id: image.id.clone(),
                    reclaimable: false,
                    why: "it is a build base, not an environment image; \
                          `lodi gc` never removes one"
                        .into(),
                });
            }
            continue;
        };
        if !is_h32(rest) {
            rows.push(ImageRow {
                tag: image.tag.clone(),
                id: image.id.clone(),
                reclaimable: false,
                why: format!(
                    "its tag is not `{IPFX}<32 lower-case hex>`, so it is not an image Lodi named",
                    IPFX = IMAGE_PREFIX
                ),
            });
            continue;
        }
        if live_tags.contains(&image.tag) {
            rows.push(ImageRow {
                tag: image.tag.clone(),
                id: image.id.clone(),
                reclaimable: false,
                why: "a live session is using it".into(),
            });
            continue;
        }
        if let Some(user) = containers
            .iter()
            .find(|c| c.image_id == image.id || c.image == image.tag)
        {
            rows.push(ImageRow {
                tag: image.tag.clone(),
                id: image.id.clone(),
                reclaimable: false,
                why: format!("container `{}` uses it", user.name),
            });
            continue;
        }
        rows.push(ImageRow {
            tag: image.tag.clone(),
            id: image.id.clone(),
            reclaimable: true,
            why: "no live session and no container uses it".into(),
        });
    }
    rows.sort_by(|a, b| (&a.tag, &a.id).cmp(&(&b.tag, &b.id)));
    rows
}

/// Remove the reclaimable tags of the plan, each re-verified against a fresh listing
/// immediately before it acts (design call D8). A row that stopped being reclaimable is kept
/// with its new reason rather than removed.
fn remove_images(podman: &Podman, plan: &mut Plan) -> Result<(), Diagnostic> {
    let tags: Vec<String> = plan
        .images
        .iter()
        .filter(|r| r.reclaimable)
        .map(|r| r.tag.clone())
        .collect();
    for tag in tags {
        let images = podman.list_images()?;
        let containers = podman.list_containers()?;
        let fresh = classify_images(&images, &containers, &BTreeSet::new());
        let still = fresh.iter().find(|r| r.tag == tag);
        let row = plan
            .images
            .iter_mut()
            .find(|r| r.tag == tag)
            .expect("the row is the one just read from the plan");
        match still {
            Some(r) if r.reclaimable => {
                podman.remove_image(&tag)?;
            }
            Some(r) => {
                row.reclaimable = false;
                row.why = r.why.clone();
            }
            None => {
                row.reclaimable = false;
                row.why = "it left Podman's storage while this run was working".into();
            }
        }
    }
    Ok(())
}

/// Do what the plan says, in its printed order, verifying nothing leaves `$LODI_HOME`.
fn apply(plan: &Plan, dev: u64) -> Result<(), Diagnostic> {
    for removal in &plan.removals {
        for path in &removal.paths {
            remove_guarded(path, dev).map_err(|e| store_io(path, e))?;
        }
    }
    Ok(())
}

fn count(n: usize, class: Class) -> String {
    format!(
        "{n} {}",
        if n == 1 {
            class.singular()
        } else {
            class.plural()
        }
    )
}

/// The lines the command prints on stdout. `--dry-run` prints the same lines with the verb in
/// the conditional: `removed` becomes `would remove` and `freed` becomes `would free`, so the
/// two runs compare line by line (acceptance A13).
pub fn render(plan: &Plan, options: &Options) -> Vec<String> {
    if plan.nothing {
        return vec!["nothing to collect".into()];
    }
    let remove = if options.dry_run {
        "would remove"
    } else {
        "removed"
    };
    let free = if options.dry_run {
        "would free"
    } else {
        "freed"
    };
    let mut lines = Vec::new();
    if options.verbose {
        for r in &plan.removals {
            lines.push(format!(
                "{remove} {} {} ({})",
                r.class.singular(),
                r.name,
                r.why
            ));
        }
    }
    for class in Class::ALL {
        lines.push(format!("{remove} {}", count(plan.count(class), class)));
    }
    lines.push(format!("{free} {} bytes", plan.freed()));
    if plan.images_inspected {
        let reclaimable = plan.images.iter().filter(|r| r.reclaimable).count();
        let kept = plan.images.len() - reclaimable;
        lines.push(format!(
            "images: {}, {kept} kept",
            if options.images {
                format!("{remove} {reclaimable}")
            } else {
                format!("{reclaimable} reclaimable, none removed")
            }
        ));
        for row in &plan.images {
            if row.reclaimable && options.images {
                lines.push(format!("  {remove} image {} ({})", row.tag, row.why));
            } else if row.reclaimable {
                lines.push(format!(
                    "  kept image {} ({}); reclaim it with `podman rmi {}` or `lodi gc --images`",
                    row.tag, row.why, row.tag
                ));
            } else {
                lines.push(format!("  kept image {} ({})", row.tag, row.why));
            }
        }
    }
    lines
}

/// The `--dry-run` verbs mapped back, so a prediction and the collection that follows it can be
/// compared line by line (acceptance A13; used by `tests/gc.rs` and the probe).
pub fn as_applied(line: &str) -> String {
    if let Some(rest) = line.strip_prefix("would remove") {
        return format!("removed{rest}");
    }
    if let Some(rest) = line.strip_prefix("would free") {
        return format!("freed{rest}");
    }
    if let Some(rest) = line.strip_prefix("  would remove") {
        return format!("  removed{rest}");
    }
    line.replace("would remove", "removed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn image(tag: &str, id: &str) -> ImageEntry {
        ImageEntry {
            tag: tag.into(),
            id: id.into(),
        }
    }

    #[test]
    fn only_the_exact_tag_is_lodis_and_a_container_keeps_its_image() {
        let h = "0".repeat(32);
        let other = "a".repeat(32);
        let images = [
            image(&format!("{IMAGE_PREFIX}{h}"), "id-a"),
            image(&format!("{IMAGE_PREFIX}{other}"), "id-b"),
            image(&format!("{IMAGE_PREFIX}not-a-hash"), "id-c"),
            image(&format!("{BASE_PREFIX}123-0"), "id-d"),
            image("docker.io/library/debian:bookworm", "id-e"),
        ];
        let containers = [ContainerEntry {
            name: "someone-elses".into(),
            image_id: "id-b".into(),
            image: format!("{IMAGE_PREFIX}{other}"),
        }];
        let rows = classify_images(&images, &containers, &BTreeSet::new());
        let by_tag: BTreeMap<&str, &ImageRow> = rows.iter().map(|r| (r.tag.as_str(), r)).collect();
        // Nothing that is not Lodi's by its own name is listed at all.
        assert!(!by_tag.contains_key("docker.io/library/debian:bookworm"));
        assert!(by_tag[&*format!("{IMAGE_PREFIX}{h}")].reclaimable);
        let held = by_tag[&*format!("{IMAGE_PREFIX}{other}")];
        assert!(!held.reclaimable && held.why.contains("someone-elses"));
        let odd = by_tag[&*format!("{IMAGE_PREFIX}not-a-hash")];
        assert!(!odd.reclaimable && odd.why.contains("32 lower-case hex"));
        let base = by_tag[&*format!("{BASE_PREFIX}123-0")];
        assert!(!base.reclaimable && base.why.contains("build base"));
    }

    #[test]
    fn a_live_session_keeps_its_image() {
        let h = "b".repeat(32);
        let tag = format!("{IMAGE_PREFIX}{h}");
        let live: BTreeSet<String> = [tag.clone()].into_iter().collect();
        let rows = classify_images(&[image(&tag, "id")], &[], &live);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].reclaimable && rows[0].why.contains("live session"));
    }

    #[test]
    fn the_dry_run_verbs_map_back_onto_the_applied_ones() {
        assert_eq!(as_applied("would remove 2 entries"), "removed 2 entries");
        assert_eq!(as_applied("would free 17 bytes"), "freed 17 bytes");
        assert_eq!(
            as_applied("  would remove image localhost/lodi-env:x (why)"),
            "  removed image localhost/lodi-env:x (why)"
        );
        assert_eq!(as_applied("nothing to collect"), "nothing to collect");
    }

    #[test]
    fn hashes_are_recognised_only_in_lower_case_hex_of_the_right_length() {
        assert!(is_h32(&"a".repeat(32)));
        assert!(!is_h32(&"A".repeat(32)));
        assert!(!is_h32(&"a".repeat(31)));
        assert!(!is_h32("not-a-hash"));
        assert!(is_sha256_name(&"0".repeat(64)));
        assert!(!is_sha256_name(&"0".repeat(63)));
    }
}
