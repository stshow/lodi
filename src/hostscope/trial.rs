//! The trial boot (bv-1, LD-425): a change to the kernel's command line boots once, as a trial,
//! before it becomes the default.
//!
//! An apply that changes `[kernel] parameters` leaves the default entry as it is. It adds one
//! more GRUB entry, `lodi-trial`: a copy of the default entry with the new parameters and the
//! marker `lodi.trial=1`, from lodi's script `/etc/grub.d/43_lodi_trial`, which `grub-mkconfig`
//! copies into the loader's configuration. It then sets `lodi_trial=1` in `EFI/lodi.env` on the
//! EFI system partition. As GRUB starts, the script's code reads that file, clears the variable
//! and, only once that write has succeeded, boots the trial entry. So the trial boots once, and
//! every boot after it is the default again, whatever happens in it. The file lives on the EFI
//! system partition because GRUB cannot write to btrfs, which Arch's and Fedora's `/boot` are
//! on, and so could not forget a one-shot entry kept beside its configuration there.
//!
//! `lodi boot confirm`, run in the trial boot, makes the change the default: it writes the
//! parameters where [`super::kernel`] keeps them and has the loader write its configuration
//! again. The entry that was the default before stays as `lodi-known-good` (script
//! `42_lodi_known_good`), a fallback in the loader's menu; taking the declaration out does the
//! same with the confirmed entry, so the distribution's defaults come back beside the last entry
//! that booted and was confirmed. On Fedora the default entry is a loader entry under
//! `/boot/loader/entries`, and lodi's entries are GRUB menu entries made from it.
//!
//! Under systemd-boot (bl-1's `[boot] loader`), the trial is systemd-boot's own one-shot: lodi's
//! entry `lodi-trial.conf` on the ESP, a copy of the default entry with the new command line and
//! the marker and without a `sort-key`, so it sorts after the distribution's entries, and
//! `bootctl set-oneshot`, which systemd-boot forgets as it boots the entry. Confirmed, lodi
//! writes `/etc/kernel/cmdline` and each kernel's entry with `kernel-install`, and keeps the
//! entry that was the default as `lodi-known-good.conf`. A switch of the loader itself is tried
//! by the firmware's `BootNext` ([`super::boot::SwitchTrial`]); `lodi boot confirm` confirms
//! every trial the running boot is.
//!
//! What waits for a confirmation is `<root>/var/lib/lodi/host/boot-trial`: the parameters tried
//! and the boot id of the apply that set them. The next command reads it against the running
//! kernel's boot id and command line: the same boot is still waiting for the reboot, the marker
//! is the trial itself, and any other boot is one after a trial that was not confirmed, which
//! lodi says.

use crate::diag::Diagnostic;

use super::basics::{self, Step, Write};
use super::safety::{Distro, Gate, Operation};
use super::{HostError, Options};

/// The one-shot entry's id, and the fallback's.
pub const TRIAL: &str = "lodi-trial";
pub const KNOWN_GOOD: &str = "lodi-known-good";
/// The word on the trial entry's command line that tells the trial boot from any other.
const MARKER: &str = "lodi.trial=1";
/// What waits for a confirmation, inside the root.
const STATE: &str = "/var/lib/lodi/host/boot-trial";
const ENTRIES: &str = "/boot/loader/entries";
/// lodi's one-shot flag, on the EFI system partition.
const ENV: &str = "EFI/lodi.env";
const CONFIRM: &str = "`sudo lodi boot confirm`";

/// A trial the machine waits to confirm: the parameters it tries, and the boot that set it.
/// Under systemd-boot, also the ESP and the command line its entry boots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trial {
    pub parameters: String,
    boot_id: String,
    systemd_boot: Option<SystemdBootTrial>,
}

/// systemd-boot's side of a trial: the ESP inside the root, the whole command line, and the
/// parameters the default boots, which `/etc/kernel/cmdline` holds until it is confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SystemdBootTrial {
    esp: String,
    cmdline: String,
    confirmed: String,
}

/// Where a trial stands, as the running kernel tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The machine has not booted since the apply that set it.
    Pending,
    /// This boot is the trial.
    Running,
    /// The machine booted again after the trial, or never took it: the earlier entry is back.
    Reverted,
}

