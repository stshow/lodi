//! The boot's fallback entry (LD-423, LD-424, LD-491): a change to the kernel's command line is
//! the default from the next boot, and the entry that was the default stays in the loader's
//! menu, to be picked by hand if the new one fails. Nothing boots once and nothing waits to be
//! confirmed.
//!
//! Under GRUB the change is lodi's line in the GRUB file and the loader's configuration written
//! again ([`super::kernel`]); the fallback is lodi's entry `lodi-known-good`, a copy of the entry
//! that was the default, from lodi's script `/etc/grub.d/42_lodi_known_good`, which
//! `grub-mkconfig` copies into the loader's configuration after the distribution's entries. On
//! Fedora the default entry is a loader entry under `/boot/loader/entries`, and lodi's entry is a
//! GRUB menu entry made from it.
//!
//! Under systemd-boot (`[boot] loader`) the change is `/etc/kernel/cmdline` and each kernel's
//! entry written again with `kernel-install`; the fallback is `lodi-known-good.conf` on the EFI
//! system partition, without a `sort-key`, so it sorts after the distribution's entries and is
//! never their default.
//!
//! The fallback is replaced only by an entry that has booted (LD-492): the entry that was the
//! default becomes the fallback when the running kernel's command line is the one it boots, or
//! when there is no fallback yet. Two changes before a reboot keep the fallback that booted.

use std::collections::BTreeSet;

use crate::diag::Diagnostic;

use super::basics::{self, Step, Write};
use super::safety::{Distro, Gate};

/// The fallback entry's id.
pub const KNOWN_GOOD: &str = "lodi-known-good";
const ENTRIES: &str = "/boot/loader/entries";
/// lodi's GRUB script that writes the fallback entry into `grub.cfg`.
const SCRIPT: &str = "/etc/grub.d/42_lodi_known_good";

/// The running kernel's command line, as a set of words, without those the loader adds itself.
fn running(gate: &Gate) -> BTreeSet<String> {
    words(&std::fs::read_to_string(gate.root.join("proc/cmdline")).unwrap_or_default())
}

fn words(line: &str) -> BTreeSet<String> {
    line.split_whitespace()
        .filter(|word| !word.starts_with("BOOT_IMAGE=") && !word.starts_with("initrd="))
        .map(str::to_string)
        .collect()
}

/// Whether one of `lines` (command lines an entry boots) is the running kernel's.
fn has_booted<'a>(gate: &Gate, lines: impl IntoIterator<Item = &'a str>) -> bool {
    let running = running(gate);
    !running.is_empty() && lines.into_iter().any(|line| words(line) == running)
}

