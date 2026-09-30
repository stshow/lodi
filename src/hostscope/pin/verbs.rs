//! M-Pin's three verbs (m140-verbs, pv-1; handoff §3 and §5 V1–V8, LD-397): `host versions
//! NAME`, and `host pin` / `host unpin` for one name or `--all`, the analogues of
//! `nix flake lock --update-input` and `nix flake update`.
//!
//! They take the import's gate (an armed root and a supported distribution; no privilege step and
//! no apply lock), choose the host exactly as a plan does, and never touch the machine: they read
//! what is installed, ask the dated archive what it serves, and write only the host directory.
//! `pin` and `unpin` hold [`dirlock`] on the host directory, resolve **before** they write, write
//! `pins.lock` and then `host.toml` through [`commit`], and write nothing on any failure. Under
//! `sudo` on a host directory a user owns they become that user first, as the import does, so
//! nothing there is born root's; the host in place (`/etc/lodi`) is root's and needs root.
//!
//! The parts:
//! - [`rewrite`] changes exactly the named keys of `host.toml` through `toml_edit` and keeps every
//!   other byte and comment;
//! - [`cache`] is the one-day, name-keyed cache of what versions an archive offers (D11);
//! - [`table`] renders the `versions` table and the unknown-name refusal;
//! - [`dirlock`] is the advisory lock on the host directory the pin verbs hold, which is not the
//!   machine's apply lock and needs no root;
//! - [`commit`] writes `pins.lock` first, then `host.toml`, each atomically, keeping the mode, the
//!   owner and the group of what it replaces.

pub mod cache;
pub mod commit;
pub mod dirlock;
pub mod rewrite;
pub mod table;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::MetadataExt;

use super::{Context, Declared, PinsLock, Request, Resolution, Resolved};
use crate::diag::Diagnostic;
use crate::fetch::Fetcher;
use crate::hostscope::privilege::{self, Privilege};
use crate::hostscope::safety::{Distro, Gate, Operation, Options};
use crate::hostscope::{HostError, manifest, pm, source};
use crate::util::{format_utc, parse_utc};

use commit::LockChange;

/// ` SOURCE[ --host NAME][ --root DIR]`: the host this invocation chose, as every line it prints
/// to copy repeats it. An import's `--out DIR` is a host directory a pin names as `SOURCE`.
pub fn selection(options: &Options) -> String {
    let mut out = String::new();
    if let Some(source) = options.source.as_ref().or(options.out.as_ref()) {
        let _ = write!(out, " {}", word(&source.display().to_string()));
    }
    if let Some(host) = &options.host {
        let _ = write!(out, " --host {}", word(host));
    }
    if let Some(root) = &options.root {
        let _ = write!(out, " --root {}", word(&root.display().to_string()));
    }
    out
}

/// `text` as one shell word: as it is when nothing in it is special, else single-quoted.
pub fn word(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._+:=@,%-".contains(c));
    if plain {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\"'\"'"))
    }
}

/// `sudo ` where the host a printed line names is root's: the host in place on the running
/// system.
pub fn sudo_for(system_root: bool, in_place: bool) -> &'static str {
    if system_root && in_place { "sudo " } else { "" }
}

/// The gate and the chosen host, with the process become the host directory's owner when a verb
/// that writes it runs as root on a directory a user owns.
struct Opened {
    gate: Gate,
    host: source::Host,
}

fn open(options: &Options, operation: Operation) -> Result<Opened, HostError> {
    let mut gate = Gate::open(options, operation)?;
    if gate.distro == Distro::Fedora && options.all && operation == Operation::Pin {
        return Err(Diagnostic::new(
            "E_UNSUPPORTED",
            "`lodi host pin --all` pins a date, and fedora has no dated archive",
        )
        .hint("pin each package to a build with `lodi host pin NAME`; nothing was written")
        .into());
    }
    let host = source::select(
        &gate,
        options,
        &privilege::owners(&privilege::System),
        false,
    )?;
    if operation == Operation::Versions {
        return Ok(Opened { gate, host });
    }
    if host.in_place && gate.system_root && gate.euid != 0 {
        return Err(Diagnostic::new(
            "E_NEED_ROOT",
            format!(
                "`{}` writes {}, which is root's",
                operation.command(),
                host.dir.display()
            ),
        )
        .hint(
            "run it with sudo, or pin a host directory you own by naming it (SOURCE); nothing \
             was written",
        )
        .into());
    }
    if !host.in_place {
        let meta = fs::symlink_metadata(&host.dir)
            .map_err(|e| Diagnostic::new("E_STORE_IO", format!("{}: {e}", host.dir.display())))?;
        if let Some((uid, gid)) = privilege::writer(&privilege::System, meta.uid(), meta.gid()) {
            privilege::System.become_user(uid, gid)?;
            gate.euid = uid;
            gate.egid = gid;
        }
    }
    Ok(Opened { gate, host })
}