fn proc(gate: &Gate, path: &str) -> String {
    std::fs::read_to_string(gate.root.join(path)).unwrap_or_default()
}

pub(super) fn boot_id(gate: &Gate) -> String {
    proc(gate, "proc/sys/kernel/random/boot_id")
        .trim()
        .to_string()
}

impl Trial {
    pub fn read(gate: &Gate) -> Option<Trial> {
        let text = String::from_utf8(basics::read(gate, STATE)?).ok()?;
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
                .map(str::to_string)
        };
        let systemd_boot = (field("via").as_deref() == Some("systemd-boot"))
            .then(|| {
                Some(SystemdBootTrial {
                    esp: field("esp")?,
                    cmdline: field("cmdline")?,
                    confirmed: field("confirmed").unwrap_or_default(),
                })
            })
            .flatten();
        Some(Trial {
            parameters: field("parameters")?,
            boot_id: field("boot_id").unwrap_or_default(),
            systemd_boot,
        })
    }

    /// Whether systemd-boot tries it, not GRUB.
    pub fn is_systemd_boot(&self) -> bool {
        self.systemd_boot.is_some()
    }

    /// Under systemd-boot, the parameters the default boots while it waits.
    pub fn confirmed(&self) -> Option<&str> {
        self.systemd_boot.as_ref().map(|sd| sd.confirmed.as_str())
    }

    /// Whether it tries the command line `cmdline` under systemd-boot.
    pub fn tries_cmdline(&self, cmdline: &str) -> bool {
        self.systemd_boot
            .as_ref()
            .is_some_and(|sd| sd.cmdline == cmdline.trim_end())
    }

    pub fn status(&self, gate: &Gate) -> Status {
        if proc(gate, "proc/cmdline")
            .split_whitespace()
            .any(|word| word == MARKER)
        {
            Status::Running
        } else if boot_id(gate) == self.boot_id {
            Status::Pending
        } else {
            Status::Reverted
        }
    }

    /// The line a plan says about this trial.
    pub fn note(&self, status: Status) -> String {
        format!("= parameters {} ({})", self.parameters, why(status))
    }
}

/// What a plan says about a trial in each state.
pub(super) fn why(status: Status) -> String {
    match status {
        Status::Pending => format!("a trial boot follows: reboot, then run {CONFIRM}"),
        Status::Running => format!("this boot is the trial: {CONFIRM} makes it the default"),
        Status::Reverted => "not confirmed: the trial reverted to the earlier entry".to_string(),
    }
}

/// Where lodi keeps the entry `id`: a script `grub-mkconfig` copies into `grub.cfg`.
fn script(id: &str) -> &'static str {
    if id == TRIAL {
        "/etc/grub.d/43_lodi_trial"
    } else {
        "/etc/grub.d/42_lodi_known_good"
    }
}

/// Where the EFI system partition is mounted, as the first of the usual places with an `EFI`
/// folder in it.
fn esp(gate: &Gate) -> Option<&'static str> {
    ["/efi", "/boot/efi", "/boot"]
        .into_iter()
        .find(|path| gate.root.join(&path[1..]).join("EFI").is_dir())
}

/// The one-shot flag GRUB reads, and clears, as it starts.
fn flag(esp: &str) -> String {
    format!("{esp}/{ENV}")
}

/// A GRUB environment block with `lines`, the 1024 bytes `grub-editenv` writes: GRUB rewrites
/// a block only in place.
fn env_block(lines: &str) -> Vec<u8> {
    let head = format!("# GRUB Environment Block\n{lines}");
    format!("{head}{}", "#".repeat(1024 - head.len())).into_bytes()
}

