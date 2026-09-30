//! `[kernel]` and `[sysctl]` (bc-1, LD-423): kernel parameters for the next boot, modules loaded
//! now and at boot, modules kept from loading, and sysctl values set now and at boot.
//!
//! Each lives where the distribution looks for it, in a place that is lodi's alone. The
//! parameters are one line of lodi's own at the end of `/etc/default/grub`, which adds them to
//! `GRUB_CMDLINE_LINUX`, and `grub-mkconfig` writes the loader's configuration from it; on
//! Fedora, whose `grub2-mkconfig` no longer rewrites its boot entries, `grubby` adds (and takes
//! out) the words in every entry and in `/etc/kernel/cmdline`, which a later kernel gets its
//! command line from. The modules are `/etc/modules-load.d/lodi.conf`,
//! each loaded now with `modprobe`; the blacklist is `/etc/modprobe.d/lodi-blacklist.conf`; the
//! sysctl values are `/etc/sysctl.d/90-lodi.conf`, each set now with `sysctl -w`.
//!
//! Taking a declaration out removes lodi's line or file and nothing else, so the distribution's
//! own settings are what is left. A sysctl value is also set back now to what it was before lodi
//! first set it, which the record keeps (`sysctl.NAME`); the record also keeps the parameters the
//! loader was last written with (`kernel.parameters`), so a loader write that never finished is
//! done again. Nothing is read or run for a table that is not declared and left nothing behind,
//! so a host without these tables plans and applies exactly as before.

use std::collections::{BTreeMap, BTreeSet};

use crate::diag::Diagnostic;

use super::basics::{BasicAction, Change, Step, Tool, Write};
use super::lock::{BasicRecord, HostLock};
use super::manifest::{HostManifest, Loader};
use super::pm::Invocation;
use super::safety::{Distro, Gate};

const GRUB: &str = "/etc/default/grub";
/// What ends lodi's one line in the GRUB file.
const MARK: &str = "# lodi: host.toml [kernel] parameters";
const PREFIX: &str = "GRUB_CMDLINE_LINUX=\"$GRUB_CMDLINE_LINUX ";
const MODULES: &str = "/etc/modules-load.d/lodi.conf";
const BLACKLIST: &str = "/etc/modprobe.d/lodi-blacklist.conf";
const SYSCTL: &str = "/etc/sysctl.d/90-lodi.conf";
/// The record's key of the parameters, and the prefix of each sysctl value's.
pub const PARAMETERS_KEY: &str = "kernel.parameters";
pub const SYSCTL_KEY: &str = "sysctl.";

/// The bootloader packages exact mode never removes while installed (bc-1); the kernels
/// themselves the baseline keeps.
pub const LOADERS: &[&str] = &[
    "grub-common",
    "grub2-common",
    "grub-efi-amd64",
    "grub-efi-amd64-bin",
    "grub-efi-amd64-signed",
    "grub-efi-amd64-unsigned",
    "grub-pc",
    "grub-pc-bin",
    "grub",
    "grub2-efi-x64",
    "grub2-pc",
    "grub2-tools",
    "grub2-tools-minimal",
    "shim-signed",
    "shim-x64",
    "shim",
    "efibootmgr",
    "systemd-boot",
    "systemd-boot-efi",
    "systemd-boot-unsigned",
];

