//! `lodi update`, `lodi pin` and `lodi unpin` on the config discovery finds (#696, LD-499).
//!
//! They read the config, the archive and this machine's installed versions, and write only the
//! config's `host.toml` and `lodi.lock` (with lodi's caches): no root, no elevation, no host
//! change. Every lookup happens before any write, under the config folder's advisory lock, and
//! the two files change together or not at all ([`commit`]).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::lock::{self, Held, Lock};
use super::lookup::{Archive, Machine, http};
use super::{Config, Found, Identity, Location, Selection, User};
use crate::diag::Diagnostic;
use crate::hostscope::pin::verbs::{rewrite, table};
use crate::hostscope::pin::{self, PinsLock};
use crate::hostscope::safety::{Distro, Operation};
use crate::progress::{self, Event, Progress};

/// What one invocation was given.
#[derive(Debug, Clone)]
pub struct Invocation {
    /// The typed path, made absolute by the caller, or a URL.
    pub typed: Option<OsString>,
    /// `LODI_REPO`.
    pub env: Option<OsString>,
    pub identity: Identity,
    /// `--host NAME`.
    pub host: Option<String>,
    pub verbose: bool,
}

/// What `pin` and `unpin` act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Package(String),
    All,
}

/// The config, its layout and the chosen host.
struct Opened {
    found: Found,
    config: Config,
    selection: Selection,
}

fn open(inv: &Invocation) -> Result<Opened, Diagnostic> {
    let found = super::find(inv.typed.as_deref(), inv.env.as_deref(), &inv.identity)?;
    let dir = match &found.location {
        Location::Local(dir) => dir.clone(),
        Location::Url(url) => {
            return Err(Diagnostic::new(
                "E_UNSUPPORTED",
                format!("{url} is a fetched config, which lodi never writes"),
            )
            .hint("update a checkout of it instead, commit and push"));
        }
    };
    let config = super::read_layout(&dir, &found.user)?;
    super::switch::recipes(&config, &found.user);
    let selection = config.named_or_only(inv.host.as_deref())?;
    Ok(Opened {
        found,
        config,
        selection,
    })
}

/// `next: lodi switch`, and on which machine when the host changed is not this one: a switch
/// takes its host from the machine's hostname, and its `--host` means the host part only.
fn next(opened: &Opened) -> String {
    match &opened.selection.name {
        Some(name) if *name != opened.found.user.hostname => {
            format!("next: lodi switch, run on {name}")
        }
        _ => "next: lodi switch".to_string(),
    }
}

fn finish(opened: &Opened) {
    if let Err(error) = super::remember(&opened.found) {
        eprintln!("lodi: warning: {}", error.message);
    }
}

/// The host part `pin` and `unpin` change; a config with no host stops.
fn host_part(opened: &Opened) -> Result<super::Part, Diagnostic> {
    opened.selection.host.clone().ok_or_else(|| {
        Diagnostic::new(
            "E_NO_MANIFEST",
            format!("{} has no host.toml to pin", opened.config.root.display()),
        )
        .hint("pins belong to a host: import one with `lodi import`")
    })
}

fn archive(opened: &Opened, now: i64) -> Result<Archive, Diagnostic> {
    let user = &opened.found.user;
    Ok(Archive::new(
        &opened.config.root,
        Machine::of(&user.root)?,
        &user.home_or_root(),
        now,
    ))
}

/// Write each of `texts` and then `lock`, each atomically; when the lock cannot be written, the
/// texts are put back, so the files change together or not at all. `Ok(false)` when nothing
/// changed.
pub fn commit(
    held: &Held,
    user: &User,
    texts: &BTreeMap<PathBuf, String>,
    lock: &Lock,
) -> Result<bool, Diagnostic> {
    let mut written: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let failed = |path: &Path, e: io::Error| {
        Diagnostic::new(
            "E_STORE_IO",
            format!("cannot write {}: {e}", path.display()),
        )
    };
    let put_back = |written: &[(PathBuf, Vec<u8>)]| {
        for (path, old) in written {
            let _ = write_text(path, old, user);
        }
    };
    for (path, text) in texts {
        let old = std::fs::read(path).map_err(|e| failed(path, e))?;
        if old == text.as_bytes() {
            continue;
        }
        if let Err(e) = write_text(path, text.as_bytes(), user) {
            put_back(&written);
            return Err(failed(path, e));
        }
        written.push((path.clone(), old));
    }
    match held.write(lock, user) {
        Ok(changed) => Ok(changed || !written.is_empty()),
        Err(error) => {
            put_back(&written);
            Err(error)
        }
    }
}