/// The entry the loader boots by default, as a menu entry: the first one of `grub.cfg`, or on
/// Fedora the loader entry GRUB's environment saves, else the last one that is not lodi's.
fn default_entry(gate: &Gate) -> Result<Vec<String>, Diagnostic> {
    let missing = |what: String| {
        Diagnostic::new("E_BOOT_LOADER", format!("no default boot entry: {what}"))
            .hint("lodi tries a kernel parameter change on a copy of that entry; nothing changed")
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

/// `args` without the last of each word of `drop`, then with `add`.
fn rewrite(args: &[&str], drop: &[&str], add: &[&str]) -> Vec<String> {
    let mut args: Vec<&str> = args.to_vec();
    for word in drop {
        if let Some(at) = args.iter().rposition(|arg| arg == word) {
            args.remove(at);
        }
    }
    args.iter()
        .chain(add)
        .map(|word| word.to_string())
        .collect()
}

/// The code in `grub.cfg` that boots the trial once: it finds lodi's flag on the EFI system
/// partition, clears it, and only once that write has succeeded makes the trial the default.
const ONE_SHOT: &str = "insmod fat
if search --no-floppy --file --set=lodi_esp /EFI/lodi.env; then
  if load_env -f ($lodi_esp)/EFI/lodi.env lodi_trial; then
    if [ \"${lodi_trial}\" = 1 ]; then
      set lodi_trial=0
      if save_env -f ($lodi_esp)/EFI/lodi.env lodi_trial; then
        set default=lodi-trial
      fi
    fi
  fi
fi
";

/// The default entry as lodi's entry `id`, its command line rewritten, as a `/etc/grub.d`
/// script; the trial's carries the code that boots it once.
fn copy(lines: &[String], id: &str, drop: &[&str], add: &[&str]) -> Vec<u8> {
    let title = if id == TRIAL {
        "lodi: trial boot"
    } else {
        "lodi: known good"
    };
    let mut out = String::new();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let words: Vec<&str> = trimmed.split_whitespace().collect();
        let line = if index == 0 {
            format!("menuentry '{title}' --id {id} {{")
        } else if words.first().is_some_and(|w| w.starts_with("linux")) && words.len() > 1 {
            let indent = &line[..line.len() - trimmed.len()];
            let args = rewrite(&words[2..], drop, add).join(" ");
            format!("{indent}{} {} {args}", words[0], words[1])
        } else {
            line.clone()
        };
        out.push_str(&line);
        out.push('\n');
    }
    if id == TRIAL {
        out.push_str(ONE_SHOT);
    }
    format!(
        "#!/bin/sh\n# Written by lodi for host.toml [kernel] parameters: the {title} entry.\n\
         cat <<'EOF'\n{out}EOF\n"
    )
    .into_bytes()
}

fn entry_write(id: &str, bytes: Option<Vec<u8>>) -> Write {
    Write {
        path: script(id).to_string(),
        bytes,
        mode: 0o755,
    }
}

/// The trial of `want` in place of `confirmed`: lodi's trial entry, the record of it, the
/// loader's configuration written again, and the flag that has GRUB boot it once.
pub fn trial(
    gate: &Gate,
    confirmed: &str,
    want: &str,
    mkconfig: super::pm::Invocation,
) -> Result<(String, Step, Trial), Diagnostic> {
    let esp = esp(gate).ok_or_else(|| {
        Diagnostic::new(
            "E_BOOT_LOADER",
            "no EFI system partition at /efi, /boot/efi or /boot",
        )
        .hint("lodi tries a kernel parameter change once through a file there; nothing changed")
    })?;
    let default = default_entry(gate)?;
    let drop: Vec<&str> = confirmed.split_whitespace().collect();
    let add: Vec<&str> = want.split_whitespace().chain([MARKER]).collect();
    let bytes = copy(&default, TRIAL, &drop, &add);
    let detail = format!("a trial boot: {TRIAL} once, by {}", flag(esp));
    let trial = Trial {
        parameters: want.to_string(),
        boot_id: boot_id(gate),
        systemd_boot: None,
    };
    let state = format!(
        "parameters={}\nboot_id={}\n",
        trial.parameters, trial.boot_id
    );
    Ok((
        detail,
        Step::Trial {
            writes: vec![
                entry_write(TRIAL, Some(bytes)),
                Write {
                    path: STATE.to_string(),
                    bytes: Some(state.into_bytes()),
                    mode: 0o600,
                },
            ],
            run: vec![mkconfig],
            // Set once the entry is in `grub.cfg`; the partition's FAT keeps only mode 0600.
            after: vec![Write {
                path: flag(esp),
                bytes: Some(env_block("lodi_trial=1\n")),
                mode: 0o600,
            }],
        },
        trial,
    ))
}

/// A new default for the loader: `grub` (the GRUB file lodi's line is in) written, a trial
/// forgotten, the configuration written again, and the entry that was the default kept as the
/// known-good fallback when `keep` says the default changes.
pub fn new_default(
    gate: &Gate,
    grub: Write,
    keep: bool,
    run: Vec<super::pm::Invocation>,
) -> Result<Step, Diagnostic> {
    let mut writes = vec![grub];
    if keep {
        let default = default_entry(gate)?;
        writes.push(entry_write(
            KNOWN_GOOD,
            Some(copy(&default, KNOWN_GOOD, &[], &[])),
        ));
    }
    let forget = |path: String| Write {
        path,
        bytes: None,
        mode: 0o600,
    };
    writes.push(entry_write(TRIAL, None));
    writes.push(forget(STATE.to_string()));
    writes.extend(esp(gate).map(|esp| forget(flag(esp))));
    Ok(Step::Trial {
        writes,
        run,
        after: Vec::new(),
    })
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
        .hint("lodi tries a kernel parameter change on a copy of that entry; nothing changed")
    })?;
    let path = format!("/{esp}/loader/entries/{name}");
    Ok(String::from_utf8_lossy(&basics::read(gate, &path).unwrap_or_default()).into_owned())
}

