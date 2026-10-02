//! What `lodi pin` and `lodi unpin` (`crate::config::verbs`) share with the host engine: the
//! version a name is pinned to, what the dated archive served at a date, a `--to` value read
//! as a snapshot, and one `[packages.pin]` entry set or removed (LD-397). 2.0 writes no
//! `pins.lock` (#710).
//!
//! The parts:
//! - [`rewrite`] changes exactly the named keys of `host.toml` through `toml_edit` and keeps every
//!   other byte and comment;
//! - [`cache`] is the one-day, name-keyed cache of what versions an archive offers (D11);
//! - [`table`] renders the versions table and the unknown-name refusal;
//! - [`dirlock`] is the advisory lock on a folder the pin verbs hold, which is not the machine's
//!   apply lock and needs no root.

pub mod cache;
pub mod dirlock;
pub mod rewrite;
pub mod table;

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::{Context, Declared, PinsLock, Request, Resolution, Resolved};
use crate::diag::Diagnostic;
use crate::fetch::Fetcher;
use crate::hostscope::manifest;
use crate::hostscope::safety::{Distro, Options};
use crate::util::{format_utc, parse_utc};

/// ` SOURCE[ --root DIR]`: the host this invocation chose, as every line it prints to copy
/// repeats it.
pub fn selection(options: &Options) -> String {
    let mut out = String::new();
    if let Some(source) = &options.source {
        let _ = write!(out, " {}", word(&source.display().to_string()));
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
pub(crate) fn served(
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

// ------------------------------------------------------------------------------ versions ---

/// The version `name` is pinned to on this machine, and the instant it was read at: its own pin
/// (from `pins.lock` when that records it), else what the file-level snapshot serves of it.
pub(crate) fn pinned_version(
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

/// `--all --to VALUE`: a date, written as the whole instant it floors to — the hour on apt, the
/// day on Arch.
pub(crate) fn snapshot_value(distro: Distro, to: &str) -> Result<String, Diagnostic> {
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

/// Set `name`'s pin where it lives — in `[packages.<distro>.pin]` when that table holds it, else
/// in `[packages.pin]` — or, with no value, remove it from both.
pub(crate) fn set_entry(
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

// --------------------------------------------------------------------------------- unpin ---

// -------------------------------------------------------------------------------- update ---