/// Replace `path` atomically, keeping its mode; under `sudo` its owner is the folder's.
pub(super) fn write_text(path: &Path, bytes: &[u8], user: &User) -> io::Result<()> {
    let mode = std::fs::metadata(path)?.mode() & 0o7777;
    let folder = std::fs::metadata(path.parent().unwrap_or(Path::new("/")))?;
    crate::lock::write_atomic_with(path, bytes, |tmp| {
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode))?;
        if user.sudo {
            std::os::unix::fs::lchown(tmp, Some(folder.uid()), Some(folder.gid()))?;
        }
        Ok(())
    })
}

// -------------------------------------------------------------------------------- update ---

/// What one update would write: the lock, and each host manifest whose snapshot moved.
pub struct Planned {
    pub lock: Lock,
    pub texts: BTreeMap<PathBuf, String>,
}

/// The update step `lodi update` and `lodi switch --update` share: the host of `selection`
/// and every home it names looked up again, one step each, nothing written.
pub fn plan_update(
    config: &Config,
    selection: &Selection,
    old: &Lock,
    archive: &mut Archive,
    steps: &mut dyn progress::Sink,
) -> Result<Planned, Diagnostic> {
    let mut lock = old.clone();
    let mut step =
        |name: String, lock: &mut Lock, run: &mut dyn FnMut(&Lock) -> Result<Lock, Diagnostic>| {
            steps.event(Event::Begin {
                name,
                expected: None,
            });
            *lock = run(lock)?;
            steps.event(Event::Finish {
                count: None,
                detail: None,
            });
            Ok::<(), Diagnostic>(())
        };
    if let Some(host) = &selection.host {
        step(format!("look up {}", host.key), &mut lock, &mut |l| {
            lock::refresh(l, &[&host.key], &[], archive)
        })?;
    }
    for home in config.homes(selection)? {
        step(format!("lock {}", home.key), &mut lock, &mut |l| {
            lock::refresh(l, &[], &[&home.key], archive)
        })?;
    }
    let texts = archive
        .moved()
        .iter()
        .map(|(key, text)| (config.root.join(key), text.clone()))
        .collect();
    Ok(Planned { lock, texts })
}

/// The steps of [`plan_update`], by name, for the step list's total.
pub fn update_steps(config: &Config, selection: &Selection) -> Result<Vec<String>, Diagnostic> {
    let mut steps: Vec<String> = selection
        .host
        .iter()
        .map(|host| format!("look up {}", host.key))
        .collect();
    for home in config.homes(selection)? {
        steps.push(format!("lock {}", home.key));
    }
    Ok(steps)
}

/// `lodi update` on the config: refresh the lock of the chosen host and its homes, and move its
/// `[host] snapshot`, without switching.
pub fn update(inv: &Invocation) -> Result<String, Diagnostic> {
    let opened = open(inv)?;
    let user = &opened.found.user;
    let now = crate::util::now_utc();
    let names = update_steps(&opened.config, &opened.selection)?;
    let steps: Vec<&str> = names.iter().map(String::as_str).collect();
    let logs = user.logs("update", now);
    let mut shown = Progress::start(
        io::stderr(),
        Box::new(progress::Wall::new()),
        progress::Mode::detect(),
        inv.verbose,
        &steps,
        logs,
    );
    let result = (|| {
        let held = lock::hold(&opened.config.root)?;
        let old = held.read()?;
        let mut archive = archive(&opened, now)?;
        let planned = plan_update(
            &opened.config,
            &opened.selection,
            &old,
            &mut archive,
            &mut shown,
        )?;
        let changed = commit(&held, user, &planned.texts, &planned.lock)?;
        Ok((old, planned.lock, changed))
    })();
    let (old, new, changed) = match result {
        Ok(done) => done,
        Err(error) => {
            shown.fail(&error);
            return Err(error);
        }
    };
    let detail = if changed {
        moved(&old, &new)
    } else {
        "up to date; nothing written".to_string()
    };
    shown.summary("updated", &detail);
    drop(shown);
    finish(&opened);
    Ok(format!("{}\n", next(&opened)))
}