/// systemd-boot's entry `text` as lodi's entry `id`, booting `options` when given. It has no
/// `sort-key`, so it sorts after the distribution's entries and is never their default.
fn sd_copy(text: &str, id: &str, options: Option<&str>) -> Vec<u8> {
    let title = if id == TRIAL {
        "lodi: trial boot"
    } else {
        "lodi: known good"
    };
    let mut out = format!("title {title}\n");
    for line in text.lines() {
        let key = line.split_whitespace().next().unwrap_or_default();
        match (key, options) {
            ("title" | "sort-key", _) => {}
            ("options", Some(options)) => out.push_str(&format!("options {options}\n")),
            _ => out.push_str(&format!("{line}\n")),
        }
    }
    out.into_bytes()
}

/// The trial of the command line `cmdline` under systemd-boot: lodi's trial entry with it and
/// the marker, the record of it, and `bootctl set-oneshot`, which systemd-boot forgets as it
/// boots the entry once.
pub fn sd_trial(
    gate: &Gate,
    esp: &str,
    cmdline: &str,
    parameters: &str,
    confirmed: &str,
) -> Result<(String, Step, Trial), Diagnostic> {
    let default = sd_default(gate, esp)?;
    let options = format!("{} {MARKER}", cmdline.trim_end());
    let bootctl = super::basics::Tool::find(gate, "bootctl", "[kernel] parameters")?;
    let esp_path = gate.root.join(esp).display().to_string();
    let oneshot = bootctl.invocation(&[
        &format!("--esp-path={esp_path}"),
        "set-oneshot",
        &format!("{TRIAL}.conf"),
    ]);
    let detail = format!("a trial boot: {TRIAL} once, by {}", oneshot.command_line());
    let trial = Trial {
        parameters: parameters.to_string(),
        boot_id: boot_id(gate),
        systemd_boot: Some(SystemdBootTrial {
            esp: esp.to_string(),
            cmdline: cmdline.trim_end().to_string(),
            confirmed: confirmed.to_string(),
        }),
    };
    let state = format!(
        "parameters={}\nboot_id={}\nvia=systemd-boot\nesp={esp}\ncmdline={}\n\
         confirmed={confirmed}\n",
        trial.parameters,
        trial.boot_id,
        cmdline.trim_end()
    );
    Ok((
        detail,
        Step::Trial {
            writes: vec![
                Write {
                    path: sd_entry(esp, TRIAL),
                    bytes: Some(sd_copy(&default, TRIAL, Some(&options))),
                    mode: 0o644,
                },
                Write {
                    path: STATE.to_string(),
                    bytes: Some(state.into_bytes()),
                    mode: 0o600,
                },
            ],
            run: vec![oneshot],
            after: Vec::new(),
        },
        trial,
    ))
}

