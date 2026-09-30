//! The OS basics a host declares (sd-1, LD-420): `[system]`'s hostname, time zone, locale and
//! keymap, `[firewall]`'s one nftables table ([`super::firewall`]) and `[network]` through the
//! machine's own network stack ([`super::network`]).
//!
//! A basic the manifest does not declare and the record never held is never read or set, so a
//! host without these tables plans and applies exactly as before; a hostname is set only when
//! `[system]` names one. Each `[system]` basic is set with the distribution's own tool:
//! `hostnamectl set-hostname`, `timedatectl set-timezone`, `localectl set-locale` and `localectl
//! set-x11-keymap`, which sets the console keymap from the layout; where `/etc/default/keyboard`
//! holds the layout, as on Debian and Ubuntu, lodi sets its `XKBLAYOUT` line instead. The record
//! keeps what each was before lodi first set it, and an apply puts that back once the declaration
//! is gone, so `git revert` and an apply bring the earlier settings back.
//!
//! Each tool is resolved and run like a package manager ([`super::pm::Invocation`]): by bare
//! name, with the environment cleared, and every value after `--`. None of these tools has a root
//! of its own, so under `--root DIR` every call carries `--root=DIR` first: the test machine's
//! shims take it, and a real tool refuses it as an unknown option and changes nothing. A scratch
//! root never reaches the running system (`AGENTS.md` §8).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::diag::Diagnostic;

use super::lock::{BasicRecord, HostLock};
use super::manifest::HostManifest;
use super::pm::{self, Invocation, noninteractive_env};
use super::safety::{self, Gate};

/// The packages of the network stacks and firewall tools this module drives: exact mode never
/// removes one that is installed (sd-1).
pub const STACK: &[&str] = &[
    "nftables",
    "iptables",
    "netplan.io",
    "network-manager",
    "NetworkManager",
    "systemd-networkd",
    "ifupdown",
    "iproute2",
    "iproute",
];

/// What one basic's line does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Add,
    Set,
    Remove,
    Unchanged,
}

/// One file lodi writes, or removes when `bytes` is `None`, at an absolute path inside the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Write {
    pub path: String,
    pub bytes: Option<Vec<u8>>,
    pub mode: u32,
}

/// What an apply does for one basic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    None,
    /// Run `first`, write the files, then run `then`.
    Run {
        first: Vec<Invocation>,
        writes: Vec<Write>,
        then: Vec<Invocation>,
    },
    /// A staged network change ([`super::network::perform`]).
    Network(Box<super::network::Stage>),
    /// A loader switch or one of its settings ([`super::boot::perform`]).
    Boot(Box<super::boot::Stage>),
    /// A trial boot entry change ([`super::trial::perform`]): write, run, then write `after`.
    Trial {
        writes: Vec<Write>,
        run: Vec<Invocation>,
        after: Vec<Write>,
    },
}

/// One declared basic, or one whose declaration is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasicAction {
    pub key: &'static str,
    pub change: Change,
    /// What the line names after the key: `OLD -> NEW`, `table inet lodi`, the interfaces.
    pub what: String,
    /// What the line says in parentheses: the command, and for the network its confirmation.
    pub detail: String,
    pub step: Step,
}

impl BasicAction {
    pub fn line(&self) -> String {
        let verb = match self.change {
            Change::Add => "+",
            Change::Set => "~",
            Change::Remove => "-",
            Change::Unchanged => return format!("= {} {}", self.key, self.what),
        };
        format!("{verb} {} {} ({})", self.key, self.what, self.detail)
    }

    pub fn kind_name(&self) -> &'static str {
        match self.key {
            "hostname" => "basic.hostname",
            "timezone" => "basic.timezone",
            "locale" => "basic.locale",
            "keymap" => "basic.keymap",
            "firewall" => "basic.firewall",
            "parameters" | "modules" | "blacklist" => "basic.kernel",
            "sysctl" => "basic.sysctl",
            "loader" | "timeout" | "default" | "cmdline" => "basic.boot",
            _ => "basic.network",
        }
    }

    /// Every command the action runs, for the journal.
    pub fn invocations(&self) -> Vec<&Invocation> {
        match &self.step {
            Step::None => Vec::new(),
            Step::Run { first, then, .. } => first.iter().chain(then).collect(),
            Step::Network(stage) => stage.invocations(),
            Step::Boot(stage) => stage.invocations(),
            Step::Trial { run, .. } => run.iter().collect(),
        }
    }
}