/// The manifest's text and the path it is shown as; `E_NO_MANIFEST` when there is none.
fn manifest_text(gate: &Gate, host: &source::Host) -> Result<(String, String), HostError> {
    let shown = host.manifest_path().display().to_string();
    let bytes = source::read_manifest(gate, host)?.ok_or_else(|| {
        Diagnostic::new("E_NO_MANIFEST", format!("no host manifest at {shown}")).hint(
            if host.in_place {
                manifest::import_hint(&gate.root, gate.system_root, &shown)
            } else {
                "write one there with `lodi host import`, or by hand".to_string()
            },
        )
    })?;
    let text = String::from_utf8(bytes)
        .map_err(|_| Diagnostic::new("E_SYNTAX", format!("{shown} is not UTF-8")))?;
    Ok((text, shown))
}

fn parse(gate: &Gate, text: &str, shown: &str) -> Result<manifest::HostManifest, HostError> {
    let parsed = manifest::parse(text, shown, &manifest::Context::from_gate(gate))?;
    gate.os
        .assert_distro(parsed.host.distro.as_deref(), shown)?;
    Ok(parsed)
}

fn context<'a>(
    gate: &'a Gate,
    sources: &'a BTreeMap<String, manifest::SourceEntry>,
    now: i64,
) -> Context<'a> {
    Context {
        distro: gate.distro,
        codename: match gate.distro {
            Distro::Arch => "rolling",
            Distro::Fedora => &gate.os.version_id,
            _ => &gate.os.codename,
        },
        arch: gate.arch(),
        sources,
        now,
    }
}

/// The fetcher every verb reads the archive through, configured from the environment.
fn http() -> Result<Box<dyn Fetcher>, Diagnostic> {
    crate::fetch::HttpFetcher::from_env()
        .map(|fetcher| Box::new(fetcher) as Box<dyn Fetcher>)
        .map_err(|error| Diagnostic::new("E_CONFIG", error))
}

fn refusal(r: super::Refusal) -> Diagnostic {
    Diagnostic::new(r.code, r.message).hint(r.hint)
}

/// The lock when it records this machine's distribution, release and architecture, else an
/// empty one for it.
fn machine_lock(cx: &Context, lock: Option<&PinsLock>) -> PinsLock {
    lock.filter(|lock| {
        lock.distro == cx.distro.name() && lock.release == cx.codename && lock.arch == cx.arch
    })
    .cloned()
    .unwrap_or_else(|| PinsLock::new(cx.distro.name(), cx.codename, cx.arch))
}

/// What the dated archive served of `name` at the date `requested` names, resolved as a pin of
/// that value is, with the index digests read on the way; `None` when it served no `name`.
fn served(
    cx: &Context,
    name: &str,
    requested: &str,
) -> Result<Option<(Resolved, Resolution)>, Diagnostic> {
    let request = super::parse_value(name, requested).map_err(refusal)?;
    let declared = BTreeMap::from([(
        name.to_string(),
        Declared {
            requested: requested.to_string(),
            request,
        },
    )]);
    match super::resolve(cx, None, &declared, None, &mut http) {
        Ok(mut resolution) => Ok(resolution
            .pins
            .remove(name)
            .map(|resolved| (resolved, resolution))),
        Err(e) if e.code == "E_NO_MATCH" => Ok(None),
        Err(e) => Err(e),
    }
}

/// The line that runs the next apply of this host.
fn next_apply(gate: &Gate, options: &Options) -> String {
    format!(
        "next: {}lodi host apply{}",
        if gate.system_root { "sudo " } else { "" },
        selection(options)
    )
}

// ------------------------------------------------------------------------------ versions ---