/// Plan the kernel's settings, in apply order: parameters, modules, blacklist, sysctl.
pub fn plan(
    gate: &Gate,
    manifest: &HostManifest,
    lock: Option<&HostLock>,
    out: &mut Vec<BasicAction>,
    record: &mut BTreeMap<String, BasicRecord>,
    notes: &mut Vec<String>,
) -> Result<(), Diagnostic> {
    let recorded = |key: &str| lock.and_then(|lock| lock.basics.get(key));
    let declared = &manifest.kernel.parameters;
    if manifest
        .boot
        .as_ref()
        .is_some_and(|boot| boot.loader == Loader::SystemdBoot)
    {
        // systemd-boot boots them, from `/etc/kernel/cmdline` ([`super::boot`]); GRUB, now the
        // fallback, keeps the command line it booted (bv-1).
        if !declared.is_empty() {
            record.insert(
                PARAMETERS_KEY.to_string(),
                BasicRecord {
                    value: declared.join(" "),
                    was: None,
                },
            );
        }
    } else {
        out.extend(parameters(
            gate,
            declared,
            recorded(PARAMETERS_KEY),
            record,
            notes,
        )?);
    }
    out.extend(modules(gate, &manifest.kernel.modules)?);
    out.extend(listed(
        "blacklist",
        BLACKLIST,
        gate,
        &manifest.kernel.blacklist,
        |name| format!("blacklist {name}"),
    ));
    sysctl(gate, manifest, lock, out, record)
}

/// `OLD -> NEW`, or `NEW` alone when nothing was there or it is the same.
fn what(old: &str, new: &str) -> String {
    if old.is_empty() || old == new {
        new.to_string()
    } else {
        format!("{old} -> {new}")
    }
}

/// The parameters lodi's line in the GRUB file holds, if it has one.
fn parameters_in(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.strip_suffix(MARK)?.trim_end();
        Some(line.strip_prefix(PREFIX)?.strip_suffix('"')?.to_string())
    })
}

/// The GRUB file with lodi's line set to `parameters`, or taken out; every other line as it was.
fn with_parameters(text: &str, parameters: Option<&str>) -> Vec<u8> {
    let mut out: String = text
        .split_inclusive('\n')
        .filter(|line| !line.trim_end().ends_with(MARK))
        .collect();
    if let Some(parameters) = parameters {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("{PREFIX}{parameters}\" {MARK}\n"));
    }
    out.into_bytes()
}

/// The GRUB file with lodi's line holding `parameters` (bv-1's confirmation).
pub fn grub_write(gate: &Gate, parameters: &str) -> Result<Write, Diagnostic> {
    let text = super::basics::read(gate, GRUB).ok_or_else(|| {
        Diagnostic::new(
            "E_BOOT_LOADER",
            format!("no {GRUB} to set [kernel] parameters in"),
        )
        .hint("lodi sets kernel parameters through GRUB; nothing was changed")
    })?;
    Ok(Write {
        path: GRUB.to_string(),
        bytes: Some(with_parameters(
            &String::from_utf8_lossy(&text),
            Some(parameters),
        )),
        mode: 0o644,
    })
}

/// The parameters of lodi's line in the GRUB file: those the default boots with.
pub fn confirmed(gate: &Gate) -> String {
    super::basics::read(gate, GRUB)
        .and_then(|text| parameters_in(&String::from_utf8_lossy(&text)))
        .unwrap_or_default()
}

/// The calls that make the default boot `want` in place of `old`: [`loader`]'s, and on Fedora,
/// where those are `grubby`'s, [`mkconfig`] after them for lodi's entries.
pub fn default_calls(gate: &Gate, old: &str, want: &str) -> Result<Vec<Invocation>, Diagnostic> {
    let mut calls = loader(gate, old, want)?;
    if gate.distro == Distro::Fedora {
        calls.push(mkconfig(gate, "[kernel] parameters")?);
    }
    Ok(calls)
}

/// The loader's own calls that bring the next boot's command line from `old` to `want`: on
/// Fedora `grubby` adds `want` and takes out what of `old` is not in it, told to leave the GRUB
/// file (lodi's line in it) alone, elsewhere `grub-mkconfig` writes the configuration from it.
fn loader(gate: &Gate, old: &str, want: &str) -> Result<Vec<Invocation>, Diagnostic> {
    if gate.distro == Distro::Fedora {
        let tool = Tool::find(gate, "grubby", "[kernel] parameters")?;
        let wanted: Vec<&str> = want.split_whitespace().collect();
        let gone: Vec<&str> = old
            .split_whitespace()
            .filter(|word| !wanted.contains(word))
            .collect();
        let calls = [("--args", wanted), ("--remove-args", gone)];
        return Ok(calls
            .into_iter()
            .filter(|(_, words)| !words.is_empty())
            .map(|(flag, words)| {
                let words = format!("{flag}={}", words.join(" "));
                // Last: Fedora 44's grubby refuses the flag before `--update-kernel`.
                let args = [
                    "--update-kernel=ALL",
                    words.as_str(),
                    "--no-etc-grub-update",
                ];
                tool.invocation(&args)
            })
            .collect());
    }
    Ok(vec![mkconfig(gate, "[kernel] parameters")?])
}