/// One basics tool, resolved for one root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
    name: &'static str,
    pub path: PathBuf,
    root: Option<String>,
}

impl Tool {
    /// The tool, or `E_NO_RUNTIME` naming what needs it.
    pub fn find(gate: &Gate, name: &'static str, needs: &str) -> Result<Tool, Diagnostic> {
        Tool::find_optional(gate, name)?.ok_or_else(|| {
            Diagnostic::new(
                "E_NO_RUNTIME",
                format!(
                    "`{name}` is not on {}, and host.toml declares {needs}",
                    safety::searched(gate.system_root)
                ),
            )
            .hint(
                "install the package that ships it, apply that, then apply again; nothing was \
                 changed",
            )
        })
    }

    pub fn find_optional(gate: &Gate, name: &'static str) -> Result<Option<Tool>, Diagnostic> {
        match safety::resolve_program(name, gate.system_root, gate.euid) {
            Ok(Some(path)) => Ok(Some(Tool {
                name,
                path,
                root: (!gate.system_root).then(|| format!("--root={}", gate.root.display())),
            })),
            Ok(None) => Ok(None),
            Err(why) => Err(safety::untrusted_runtime(name, &why)),
        }
    }

    /// `TOOL [--root=DIR] ARGS...`.
    pub fn invocation(&self, args: &[&str]) -> Invocation {
        let argv: Vec<String> = self
            .root
            .iter()
            .cloned()
            .chain(args.iter().map(|arg| (*arg).to_string()))
            .collect();
        Invocation::resolved(self.name, &self.path, &argv, noninteractive_env())
    }

    /// Run a reading call: `Some(stdout)` when it succeeds, `None` when it exits non-zero.
    pub fn read(&self, args: &[&str]) -> Result<Option<String>, Diagnostic> {
        let invocation = self.invocation(args);
        let output = invocation.command().output().map_err(|e| {
            Diagnostic::new(
                "E_APPLY",
                format!("cannot run {}: {e}", invocation.path.display()),
            )
        })?;
        Ok(output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned()))
    }
}

/// Plan every basic, in apply order: the `[system]` four, the kernel's settings
/// ([`super::kernel`]), the loader ([`super::boot`]), the firewall, the network. Returns the
/// record a converged apply writes.
pub fn plan(
    gate: &Gate,
    manifest: &HostManifest,
    lock: Option<&HostLock>,
    out: &mut Vec<BasicAction>,
    notes: &mut Vec<String>,
) -> Result<BTreeMap<String, BasicRecord>, Diagnostic> {
    let mut record = BTreeMap::new();
    plan_system(gate, manifest, lock, out, &mut record)?;
    super::kernel::plan(gate, manifest, lock, out, &mut record, notes)?;
    super::boot::plan(gate, manifest, lock, out, &mut record, notes)?;
    for part in [super::firewall::plan, super::network::plan] {
        if let Some((action, kept)) = part(gate, manifest, lock)? {
            if let Some(kept) = kept {
                record.insert(action.key.to_string(), kept);
            }
            out.push(action);
        }
    }
    Ok(record)
}

/// `localectl status`, read once: the locale's variables and the keyboard layout.
struct Localectl {
    tool: Tool,
    vars: Vec<String>,
    layout: String,
}

