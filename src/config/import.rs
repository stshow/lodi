//! `lodi import` into a config (#693, LD-497; #707, LD-505; #708, LD-506).
//!
//! It finds its folder as every 2.0 command does ([`super::find_for_import`]), decides every
//! refusal before it reads or writes anything, then, for the host, asks once whether lodi may
//! manage it (no "may manage" marker yet, `crate::marker`). The capture, the marker and the host
//! record (what was captured, as a switch records it, so that the first switch after an edit
//! applies it, LD-515) are the only root-needing work: they run through [`crate::elevate`] (in
//! place when already root), which returns the capture as JSON. This process then looks the lock up through the real
//! [`Archive`] (best effort) and only then writes: missing folders, the home file when missing,
//! the host's files under `files/host/` beside its file, `lodi.lock`, the host's file, and
//! `config.toml` last, so no file of the config is written by root. A failed write removes what
//! this run created. `git init` runs only on a folder this run created, as the person, and
//! nothing is committed.
//!
//! Which host this is ([`Host`]): the first (`host.toml`), this host's own file imported again
//! and merged into ([`again`]), or a second host at `<hostname>/host.toml` that `config.toml`
//! is written or extended to list. `--dry-run` reads the host as the person, never through an
//! elevator, stops after the merge and shows what would be written, naming what it could not
//! read, writing nothing.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io::{self, BufRead, IsTerminal, Write as _};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use super::lock::{self, Lock};
use super::lookup::{Archive, Machine};
use super::verbs::Invocation;
use super::{Location, Origin, User};
use crate::diag::Diagnostic;
use crate::elevate::{self, Context, Elevation, Request};
use crate::hostscope::import;
use crate::hostscope::safety::{Gate, Options};
use crate::progress::{self, Event, Progress, Sink as _};
use serde_json::{Value, json};

mod again;

const HOST: &str = "host.toml";
const HOME: &str = "home.toml";
/// Where the files a manifest refers to travel, beside it, by part.
const HOST_FILES: &str = "files/host/";

/// The one-time question (#686).
const QUESTION: &str = "May lodi manage this host?";

/// Whether a person is there to answer and to give a password: standard input and standard
/// error are a terminal.
fn terminal() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

fn need_root(euid: u32, why: &str) -> Diagnostic {
    Diagnostic::new(
        "E_NEED_ROOT",
        format!("importing the host reads it as root, this process runs as uid {euid}, and {why}"),
    )
    .hint(
        "run it under sudo (sudo lodi import), or capture only your home with lodi import \
         --home; nothing was written",
    )
}