/// What moved between two locks, in one line.
fn moved(old: &Lock, new: &Lock) -> String {
    let mut parts = Vec::new();
    for (key, section) in &new.hosts {
        let before = old.hosts.get(key);
        let at = |s: Option<&lock::HostSection>| {
            s.and_then(|s| s.snapshot.as_ref())
                .map(|s| s.instant.clone())
        };
        if at(before) != at(Some(section))
            && let Some(now) = at(Some(section))
        {
            parts.push(format!("{key} snapshot {now}"));
        }
        let pins = section
            .pins
            .iter()
            .filter(|(name, pin)| before.and_then(|b| b.pins.get(*name)) != Some(*pin))
            .count();
        if pins > 0 {
            let noun = if pins == 1 { "pin" } else { "pins" };
            parts.push(format!("{key} {pins} {noun}"));
        }
    }
    for (key, section) in &new.homes {
        if old.homes.get(key) != Some(section) {
            let tools = section.tools.len();
            let noun = if tools == 1 { "tool" } else { "tools" };
            parts.push(format!("{key} {tools} {noun}"));
        }
    }
    if parts.is_empty() {
        "lodi.lock written".to_string()
    } else {
        parts.join(", ")
    }
}

/// `lodi update` in a project: every tool and the base resolved afresh, `./lodi.lock` written.
pub fn update_project(dir: &Path) -> Result<String, crate::lock::Failure> {
    let fetcher = crate::fetch::HttpFetcher::from_env()
        .map_err(|e| crate::lock::Failure::one(Diagnostic::new("E_CONFIG", e)))?;
    let now = crate::util::now_utc();
    match crate::lock::update_project(dir, &fetcher, now)? {
        crate::lock::Outcome::UpToDate(_) => Ok(format!(
            "lodi: {} is up to date; nothing written\n",
            crate::lock::LOCK_FILE
        )),
        crate::lock::Outcome::Written { lock, resolved } => {
            Ok(format!("lodi: {}\n", crate::lock::wrote(&lock, &resolved)))
        }
    }
}

// ----------------------------------------------------------------------------------- pin ---

/// `lodi pin PKG --to V`, `lodi pin --all [--to DATE]`: change `host.toml`, look the change up,
/// then write it with the lock; `lodi pin PKG` with no `--to` lists the versions instead.
pub fn pin(inv: &Invocation, target: &Target, to: Option<&str>) -> Result<String, Diagnostic> {
    if let (Target::Package(name), None) = (target, to) {
        return versions(inv, name);
    }
    let opened = open(inv)?;
    let part = host_part(&opened)?;
    let now = crate::util::now_utc();
    let held = lock::hold(&opened.config.root)?;
    let old = held.read()?;
    let mut archive = archive(&opened, now)?;
    let text = archive.text(&part.key)?;
    let machine = archive.machine().clone();
    let shown = part.path.display().to_string();
    let parsed = machine.parse(&text, &shown)?;
    let distro = machine.distro.name();
    let (edit, what) = match target {
        Target::All => {
            if machine.distro == Distro::Fedora {
                return Err(Diagnostic::new(
                    "E_UNSUPPORTED",
                    "`lodi pin --all` pins a date, and fedora has no dated archive",
                )
                .hint("pin each package to a build with `lodi pin PKG --to BUILD`"));
            }
            let value = match to {
                None => rewrite::default_snapshot(distro, now)?,
                Some(to) => pin::verbs::snapshot_value(machine.distro, to)?,
            };
            (
                rewrite::set_snapshot(&text, Some(&value))?,
                format!("[host] snapshot to {value}"),
            )
        }
        Target::Package(name) => {
            if !declares(&machine, &parsed, name) {
                return Err(Diagnostic::new(
                    "E_UNKNOWN_PACKAGE",
                    format!(
                        "`{name}` is not in the package list of {shown}, so it cannot be pinned"
                    ),
                )
                .hint(format!(
                    "add `{name}` to [packages] first; nothing was written"
                )));
            }
            let value = to.unwrap_or_default();
            (
                pin::verbs::set_entry(&text, distro, name, Some(value))?,
                format!("{name} to {value}"),
            )
        }
    };
    machine.parse(&edit.text, &shown)?;
    archive.with_text(&part.key, edit.text.clone());
    let new = lock::fill(&old, &[&part.key], &[], &mut archive)?;
    let texts = BTreeMap::from([(part.path.clone(), edit.text.clone())]);
    let mut out = String::new();
    if !commit(&held, &opened.found.user, &texts, &new)? {
        let _ = writeln!(out, "already pinned {what}; nothing written");
        finish(&opened);
        return Ok(out);
    }
    let _ = writeln!(out, "pinned {what}");
    if let Target::Package(name) = target
        && let Some(record) = new.hosts.get(&part.key).and_then(|s| s.pins.get(name))
    {
        let from = match &record.snapshot {
            Some(at) => format!("{} at {at}", record.repository),
            None => record.repository.clone(),
        };
        let _ = writeln!(out, "{name}: {} from {from}", record.version);
    }
    let _ = writeln!(out, "{}", next(&opened));
    finish(&opened);
    Ok(out)
}