/// `lodi host versions NAME`: newest first, every version the archive offers with the day it
/// arrived and whether it is installed, pinned or the latest, then one line to copy. Where the
/// release has a per-package interface (Ubuntu) that is every version its own pockets
/// published; elsewhere (Debian, Arch; P1) it is what the dated archive serves now, beside the
/// pinned and the installed version. The archive's answer is kept a day in [`cache`].
pub fn versions(options: &Options) -> Result<String, HostError> {
    let Opened { gate, host } = open(options, Operation::Versions)?;
    let name = options
        .name
        .as_deref()
        .ok_or_else(|| Diagnostic::new("E_UNSUPPORTED", "`lodi host versions` needs a name"))?;
    let now = crate::util::now_utc();
    let parsed = match source::read_manifest(&gate, &host)? {
        Some(bytes) => {
            let shown = host.manifest_path().display().to_string();
            let text = String::from_utf8(bytes)
                .map_err(|_| Diagnostic::new("E_SYNTAX", format!("{shown} is not UTF-8")))?;
            Some(parse(&gate, &text, &shown)?)
        }
        None => None,
    };
    if gate.distro == Distro::Fedora {
        return fedora_versions(&gate, options, &host, parsed.as_ref(), name);
    }
    let lock = match parsed {
        Some(_) => crate::flakelock::read_host(&host, super::read_lock(&host)?)?.pins,
        None => None,
    };
    let no_sources = BTreeMap::new();
    let cx = context(
        &gate,
        parsed.as_ref().map_or(&no_sources, |p| &p.sources),
        now,
    );
    let backend = pm::backend_for(gate.distro, &gate.root, false, gate.operation);
    let installed = backend.observe()?.installed.get(name).cloned();

    let interface = super::per_package_interface(&cx)?;
    let home = crate::store::Store::home_from_env()?;
    let cache = cache::VersionsCache::new(&home, gate.distro.name());
    let default = rewrite::default_snapshot(gate.distro.name(), now)?;
    let lookup = cache.lookup(name, now, || {
        if interface {
            let fetcher = http()?;
            let listed = super::published_versions(fetcher.as_ref(), &cx, name)?;
            return Ok(listed
                .unwrap_or_default()
                .into_iter()
                .map(|(version, arrived)| cache::Arrival { arrived, version })
                .collect());
        }
        Ok(served(&cx, name, &default)?
            .map(|(resolved, _)| cache::Arrival {
                arrived: resolved.snapshot.unwrap_or(now),
                version: resolved.version,
            })
            .into_iter()
            .collect())
    })?;
    let Some(newest) = lookup.rows.iter().max_by_key(|a| a.arrived) else {
        return Err(
            table::unknown_package(name, gate.distro.name(), &backend.nearest(name)).into(),
        );
    };

    let pinned = pinned_version(&cx, parsed.as_ref(), lock.as_ref(), name)?;
    let mut rows: Vec<table::Row> = lookup
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
        .chain(installed.iter().map(|version| (version.clone(), None)));
    for (version, at) in extra {
        if !rows.iter().any(|row| row.version == version) {
            rows.push(table::Row { version, at });
        }
    }
    let to = if interface {
        word(&newest.version)
    } else {
        format_utc(newest.arrived)
    };
    let copy = format!(
        "{}lodi host pin {name} --to {to}{}",
        sudo_for(gate.system_root, host.in_place),
        selection(options)
    );
    Ok(table::listing(
        if interface { "arrived" } else { "served" },
        &rows,
        installed.as_deref(),
        pinned.as_ref().map(|(version, _)| version.as_str()),
        Some(&copy),
    ))
}

/// The version `name` is pinned to on this machine, and the instant it was read at: its own pin
/// (from `pins.lock` when that records it), else what the file-level snapshot serves of it.
fn pinned_version(
    cx: &Context,
    parsed: Option<&manifest::HostManifest>,
    lock: Option<&PinsLock>,
    name: &str,
) -> Result<Option<(String, Option<i64>)>, Diagnostic> {
    let Some(parsed) = parsed else {
        return Ok(None);
    };
    let lock = machine_lock(cx, lock);
    if let Some(declared) = parsed.packages.pins(cx.distro.name()).get(name) {
        if let Some(record) = lock.pins.get(name)
            && record.requested == declared.requested
            && record.policy == declared.request.policy()
        {
            let at = record.snapshot.as_deref().and_then(parse_utc);
            return Ok(Some((record.version.clone(), at)));
        }
        if let Request::Version(version) = &declared.request {
            return Ok(Some((version.clone(), None)));
        }
        return Ok(served(cx, name, &declared.requested)?
            .map(|(resolved, _)| (resolved.version, resolved.snapshot)));
    }
    let Some(snapshot) = parsed.host.snapshot.as_deref() else {
        return Ok(None);
    };
    Ok(served(cx, name, snapshot)?.map(|(resolved, _)| (resolved.version, resolved.snapshot)))
}

// ----------------------------------------------------------------------------------- pin ---