/// The question on the terminal: yes for `y` or `yes`, anything else is no.
fn ask() -> bool {
    eprint!("{QUESTION} [y/N] ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    let _ = io::stdin().lock().read_line(&mut line);
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// The host part's consent and elevation, before anything is read or written: whether the
/// marker is to be written (the question's yes), and the elevation the root-needing work runs
/// through. Every stop here writes nothing and runs no elevator. A `dry_run` asks nothing, needs
/// no terminal and writes no marker: it reads the host as the person (#702 story 79).
fn consent(
    inv: &Invocation,
    user: &User,
    yes: bool,
    dry_run: bool,
) -> Result<(Option<&'static str>, Elevation), Diagnostic> {
    let euid = inv.identity.euid;
    let path = std::env::var_os("PATH");
    let exe = std::env::current_exe()
        .map_err(|e| Diagnostic::new("E_STORE_IO", format!("lodi's own binary: {e}")))?;
    let mut elevation = Elevation::new(&exe, path.as_deref(), user, euid);
    elevation.root = (user.root != Path::new("/")).then(|| user.root.clone());
    elevation.verbose = inv.verbose;
    if dry_run {
        return Ok((None, elevation));
    }
    let allowed = crate::marker::may_manage(&user.root, euid);
    let person = terminal();
    if !allowed && !yes && !person {
        return Err(Diagnostic::new(
            "E_DECLINED",
            format!(
                "lodi asks once whether it may manage this host ({QUESTION}), and there is no \
                 terminal to answer on"
            ),
        )
        .hint(
            "answer it with lodi import --yes, or capture only your home with lodi import \
             --home; nothing was written",
        ));
    }
    if euid != 0 {
        if !person {
            return Err(need_root(
                euid,
                "there is no terminal to ask for a password on",
            ));
        }
        elevate::choose(path.as_deref())
            .map_err(|_| need_root(euid, "none of sudo, doas or run0 is on PATH"))?;
    }
    let answer = match (allowed, yes) {
        (true, _) => None,
        (false, true) => Some("--yes"),
        (false, false) if ask() => Some("yes"),
        (false, false) => {
            return Err(Diagnostic::new(
                "E_DECLINED",
                "you answered no: lodi may not manage this host",
            )
            .hint("nothing was written; lodi import --home captures only your home"));
        }
    };
    Ok((answer, elevation))
}

/// What this import does with the host, decided before anything is read.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Host {
    /// No host yet: the top-level `host.toml`.
    First,
    /// This host's own file, at its key from the root: imported again, merged into it.
    Again(String),
    /// The file `config.toml` names for this host, not there yet: written as a first one.
    Listed(String),
    /// Another host's config: this one goes to `<hostname>/host.toml`, and `config.toml` is
    /// written (`extend` false) or extended with the text given.
    Second {
        key: String,
        index: String,
        extend: bool,
    },
}

/// Where this import writes, decided before anything is read.
struct Target {
    dir: PathBuf,
    /// The folders to create, outermost first.
    missing: Vec<PathBuf>,
    /// The host part; `None` for `--home`.
    host: Option<Host>,
    /// The home file's key from the root, if any, and whether it is there already.
    home: Option<String>,
    home_there: bool,
    /// Under `sudo`, who the written files belong to: the owner of the nearest folder that
    /// exists, as `config::verbs` and the lock writer give them (LD-499).
    owner: Option<(u32, u32)>,
}

impl Target {
    /// The folder the host file goes in: the config's own, or the second host's.
    fn host_folder(&self) -> PathBuf {
        match &self.host {
            Some(Host::Again(key) | Host::Listed(key) | Host::Second { key, .. }) => self
                .dir
                .join(key)
                .parent()
                .map_or_else(|| self.dir.clone(), Path::to_path_buf),
            _ => self.dir.clone(),
        }
    }
}

fn refuse(code: &'static str, dir: &Path, origin: Origin, why: &str) -> Diagnostic {
    Diagnostic::new(
        code,
        format!("{} ({}) {why}", dir.display(), origin.words()),
    )
}

/// A `dir` inside a folder the host scope keeps for itself below the root (`etc/lodi`,
/// `var/lib/lodi`), through any `..` and symbolic link: `E_STORE_IO`.
fn owned_by_the_host(dir: &Path, user: &User) -> Result<(), Diagnostic> {
    use crate::hostscope::{resolve_for_write, safety};
    let resolved = resolve_for_write(dir);
    for owned in [safety::MANIFEST, safety::STATE] {
        let Some(owned) = user.root.join(owned).parent().map(resolve_for_write) else {
            continue;
        };
        if resolved.starts_with(&owned) {
            return Err(Diagnostic::new(
                "E_STORE_IO",
                format!(
                    "{} is inside {}, which lodi keeps for the host itself",
                    dir.display(),
                    owned.display()
                ),
            )
            .hint("import into a folder of your own, such as ~/.config/lodi; nothing was read"));
        }
    }
    Ok(())
}

/// Every refusal, in order, before any read or write.
fn decide(dir: &Path, origin: Origin, user: &User, home_only: bool) -> Result<Target, Diagnostic> {
    let missing = refuse_folder(dir, origin, user)?;
    let (host, home) = if home_only {
        let home = if dir.join(super::INDEX).symlink_metadata().is_ok() {
            index_home(dir, user)?
        } else {
            HOME.to_string()
        };
        (None, Some(home))
    } else {
        let (host, home) = which_host(dir, user)?;
        (Some(host), home)
    };
    let owner = dir
        .ancestors()
        .find_map(|at| at.symlink_metadata().ok())
        .filter(|_| user.sudo)
        .map(|up| (up.uid(), up.gid()));
    let target = Target {
        owner,
        dir: dir.to_path_buf(),
        home_there: home
            .as_ref()
            .is_some_and(|home| dir.join(home).symlink_metadata().is_ok()),
        missing: missing.into_iter().rev().collect(),
        host,
        home,
    };
    // The host's files go under `files/host/` beside its file: each folder of that which is
    // there is judged now, so a link is refused before the host is read, not at the write.
    if target.host.is_some() {
        let folder = target.host_folder();
        for at in [folder.join("files"), folder.join(HOST_FILES)] {
            if at.symlink_metadata().is_ok() {
                super::judge_folder(&at, user)?;
            }
        }
    }
    Ok(target)
}

/// The refusals of the folder itself, in order; then the folders above it that are not there,
/// deepest first.
fn refuse_folder(dir: &Path, origin: Origin, user: &User) -> Result<Vec<PathBuf>, Diagnostic> {
    owned_by_the_host(dir, user)?;
    let there = dir.symlink_metadata().ok();
    if there.as_ref().is_some_and(|meta| !meta.is_dir()) {
        return Err(refuse("E_CONFIG", dir, origin, "is not a folder"));
    }
    if dir.join(crate::lock::MANIFEST_FILE).exists() {
        return Err(refuse(
            "E_CONFIG",
            dir,
            origin,
            "is a project (it holds lodi.toml), not a config",
        )
        .hint("import into a folder of its own, such as ~/.config/lodi"));
    }
    if let Some(root) = super::enclosing(dir, user) {
        return Err(refuse(
            "E_CONFIG",
            dir,
            origin,
            &format!("is inside the config {}", root.display()),
        )
        .hint(format!("import into the config's root, {}", root.display())));
    }
    if there.is_some() && !super::is_config(dir) && !empty(dir)? {
        return Err(refuse(
            "E_CONFIG",
            dir,
            origin,
            "holds files and is not a config (no lodi.lock, config.toml, host.toml or home.toml)",
        )
        .hint("import into an empty folder, a config, or a folder that is not there yet"));
    }
    super::judge_folder(dir, user).map_err(|mut refused| {
        refused.message = format!("{} (from {})", refused.message, origin.words());
        refused
    })?;
    let missing: Vec<PathBuf> = dir
        .ancestors()
        .take_while(|at| at.symlink_metadata().is_err())
        .map(Path::to_path_buf)
        .collect();
    let inside_home = user.home.as_ref().is_some_and(|home| dir.starts_with(home));
    if !missing.is_empty() && origin != Origin::Typed && !inside_home {
        return Err(refuse(
            "E_CONFIG",
            dir,
            origin,
            "is not there, and lodi creates a folder outside your home only when you type it",
        )
        .hint("create it, or type it: lodi import PATH"));
    }
    Ok(missing)
}

/// Which host this one is in the config at `dir`, and the invoking user's home file there.
fn which_host(dir: &Path, user: &User) -> Result<(Host, Option<String>), Diagnostic> {
    let hostname = &user.hostname;
    let index = dir.join(super::INDEX);
    if index.symlink_metadata().is_ok() {
        let config = super::read_layout(dir, user)?;
        if let Some((key, home)) = config.listed(hostname) {
            // A file the entry names that is there is judged; one that is not is written.
            let there = |key: &str| dir.join(key).symlink_metadata().is_ok();
            let parts = super::Parts {
                host: there(&key),
                home: home.as_deref().is_some_and(there),
            };
            config.this_host_parts(parts)?;
            let host = if parts.host {
                Host::Again(key)
            } else {
                Host::Listed(key)
            };
            return Ok((host, home));
        }
        let home = login(user, &index)
            .ok()
            .and_then(|login| config.home_of(login))
            .unwrap_or_else(|| HOME.to_string());
        let text = std::fs::read_to_string(&index)
            .map_err(|e| Diagnostic::new("E_STORE_IO", format!("{}: {e}", index.display())))?;
        let key = second(dir, hostname)?;
        let text = listed(&text, &index, hostname, &key, login(user, &index)?, &home)?;
        return Ok((
            Host::Second {
                key,
                index: text,
                extend: true,
            },
            Some(home),
        ));
    }
    let path = dir.join(HOST);
    if path.symlink_metadata().is_err() {
        return Ok((Host::First, Some(HOME.to_string())));
    }
    let config = super::read_layout(dir, user)?;
    config.this_host()?;
    let named = declared_hostname(&path)?;
    if named == *hostname {
        return Ok((Host::Again(HOST.to_string()), Some(HOME.to_string())));
    }
    if !crate::hostscope::source::safe_component(&named) {
        return Err(Diagnostic::new(
            "E_CONFIG",
            format!(
                "{} names the host {named:?}, which is not one safe name for {}",
                path.display(),
                index.display()
            ),
        ));
    }
    let key = second(dir, hostname)?;
    let login = login(user, &index)?;
    let text = listed("", &index, &named, HOST, login, HOME)?;
    let text = listed(&text, &index, hostname, &key, login, HOME)?;
    Ok((
        Host::Second {
            key,
            index: text,
            extend: false,
        },
        Some(HOME.to_string()),
    ))
}

/// The invoking user's login, which `config.toml` lists homes by.
fn login<'a>(user: &'a User, index: &Path) -> Result<&'a str, Diagnostic> {
    user.login.as_deref().ok_or_else(|| {
        Diagnostic::new(
            "E_CONFIG",
            format!(
                "{} lists homes by login, and uid {} has no login in the passwd file",
                index.display(),
                user.uid
            ),
        )
    })
}