/// The entry the loader boots by default, as a menu entry: the first one of `grub.cfg`, or on
/// Fedora the loader entry GRUB's environment saves, else the last one that is not lodi's.
fn default_entry(gate: &Gate) -> Result<Vec<String>, Diagnostic> {
    let missing = |what: String| {
        Diagnostic::new("E_BOOT_LOADER", format!("no default boot entry: {what}"))
            .hint("lodi keeps a copy of that entry as the fallback; nothing changed")
    };
    let text = |path: &str| {
        basics::read(gate, path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    };
    if gate.distro != Distro::Fedora {
        let config = "/boot/grub/grub.cfg";
        let text = text(config).ok_or_else(|| missing(format!("no {config}")))?;
        let lines: Vec<String> = text
            .lines()
            .skip_while(|line| !line.starts_with("menuentry "))
            .map(str::to_string)
            .collect();
        let end = lines
            .iter()
            .position(|line| line.starts_with('}'))
            .ok_or_else(|| missing(format!("no menu entry in {config}")))?;
        return Ok(lines[..=end].to_vec());
    }
    let saved = text("/boot/grub2/grubenv")
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("saved_entry="))
        .map(str::to_string)
        .filter(|id| !id.is_empty() && !id.contains('/'));
    let mut ids: Vec<String> = std::fs::read_dir(gate.root.join(&ENTRIES[1..]))
        .map(|dir| {
            dir.flatten()
                .filter_map(|e| {
                    e.file_name()
                        .to_str()?
                        .strip_suffix(".conf")
                        .map(str::to_string)
                })
                .filter(|id| !id.starts_with("lodi-"))
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    let id = saved
        .filter(|id| ids.contains(id))
        .or(ids.pop())
        .ok_or_else(|| missing(format!("no entry in {ENTRIES}")))?;
    let text = text(&format!("{ENTRIES}/{id}.conf")).unwrap_or_default();
    let key = |key: &str| -> Vec<&str> {
        text.lines()
            .filter_map(|line| line.strip_prefix(key)?.strip_prefix(' '))
            .map(str::trim)
            .collect()
    };
    let (linux, options, initrd) = (key("linux"), key("options"), key("initrd"));
    let Some(linux) = linux.first() else {
        return Err(missing(format!("no kernel in {ENTRIES}/{id}.conf")));
    };
    let mut lines = vec![
        format!("menuentry '{id}' {{"),
        format!("\tlinux {linux} {}", options.join(" ")),
    ];
    if !initrd.is_empty() {
        lines.push(format!("\tinitrd {}", initrd.join(" ")));
    }
    lines.push("}".to_string());
    Ok(lines)
}

/// The command lines a GRUB menu entry boots: the words of each `linux` line after the kernel.
fn grub_cmdlines(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            words.next().filter(|word| word.starts_with("linux"))?;
            words.next()?;
            Some(words.collect::<Vec<_>>().join(" "))
        })
        .collect()
}