/// `lodi host pin NAME [--to VERSION|DATE]` and `lodi host pin --all [--to DATE]`: change the one
/// key, resolve every pin of the new file (a request `pins.lock` records is taken from it), and
/// write `pins.lock` and then `host.toml` — or, on any failure, nothing.
pub fn pin(options: &Options) -> Result<String, HostError> {
    let Opened { gate, host } = open(options, Operation::Pin)?;
    let repo = repository(&host);
    let _held = dirlock::HostDirLock::take(repo.as_deref().unwrap_or(&host.dir))?;
    let (text, shown) = manifest_text(&gate, &host)?;
    let parsed = parse(&gate, &text, &shown)?;
    let old = crate::flakelock::read_host(&host, super::read_lock(&host)?)?.pins;
    let now = crate::util::now_utc();
    let cx = context(&gate, &parsed.sources, now);
    let distro = gate.distro.name();
    let mut work = machine_lock(&cx, old.as_ref());
    let mut fetched: BTreeMap<i64, BTreeMap<String, String>> = BTreeMap::new();

    let (edit, what) = if options.all {
        let value = match &options.to {
            None => rewrite::default_snapshot(distro, now)?,
            Some(to) => snapshot_value(gate.distro, to)?,
        };
        (
            rewrite::set_snapshot(&text, Some(&value))?,
            format!("[host] snapshot to {value}"),
        )
    } else {
        let name = options
            .name
            .as_deref()
            .ok_or_else(|| Diagnostic::new("E_UNSUPPORTED", "`lodi host pin` needs a name"))?;
        if !declares(&gate, &parsed, name) {
            return Err(Diagnostic::new(
                "E_UNKNOWN_PACKAGE",
                format!("`{name}` is not in the package list of {shown}, so it cannot be pinned"),
            )
            .hint(format!(
                "add `{name}` to [packages] common or to a per-distro `add` first; nothing was \
                 written"
            ))
            .into());
        }
        // The manifest's own rule decides whether it may pin the name, before anything is read.
        parse(
            &gate,
            &set_entry(&text, distro, name, Some("1970-01-01"))?.text,
            &shown,
        )?;
        let value = match &options.to {
            _ if gate.distro == Distro::Fedora => fedora_build(&gate, options, &mut work, name)?,
            Some(to) => to.clone(),
            None => {
                let observed =
                    pm::backend_for(gate.distro, &gate.root, false, gate.operation).observe()?;
                let installed = observed.installed.get(name).cloned().ok_or_else(|| {
                    Diagnostic::new(
                        "E_UNKNOWN_PACKAGE",
                        format!("{name} is not installed, so there is no installed version to pin"),
                    )
                    .hint(format!(
                        "name a version or a date with --to; `lodi host versions {name}{}` lists \
                         what the archive offers",
                        selection(options)
                    ))
                })?;
                if super::per_package_interface(&cx)? {
                    installed
                } else {
                    let (value, resolved, resolution) =
                        day_serving(&cx, &parsed, name, &installed, options)?;
                    work.pins.insert(name.to_string(), resolved.record());
                    fetched.extend(resolution.fetched);
                    value
                }
            }
        };
        (
            set_entry(&text, distro, name, Some(&value))?,
            format!("{name} to {value}"),
        )
    };
    let reparsed = parse(&gate, &edit.text, &shown)?;
    let declared = reparsed.packages.pins(distro);
    let mut resolution = super::resolve(
        &cx,
        reparsed.host.snapshot.as_deref(),
        &declared,
        Some(&work),
        &mut http,
    )?;
    if let Some(snapshot) = resolution.snapshot.as_mut()
        && snapshot.indexes.is_empty()
        && !resolution.fetched.contains_key(&snapshot.instant)
    {
        snapshot.indexes = match fetched.get(&snapshot.instant) {
            Some(found) => found.clone(),
            None => {
                super::index_digests(http()?.as_ref(), cx.distro, cx.codename, snapshot.instant)?
            }
        };
    }
    let lock = resolution.lock(distro, cx.codename, cx.arch);
    let change = if lock.snapshot.is_none() && lock.pins.is_empty() {
        LockChange::Remove
    } else {
        LockChange::Write(super::render_lock(&lock).into_bytes())
    };
    let (outcome, written) = write(
        &host,
        repo.as_deref(),
        change,
        edit.changed.then_some(edit.text.as_str()),
    )?;
    let mut out = String::new();
    if !outcome.changed {
        let _ = writeln!(
            out,
            "already pinned {what} and recorded in {}; nothing written",
            if repo.is_some() {
                crate::flakelock::FILE
            } else {
                super::FILE
            }
        );
        return Ok(out);
    }
    let _ = writeln!(
        out,
        "{} {what}",
        if outcome.host { "pinned" } else { "recorded" }
    );
    if let Some(name) = options.name.as_deref()
        && let Some(resolved) = resolution.pins.get(name)
    {
        let _ = writeln!(
            out,
            "{name}: {} from {}",
            resolved.version,
            resolved.origin()
        );
    }
    for line in written {
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(out, "{}", next_apply(&gate, options));
    Ok(out)
}

/// `lodi host pin NAME [--to BUILD]` on Fedora (fk-1): the named build, or the installed one,
/// resolved from a configured repository and recorded in `work` — unless `work` already records
/// exactly that build, which is then taken as it is, with no request.
fn fedora_build(
    gate: &Gate,
    options: &Options,
    work: &mut PinsLock,
    name: &str,
) -> Result<String, HostError> {
    let backend = pm::dnf::Dnf::new(&gate.root, gate.operation);
    let build = match &options.to {
        Some(to) => super::fedora::parse_value(name, to)?,
        None => {
            let observed = pm::Backend::observe(&backend)?;
            let installed = observed.installed.get(name).ok_or_else(|| {
                Diagnostic::new(
                    "E_UNKNOWN_PACKAGE",
                    format!("{name} is not installed, so there is no installed build to pin"),
                )
                .hint(format!(
                    "name a build with --to; `lodi host versions {name}{}` lists the builds",
                    selection(options)
                ))
            })?;
            super::fedora::parse_value(name, installed)?
        }
    };
    if let Some(record) = work.pins.get(name)
        && build.arch.is_some()
        && record.requested == build.full()
    {
        return Ok(record.requested.clone());
    }
    let resolved = super::fedora::lookup(&gate.root, gate.operation, name, &build)?;
    work.pins.insert(name.to_string(), resolved.record());
    Ok(resolved.requested)
}

/// `lodi update` on Fedora (ff-1): each pin `host.toml` moved to a build `work` does not record
/// is resolved from a configured repository and recorded, as `lodi host pin --to` records it.
fn fedora_record_moved(
    gate: &Gate,
    declared: &BTreeMap<String, Declared>,
    work: &mut PinsLock,
) -> Result<(), HostError> {
    for (name, pin) in declared {
        let Request::Version(value) = &pin.request else {
            continue;
        };
        if work
            .pins
            .get(name)
            .is_some_and(|r| r.requested == pin.requested && r.policy == "version")
        {
            continue;
        }
        let build = super::fedora::parse_value(name, value)?;
        let mut resolved = super::fedora::lookup(&gate.root, gate.operation, name, &build)?;
        resolved.requested = pin.requested.clone();
        work.pins.insert(name.clone(), resolved.record());
    }
    Ok(())
}

/// `lodi host versions NAME` on Fedora (fk-1): the builds the configured repositories offer.
fn fedora_versions(
    gate: &Gate,
    options: &Options,
    host: &source::Host,
    parsed: Option<&manifest::HostManifest>,
    name: &str,
) -> Result<String, HostError> {
    let backend = pm::dnf::Dnf::new(&gate.root, gate.operation);
    let offers = backend.offers(name)?;
    let installed = pm::Backend::observe(&backend)?.installed.get(name).cloned();
    let Some(newest) = offers.first() else {
        return Err(table::unknown_package(
            name,
            gate.distro.name(),
            &pm::Backend::nearest(&backend, name),
        )
        .into());
    };
    let pinned = parsed
        .and_then(|p| p.packages.pins(gate.distro.name()).remove(name))
        .map(|pin| pin.requested);
    let copy = format!(
        "{}lodi host pin {name} --to {}{}",
        sudo_for(gate.system_root, host.in_place),
        newest.build.full(),
        selection(options)
    );
    Ok(super::fedora::listing(
        &offers,
        installed.as_deref(),
        pinned.as_deref(),
        &copy,
    ))
}

/// Whether the manifest declares `name` for some machine: the names a pin may take (LD-395).
fn declares(gate: &Gate, parsed: &manifest::HostManifest, name: &str) -> bool {
    let packages = &parsed.packages;
    packages
        .effective(gate.distro.name(), gate.arch())
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

/// `--all --to VALUE`: a date, written as the whole instant it floors to — the hour on apt, the
/// day on Arch.
fn snapshot_value(distro: Distro, to: &str) -> Result<String, Diagnostic> {
    match super::parse_value("snapshot", to).map_err(refusal)? {
        Request::Date(instant) => Ok(format_utc(if distro == Distro::Arch {
            instant - instant.rem_euclid(86_400)
        } else {
            instant
        })),
        Request::Version(_) => Err(Diagnostic::new(
            "E_TYPE",
            format!("`--all --to {to}`: the file-level snapshot is a date, not a version"),
        )
        .hint("give a day such as 2026-09-18 or an instant such as 2026-09-18T14:00:00Z")),
    }
}

/// On a release with no per-package interface (Debian, Arch; P1) a version is no pin, so `pin
/// NAME` with no `--to` pins the date whose dated archive serves exactly the installed version:
/// the name's own date pin first, so that pinning it again changes nothing, then the file's
/// snapshot, then now.
fn day_serving(
    cx: &Context,
    parsed: &manifest::HostManifest,
    name: &str,
    installed: &str,
    options: &Options,
) -> Result<(String, Resolved, Resolution), Diagnostic> {
    let now = rewrite::default_snapshot(cx.distro.name(), cx.now)?;
    let own = parsed
        .packages
        .pins(cx.distro.name())
        .remove(name)
        .filter(|pin| matches!(pin.request, Request::Date(_)))
        .map(|pin| pin.requested);
    let mut tried = Vec::new();
    for requested in own
        .iter()
        .chain(parsed.host.snapshot.iter())
        .chain(std::iter::once(&now))
    {
        if let Some((resolved, resolution)) = served(cx, name, requested)? {
            if resolved.version == installed {
                return Ok((requested.clone(), resolved, resolution));
            }
            tried.push(format!("{} at {requested}", resolved.version));
        }
    }
    Err(Diagnostic::new(
        "E_NO_MATCH",
        format!(
            "this machine has {name} {installed}, and the dated {} archive serves {}",
            cx.distro.name(),
            if tried.is_empty() {
                "no such package".to_string()
            } else {
                tried.join(", ")
            }
        ),
    )
    .hint(format!(
        "pin {name} with --to a day that served {installed}; `lodi host versions {name}{}` lists \
         what the archive offers",
        selection(options)
    )))
}

/// Set `name`'s pin where it lives — in `[packages.<distro>.pin]` when that table holds it, else
/// in `[packages.pin]` — or, with no value, remove it from both.
fn set_entry(
    text: &str,
    distro: &str,
    name: &str,
    value: Option<&str>,
) -> Result<rewrite::Edit, Diagnostic> {
    let own = rewrite::set_pin_in(text, Some(distro), name, None)?;
    match value {
        Some(value) if own.changed => rewrite::set_pin_in(text, Some(distro), name, Some(value)),
        Some(value) => rewrite::set_pin(text, name, Some(value)),
        None => {
            let common = rewrite::set_pin(&own.text, name, None)?;
            Ok(rewrite::Edit {
                changed: own.changed || common.changed,
                text: common.text,
            })
        }
    }
}

/// The repository a SOURCE chose this host from, whose root lock the verbs write (LD-416);
/// `None` for the host in place, which keeps 1.4's `pins.lock`.
fn repository(host: &source::Host) -> Option<std::path::PathBuf> {
    crate::flakelock::Repository::of(host).map(|repo| repo.dir)
}

/// Write the lock change and the manifest: 1.4's `pins.lock` beside the host in place, and in a
/// repository the host's section of the root lock, every 1.4 lock of the repository moved in
/// first (LD-416). The keys `lodi update` locked stay with the section. Returns what changed and
/// the lines that say so.
fn write(
    host: &source::Host,
    repo: Option<&std::path::Path>,
    change: LockChange,
    text: Option<&str>,
) -> Result<(commit::Outcome, Vec<String>), HostError> {
    let Some(repo) = repo else {
        let outcome = commit::write(&host.dir, change, text, || Ok(()))?;
        let mut lines = Vec::new();
        if outcome.lock {
            let path = host.dir.join(super::FILE);
            let verb = if path.exists() { "wrote" } else { "removed" };
            lines.push(format!("{verb} {}", path.display()));
        }
        if outcome.host {
            lines.push(format!("wrote {}", host.manifest_path().display()));
        }
        return Ok((outcome, lines));
    };
    let mut writer = crate::flakelock::Writer::open(repo)?;
    let keys = writer
        .host(&host.dir)
        .map(|section| section.keys.clone())
        .unwrap_or_default();
    let section = match change {
        LockChange::Keep => writer.host(&host.dir).cloned(),
        LockChange::Remove => None,
        LockChange::Write(bytes) => Some(crate::flakelock::HostSection::from_pins(
            super::parse_lock(&bytes, &host.dir.join(super::FILE).display().to_string())?,
        )),
    };
    let section = match section {
        Some(mut section) => {
            section.keys = keys;
            Some(section)
        }
        None if !keys.is_empty() => writer.host(&host.dir).cloned().map(|mut section| {
            section.snapshot = None;
            section.pins.clear();
            section
        }),
        None => None,
    };
    writer.set_host_dir(
        &host.dir,
        section.filter(|s| !s.is_empty() || !s.keys.is_empty()),
    )?;
    let manifest = host.manifest_path();
    let committed = writer.commit(text.map(|text| (manifest.as_path(), text)), || Ok(()))?;
    let mut lines = committed.lines();
    if committed.manifest {
        lines.push(format!("wrote {}", manifest.display()));
    }
    let outcome = commit::Outcome {
        changed: committed.lock || committed.manifest || !committed.migrated.is_empty(),
        lock: committed.lock,
        host: committed.manifest,
    };
    Ok((outcome, lines))
}

// --------------------------------------------------------------------------------- unpin ---

/// `lodi host unpin NAME`: remove the one key and its one `pins.lock` entry, with no request.
/// `lodi host unpin --all`: remove `[host] snapshot`, every pin table and `pins.lock`. A name
/// with no pin is exit 0, said, and nothing is written.
pub fn unpin(options: &Options) -> Result<String, HostError> {
    let Opened { gate, host } = open(options, Operation::Unpin)?;
    let repo = repository(&host);
    let _held = dirlock::HostDirLock::take(repo.as_deref().unwrap_or(&host.dir))?;
    let (text, shown) = manifest_text(&gate, &host)?;
    let old = crate::flakelock::read_host(&host, super::read_lock(&host)?)?.pins;
    let distro = gate.distro.name();
    let (edit, change, what) = if options.all {
        (
            rewrite::float_all(&text)?,
            LockChange::Remove,
            "every pin".to_string(),
        )
    } else {
        let name = options
            .name
            .as_deref()
            .ok_or_else(|| Diagnostic::new("E_UNSUPPORTED", "`lodi host unpin` needs a name"))?;
        let change = match old {
            Some(mut lock) if lock.pins.contains_key(name) => {
                lock.pins.remove(name);
                if lock.snapshot.is_none() && lock.pins.is_empty() {
                    LockChange::Remove
                } else {
                    LockChange::Write(super::render_lock(&lock).into_bytes())
                }
            }
            _ => LockChange::Keep,
        };
        (
            set_entry(&text, distro, name, None)?,
            change,
            name.to_string(),
        )
    };
    parse(&gate, &edit.text, &shown)?;
    let (outcome, written) = write(
        &host,
        repo.as_deref(),
        change,
        edit.changed.then_some(edit.text.as_str()),
    )?;
    let mut out = String::new();
    if !outcome.changed {
        let who = if options.all { "the file" } else { &what };
        let _ = writeln!(out, "{who} has no pin; nothing written");
        return Ok(out);
    }
    let _ = writeln!(out, "unpinned {what}");
    for line in written {
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(out, "{}", next_apply(&gate, options));
    Ok(out)
}

// -------------------------------------------------------------------------------- update ---

/// `lodi update` (DONE D4; F-25, LD-448, LD-416): the selected host's section of the
/// repository's root lock, and its homes', resolved afresh and written with the manifest in one
/// change — or, on any failure, nothing. The file-level snapshot moves to the day the dated
/// archive last published; a floating host gets none, and an explicit package pin never moves.
/// Each `signed_by_url` key is fetched and verified against its declared digest, which is then
/// locked (Q3). Every 1.4 lock of the repository is moved into the root lock on the way.
pub fn update(options: &Options) -> Result<String, HostError> {
    if let Some(source) = &options.source
        && crate::hostscope::remote::is_url(source.as_os_str())
    {
        return Err(Diagnostic::new(
            "E_UNSUPPORTED",
            format!(
                "`lodi update` writes the repository's lodi.lock, and {} is a fetched tree, which \
                 is never written",
                source.display()
            ),
        )
        .hint("run it in a checkout of that repository, commit the lodi.lock, push, and apply with --refresh")
        .into());
    }
    let Opened { gate, host } = open(options, Operation::Update)?;
    let Some(repo) = repository(&host) else {
        return Err(Diagnostic::new(
            "E_UNSUPPORTED",
            format!(
                "`lodi update` writes a repository's lodi.lock, and {} is the manifest in place",
                host.dir.display()
            ),
        )
        .hint("name the repository (`lodi update ~/lodi`); `sudo lodi host pin --all` moves the snapshot in place")
        .into());
    };
    let _held = dirlock::HostDirLock::take(&repo)?;
    let (text, shown) = manifest_text(&gate, &host)?;
    let parsed = parse(&gate, &text, &shown)?;
    let now = crate::util::now_utc();
    let cx = context(&gate, &parsed.sources, now);
    let distro = gate.distro.name();
    let mut writer = crate::flakelock::Writer::open(&repo)?;
    let before = writer.host(&host.dir).cloned();
    let mut work = machine_lock(&cx, before.as_ref().map(|s| s.to_pins()).as_ref());

    let mut out = String::new();
    let edit = match parsed.host.snapshot.as_deref() {
        Some(old) => {
            let value = rewrite::default_snapshot(distro, now)?;
            let _ = writeln!(out, "[host] snapshot {old} -> {value}");
            rewrite::set_snapshot(&text, Some(&value))?
        }
        None => {
            let _ = writeln!(out, "[host] has no snapshot: it floats, and none is added");
            rewrite::Edit {
                changed: false,
                text: text.clone(),
            }
        }
    };
    let reparsed = parse(&gate, &edit.text, &shown)?;
    let declared = reparsed.packages.pins(distro);
    if gate.distro == Distro::Fedora {
        fedora_record_moved(&gate, &declared, &mut work)?;
    }
    let mut resolution = super::resolve(
        &cx,
        reparsed.host.snapshot.as_deref(),
        &declared,
        Some(&work),
        &mut http,
    )?;
    if let Some(snapshot) = resolution.snapshot.as_mut()
        && snapshot.indexes.is_empty()
        && !resolution.fetched.contains_key(&snapshot.instant)
    {
        snapshot.indexes =
            super::index_digests(http()?.as_ref(), cx.distro, cx.codename, snapshot.instant)?;
    }
    for (name, pin) in &declared {
        if let Some(resolved) = resolution.pins.get(name) {
            let _ = writeln!(
                out,
                "{name}: {} kept at {}",
                pin.requested, resolved.version
            );
        }
    }
    let mut section =
        crate::flakelock::HostSection::from_pins(resolution.lock(distro, cx.codename, cx.arch));
    section.keys = locked_keys(&reparsed, before.as_ref())?;
    writer.set_host_dir(&host.dir, (!section.is_empty()).then_some(section))?;

    let fetcher = crate::fetch::HttpFetcher::from_env()
        .map_err(|error| Diagnostic::new("E_CONFIG", error))?;
    for home in crate::hostscope::homepart::declared(&gate, &host)? {
        match home {
            crate::hostscope::homepart::Declared::Missing(login) => {
                let _ = writeln!(
                    out,
                    "home {login} skipped: the root's /etc/passwd names no {login}"
                );
            }
            crate::hostscope::homepart::Declared::Home(home) => {
                let Some(target) = &home.target else {
                    continue;
                };
                let lock = crate::home::tools::locked_set(
                    home.manifest.tools.clone(),
                    home.lock.as_ref(),
                    &fetcher,
                    now,
                )
                .map_err(|failure| {
                    let mut error = HostError::from(Diagnostic::new(
                        failure.diagnostics.first().map_or("E_FETCH", |d| d.code),
                        format!("home {}: its tools could not be resolved", home.name),
                    ));
                    error.diagnostics.extend(failure.diagnostics);
                    error
                })?;
                if let Some(lock) = &lock {
                    let _ = writeln!(
                        out,
                        "home {}: {} tools locked",
                        home.name,
                        lock.packages.len()
                    );
                }
                writer.set_home(
                    &target.key,
                    lock.map(crate::flakelock::HomeSection::from_lock),
                );
            }
        }
    }
    let manifest = host.manifest_path();
    let committed = writer.commit(
        edit.changed
            .then_some((manifest.as_path(), edit.text.as_str())),
        || Ok(()),
    )?;
    for line in committed.lines() {
        let _ = writeln!(out, "{line}");
    }
    if committed.manifest {
        let _ = writeln!(out, "wrote {}", manifest.display());
    }
    if !committed.lock && !committed.manifest && committed.migrated.is_empty() {
        let _ = writeln!(out, "already up to date; nothing written");
    }
    let _ = writeln!(
        out,
        "next: {}lodi apply {}",
        if gate.system_root { "sudo " } else { "" },
        word(&repo.display().to_string())
    );
    Ok(out)
}

/// Q3: every `signed_by_url` key's SHA-256, fetched and verified against the declared digest —
/// or taken from the section when it already locks exactly that digest, with no request.
fn locked_keys(
    parsed: &manifest::HostManifest,
    before: Option<&crate::flakelock::HostSection>,
) -> Result<BTreeMap<String, String>, HostError> {
    let mut keys = BTreeMap::new();
    let mut wanted = Vec::new();
    for (name, source) in &parsed.sources {
        let Some(url) = &source.signed_by_url else {
            continue;
        };
        let declared = format!("sha256:{}", source.signed_by_sha256);
        if before.and_then(|s| s.keys.get(name)) == Some(&declared) {
            keys.insert(name.clone(), declared);
        } else {
            wanted.push((name.clone(), url.clone()));
        }
    }
    if !wanted.is_empty() {
        let fetched = crate::hostscope::sources::fetch(parsed, &wanted, http()?.as_ref())?;
        for (name, url) in wanted {
            let bytes = &fetched[&url];
            keys.insert(name, format!("sha256:{}", crate::util::sha256_hex(bytes)));
        }
    }
    Ok(keys)
}