impl Localectl {
    fn read(gate: &Gate) -> Result<Localectl, Diagnostic> {
        let tool = Tool::find(gate, "localectl", "[system] locale or keymap")?;
        let text = tool.read(&["status"])?.unwrap_or_default();
        let (mut vars, mut layout, mut in_locale) = (Vec::new(), String::new(), false);
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(value) = trimmed.strip_prefix("System Locale:") {
                in_locale = true;
                vars.push(value.trim().to_string());
            } else if in_locale && !trimmed.contains(':') && trimmed.contains('=') {
                vars.push(trimmed.to_string());
            } else {
                in_locale = false;
                if let Some(value) = trimmed.strip_prefix("X11 Layout:") {
                    layout = value.trim().to_string();
                }
            }
        }
        if layout == "(unset)" {
            layout.clear();
        }
        vars.retain(|var| var.contains('='));
        Ok(Localectl { tool, vars, layout })
    }

    fn lang(&self) -> String {
        self.vars
            .iter()
            .find_map(|var| var.strip_prefix("LANG="))
            .unwrap_or("")
            .to_string()
    }
}

/// Where Debian and Ubuntu keep the keyboard layout.
const KEYBOARD: &str = "/etc/default/keyboard";

/// The keyboard file with its `XKBLAYOUT` line set to `layout`, every other line as it was.
fn with_layout(text: &[u8], layout: &str) -> Vec<u8> {
    let line = format!("XKBLAYOUT=\"{layout}\"");
    let text = String::from_utf8_lossy(text);
    let mut lines: Vec<&str> = text.lines().collect();
    match lines.iter().position(|l| l.starts_with("XKBLAYOUT=")) {
        Some(at) => lines[at] = &line,
        None => lines.push(&line),
    }
    format!("{}\n", lines.join("\n")).into_bytes()
}

/// The time zone `etc/localtime` points at, or empty.
fn zone_of(root: &Path) -> String {
    std::fs::read_link(root.join("etc/localtime"))
        .ok()
        .and_then(|target| {
            let target = target.display().to_string();
            target
                .split_once("zoneinfo/")
                .map(|(_, zone)| zone.to_string())
        })
        .unwrap_or_default()
}

/// The value of the first `name=` line in the file at `path`, unquoted, or empty.
fn assignment(gate: &Gate, path: &str, name: &str) -> String {
    let text = read(gate, path).unwrap_or_default();
    String::from_utf8_lossy(&text)
        .lines()
        .find_map(|line| line.trim().strip_prefix(name)?.strip_prefix('='))
        .map(|value| {
            value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string()
        })
        .unwrap_or_default()
}