/// The second host's key, `<hostname>/host.toml`, when its hostname is one safe folder name and
/// no `<hostname>` is there yet.
fn second(dir: &Path, hostname: &str) -> Result<String, Diagnostic> {
    if !crate::hostscope::source::safe_component(hostname) || hostname.starts_with('.') {
        return Err(Diagnostic::new(
            "E_PATH_ESCAPE",
            format!(
                "this host's name {hostname:?} is not one safe folder name, so a second host \
                 cannot be written to {}",
                dir.join(hostname).join(HOST).display()
            ),
        )
        .hint("nothing was written"));
    }
    let folder = dir.join(hostname);
    if folder.symlink_metadata().is_ok() {
        return Err(Diagnostic::new(
            "E_CONFIG",
            format!(
                "{} is there already and is not this host's, so this host cannot be written \
                 into it",
                folder.display()
            ),
        )
        .hint("move it away, or name this host in config.toml; nothing was written"));
    }
    Ok(format!("{hostname}/{HOST}"))
}

/// The `[system] hostname` the flat config's `host.toml` declares, or a stop: one host's
/// packages are never merged into another's file on a guess.
fn declared_hostname(path: &Path) -> Result<String, Diagnostic> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Diagnostic::new("E_STORE_IO", format!("{}: {e}", path.display())))?;
    let doc: Option<toml_edit::DocumentMut> = text.parse().ok();
    let named = doc.as_ref().and_then(|doc| {
        doc.get("system")?
            .get("hostname")?
            .as_str()
            .map(str::to_string)
    });
    named.ok_or_else(|| {
        Diagnostic::new(
            "E_CONFIG",
            format!(
                "{} declares no [system] hostname, so lodi cannot tell whether it is this host",
                path.display()
            ),
        )
        .hint(
            "add `hostname = \"NAME\"` under [system] in it, or a config.toml naming its \
             host; nothing was written",
        )
    })
}

/// `text` (a `config.toml`) with `[hosts.NAME]` added, listing `host` and `login`'s `home`;
/// every comment and the order of what is there are kept.
fn listed(
    text: &str,
    index: &Path,
    name: &str,
    host: &str,
    login: &str,
    home: &str,
) -> Result<String, Diagnostic> {
    use toml_edit::{DocumentMut, InlineTable, Item, Table, value};
    let bad = |why: &str| Diagnostic::new("E_CONFIG", format!("{} {why}", index.display()));
    let mut doc: DocumentMut = text.parse().map_err(|_| bad("is not TOML"))?;
    let hosts = doc.entry("hosts").or_insert_with(|| {
        let mut table = Table::new();
        table.set_implicit(true);
        Item::Table(table)
    });
    let hosts = hosts
        .as_table_mut()
        .ok_or_else(|| bad("names `hosts` as something other than a table"))?;
    let mut entry = Table::new();
    entry.insert("host", value(host));
    let mut homes = InlineTable::new();
    homes.insert(login, home.into());
    entry.insert("homes", value(homes));
    if !text.is_empty() || !hosts.is_empty() {
        entry.decor_mut().set_prefix("\n");
    }
    hosts.insert(name, Item::Table(entry));
    Ok(doc.to_string())
}