/// The loader's own tool that writes GRUB's configuration from the GRUB file, with lodi's entries
/// from `/etc/grub.d`; `needs` names the table that needs it.
pub fn mkconfig(gate: &Gate, needs: &str) -> Result<Invocation, Diagnostic> {
    let (name, config) = if gate.distro == Distro::Fedora {
        ("grub2-mkconfig", "boot/grub2/grub.cfg")
    } else {
        ("grub-mkconfig", "boot/grub/grub.cfg")
    };
    let tool = Tool::find(gate, name, needs)?;
    let config = gate.root.join(config).display().to_string();
    Ok(tool.invocation(&["-o", config.as_str()]))
}

/// A change of the parameters boots once as a trial first ([`super::trial`]); their removal
/// is the distribution's own command line again, with the confirmed entry kept as a fallback.
fn parameters(
    gate: &Gate,
    declared: &[String],
    recorded: Option<&BasicRecord>,
    record: &mut BTreeMap<String, BasicRecord>,
    notes: &mut Vec<String>,
) -> Result<Option<BasicAction>, Diagnostic> {
    let text = super::basics::read(gate, GRUB).map(|t| String::from_utf8_lossy(&t).into_owned());
    let line = text.as_deref().and_then(parameters_in);
    let trial = super::trial::Trial::read(gate).filter(|trial| !trial.is_systemd_boot());
    if declared.is_empty() && recorded.is_none() && line.is_none() && trial.is_none() {
        return Ok(None);
    }
    let want = declared.join(" ");
    let Some(text) = text else {
        if declared.is_empty() {
            return Ok(None);
        }
        return Err(Diagnostic::new(
            "E_BOOT_LOADER",
            format!("no {GRUB} to set [kernel] parameters in"),
        )
        .hint("lodi sets kernel parameters through GRUB; nothing was changed"));
    };
    let status = trial.as_ref().map(|trial| (trial, trial.status(gate)));
    let old = recorded
        .map(|r| r.value.clone())
        .or(line.clone())
        .unwrap_or_default();
    let grub = |parameters: Option<&str>| Write {
        path: GRUB.to_string(),
        bytes: Some(with_parameters(&text, parameters)),
        mode: 0o644,
    };
    if declared.is_empty() {
        if let Some((trial, super::trial::Status::Reverted)) = status {
            notes.push(trial.note(super::trial::Status::Reverted));
        }
        // The default changes only when lodi's line held parameters: keep that entry.
        let calls = default_calls(gate, line.as_deref().unwrap_or_default(), "")?;
        let step = super::trial::new_default(gate, grub(None), line.is_some(), calls)?;
        let detail = match line {
            Some(_) => format!(
                "the distribution's own at the next boot, and the entry before it as {}",
                super::trial::KNOWN_GOOD
            ),
            None => "the distribution's own at the next boot".to_string(),
        };
        return Ok(Some(BasicAction {
            key: "parameters",
            change: Change::Remove,
            what: line.unwrap_or(old),
            detail,
            step,
        }));
    }
    record.insert(
        PARAMETERS_KEY.to_string(),
        BasicRecord {
            value: want.clone(),
            was: None,
        },
    );
    let change = if recorded.is_none() && line.is_none() {
        Change::Add
    } else {
        Change::Set
    };
    if line.as_deref() == Some(want.as_str()) {
        // The default boots with them: confirmed. A record that lost them writes them again.
        if recorded.map(|r| r.value.as_str()) == Some(want.as_str()) && trial.is_none() {
            return Ok(None);
        }
        let step = super::trial::new_default(
            gate,
            grub(Some(&want)),
            false,
            default_calls(gate, &old, &want)?,
        )?;
        return Ok(Some(BasicAction {
            key: "parameters",
            change,
            what: want,
            detail: "the default entry".to_string(),
            step,
        }));
    }
    match status {
        Some((trial, now)) if trial.parameters == want && now != super::trial::Status::Reverted => {
            notes.push(trial.note(now));
            return Ok(None);
        }
        Some((trial, super::trial::Status::Reverted)) => {
            notes.push(trial.note(super::trial::Status::Reverted));
        }
        _ => {}
    }
    let confirmed = line.unwrap_or_default();
    let (detail, step, trial) = super::trial::trial(
        gate,
        &confirmed,
        &want,
        mkconfig(gate, "[kernel] parameters")?,
    )?;
    notes.push(trial.note(super::trial::Status::Pending));
    Ok(Some(BasicAction {
        key: "parameters",
        change,
        what: what(&old, &want),
        detail,
        step,
    }))
}