/// Whether the manifest declares `name` for some machine: the names a pin may take.
fn declares(
    machine: &Machine,
    parsed: &crate::hostscope::manifest::HostManifest,
    name: &str,
) -> bool {
    let packages = &parsed.packages;
    packages
        .effective(machine.distro.name(), machine.arch())
        .iter()
        .chain(
            packages
                .per_distro
                .values()
                .chain(packages.per_arch.values())
                .flat_map(|table| &table.add),
        )
        .any(|declared| declared == name)
}

// --------------------------------------------------------------------------------- unpin ---

/// `lodi unpin PKG`: remove one pin and its lock entry; `lodi unpin --all`: remove the snapshot,
/// every pin and their entries. Nothing to remove is exit 0 and no write.
pub fn unpin(inv: &Invocation, target: &Target) -> Result<String, Diagnostic> {
    let opened = open(inv)?;
    let part = host_part(&opened)?;
    let held = lock::hold(&opened.config.root)?;
    let old = held.read()?;
    let mut archive = archive(&opened, crate::util::now_utc())?;
    let text = archive.text(&part.key)?;
    let distro = archive.machine().distro.name();
    let (edit, what) = match target {
        Target::All => (rewrite::float_all(&text)?, "every pin".to_string()),
        Target::Package(name) => (
            pin::verbs::set_entry(&text, distro, name, None)?,
            name.clone(),
        ),
    };
    let mut out = String::new();
    if !edit.changed {
        let who = if *target == Target::All {
            "the host"
        } else {
            &what
        };
        let _ = writeln!(out, "{who} has no pin; nothing written");
        finish(&opened);
        return Ok(out);
    }
    let shown = part.path.display().to_string();
    archive.machine().parse(&edit.text, &shown)?;
    archive.with_text(&part.key, edit.text.clone());
    let new = lock::fill(&old, &[&part.key], &[], &mut archive)?;
    let texts = BTreeMap::from([(part.path.clone(), edit.text.clone())]);
    commit(&held, &opened.found.user, &texts, &new)?;
    let _ = writeln!(out, "unpinned {what}");
    let _ = writeln!(out, "{}", next(&opened));
    finish(&opened);
    Ok(out)
}

// ------------------------------------------------------------------------------ versions ---

/// `lodi pin PKG`: the versions of `name` newest first, with when each was served and whether
/// it is installed, pinned or latest, then one line to copy. It writes only the versions cache.
/// The installed column reads this machine only when the chosen host is this machine.
fn versions(inv: &Invocation, name: &str) -> Result<String, Diagnostic> {
    let opened = open(inv)?;
    let part = host_part(&opened)?;
    let user = &opened.found.user;
    let machine = Machine::of(&user.root)?;
    let shown = part.path.display().to_string();
    let text = std::fs::read_to_string(&part.path)
        .map_err(|e| Diagnostic::new("E_NO_MANIFEST", format!("cannot read {shown}: {e}")))?;
    let parsed = machine.parse(&text, &shown)?;
    let this = opened
        .selection
        .name
        .as_ref()
        .is_none_or(|name| *name == user.hostname);
    let backend =
        crate::hostscope::pm::backend_for(machine.distro, &machine.root, Operation::Versions);
    let installed = match this {
        true => backend.observe()?.installed.get(name).cloned(),
        false => None,
    };
    let listing = Listing {
        name,
        declared: declares(&machine, &parsed, name),
        machine: &machine,
        parsed: &parsed,
        installed,
        key: &part.key,
        host: inv.host.as_deref(),
        backend,
    };
    let shown = match machine.distro {
        Distro::Fedora => listing.fedora()?,
        _ => listing.served(&opened.config.root)?,
    };
    finish(&opened);
    Ok(shown)
}