/// The default entry as lodi's fallback entry, as a `/etc/grub.d` script.
fn copy(lines: &[String]) -> Vec<u8> {
    let title = "lodi: known good";
    let mut out = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index == 0 {
            out.push_str(&format!("menuentry '{title}' --id {KNOWN_GOOD} {{"));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    format!(
        "#!/bin/sh\n# Written by lodi for host.toml [kernel] parameters: the {title} entry.\n\
         cat <<'EOF'\n{out}EOF\n"
    )
    .into_bytes()
}

/// What a plan says about the fallback when the default changes.
pub fn kept(name: &str, replaced: bool) -> String {
    if replaced {
        format!("the entry before it stays as {name}")
    } else {
        format!("{name} stays as it is: the default has not booted")
    }
}

/// A new default for GRUB: `grub` (the GRUB file lodi's line is in) written and the
/// configuration written again (`run`); when `changes` says the default changes, the entry that
/// was the default replaces the fallback if it has booted or there is none. Returns the step and
/// whether the fallback is replaced.
pub fn new_default(
    gate: &Gate,
    grub: Write,
    changes: bool,
    run: Vec<super::pm::Invocation>,
) -> Result<(Step, bool), Diagnostic> {
    let mut writes = vec![grub];
    let mut replaced = false;
    if changes {
        let default = default_entry(gate)?;
        let cmdlines = grub_cmdlines(&default);
        replaced = basics::read(gate, SCRIPT).is_none()
            || has_booted(gate, cmdlines.iter().map(String::as_str));
        if replaced {
            writes.push(Write {
                path: SCRIPT.to_string(),
                bytes: Some(copy(&default)),
                mode: 0o755,
            });
        }
    }
    Ok((Step::Entry { writes, run }, replaced))
}

/// systemd-boot's entries on the ESP `esp`, at their absolute paths inside the root.
fn sd_entry(esp: &str, id: &str) -> String {
    format!("/{esp}/loader/entries/{id}.conf")
}

/// The entry systemd-boot boots by default: the newest on the ESP that is not lodi's.
fn sd_default(gate: &Gate, esp: &str) -> Result<String, Diagnostic> {
    let mut names: Vec<String> = std::fs::read_dir(gate.root.join(esp).join("loader/entries"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|name| name.ends_with(".conf") && !name.starts_with("lodi-"))
        .collect();
    names.sort();
    let name = names.pop().ok_or_else(|| {
        Diagnostic::new(
            "E_BOOT_LOADER",
            format!("no default boot entry: no entry in /{esp}/loader/entries"),
        )
        .hint("lodi keeps a copy of that entry as the fallback; nothing changed")
    })?;
    let path = format!("/{esp}/loader/entries/{name}");
    Ok(String::from_utf8_lossy(&basics::read(gate, &path).unwrap_or_default()).into_owned())
}

/// systemd-boot's entry `text` as lodi's fallback entry. It has no `sort-key`, so it sorts after
/// the distribution's entries and is never their default.
fn sd_copy(text: &str) -> Vec<u8> {
    let mut out = "title lodi: known good\n".to_string();
    for line in text.lines() {
        let key = line.split_whitespace().next().unwrap_or_default();
        if !matches!(key, "title" | "sort-key") {
            out.push_str(&format!("{line}\n"));
        }
    }
    out.into_bytes()
}

/// A new default command line for systemd-boot: `/etc/kernel/cmdline` written and each kernel's
/// entry again (`run`, `kernel-install`), and the entry that was the default kept as the
/// fallback when it has booted or there is none. Returns the step and whether the fallback is
/// replaced.
pub fn sd_new_default(
    gate: &Gate,
    esp: &str,
    cmdline: &str,
    run: Vec<super::pm::Invocation>,
) -> Result<(Step, bool), Diagnostic> {
    let mut writes = vec![Write {
        path: "/etc/kernel/cmdline".to_string(),
        bytes: Some(format!("{}\n", cmdline.trim_end()).into_bytes()),
        mode: 0o644,
    }];
    let default = sd_default(gate, esp)?;
    let options = default
        .lines()
        .filter_map(|line| line.strip_prefix("options"))
        .collect::<Vec<_>>()
        .join(" ");
    let path = sd_entry(esp, KNOWN_GOOD);
    let replaced = basics::read(gate, &path).is_none() || has_booted(gate, [options.as_str()]);
    if replaced {
        writes.push(Write {
            path,
            bytes: Some(sd_copy(&default)),
            mode: 0o644,
        });
    }
    Ok((Step::Entry { writes, run }, replaced))
}

/// Each of the machine's loader entries under `/boot/loader/entries`, at its absolute path
/// inside the root, with its bytes.
fn loader_entries(gate: &Gate) -> Vec<(String, Vec<u8>)> {
    let mut found: Vec<(String, Vec<u8>)> = std::fs::read_dir(gate.root.join(&ENTRIES[1..]))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            if !name.ends_with(".conf") || name.starts_with("lodi-") {
                return None;
            }
            let path = format!("{ENTRIES}/{name}");
            Some((path.clone(), basics::read(gate, &path)?))
        })
        .collect();
    found.sort();
    found
}

/// Carry out a new default: write, then run the loader's tools.
pub fn perform(gate: &Gate, step: &Step) -> Result<(), Diagnostic> {
    let Step::Entry { writes, run } = step else {
        return Ok(());
    };
    for write in writes {
        basics::apply_write(gate, write)?;
    }
    for call in run {
        // Fedora 44's `grub2-mkconfig` writes each loader entry's options again from the GRUB
        // file. lodi runs it only for its own entries, so the machine's are put back as they
        // were: only `grubby` edits them, as LD-423 does.
        let kept = (gate.distro == Distro::Fedora && call.program == "grub2-mkconfig")
            .then(|| loader_entries(gate));
        super::pm::run(call)?;
        for (path, bytes) in kept.into_iter().flatten() {
            let write = Write {
                path,
                bytes: Some(bytes),
                mode: 0o644,
            };
            basics::apply_write(gate, &write)?;
        }
    }
    Ok(())
}