/// One of lodi's list files: its bytes for `names`, `None` for none.
fn list_file(table: &str, names: &[String], line: impl Fn(&str) -> String) -> Option<Vec<u8>> {
    if names.is_empty() {
        return None;
    }
    let mut text = format!("# Written by lodi from host.toml [kernel] {table}.\n");
    for name in names {
        text.push_str(&line(name));
        text.push('\n');
    }
    Some(text.into_bytes())
}

/// A list file's action, when its bytes are not as declared.
fn listed(
    key: &'static str,
    path: &str,
    gate: &Gate,
    names: &[String],
    line: impl Fn(&str) -> String,
) -> Option<BasicAction> {
    let now = super::basics::read(gate, path);
    let want = list_file(key, names, line);
    if now == want {
        return None;
    }
    let old: Vec<String> = now
        .as_deref()
        .map(String::from_utf8_lossy)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| l.trim().trim_start_matches("blacklist ").to_string())
        .collect();
    let (change, shown) = match (&now, &want) {
        (None, _) => (Change::Add, names.join(" ")),
        (_, None) => (Change::Remove, old.join(" ")),
        _ => (Change::Set, what(&old.join(" "), &names.join(" "))),
    };
    Some(BasicAction {
        key,
        change,
        what: shown,
        detail: path.to_string(),
        step: Step::Run {
            first: Vec::new(),
            writes: vec![Write {
                path: path.to_string(),
                bytes: want,
                mode: 0o644,
            }],
            then: Vec::new(),
        },
    })
}

/// The modules: lodi's file, and a `modprobe` of each declared one not loaded now.
fn modules(gate: &Gate, names: &[String]) -> Result<Option<BasicAction>, Diagnostic> {
    let loaded = |name: &String| {
        gate.root
            .join("sys/module")
            .join(name.replace('-', "_"))
            .exists()
    };
    let unloaded: Vec<&String> = names.iter().filter(|name| !loaded(name)).collect();
    let action = listed("modules", MODULES, gate, names, str::to_string);
    if unloaded.is_empty() {
        return Ok(action);
    }
    let modprobe = Tool::find(gate, "modprobe", "[kernel] modules")?;
    let loads: Vec<Invocation> = unloaded
        .iter()
        .map(|name| modprobe.invocation(&["--", name]))
        .collect();
    let detail = loads
        .iter()
        .map(Invocation::command_line)
        .collect::<Vec<_>>()
        .join("; ");
    Ok(Some(match action {
        Some(mut action) => {
            if let Step::Run { then, .. } = &mut action.step {
                *then = loads;
            }
            action.detail = format!("{MODULES}; {detail}");
            action
        }
        // The file is as declared, and a module is not loaded: load it now.
        None => BasicAction {
            key: "modules",
            change: Change::Set,
            what: names.join(" "),
            detail,
            step: Step::Run {
                first: Vec::new(),
                writes: Vec::new(),
                then: loads,
            },
        },
    }))
}