/// The `LANG` localed keeps in its file, as the import reads it, or empty. A plan reads the
/// current locale here first too, so a locale read back from this machine is never a change
/// (#583): Debian's, Ubuntu's and Arch's `localectl status` may name no LANG at all.
fn file_locale(gate: &Gate) -> String {
    ["/etc/locale.conf", "/etc/default/locale"]
        .iter()
        .map(|path| assignment(gate, path, "LANG"))
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

/// A locale name with its codeset spelled one way: `C.UTF-8`, `C.utf8` and `C.utf-8` are one.
fn locale_key(name: &str) -> String {
    match name.trim().split_once('.') {
        Some((lang, rest)) => {
            let (codeset, modifier) = match rest.split_once('@') {
                Some((codeset, modifier)) => (codeset, format!("@{modifier}")),
                None => (rest, String::new()),
            };
            format!(
                "{lang}.{}{modifier}",
                codeset.replace('-', "").to_lowercase()
            )
        }
        None => name.trim().to_string(),
    }
}

/// Whether `target` is among the locales `localectl list-locales` printed. C and POSIX are on
/// every machine, whatever the listing says.
fn locale_known(listing: &str, target: &str) -> bool {
    let target = locale_key(target);
    ["C", "POSIX", "C.utf8"]
        .into_iter()
        .chain(listing.lines())
        .any(|line| locale_key(line) == target)
}

/// The basics this machine has, as `[system]` declares them, for the import. They are read
/// from the files localed and hostnamed keep them in, and no program is run; a basic the
/// machine has no reading of is left out.
pub fn readings(gate: &Gate) -> Vec<(&'static str, String)> {
    let locale = file_locale(gate);
    let keymap = file_keymap(gate);
    // A plan reads the locale and the keymap through `localectl`: without it, neither is written.
    let (locale, keymap) = match Tool::find_optional(gate, "localectl") {
        Ok(Some(_)) => (locale, keymap),
        _ => (String::new(), String::new()),
    };
    [
        ("hostname", current(gate, "hostname", None)),
        ("timezone", current(gate, "timezone", None)),
        ("locale", locale),
        ("keymap", keymap),
    ]
    .into_iter()
    .filter(|(key, value)| super::manifest::is_basic(key, value))
    .collect()
}

/// Where localed keeps the X11 layout on Arch and Fedora.
const X11_KEYBOARD: &str = "/etc/X11/xorg.conf.d/00-keyboard.conf";

/// The first keyboard layout the files localed keeps name, as the import reads it, or empty. A
/// plan reads the current keymap here first too, so a layout read back from this machine is
/// never a change (#585): the Debian and Ubuntu guests' `localectl status` may name none while
/// `/etc/default/keyboard` holds it.
fn file_keymap(gate: &Gate) -> String {
    let mut keymap = assignment(gate, KEYBOARD, "XKBLAYOUT");
    if keymap.is_empty() {
        let text = read(gate, X11_KEYBOARD).unwrap_or_default();
        keymap = String::from_utf8_lossy(&text)
            .lines()
            .find_map(|line| {
                let words: Vec<&str> = line.split_whitespace().collect();
                (words.len() == 3 && words[0] == "Option" && words[1] == "\"XkbLayout\"")
                    .then(|| words[2].trim_matches('"').to_string())
            })
            .unwrap_or_default();
    }
    keymap.split(',').next().unwrap_or("").to_string()
}

/// The basics record an import writes for what it declares: each declared basic the machine
/// has, as the record a converged apply writes holds it, so a plan of the machine it came from
/// is `nothing to do` and never a record-only update (#585). What `previous` recorded is kept:
/// a basic's `was` stays what lodi first found, and one no longer declared stays known.
pub fn record_as_read<'a>(
    declared: impl IntoIterator<Item = (&'a str, Option<&'a String>)>,
    readings: &[(&'static str, String)],
    previous: &BTreeMap<String, BasicRecord>,
) -> BTreeMap<String, BasicRecord> {
    let mut record = previous.clone();
    for (key, want) in declared {
        let Some(want) = want else { continue };
        if !readings.iter().any(|(k, v)| *k == key && v == want) {
            continue;
        }
        let was = match previous.get(key) {
            Some(before) => before.was.clone(),
            None => Some(want.clone()),
        };
        record.insert(
            key.to_string(),
            BasicRecord {
                value: want.clone(),
                was,
            },
        );
    }
    record
}

/// What the machine has for one basic, or empty.
fn current(gate: &Gate, key: &str, localectl: Option<&Localectl>) -> String {
    match key {
        "hostname" => std::fs::read_to_string(gate.root.join("etc/hostname"))
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string(),
        "timezone" => zone_of(&gate.root),
        "locale" => match file_locale(gate) {
            file if file.is_empty() => localectl.map(Localectl::lang).unwrap_or_default(),
            file => file,
        },
        _ => match file_keymap(gate) {
            file if file.is_empty() => localectl.map(|l| l.layout.clone()).unwrap_or_default(),
            file => file,
        },
    }
}

fn plan_system(
    gate: &Gate,
    manifest: &HostManifest,
    lock: Option<&HostLock>,
    out: &mut Vec<BasicAction>,
    record: &mut BTreeMap<String, BasicRecord>,
) -> Result<(), Diagnostic> {
    let mut localectl: Option<Localectl> = None;
    let mut problems = Vec::new();
    for (key, declared) in manifest.system.each() {
        let before = lock.and_then(|lock| lock.basics.get(key));
        if declared.is_none() && before.is_none() {
            continue;
        }
        if matches!(key, "locale" | "keymap") && localectl.is_none() {
            localectl = Some(Localectl::read(gate)?);
        }
        let current = current(gate, key, localectl.as_ref());
        let target = match declared {
            Some(want) => {
                let was = match before {
                    Some(before) => before.was.clone(),
                    None => super::manifest::is_basic(key, &current).then(|| current.clone()),
                };
                record.insert(
                    key.to_string(),
                    BasicRecord {
                        value: want.clone(),
                        was,
                    },
                );
                want.clone()
            }
            // Gone from the manifest: back to what it was, if lodi knows.
            None => match before.and_then(|before| before.was.clone()) {
                Some(was) => was,
                None => continue,
            },
        };
        let shown = |value: &str| match (key, value) {
            (_, "") => "(unset)".to_string(),
            ("locale", value) => format!("LANG={value}"),
            (_, value) => value.to_string(),
        };
        // An unchanged basic is no plan line, as an unchanged package is none (#585); the
        // record above still holds it.
        if current == target {
            continue;
        }
        let invocation = match key {
            "hostname" => Tool::find(gate, "hostnamectl", "[system] hostname")?.invocation(&[
                "set-hostname",
                "--",
                &target,
            ]),
            "timezone" => {
                if !gate.root.join("usr/share/zoneinfo").join(&target).is_file() {
                    problems.push(format!("no time zone {target} in /usr/share/zoneinfo"));
                }
                Tool::find(gate, "timedatectl", "[system] timezone")?.invocation(&[
                    "set-timezone",
                    "--",
                    &target,
                ])
            }
            "locale" => {
                let localectl = localectl.as_ref().expect("read above");
                let known = localectl.tool.read(&["list-locales"])?.unwrap_or_default();
                if !locale_known(&known, &target) {
                    problems.push(format!("the locale {target} is not on this machine"));
                }
                let lang = format!("LANG={target}");
                let mut args = vec!["set-locale", "--", lang.as_str()];
                args.extend(
                    localectl
                        .vars
                        .iter()
                        .filter(|var| !var.starts_with("LANG="))
                        .map(String::as_str),
                );
                localectl.tool.invocation(&args)
            }
            _ => match read(gate, KEYBOARD) {
                // Debian's localed keeps the layout here, and Ubuntu's refuses to set it.
                Some(text) => {
                    out.push(BasicAction {
                        key,
                        change: Change::Set,
                        what: format!("{} -> {}", shown(&current), shown(&target)),
                        detail: format!("XKBLAYOUT in {KEYBOARD}"),
                        step: Step::Run {
                            first: Vec::new(),
                            writes: vec![Write {
                                path: KEYBOARD.to_string(),
                                bytes: Some(with_layout(&text, &target)),
                                mode: 0o644,
                            }],
                            then: Vec::new(),
                        },
                    });
                    continue;
                }
                None => localectl.as_ref().expect("read above").tool.invocation(&[
                    "set-x11-keymap",
                    "--",
                    &target,
                ]),
            },
        };
        out.push(BasicAction {
            key,
            change: Change::Set,
            what: format!("{} -> {}", shown(&current), shown(&target)),
            detail: invocation.command_line(),
            step: Step::Run {
                first: Vec::new(),
                writes: Vec::new(),
                then: vec![invocation],
            },
        });
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(Diagnostic::new("E_UNKNOWN_SETTING", problems.join("; "))
        .hint("nothing changed; correct the name, or install what provides it"))
}

/// Carry out one basic's step.
pub fn perform(gate: &Gate, action: &BasicAction) -> Result<(), Diagnostic> {
    match &action.step {
        Step::None => Ok(()),
        Step::Run {
            first,
            writes,
            then,
        } => {
            first.iter().try_for_each(pm::run)?;
            for write in writes {
                apply_write(gate, write)?;
            }
            then.iter().try_for_each(pm::run)
        }
        Step::Network(stage) => super::network::perform(gate, stage),
        Step::Boot(stage) => super::boot::perform(gate, stage),
        Step::Trial { .. } => super::trial::perform(gate, &action.step),
    }
}

/// Write one of lodi's own files, or remove it.
pub fn apply_write(gate: &Gate, write: &Write) -> Result<(), Diagnostic> {
    match &write.bytes {
        Some(bytes) => super::files::write_owned(&gate.root, &write.path, bytes, write.mode),
        None if gate.root.join(&write.path[1..]).exists() => {
            super::files::remove(&gate.root, &write.path)
        }
        None => Ok(()),
    }
}

/// The bytes of one of lodi's own files as the machine has it, or `None`.
pub fn read(gate: &Gate, path: &str) -> Option<Vec<u8>> {
    std::fs::read(gate.root.join(&path[1..])).ok()
}