/// A new default command line for systemd-boot: `/etc/kernel/cmdline` written and each kernel's
/// entry again (`run`, `kernel-install`), a trial forgotten, and the entry that was the default
/// kept as the known-good fallback when `keep` says so.
pub fn sd_new_default(
    gate: &Gate,
    esp: &str,
    cmdline: &str,
    keep: bool,
    run: Vec<super::pm::Invocation>,
) -> Result<Step, Diagnostic> {
    let mut writes = vec![Write {
        path: "/etc/kernel/cmdline".to_string(),
        bytes: Some(format!("{}\n", cmdline.trim_end()).into_bytes()),
        mode: 0o644,
    }];
    if keep {
        let default = sd_default(gate, esp)?;
        writes.push(Write {
            path: sd_entry(esp, KNOWN_GOOD),
            bytes: Some(sd_copy(&default, KNOWN_GOOD, None)),
            mode: 0o644,
        });
    }
    for path in [sd_entry(esp, TRIAL), STATE.to_string()] {
        writes.push(Write {
            path,
            bytes: None,
            mode: 0o600,
        });
    }
    Ok(Step::Trial {
        writes,
        run,
        after: Vec::new(),
    })
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

/// Carry out a boot step: write, run, write what comes after the loader's tool.
pub fn perform(gate: &Gate, step: &Step) -> Result<(), Diagnostic> {
    let Step::Trial { writes, run, after } = step else {
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
    for write in after {
        basics::apply_write(gate, write)?;
    }
    Ok(())
}

fn refused(message: String, hint: &str) -> HostError {
    Diagnostic::new("E_BOOT_TRIAL", message)
        .hint(format!("{hint}; nothing was changed"))
        .into()
}

/// Why `lodi boot confirm` has nothing to confirm for a trial of `what` in `status`.
fn not_running(what: &str, status: Status) -> HostError {
    if status == Status::Pending {
        refused(
            format!("the trial boot of `{what}` has not booted yet"),
            "reboot, then confirm it in the trial boot",
        )
    } else {
        refused(
            format!(
                "this boot is not the trial of `{what}`: it was not confirmed, and this machine \
                 booted its earlier entry"
            ),
            "an apply sets the trial again",
        )
    }
}

/// `lodi boot confirm`: in the trial boot, make each trial that booted the default.
pub fn confirm(options: &Options) -> Result<String, HostError> {
    let mut gate = Gate::open(options, Operation::Apply)?;
    let parameters = Trial::read(&gate).map(|trial| {
        let status = trial.status(&gate);
        (trial, status)
    });
    let switch = super::boot::SwitchTrial::read(&gate).map(|trial| {
        let status = trial.status(&gate);
        (trial, status)
    });
    let running = parameters
        .as_ref()
        .is_some_and(|(_, s)| *s == Status::Running)
        || switch.as_ref().is_some_and(|(_, s)| *s == Status::Running);
    if !running {
        return Err(match (&parameters, &switch) {
            (Some((trial, status)), _) => not_running(&trial.parameters, *status),
            (None, Some((trial, status))) => not_running(&trial.what(), *status),
            (None, None) => refused(
                "no trial boot waits for a confirmation".to_string(),
                "an apply that changes the boot sets one",
            ),
        });
    }
    gate.lock_for_apply()?;
    let mut said = String::new();
    if let Some((trial, Status::Running)) = &switch {
        said.push_str(&trial.confirm(&gate)?);
    }
    if let Some((trial, Status::Running)) = &parameters {
        let step = match &trial.systemd_boot {
            Some(sd) => sd_new_default(
                &gate,
                &sd.esp,
                &sd.cmdline,
                true,
                super::boot::kernel_installs(&gate)?,
            )?,
            None => {
                let grub = super::kernel::grub_write(&gate, &trial.parameters)?;
                let calls = super::kernel::default_calls(
                    &gate,
                    &super::kernel::confirmed(&gate),
                    &trial.parameters,
                )?;
                new_default(&gate, grub, true, calls)?
            }
        };
        perform(&gate, &step)?;
        said.push_str(&format!(
            "~ parameters {} (the default entry from now on)\nconfirmed; the entry that was the \
             default stays as {KNOWN_GOOD}\n",
            trial.parameters
        ));
    }
    Ok(said)
}