/// A sysctl value as the kernel prints it, its words separated by single spaces.
fn live(gate: &Gate, name: &str) -> Option<String> {
    let path = gate.root.join("proc/sys").join(name.replace('.', "/"));
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn sysctl(
    gate: &Gate,
    manifest: &HostManifest,
    lock: Option<&HostLock>,
    out: &mut Vec<BasicAction>,
    record: &mut BTreeMap<String, BasicRecord>,
) -> Result<(), Diagnostic> {
    let recorded: BTreeMap<&str, &BasicRecord> = lock
        .map(|lock| {
            lock.basics
                .iter()
                .filter_map(|(key, r)| Some((key.strip_prefix(SYSCTL_KEY)?, r)))
                .collect()
        })
        .unwrap_or_default();
    let now = super::basics::read(gate, SYSCTL);
    let want = (!manifest.sysctl.is_empty()).then(|| {
        let mut text =
            "# Written by lodi from host.toml [sysctl]: read at every boot.\n".to_string();
        for (name, value) in &manifest.sysctl {
            text.push_str(&format!("{name} = {value}\n"));
        }
        text.into_bytes()
    });
    if want.is_none() && now.is_none() && recorded.is_empty() {
        return Ok(());
    }
    let file = Write {
        path: SYSCTL.to_string(),
        bytes: want.clone(),
        mode: 0o644,
    };
    let names: BTreeSet<&str> = manifest
        .sysctl
        .keys()
        .map(String::as_str)
        .chain(recorded.keys().copied())
        .collect();
    let mut problems = Vec::new();
    let mut actions = Vec::new();
    let mut tool = None;
    for name in names {
        let current = live(gate, name);
        let (change, target) = match manifest.sysctl.get(name) {
            Some(value) => {
                let Some(current) = &current else {
                    problems.push(format!("no sysctl {name} on this machine"));
                    continue;
                };
                let was = match recorded.get(name) {
                    Some(before) => before.was.clone(),
                    None => Some(current.clone()),
                };
                record.insert(
                    format!("{SYSCTL_KEY}{name}"),
                    BasicRecord {
                        value: value.clone(),
                        was,
                    },
                );
                (Change::Set, value.clone())
            }
            // Gone from the manifest: back to what it was, if lodi knows and the kernel has it.
            None => match (recorded.get(name).and_then(|r| r.was.clone()), &current) {
                (Some(was), Some(_)) => (Change::Remove, was),
                _ => continue,
            },
        };
        let current = current.unwrap_or_default();
        if current == target {
            continue;
        }
        let sysctl = match &tool {
            Some(tool) => tool,
            None => tool.insert(Tool::find(gate, "sysctl", "[sysctl]")?),
        };
        let set = sysctl.invocation(&["-w", &format!("{name}={target}")]);
        actions.push(BasicAction {
            key: "sysctl",
            change,
            what: format!("{name} {current} -> {target}"),
            detail: set.command_line(),
            step: Step::Run {
                first: Vec::new(),
                writes: vec![file.clone()],
                then: vec![set],
            },
        });
    }
    if !problems.is_empty() {
        return Err(Diagnostic::new("E_UNKNOWN_SETTING", problems.join("; "))
            .hint("nothing changed; correct the name, or load the module that provides it"));
    }
    if actions.is_empty() && now != want {
        // Every value is set now; the file read at boot is not as declared.
        actions.push(BasicAction {
            key: "sysctl",
            change: match (&now, &want) {
                (None, _) => Change::Add,
                (_, None) => Change::Remove,
                _ => Change::Set,
            },
            what: SYSCTL.to_string(),
            detail: "at every boot".to_string(),
            step: Step::Run {
                first: Vec::new(),
                writes: vec![file],
                then: Vec::new(),
            },
        });
    }
    out.extend(actions);
    Ok(())
}