/// What `lodi pin PKG` lists: the package, the chosen host's file, and what this machine has
/// installed of it.
struct Listing<'a> {
    name: &'a str,
    /// Only a declared package can be pinned, so an undeclared one's listing ends saying so.
    declared: bool,
    machine: &'a Machine,
    parsed: &'a crate::hostscope::manifest::HostManifest,
    installed: Option<String>,
    /// The host's key in `config.toml`, and `--host` as typed.
    key: &'a str,
    host: Option<&'a str>,
    backend: Box<dyn crate::hostscope::pm::Backend>,
}

impl Listing<'_> {
    /// The listing's last line: the pin command to copy, or why there is none.
    fn copy(&self, to: &str) -> String {
        let name = self.name;
        if !self.declared {
            return format!(
                "{name} is not in the package list of {}; add it to [packages] there to pin it",
                self.key
            );
        }
        let host = self
            .host
            .map(|h| format!(" --host {}", pin::verbs::word(h)))
            .unwrap_or_default();
        format!("lodi pin {name} --to {}{host}", pin::verbs::word(to))
    }

    fn unknown(&self) -> Diagnostic {
        let distro = self.machine.distro.name();
        table::unknown_package(self.name, distro, &self.backend.nearest(self.name))
    }

    /// Fedora's listing: the builds its repositories offer.
    fn fedora(&self) -> Result<String, Diagnostic> {
        let dnf = crate::hostscope::pm::dnf::Dnf::new(&self.machine.root, Operation::Versions);
        let offers = dnf.offers(self.name)?;
        let Some(newest) = offers.first() else {
            return Err(self.unknown());
        };
        let pins = self.parsed.packages.pins("fedora");
        let pinned = pins.get(self.name).map(|p| p.requested.clone());
        let line = self.copy(&newest.build.full());
        Ok(pin::fedora::listing(
            &offers,
            self.installed.as_deref(),
            pinned.as_deref(),
            &line,
        ))
    }

    /// The listing of a dated archive: each version with when it arrived (or was served), read
    /// through the versions cache under the store in `config`'s lodi home.
    fn served(&self, config: &Path) -> Result<String, Diagnostic> {
        let (name, now) = (self.name, crate::util::now_utc());
        let machine = self.machine;
        let cx = machine.context(&self.parsed.sources, now);
        let section = lock::read(config)?.hosts.remove(self.key);
        let lock = section.map(|s| PinsLock {
            snapshot: s.snapshot,
            pins: s.pins,
            ..PinsLock::new(&s.distro, &s.release, &s.arch)
        });
        let interface = pin::per_package_interface(&cx)?;
        let home = crate::store::Store::home_from_env()?;
        let cache = pin::verbs::cache::VersionsCache::new(&home, machine.distro.name());
        let default = rewrite::default_snapshot(machine.distro.name(), now)?;
        let found = cache.lookup(name, now, || {
            if interface {
                let listed = pin::published_versions(http()?.as_ref(), &cx, name)?;
                return Ok(listed
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(version, arrived)| pin::verbs::cache::Arrival { arrived, version })
                    .collect());
            }
            Ok(pin::verbs::served(&cx, name, &default)?
                .map(|(resolved, _)| pin::verbs::cache::Arrival {
                    arrived: resolved.snapshot.unwrap_or(now),
                    version: resolved.version,
                })
                .into_iter()
                .collect())
        })?;
        let Some(newest) = found.rows.iter().max_by_key(|a| a.arrived) else {
            return Err(self.unknown());
        };
        let pinned = pin::verbs::pinned_version(&cx, Some(self.parsed), lock.as_ref(), name)?;
        let mut rows: Vec<table::Row> = found
            .rows
            .iter()
            .map(|a| table::Row {
                version: a.version.clone(),
                at: Some(a.arrived),
            })
            .collect();
        let extra = pinned
            .iter()
            .map(|(version, at)| (version.clone(), *at))
            .chain(self.installed.iter().map(|version| (version.clone(), None)));
        for (version, at) in extra {
            if !rows.iter().any(|row| row.version == version) {
                rows.push(table::Row { version, at });
            }
        }
        let to = match interface {
            true => newest.version.clone(),
            false => crate::util::format_utc(newest.arrived),
        };
        Ok(table::listing(
            if interface { "arrived" } else { "served" },
            &rows,
            self.installed.as_deref(),
            pinned.as_ref().map(|(version, _)| version.as_str()),
            Some(&self.copy(&to)),
        ))
    }
}