/// The home `config.toml` lists for this host and the person, there or not, or a stop.
fn index_home(dir: &Path, user: &User) -> Result<String, Diagnostic> {
    let config = super::read_layout(dir, user)?;
    let stop = |why: String| {
        Diagnostic::new("E_CONFIG", why).hint(format!(
            "import the host with lodi import, or add this host's entry to {} by hand",
            dir.join(super::INDEX).display()
        ))
    };
    let Some((_, home)) = config.listed(&user.hostname) else {
        let refused = config.this_host().err().map(|d| d.message);
        return Err(stop(format!(
            "{}: {}",
            dir.display(),
            refused.unwrap_or_default()
        )));
    };
    match home {
        Some(home) => {
            let there = dir.join(&home).symlink_metadata().is_ok();
            config.this_host_parts(super::Parts {
                host: false,
                home: there,
            })?;
            Ok(home)
        }
        None => Err(stop(format!(
            "{} lists no home for {} on {}",
            dir.join(super::INDEX).display(),
            user.login.as_deref().unwrap_or("you"),
            user.hostname
        ))),
    }
}

/// Whether `dir` holds nothing but hidden entries.
fn empty(dir: &Path) -> Result<bool, Diagnostic> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| Diagnostic::new("E_STORE_IO", format!("{}: {e}", dir.display())))?;
    Ok(entries
        .flatten()
        .all(|entry| entry.file_name().to_string_lossy().starts_with('.')))
}

/// What the host capture produced.
struct Captured {
    text: String,
    bundle: import::files::Capture,
    warnings: Vec<String>,
    declared: usize,
    not_captured: usize,
    machine: again::Machine,
    /// The bundle's carried keyrings (`Repositories::bundle`): they travel beside the file and
    /// are never `[files]` entries, so importing again does not merge them.
    keyrings: Vec<String>,
    /// The host record before this import records the host again: the merge's base.
    last: again::Last,
    /// The changed files this process could not read: a preview as the person names them.
    unread: Vec<String>,
}

impl Captured {
    fn to_json(&self) -> Value {
        let files: Vec<Value> = self
            .bundle
            .captured
            .iter()
            .map(|file| {
                json!({
                    "path": file.path, "source": file.source, "mode": file.mode,
                    "owner": file.owner, "group": file.group, "bytes": hex(&file.bytes),
                })
            })
            .collect();
        json!({
            "text": self.text, "warnings": self.warnings, "declared": self.declared,
            "not_captured": self.not_captured, "files": files,
            "machine": self.machine.to_json(), "keyrings": self.keyrings,
            "last": self.last.to_json(), "unread": self.unread,
        })
    }

    fn from_json(value: &Value) -> Option<Captured> {
        let text = |value: &Value, key: &str| value.get(key)?.as_str().map(str::to_string);
        let number = |value: &Value, key: &str| value.get(key)?.as_u64();
        let mut captured = Vec::new();
        for file in value.get("files")?.as_array()? {
            captured.push(import::files::Captured {
                path: text(file, "path")?,
                source: text(file, "source")?,
                mode: u32::try_from(number(file, "mode")?).ok()?,
                owner: text(file, "owner")?,
                group: text(file, "group")?,
                bytes: unhex(&text(file, "bytes")?)?,
            });
        }
        let warnings = value.get("warnings")?.as_array()?;
        Some(Captured {
            text: text(value, "text")?,
            bundle: import::files::Capture {
                captured,
                ..Default::default()
            },
            warnings: warnings
                .iter()
                .map(|line| line.as_str().map(str::to_string))
                .collect::<Option<_>>()?,
            declared: usize::try_from(number(value, "declared")?).ok()?,
            not_captured: usize::try_from(number(value, "not_captured")?).ok()?,
            machine: again::Machine::from_json(value.get("machine")?)?,
            keyrings: serde_json::from_value(value.get("keyrings")?.clone()).ok()?,
            last: serde_json::from_value(value.get("last")?.clone()).ok()?,
            unread: serde_json::from_value(value.get("unread")?.clone()).ok()?,
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

/// The root-needing part, as root (through [`crate::elevate`]): read the host, then write the
/// marker when `mark`. Each is a step of the caller's list.
pub fn elevated(
    mark: bool,
    record: Option<&Path>,
    cx: &Context,
    sink: &mut dyn progress::Sink,
) -> Result<Value, Diagnostic> {
    let (mut captured, read) = step(sink, "read the host", || read_host(&cx.root))?;
    if mark {
        step(sink, "write the marker", || {
            crate::marker::write(&cx.root, cx.euid)
        })?;
    }
    if let Some(folder) = record {
        let (gate, machine, capture) = &read;
        let lines = step(sink, RECORD, || {
            crate::hostscope::record_import(gate, machine, capture, folder)
        })?;
        captured.warnings.extend(lines);
    }
    Ok(captured.to_json())
}

/// The step that writes the host record (LD-515).
const RECORD: &str = "record the host";

/// What the record is written from: the gate the host was read through, and what was read.
type Read = (Gate, import::Machine, import::files::Capture);

fn read_host(root: &Path) -> Result<(Captured, Read), Diagnostic> {
    let options = Options {
        root: Some(root.to_path_buf()),
        ..Options::default()
    };
    let gate = Gate::open_for_import(&options)?;
    // Read before this run records the host again: the base of importing again (LD-519).
    let last = again::Last::read(&gate);
    let machine = import::read(&gate)?;
    let capture = import::files::capture(&gate)?;
    let text = import::emit::manifest(&machine, &import::Snapshot::now(), Some(&capture));
    let selection = import::baseline::select(&machine);
    let mut warnings = selection.warnings(machine.distro);
    warnings.extend(capture.warnings());
    let mut bundle = machine.repositories.bundle(&capture);
    let keyrings = bundle.captured[capture.captured.len()..]
        .iter()
        .map(|file| file.path.clone())
        .collect();
    for file in &mut bundle.captured {
        if let Some(rest) = file.source.strip_prefix("files/") {
            file.source = format!("{HOST_FILES}{rest}");
        }
    }
    let side = machine_side(&gate, &machine, &selection);
    let captured = Captured {
        text: text.replace("= \"files/", &format!("= \"{HOST_FILES}")),
        bundle,
        warnings,
        declared: selection.common.len(),
        not_captured: selection.not_captured(),
        machine: side,
        keyrings,
        last,
        unread: capture.unread.clone(),
    };
    Ok((captured, (gate, machine, capture)))
}

/// The machine's side of importing again, for [`again`] to merge with the config.
fn machine_side(
    gate: &Gate,
    machine: &import::Machine,
    selection: &import::baseline::Selection,
) -> again::Machine {
    let observed = &machine.observed;
    let distro = machine.distro.name();
    let mut elsewhere = std::collections::BTreeMap::new();
    for (name, label) in &selection.third_party {
        elsewhere.insert(
            name.clone(),
            format!("from a repository that is not {distro}'s own ({label})"),
        );
    }
    for name in &selection.local_only {
        elsewhere.insert(
            name.clone(),
            "from a package file, or from a source this machine no longer has".to_string(),
        );
    }
    for name in &selection.foreign {
        elsewhere.insert(
            name.clone(),
            "from no repository this package manager knows".to_string(),
        );
    }
    let holds = crate::hostscope::pm::Shape::of(gate.distro)
        .hold_is_machine_state
        .then(|| {
            selection
                .hold
                .iter()
                .filter_map(|name| Some((name.clone(), observed.installed.get(name)?.clone())))
                .collect()
        });
    let ctx = crate::hostscope::manifest::Context::from_gate(gate);
    again::Machine {
        chosen: selection.common.iter().cloned().collect(),
        explicit: import::baseline::chosen(observed).into_iter().collect(),
        elsewhere,
        holds,
        arch: ctx.arch,
        distro: ctx.distro,
        release: ctx.release,
        codename: ctx.codename,
    }
}

/// `lodi import [PATH] [--home] [--yes] [--dry-run]`: the report for standard output, `W_` lines
/// first. `--dry-run` captures, decides every refusal and merges, then shows on standard error
/// what an import would write, and writes nothing. Long because it is the command's steps in
/// order, each a few lines.
pub fn run(
    inv: &Invocation,
    home_only: bool,
    yes: bool,
    dry_run: bool,
) -> Result<String, Diagnostic> {
    let found = super::find_for_import(inv.typed.as_deref(), inv.env.as_deref(), &inv.identity)?;
    let user = found.user.clone();
    let dir = match &found.location {
        Location::Local(dir) => dir.clone(),
        Location::Url(url) => {
            return Err(Diagnostic::new(
                "E_UNSUPPORTED",
                format!("{url} is a URL, and lodi never imports into a checkout"),
            )
            .hint("import into a folder of yours, then commit and push it"));
        }
    };
    let target = decide(&dir, found.origin, &user, home_only)?;
    let host = if home_only {
        None
    } else {
        Some(consent(inv, &user, yes, dry_run)?)
    };
    let git = (!dry_run && !target.missing.is_empty())
        .then(|| program("git"))
        .flatten();
    let names = step_names(
        host.as_ref().map(|(answer, _)| answer.is_some()),
        dry_run,
        git.is_some(),
    );
    let now = crate::util::now_utc();
    let logs = user.logs("import", now).filter(|_| !dry_run);
    let mut shown = Progress::start(
        io::stderr(),
        Box::new(progress::Wall::new()),
        progress::Mode::detect(),
        inv.verbose,
        &names,
        logs,
    );
    let mut report = String::new();
    if let Some((Some(answer), _)) = &host {
        shown.event(Event::Begin {
            name: "ask".into(),
            expected: None,
        });
        shown.event(Event::Finish {
            count: None,
            detail: Some((*answer).to_string()),
        });
    }
    let result = steps(
        &target,
        &user,
        host.as_ref()
            .map(|(answer, elevation)| (answer.is_some(), elevation)),
        git.as_deref(),
        now,
        dry_run,
        &mut shown,
        &mut report,
    );
    let wrote = match result {
        Ok(wrote) => wrote,
        Err(error) => {
            shown.fail(&error);
            return Err(error);
        }
    };
    if dry_run {
        drop(shown);
        for line in wrote {
            eprintln!("{line}");
        }
        return Ok(report);
    }
    // Importing again with nothing new writes nothing, and says so (LD-528).
    let wrote = if wrote.is_empty() {
        "nothing changed".to_string()
    } else {
        wrote.join(", ")
    };
    shown.summary("imported", &format!("{}: {wrote}", dir.display()));
    drop(shown);
    if let Err(error) = super::remember(&found) {
        eprintln!("lodi: warning: {}", error.message);
    }
    let _ = writeln!(
        report,
        "next: review {}, commit it, then run lodi switch{} --dry-run",
        dir.display(),
        if home_only { " --home" } else { "" }
    );
    Ok(report)
}

/// The steps an import shows: `asked` is whether the host part asked to manage the host.
fn step_names(asked: Option<bool>, dry_run: bool, git: bool) -> Vec<&'static str> {
    let mut names = Vec::new();
    if let Some(asked) = asked {
        if asked {
            names.push("ask");
        }
        names.push("read the host");
        if asked {
            names.push("write the marker");
        }
        if !dry_run {
            names.push(RECORD);
            names.push("fill the lock");
        }
    }
    if !dry_run {
        names.push("write the config");
    }
    if git {
        names.push("git init");
    }
    names
}

fn step<T>(
    shown: &mut dyn progress::Sink,
    name: &str,
    run: impl FnOnce() -> Result<T, Diagnostic>,
) -> Result<T, Diagnostic> {
    shown.event(Event::Begin {
        name: name.to_string(),
        expected: None,
    });
    let done = run()?;
    shown.event(Event::Finish {
        count: None,
        detail: None,
    });
    Ok(done)
}

/// The host file this import writes, worked out in memory after the capture.
struct Planned {
    /// Its key from the root, and its text.
    key: String,
    text: String,
    /// Its text as it is, when it is imported again.
    before: Option<String>,
    /// The captured files to write beside it.
    bundle: import::files::Capture,
    /// Importing again: the entries added and removed, how many of the person's edits were kept
    /// and how many conflicts kept the file's side, and the `W_RECONCILE` lines.
    added: Vec<String>,
    removed: Vec<String>,
    kept: usize,
    conflicts: usize,
    warnings: Vec<String>,
}

impl Planned {
    fn of(target: &Target, user: &User, captured: &Captured) -> Result<Planned, Diagnostic> {
        let fresh = |key: &str| Planned {
            key: key.to_string(),
            text: captured.text.clone(),
            before: None,
            bundle: captured.bundle.clone(),
            added: Vec::new(),
            removed: Vec::new(),
            kept: 0,
            conflicts: 0,
            warnings: Vec::new(),
        };
        match &target.host {
            None | Some(Host::First) => Ok(fresh(HOST)),
            Some(Host::Second { key, .. } | Host::Listed(key)) => Ok(fresh(key)),
            Some(Host::Again(key)) => {
                let path = target.dir.join(key);
                let before = std::fs::read_to_string(&path).map_err(|e| {
                    Diagnostic::new("E_STORE_IO", format!("cannot read {}: {e}", path.display()))
                })?;
                let capture = import::files::Capture {
                    captured: (captured.bundle.captured.iter())
                        .filter(|file| !captured.keyrings.contains(&file.path))
                        .cloned()
                        .collect(),
                    ..Default::default()
                };
                let source = crate::hostscope::record_source(&user.root, &target.host_folder());
                let merged = again::merge(
                    &before,
                    &path,
                    &user.root,
                    &captured.machine,
                    &capture,
                    &captured.text,
                    &captured.last,
                    &source,
                )?;
                Ok(Planned {
                    key: key.clone(),
                    text: merged.text,
                    before: Some(before),
                    bundle: import::files::Capture {
                        captured: merged.declare,
                        ..Default::default()
                    },
                    added: merged.added,
                    removed: merged.removed,
                    kept: merged.kept,
                    conflicts: merged.conflicts,
                    warnings: merged.warnings,
                })
            }
        }
    }

    /// The folder holding the host file, where its `files/host/` goes.
    fn folder(&self, dir: &Path) -> PathBuf {
        dir.join(&self.key)
            .parent()
            .map_or_else(|| dir.to_path_buf(), Path::to_path_buf)
    }
}

/// What `--dry-run` shows: per file, whether it would be created or changed, and the merge.
fn preview(
    target: &Target,
    user: &User,
    home_text: Option<&str>,
    planned: Option<&Planned>,
    captured: Option<&Captured>,
) -> Vec<String> {
    let dir = &target.dir;
    let mut lines = Vec::new();
    if let Some(home) = &target.home {
        let path = dir.join(home).display().to_string();
        lines.push(match home_text {
            Some(_) => format!("would create {path}"),
            None => format!("would keep {path} as it is"),
        });
    }
    if let (Some(planned), Some(captured)) = (planned, captured) {
        let path = dir.join(&planned.key).display().to_string();
        match &planned.before {
            None => lines.push(format!(
                "would create {path}: {} package(s) declared, {} not captured",
                captured.declared, captured.not_captured
            )),
            Some(before) if *before == planned.text => {
                lines.push(format!("would keep {path}: nothing to merge"));
            }
            Some(_) => lines.push(format!(
                "would change {path}: {} added, {} removed",
                planned.added.len(),
                planned.removed.len()
            )),
        }
        lines.extend(planned.added.iter().map(|entry| format!("  + {entry}")));
        lines.extend(planned.removed.iter().map(|entry| format!("  - {entry}")));
        lines.extend(planned.warnings.iter().cloned());
        lines.extend(
            captured
                .unread
                .iter()
                .map(|path| format!("{path}: needs root to read")),
        );
        if !planned.bundle.captured.is_empty() {
            lines.push(format!(
                "would write {} file(s) under {}",
                planned.bundle.captured.len(),
                planned.folder(dir).join(HOST_FILES).display()
            ));
        }
    }
    if let Some(Host::Second { extend, .. }) = &target.host {
        let index = dir.join(super::INDEX).display().to_string();
        lines.push(if *extend {
            format!("would extend {index} with the host {}", user.hostname)
        } else {
            format!(
                "would create {index}, listing this host {} beside the first",
                user.hostname
            )
        });
    }
    lines.push("nothing was written (--dry-run)".to_string());
    lines
}

/// The run after every refusal passed: the names of what was written, or with `dry_run` the
/// preview. `host` is the host part: whether to write the marker, and how to run as root. Long
/// because each step undoes or warns about the one before it in place.
#[allow(clippy::too_many_arguments)]
fn steps(
    target: &Target,
    user: &User,
    host: Option<(bool, &Elevation)>,
    git: Option<&Path>,
    now: i64,
    dry_run: bool,
    shown: &mut dyn progress::Sink,
    report: &mut String,
) -> Result<Vec<String>, Diagnostic> {
    let person = user.home_or_root();
    let roots = crate::roots::Roots::for_host(person.clone(), target.dir.clone())
        .moved(user.xdg_config.as_deref(), user.data.as_deref());
    let home_text = target
        .home
        .as_ref()
        .filter(|_| !target.home_there)
        .map(|_| crate::home::import::template(&roots, &user.root));
    let home_only = host.is_none();
    let mut filled = None;
    let host = match host {
        None => None,
        Some((mark, elevation)) => Some(capture(target, mark, elevation, dry_run, shown)?),
    };
    let planned = match &host {
        Some(captured) => Some(Planned::of(target, user, captured)?),
        None => None,
    };
    if let Some(captured) = &host {
        for line in &captured.warnings {
            let _ = writeln!(report, "{line}");
        }
    }
    if dry_run {
        let shown = preview(
            target,
            user,
            home_text.as_deref(),
            planned.as_ref(),
            host.as_ref(),
        );
        return Ok(shown);
    }
    if let Some(planned) = &planned {
        for line in &planned.warnings {
            let _ = writeln!(report, "{line}");
        }
        filled = step(shown, "fill the lock", || {
            Ok(fill(
                target,
                user,
                planned,
                home_text.as_deref(),
                &person,
                now,
            ))
        })?;
    }
    let mut created: Vec<PathBuf> = Vec::new();
    let wrote = step(shown, "write the config", || {
        write(
            target,
            user,
            planned.as_ref(),
            home_text.as_deref(),
            filled.as_ref(),
            &roots,
            &mut created,
        )
    });
    let wrote = match wrote {
        Ok(wrote) => wrote,
        Err(error) => {
            for path in created.iter().rev() {
                let _ = if path.is_dir() {
                    std::fs::remove_dir_all(path)
                } else {
                    std::fs::remove_file(path)
                };
            }
            return Err(error);
        }
    };
    if let (Some(captured), Some(planned)) = (&host, &planned) {
        said(target, captured, planned, report);
    }
    if !home_only && filled.is_none() {
        eprintln!(
            "lodi: warning: {} was not filled; the first lodi switch fills it",
            target.dir.join(lock::FILE).display()
        );
    }
    if let Some(git) = git {
        step(shown, "git init", || {
            git_init(git, &target.dir, target.owner);
            Ok(())
        })?;
    } else if !target.missing.is_empty() {
        eprintln!(
            "lodi: warning: git is not on PATH, so {} is not in git; run git init there",
            target.dir.display()
        );
    }
    Ok(wrote)
}

/// What the import did to the host file, for the report.
fn said(target: &Target, captured: &Captured, planned: &Planned, report: &mut String) {
    let path = target.dir.join(&planned.key);
    let _ = match &planned.before {
        None => writeln!(
            report,
            "wrote {}: {} package(s) declared, {} not captured; read NOT CAPTURED in it first",
            path.display(),
            captured.declared,
            captured.not_captured
        ),
        Some(before) if *before == planned.text && planned.kept + planned.conflicts == 0 => {
            writeln!(report, "{} is unchanged: nothing to merge", path.display())
        }
        Some(before) if *before == planned.text => writeln!(
            report,
            "{} is unchanged: {} kept as you have it, {} conflict(s)",
            path.display(),
            planned.kept,
            planned.conflicts
        ),
        Some(_) => writeln!(
            report,
            "merged into {}: {} added, {} removed, {} kept as you have it, {} conflict(s)",
            path.display(),
            planned.added.len(),
            planned.removed.len(),
            planned.kept,
            planned.conflicts
        ),
    };
}

/// The host read as root: `mark` writes the marker too. The record names the host file's folder,
/// as a switch from it does (LD-515); a dry run reads here and records nothing.
fn capture(
    target: &Target,
    mark: bool,
    elevation: &Elevation,
    dry_run: bool,
    shown: &mut dyn progress::Sink,
) -> Result<Captured, Diagnostic> {
    let value = if dry_run {
        elevation.here(&Request::Import { mark, record: None }, shown)?
    } else {
        let record = Some(target.host_folder());
        elevation.run(&Request::Import { mark, record }, shown)?
    };
    Captured::from_json(&value).ok_or_else(|| {
        Diagnostic::new(
            "E_APPLY",
            "the host capture lodi ran as root came back unreadable",
        )
    })
}

/// The lock filled for what this import writes, or `None` with a warning when it cannot be. Only
/// the host and home written here are looked up; every other section, and every entry whose
/// request is unchanged, stays as it is.
fn fill(
    target: &Target,
    user: &User,
    planned: &Planned,
    home: Option<&str>,
    person: &Path,
    now: i64,
) -> Option<Lock> {
    let filled = (|| {
        let old = lock::read(&target.dir)?;
        let mut archive = Archive::new(&target.dir, Machine::of(&user.root)?, person, now);
        archive.with_text(&planned.key, planned.text.clone());
        if let (Some(key), Some(home)) = (&target.home, home) {
            archive.with_text(key, home.to_string());
        }
        let homes: Vec<&str> = target.home.iter().map(String::as_str).collect();
        lock::fill(&old, &[&planned.key], &homes, &mut archive)
    })();
    match filled {
        Ok(lock) => Some(lock),
        Err(error) => {
            eprintln!("lodi: warning: {}: {}", error.code, error.message);
            None
        }
    }
}

/// The writes, in order, each path this run creates recorded in `created` before it is made: a
/// file merged into is replaced by its final rename only, and `config.toml` comes last. Long
/// because the order of the writes is the guarantee, so they stay in one place.
fn write(
    target: &Target,
    user: &User,
    host: Option<&Planned>,
    home: Option<&str>,
    filled: Option<&Lock>,
    roots: &crate::roots::Roots,
    created: &mut Vec<PathBuf>,
) -> Result<Vec<String>, Diagnostic> {
    use crate::home::fsops::{self, RelPath};
    let failed = |path: &Path, e: io::Error| {
        Diagnostic::new(
            "E_STORE_IO",
            format!("cannot write {}: {e}", path.display()),
        )
    };
    let mut folders = target.missing.clone();
    if let Some(host) = host {
        let folder = host.folder(&target.dir);
        if folder.symlink_metadata().is_err() && !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    for dir in &folders {
        // `~/.config` at 0700, as the XDG base directory specification asks; the rest 0755.
        let private = user
            .home
            .as_ref()
            .is_some_and(|home| *dir == home.join(".config"));
        let mode = if private { 0o700 } else { 0o755 };
        std::fs::DirBuilder::new()
            .mode(mode)
            .create(dir)
            .map_err(|e| failed(dir, e))?;
        created.push(dir.clone());
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))
            .map_err(|e| failed(dir, e))?;
        give(dir, target.owner).map_err(|e| failed(dir, e))?;
    }
    let root = roots.config_root();
    let mut wrote = Vec::new();
    if let (Some(text), Some(key)) = (home, &target.home) {
        write_home(target, key, text, &root, created)?;
        wrote.push(key.clone());
    }
    let Some(host) = host else {
        return Ok(wrote);
    };
    let folder = host.folder(&target.dir);
    if !host.bundle.captured.is_empty() {
        for rel in ["files", "files/host"] {
            let at = folder.join(rel);
            if at.symlink_metadata().is_err() {
                created.push(at);
                break;
            }
        }
        host.bundle.write_bundle(&folder)?;
        give_all(&folder.join("files"), target.owner).map_err(|e| failed(&folder, e))?;
        let files = folder.join(HOST_FILES.trim_end_matches('/'));
        let files = files.strip_prefix(&target.dir).unwrap_or(&files);
        wrote.push(files.display().to_string());
    }
    // A lock that already holds what was filled is left as it is, and not named (LD-528).
    if let Some(filled) = filled
        && lock::read(&target.dir).ok().as_ref() != Some(filled)
    {
        let path = target.dir.join(lock::FILE);
        if path.symlink_metadata().is_err() {
            created.push(path);
        }
        lock::hold(&target.dir)?.write(filled, user)?;
        wrote.push(lock::FILE.to_string());
    }
    if host.before.as_deref() != Some(host.text.as_str()) {
        let path = target.dir.join(&host.key);
        if host.before.is_none() {
            created.push(path.clone());
        }
        fsops::write(
            &root,
            &RelPath::new(&host.key)?,
            host.text.as_bytes(),
            0o644,
        )?;
        give(&path, target.owner).map_err(|e| failed(&path, e))?;
        wrote.push(host.key.clone());
    }
    if let Some(Host::Second { index, extend, .. }) = &target.host {
        let path = target.dir.join(super::INDEX);
        if !extend {
            created.push(path.clone());
        }
        fsops::write(&root, &RelPath::new(super::INDEX)?, index.as_bytes(), 0o644)?;
        give(&path, target.owner).map_err(|e| failed(&path, e))?;
        wrote.push(super::INDEX.to_string());
    }
    Ok(wrote)
}

/// The home file `key` in the config, each folder and file it makes recorded in `created`.
fn write_home(
    target: &Target,
    key: &str,
    text: &str,
    root: &crate::roots::Root,
    created: &mut Vec<PathBuf>,
) -> Result<(), Diagnostic> {
    use crate::home::fsops::{self, RelPath};
    let path = target.dir.join(key);
    let mut rel = String::new();
    for part in Path::new(key)
        .parent()
        .into_iter()
        .flat_map(Path::components)
    {
        rel.push_str(&part.as_os_str().to_string_lossy());
        let at = target.dir.join(&rel);
        if at.symlink_metadata().is_err() {
            created.push(at);
        }
        rel.push('/');
    }
    created.push(path.clone());
    fsops::write(root, &RelPath::new(key)?, text.as_bytes(), 0o644)?;
    give(&path, target.owner).map_err(|e| {
        Diagnostic::new(
            "E_STORE_IO",
            format!("cannot write {}: {e}", path.display()),
        )
    })
}

/// Under `sudo`, `path` to the [`Target::owner`].
fn give(path: &Path, owner: Option<(u32, u32)>) -> io::Result<()> {
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
    }
    Ok(())
}

fn give_all(path: &Path, owner: Option<(u32, u32)>) -> io::Result<()> {
    if owner.is_none() {
        return Ok(());
    }
    give(path, owner)?;
    if path.symlink_metadata()?.is_dir() {
        for entry in std::fs::read_dir(path)? {
            give_all(&entry?.path(), owner)?;
        }
    }
    Ok(())
}

/// The first executable `name` on `PATH`.
fn program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|file| {
            std::fs::metadata(file)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// How long `git init` may take. It takes milliseconds; a git setting of the person's that
/// blocks (a FIFO at `~/.gitconfig`, say) would otherwise hang the import (LD-519).
const GIT_INIT_BOUND: std::time::Duration = std::time::Duration::from_secs(5);

/// `git init` on the folder this run created, under `sudo` as the [`Target::owner`], with the
/// person's own git settings (`init.defaultBranch`, say) and a bounded wait: one that fails or
/// does not end in [`GIT_INIT_BOUND`] warns once, and one that does not end is stopped and its
/// `.git` removed.
fn git_init(git: &Path, dir: &Path, owner: Option<(u32, u32)>) {
    let mut command = std::process::Command::new(git);
    command
        .args([OsStr::new("init"), OsStr::new("-q"), dir.as_os_str()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some((uid, gid)) = owner {
        command.uid(uid).gid(gid);
    }
    let warn = |why: &str| {
        eprintln!(
            "lodi: warning: git init {why} in {}, so it is not in git; run git init there",
            dir.display()
        );
    };
    let Ok(mut child) = command.spawn() else {
        return warn("did not start");
    };
    let deadline = std::time::Instant::now() + GIT_INIT_BOUND;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return,
            Ok(Some(_)) | Err(_) => return warn("did not succeed"),
            Ok(None) if std::time::Instant::now() >= deadline => break,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(dir.join(".git"));
    warn(&format!(
        "did not end within {} s (a git setting of yours may block, such as a FIFO)",
        GIT_INIT_BOUND.as_secs()
    ));
}
