//! `[boot]` (bl-1, LD-424): the loader the firmware starts, GRUB or systemd-boot, its timeout
//! and its default entry; the kernel command line is `[kernel] parameters` ([`super::kernel`]).
//!
//! lodi manages a UEFI machine's loader only: `[boot]` on a machine that did not start through
//! UEFI stops the plan with `E_BOOT_NOT_UEFI` before anything is read or changed. The plan reads
//! the firmware's boot entries from efivarfs and the EFI system partition's files, and runs
//! nothing. The loader in use is the first entry of `BootOrder` that starts one lodi knows.
//!
//! A switch is one staged step. lodi keeps every file of the ESP and the boot order first, then
//! installs the loader's packages, installs the loader (`bootctl install`, or `grub-install`
//! where its GRUB is not on the ESP), writes systemd-boot an entry for each installed kernel
//! with `kernel-install` from `/etc/kernel/cmdline`, or GRUB its configuration with
//! `grub-mkconfig`, and puts the new loader first in `BootOrder` and the earlier one right after
//! it, as the fallback entry. The firmware's removable path, `EFI/BOOT`, keeps the loader it had.
//! Where `/boot` is a partition of its own that is not FAT, such as Ubuntu 24.04's ext4 XBOOTLDR
//! partition, `kernel-install` would write the entries there, out of the firmware's reach: lodi
//! writes its own `/etc/kernel/install.conf` naming the ESP, and removes it when GRUB comes back.
//! Any step that fails puts the ESP's files and the boot order back, so the machine boots as it
//! did. Nothing is uninstalled: the earlier loader stays on the ESP and in the firmware.
//!
//! A switch to the declared loader boots once as a trial first (bv-1, LD-425): the earlier
//! loader stays first in `BootOrder`, the new one right after it, and `BootNext` has the firmware
//! start the new one once. `lodi boot confirm` in that boot puts it first, the earlier one right
//! after it; without it, the next start is the earlier loader. A switch back because `[boot]` is
//! gone is not a trial, and forgets one that waits. Under systemd-boot a change of the command
//! line is tried once too ([`super::trial`]).
//!
//! The timeout and the default entry are lodi's lines, each after a marker line of its own, in
//! the declared loader's file: `loader/loader.conf` on the ESP, or for GRUB lodi's drop-in
//! `/etc/default/grub.d/99-lodi-boot.cfg` where the distribution reads that folder, else the end
//! of `/etc/default/grub`. Without `[boot]`, the next apply puts the loader the machine had back
//! first and removes lodi's lines, and both loaders' entries stay.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::diag::Diagnostic;

use super::basics::{BasicAction, Change, Step, Tool, Write};
use super::lock::{BasicRecord, HostLock};
use super::manifest::{Boot, HostManifest, Loader};
use super::pm::{self, Invocation};
use super::safety::{Distro, Gate};
use super::trial::{self, Status};

/// The record's keys: the loader with the one it replaced, and each setting as lodi wrote it.
pub const LOADER_KEY: &str = "boot.loader";
pub const TIMEOUT_KEY: &str = "boot.timeout";
pub const DEFAULT_KEY: &str = "boot.default";

const EFIVARS: &str = "sys/firmware/efi/efivars";
const EFI_GLOBAL: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
const SD_BOOT: &str = "EFI/systemd/systemd-bootx64.efi";
const SD_BOOT_SOURCE: &str = "usr/lib/systemd/boot/efi/systemd-bootx64.efi";
const REMOVABLE: &str = "EFI/BOOT";
/// What every systemd-boot image carries, which is how bootctl tells it from another loader.
const SD_MARK: &[u8] = b"#### LoaderInfo: systemd-boot";
const CMDLINE: &str = "/etc/kernel/cmdline";
/// Where `kernel-install` writes, when lodi has to tell it the ESP.
const INSTALL_CONF: &str = "/etc/kernel/install.conf";
const GRUB_DROP_IN: &str = "/etc/default/grub.d/99-lodi-boot.cfg";
const MARK: &str = "# lodi: host.toml [boot]";
/// A switch waiting for `lodi boot confirm` (bv-1, LD-425), inside the root.
const SWITCH_STATE: &str = "/var/lib/lodi/host/boot-trial-loader";

/// What an apply does for one `[boot]` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    Switch(Box<Switch>),
    Edit(Edit),
}

/// A switch from one loader to the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Switch {
    from: Loader,
    to: Loader,
    /// The ESP inside the root, such as `boot/efi`.
    esp: String,
    /// The index refresh and the loader's packages, when the machine lacks them.
    packages: Vec<Invocation>,
    /// `/etc/kernel/cmdline`, for systemd-boot.
    writes: Vec<Write>,
    /// Each installed kernel's version and image, for systemd-boot's entries.
    kernels: Vec<(String, String)>,
    /// GRUB's configuration, when GRUB is switched to.
    mkconfig: Option<Invocation>,
    /// The new loader starts once, by `BootNext`, and comes first only once it is confirmed
    /// (bv-1); a switch back to the distribution's loader is not a trial.
    trial: bool,
}

/// A switch the firmware tries once (bv-1, LD-425): the loader it tries, the one it came from,
/// the entry that starts the new one, and the boot that set it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchTrial {
    to: Loader,
    from: Loader,
    entry: u16,
    boot_id: String,
}

impl SwitchTrial {
    pub fn read(gate: &Gate) -> Option<SwitchTrial> {
        let text = String::from_utf8(super::basics::read(gate, SWITCH_STATE)?).ok()?;
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
        };
        Some(SwitchTrial {
            to: Loader::parse(field("loader")?)?,
            from: Loader::parse(field("from")?)?,
            entry: u16::from_str_radix(field("entry")?, 16).ok()?,
            boot_id: field("boot_id").unwrap_or_default().to_string(),
        })
    }

    /// The trial boot is the one the firmware started from the new loader's entry.
    pub fn status(&self, gate: &Gate) -> Status {
        let current = var(&gate.root, "BootCurrent")
            .and_then(|data| data.get(4..6).map(|b| u16::from_le_bytes([b[0], b[1]])));
        let same = trial::boot_id(gate) == self.boot_id;
        if same {
            Status::Pending
        } else if current == Some(self.entry) {
            Status::Running
        } else {
            Status::Reverted
        }
    }

    /// What is tried: `FROM -> TO`.
    pub fn what(&self) -> String {
        format!("{} -> {}", self.from.name(), self.to.name())
    }

    pub fn note(&self, status: Status) -> String {
        format!("= loader {} ({})", self.what(), trial::why(status))
    }

    /// In the trial boot: the new loader first, the earlier one right after it.
    pub fn confirm(&self, gate: &Gate) -> Result<String, Diagnostic> {
        let efibootmgr = Tool::find(gate, "efibootmgr", "[boot] loader")?;
        let fw = Firmware::read(gate)?;
        let second = entry_for(gate, &efibootmgr, &fw.esp, self.from)?;
        set_order(gate, &efibootmgr, self.entry, second)?;
        if nvram(&gate.root).0.first() != Some(&self.entry) {
            return Err(no_loader(format!(
                "the firmware does not start {} first",
                self.to.name()
            )));
        }
        super::basics::apply_write(gate, &forget(SWITCH_STATE))?;
        Ok(format!(
            "~ loader {} (the default from now on)\nconfirmed; {} stays as the fallback entry\n",
            self.to.name(),
            self.from.name()
        ))
    }
}

fn forget(path: &str) -> Write {
    Write {
        path: path.to_string(),
        bytes: None,
        mode: 0o600,
    }
}

/// `first`, then `second`, then the rest of the boot order as it was.
fn set_order(gate: &Gate, efibootmgr: &Tool, first: u16, second: u16) -> Result<(), Diagnostic> {
    let (now, _) = nvram(&gate.root);
    let wanted: Vec<u16> = [first, second]
        .into_iter()
        .chain(now.into_iter().filter(|n| *n != first && *n != second))
        .collect();
    let hex: Vec<String> = wanted.iter().map(|n| format!("{n:04X}")).collect();
    pm::run(&efibootmgr.invocation(&["--bootorder", &hex.join(",")]))
}

/// One of lodi's lines in a loader's file, set or taken out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// The file, at its absolute path inside the root.
    path: String,
    key: &'static str,
    line: Option<String>,
    /// The file is lodi's alone, and goes once it holds nothing.
    owned: bool,
    then: Vec<Invocation>,
}

impl Stage {
    pub fn invocations(&self) -> Vec<&Invocation> {
        match self {
            Stage::Switch(switch) => switch.packages.iter().chain(&switch.mkconfig).collect(),
            Stage::Edit(edit) => edit.then.iter().collect(),
        }
    }
}

/// One firmware boot entry: its number, its label and, when it names one, its file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    number: u16,
    label: String,
    target: Target,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// A file on the ESP, as the firmware names it: `\EFI\debian\shimx64.efi`.
    File(String),
    /// A disk, whose removable path the firmware starts.
    Disk,
    /// The firmware's own screens, the network and anything else.
    Other,
}

/// What the plan reads of the firmware and the ESP.
struct Firmware {
    esp: String,
    order: Vec<u16>,
    entries: Vec<Entry>,
}

fn var(root: &Path, name: &str) -> Option<Vec<u8>> {
    std::fs::read(root.join(EFIVARS).join(format!("{name}-{EFI_GLOBAL}"))).ok()
}

fn words(data: &[u8]) -> Vec<u16> {
    data.chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect()
}

/// A `Boot####` variable as efivarfs shows it: four bytes of attributes, then the load option.
fn parse_entry(number: u16, data: &[u8]) -> Option<Entry> {
    let option = data.get(4..)?;
    let length = u16::from_le_bytes([*option.get(4)?, *option.get(5)?]) as usize;
    let text = words(option.get(6..)?);
    let end = text.iter().position(|w| *w == 0)?;
    let label = String::from_utf16_lossy(&text[..end]);
    let path = option.get(6 + 2 * (end + 1)..)?.get(..length)?;
    // The nodes up to the end node: a file path node names the file; a path of hardware and
    // ACPI nodes alone is a disk; a network or firmware volume node is neither.
    let (mut at, mut nodes) = (0, Vec::new());
    while at + 4 <= path.len() && path[at] != 0x7f {
        let (kind, sub) = (path[at], path[at + 1]);
        let size = (u16::from_le_bytes([path[at + 2], path[at + 3]]) as usize).max(4);
        if (kind, sub) == (4, 4) {
            let name = words(path.get(at + 4..at + size)?);
            let name: Vec<u16> = name.into_iter().take_while(|w| *w != 0).collect();
            let target = Target::File(String::from_utf16_lossy(&name));
            return Some(Entry {
                number,
                label,
                target,
            });
        }
        nodes.push(kind);
        at += size;
    }
    let target = if !nodes.is_empty() && nodes.iter().all(|kind| matches!(kind, 1 | 2)) {
        Target::Disk
    } else {
        Target::Other
    };
    Some(Entry {
        number,
        label,
        target,
    })
}

/// The boot order and every boot entry.
fn nvram(root: &Path) -> (Vec<u16>, Vec<Entry>) {
    let order = var(root, "BootOrder")
        .and_then(|data| data.get(4..).map(words))
        .unwrap_or_default();
    let mut entries: Vec<Entry> = std::fs::read_dir(root.join(EFIVARS))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|file| {
            let name = file.file_name().to_string_lossy().into_owned();
            let head = name
                .strip_suffix(&format!("-{EFI_GLOBAL}"))?
                .strip_prefix("Boot")?;
            let number = u16::from_str_radix(head, 16)
                .ok()
                .filter(|_| head.len() == 4)?;
            parse_entry(number, &std::fs::read(file.path()).ok()?)
        })
        .collect();
    entries.sort_by_key(|entry| entry.number);
    (order, entries)
}

fn not_uefi() -> Diagnostic {
    Diagnostic::new(
        "E_BOOT_NOT_UEFI",
        "this machine did not start through UEFI".to_string(),
    )
    .hint("lodi manages only a UEFI bootloader; nothing was changed")
}

fn no_loader(what: String) -> Diagnostic {
    Diagnostic::new("E_BOOT_LOADER", what).hint(
        "lodi switches only between GRUB and systemd-boot on the EFI system partition; nothing \
         was changed",
    )
}

impl Firmware {
    fn read(gate: &Gate) -> Result<Firmware, Diagnostic> {
        if !gate.root.join("sys/firmware/efi").is_dir() {
            return Err(not_uefi());
        }
        let esp = ["efi", "boot/efi", "boot"]
            .into_iter()
            .find(|dir| gate.root.join(dir).join("EFI").is_dir())
            .ok_or_else(|| {
                no_loader("no EFI system partition at /efi, /boot/efi or /boot".into())
            })?;
        let (order, entries) = nvram(&gate.root);
        Ok(Firmware {
            esp: esp.to_string(),
            order,
            entries,
        })
    }
}

/// Which loader a file on the ESP is, from its path, and its bytes where the path cannot say.
fn loader_of(root: &Path, esp: &str, file: &str) -> Option<Loader> {
    let rel = file.trim_start_matches('\\').replace('\\', "/");
    let lower = rel.to_ascii_lowercase();
    if lower == SD_BOOT.to_ascii_lowercase() {
        return Some(Loader::SystemdBoot);
    }
    let bytes = std::fs::read(root.join(esp).join(&rel)).ok()?;
    if bytes.windows(SD_MARK.len()).any(|w| w == SD_MARK) {
        return Some(Loader::SystemdBoot);
    }
    ["shimx64.efi", "grubx64.efi", "bootx64.efi"]
        .iter()
        .any(|name| lower.ends_with(name))
        .then_some(Loader::Grub)
}

/// The firmware's removable path, as it names it.
const REMOVABLE_FILE: &str = "\\EFI\\BOOT\\BOOTX64.EFI";

/// The loader an entry starts.
fn started_by(root: &Path, esp: &str, entry: &Entry) -> Option<Loader> {
    match &entry.target {
        Target::File(file) => loader_of(root, esp, file),
        Target::Disk => loader_of(root, esp, REMOVABLE_FILE),
        Target::Other => None,
    }
}

/// The loader in use: the first in the boot order lodi knows, else the removable path's.
fn in_use(root: &Path, fw: &Firmware) -> Option<Loader> {
    fw.order
        .iter()
        .filter_map(|n| fw.entries.iter().find(|entry| entry.number == *n))
        .find_map(|entry| started_by(root, &fw.esp, entry))
        .or_else(|| loader_of(root, &fw.esp, REMOVABLE_FILE))
}

/// Where a loader's own file is on the ESP, as the firmware names it: GRUB's in its own folder
/// (shim first), else on the removable path.
fn loader_file(root: &Path, esp: &str, loader: Loader) -> Option<String> {
    if loader == Loader::SystemdBoot {
        return root
            .join(esp)
            .join(SD_BOOT)
            .is_file()
            .then(|| format!("\\{}", SD_BOOT.replace('/', "\\")));
    }
    let mut dirs: Vec<String> = std::fs::read_dir(root.join(esp).join("EFI"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|dir| dir.file_name().to_string_lossy().into_owned())
        .filter(|name| !["BOOT", "systemd", "Linux"].contains(&name.as_str()))
        .collect();
    dirs.sort();
    for name in ["shimx64.efi", "grubx64.efi"] {
        for dir in &dirs {
            if root.join(esp).join("EFI").join(dir).join(name).is_file() {
                return Some(format!("\\EFI\\{dir}\\{name}"));
            }
        }
    }
    (loader_of(root, esp, REMOVABLE_FILE) == Some(Loader::Grub)).then(|| REMOVABLE_FILE.into())
}

/// Each installed kernel's version and image, at its absolute path inside the root.
fn kernels(gate: &Gate) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = std::fs::read_dir(gate.root.join("usr/lib/modules"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|dir| {
            let version = dir.file_name().to_string_lossy().into_owned();
            let image = [
                format!("usr/lib/modules/{version}/vmlinuz"),
                format!("boot/vmlinuz-{version}"),
            ]
            .into_iter()
            .find(|image| gate.root.join(image).is_file())?;
            super::manifest::is_parameter(&version).then(|| (version, format!("/{image}")))
        })
        .collect();
    found.sort();
    found
}

/// The command line systemd-boot's entries get: the machine's own, from `/etc/kernel/cmdline`
/// or else the running kernel's, without the parameters lodi added before (those the lock
/// records, and those a trial that waits found confirmed), and then the declared ones.
fn command_line(gate: &Gate, manifest: &HostManifest, lock: Option<&HostLock>) -> String {
    let confirmed = trial::Trial::read(gate)
        .and_then(|trial| trial.confirmed().map(str::to_string))
        .unwrap_or_default();
    let before: Vec<String> = lock
        .and_then(|lock| lock.basics.get(super::kernel::PARAMETERS_KEY))
        .map(|record| record.value.clone())
        .into_iter()
        .chain([confirmed])
        .flat_map(|words| {
            words
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();
    let declared = &manifest.kernel.parameters;
    let source = super::basics::read(gate, CMDLINE)
        .or_else(|| std::fs::read(gate.root.join("proc/cmdline")).ok())
        .unwrap_or_default();
    let source = String::from_utf8_lossy(&source);
    let words: Vec<&str> = source
        .split_whitespace()
        .filter(|word| !word.starts_with("BOOT_IMAGE=") && !word.starts_with("initrd="))
        .filter(|word| !before.iter().chain(declared).any(|p| p == word))
        .chain(declared.iter().map(String::as_str))
        .collect();
    format!("{}\n", words.join(" "))
}

/// lodi's `/etc/kernel/install.conf` for systemd-boot, when `/boot` is a mount of its own the
/// firmware cannot read and no one else wrote the file; else `None`.
fn install_conf(gate: &Gate, esp: &str) -> Option<Write> {
    let mounts = std::fs::read_to_string(gate.root.join("proc/self/mountinfo")).ok()?;
    let own_boot = mounts.lines().any(|line| {
        line.split_once(" - ").is_some_and(|(head, tail)| {
            head.split(' ').nth(4) == Some("/boot") && tail.split(' ').next() != Some("vfat")
        })
    });
    let theirs = super::basics::read(gate, INSTALL_CONF).is_some_and(|bytes| !lodis(&bytes));
    (esp != "boot" && own_boot && !theirs).then(|| Write {
        path: INSTALL_CONF.to_string(),
        bytes: Some(format!("{MARK}\nlayout=bls\nBOOT_ROOT=/{esp}\n").into_bytes()),
        mode: 0o644,
    })
}

/// Whether a file starts with lodi's marker line.
fn lodis(bytes: &[u8]) -> bool {
    bytes.starts_with(format!("{MARK}\n").as_bytes())
}

/// The packages a loader needs that this machine lacks.
fn packages(gate: &Gate, loader: Loader, esp: &str) -> Result<Vec<String>, Diagnostic> {
    let mut names = Vec::new();
    match loader {
        Loader::SystemdBoot if !gate.root.join(SD_BOOT_SOURCE).is_file() => {
            names.push(match gate.distro {
                Distro::Debian | Distro::Ubuntu => "systemd-boot",
                Distro::Fedora => "systemd-boot-unsigned",
                Distro::Arch => "systemd",
            })
        }
        Loader::Grub if loader_file(&gate.root, esp, Loader::Grub).is_none() => {
            names.extend(match gate.distro {
                Distro::Debian | Distro::Ubuntu => &["grub-efi-amd64"][..],
                Distro::Fedora => &["grub2-efi-x64", "shim-x64"][..],
                Distro::Arch => &["grub"][..],
            })
        }
        _ => {}
    }
    if Tool::find_optional(gate, "efibootmgr")?.is_none() {
        names.push("efibootmgr");
    }
    Ok(names.into_iter().map(str::to_string).collect())
}

fn switch(
    gate: &Gate,
    fw: &Firmware,
    from: Loader,
    to: Loader,
    manifest: &HostManifest,
    lock: Option<&HostLock>,
    trial: bool,
) -> Result<BasicAction, Diagnostic> {
    // The earlier loader is the fallback: it must be there before anything changes.
    let fallback = loader_file(&gate.root, &fw.esp, from).ok_or_else(|| {
        no_loader(format!(
            "{} is in use, and its file is not on the EFI system partition to keep as the \
             fallback",
            from.name()
        ))
    })?;
    let names = packages(gate, to, &fw.esp)?;
    let mut steps = Vec::new();
    let mut invocations = Vec::new();
    if !names.is_empty() {
        let backend = pm::backend_for(
            gate.distro,
            &gate.root,
            gate.partial_upgrade,
            gate.operation,
        );
        if pm::Shape::of(gate.distro).refresh_is_an_action {
            invocations.extend(backend.refresh()?);
        }
        invocations.extend(backend.transaction(&names, &[])?);
        steps.extend(invocations.iter().map(Invocation::command_line));
    }
    let (mut writes, mut kernels_found, mut mkconfig) = (Vec::new(), Vec::new(), None);
    match to {
        Loader::SystemdBoot => {
            if loader_file(&gate.root, &fw.esp, to).is_none() {
                steps.push("bootctl install".to_string());
            }
            kernels_found = kernels(gate);
            if kernels_found.is_empty() {
                return Err(no_loader(
                    "no kernel under /usr/lib/modules for systemd-boot to start".into(),
                ));
            }
            if let Some(conf) = install_conf(gate, &fw.esp) {
                steps.push(format!("kernel-install to the ESP ({INSTALL_CONF})"));
                writes.push(conf);
            }
            writes.push(Write {
                path: CMDLINE.to_string(),
                bytes: Some(command_line(gate, manifest, lock).into_bytes()),
                mode: 0o644,
            });
            steps.extend(
                kernels_found
                    .iter()
                    .map(|(version, _)| format!("kernel-install add {version}")),
            );
        }
        Loader::Grub => {
            if loader_file(&gate.root, &fw.esp, to).is_none() && gate.distro != Distro::Fedora {
                steps.push("grub-install".to_string());
            }
            if super::basics::read(gate, INSTALL_CONF).is_some_and(|bytes| lodis(&bytes)) {
                steps.push(format!("remove {INSTALL_CONF}"));
                writes.push(Write {
                    path: INSTALL_CONF.to_string(),
                    bytes: None,
                    mode: 0o644,
                });
            }
            let invocation = super::kernel::mkconfig(gate, "[boot]")?;
            steps.push(invocation.command_line());
            mkconfig = Some(invocation);
        }
    }
    if trial {
        steps.push(format!(
            "a trial boot: {} once, by BootNext; {} stays first until confirmed, then the \
             fallback entry {fallback}",
            to.name(),
            from.name()
        ));
    } else {
        steps.push(format!(
            "{} stays as the fallback entry {fallback}",
            from.name()
        ));
    }
    Ok(BasicAction {
        key: "loader",
        change: Change::Set,
        what: format!("{} -> {}", from.name(), to.name()),
        detail: steps.join("; "),
        step: Step::Boot(Box::new(Stage::Switch(Box::new(Switch {
            from,
            to,
            esp: fw.esp.clone(),
            packages: invocations,
            writes,
            kernels: kernels_found,
            mkconfig,
            trial,
        })))),
    })
}

/// The file a loader's settings are in, and whether it is lodi's alone.
fn settings_file(gate: &Gate, esp: &str, loader: Loader) -> (String, bool) {
    match loader {
        Loader::SystemdBoot => (format!("/{esp}/loader/loader.conf"), false),
        Loader::Grub if gate.root.join("etc/default/grub.d").is_dir() => {
            (GRUB_DROP_IN.to_string(), true)
        }
        Loader::Grub => ("/etc/default/grub".to_string(), false),
    }
}

/// A setting's line, in the loader's own words.
fn setting_line(loader: Loader, key: &str, value: &str) -> String {
    match (loader, key) {
        (Loader::SystemdBoot, _) => format!("{key} {value}"),
        (Loader::Grub, "timeout") => format!("GRUB_TIMEOUT={value}"),
        (Loader::Grub, _) => format!("GRUB_DEFAULT=\"{value}\""),
    }
}

/// The value of lodi's line for `key` in a settings file, if it has one.
fn setting_in(text: &str, loader: Loader, key: &str) -> Option<String> {
    let marker = format!("{MARK} {key}");
    let mut lines = text.lines();
    lines.find(|line| *line == marker)?;
    let line = lines.next()?;
    Some(match loader {
        Loader::SystemdBoot => line.strip_prefix(&format!("{key} "))?.to_string(),
        Loader::Grub => line.split_once('=')?.1.trim_matches('"').to_string(),
    })
}

/// The settings file with lodi's line for `key` set to `line`, or taken out.
fn with_setting(text: &str, key: &str, line: Option<&str>) -> String {
    let marker = format!("{MARK} {key}");
    let mut out = String::new();
    let mut skip = false;
    for one in text.split_inclusive('\n') {
        if skip {
            skip = false;
        } else if one.trim_end() == marker {
            skip = true;
        } else {
            out.push_str(one);
        }
    }
    if let Some(line) = line {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("{marker}\n{line}\n"));
    }
    out
}

/// The timeout and default lines of each loader: the declared loader's as declared, the other's
/// left as they are, and both taken out once `[boot]` is gone.
fn settings(
    gate: &Gate,
    fw: &Firmware,
    declared: Option<&Boot>,
    out: &mut Vec<BasicAction>,
) -> Result<(), Diagnostic> {
    for loader in [Loader::Grub, Loader::SystemdBoot] {
        if declared.is_some_and(|boot| boot.loader != loader) {
            continue;
        }
        let (path, owned) = settings_file(gate, &fw.esp, loader);
        let text = super::basics::read(gate, &path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
        let wanted = [
            (
                "timeout",
                declared.and_then(|b| b.timeout).map(|n| n.to_string()),
            ),
            ("default", declared.and_then(|b| b.default.clone())),
        ];
        for (key, want) in wanted {
            let have = text.as_deref().and_then(|t| setting_in(t, loader, key));
            if have == want {
                continue;
            }
            let then = match loader {
                Loader::Grub => vec![super::kernel::mkconfig(gate, "[boot]")?],
                Loader::SystemdBoot => Vec::new(),
            };
            let detail = std::iter::once(path.clone())
                .chain(then.iter().map(Invocation::command_line))
                .collect::<Vec<_>>()
                .join("; ");
            let (change, what) = match (&have, &want) {
                (None, Some(want)) => (Change::Add, want.clone()),
                (Some(have), None) => (Change::Remove, have.clone()),
                (Some(have), Some(want)) => (Change::Set, format!("{have} -> {want}")),
                (None, None) => continue,
            };
            out.push(BasicAction {
                key,
                change,
                what,
                detail,
                step: Step::Boot(Box::new(Stage::Edit(Edit {
                    path: path.clone(),
                    key,
                    line: want.map(|value| setting_line(loader, key, &value)),
                    owned,
                    then,
                }))),
            });
        }
    }
    Ok(())
}

/// Plan the loader, then its settings, then systemd-boot's command line.
pub fn plan(
    gate: &Gate,
    manifest: &HostManifest,
    lock: Option<&HostLock>,
    out: &mut Vec<BasicAction>,
    record: &mut BTreeMap<String, BasicRecord>,
    notes: &mut Vec<String>,
) -> Result<(), Diagnostic> {
    let recorded = |key: &str| lock.and_then(|lock| lock.basics.get(key));
    let declared = manifest.boot.as_ref();
    let tried = SwitchTrial::read(gate).map(|trial| {
        let status = trial.status(gate);
        (trial, status)
    });
    if declared.is_none()
        && tried.is_none()
        && [LOADER_KEY, TIMEOUT_KEY, DEFAULT_KEY]
            .iter()
            .all(|k| recorded(k).is_none())
    {
        return Ok(());
    }
    let fw = Firmware::read(gate)?;
    let current = in_use(&gate.root, &fw)
        .ok_or_else(|| no_loader("no loader lodi knows starts this machine".into()))?;
    let was = recorded(LOADER_KEY)
        .and_then(|r| r.was.as_deref())
        .and_then(Loader::parse)
        .unwrap_or(current);
    let target = declared.map_or(was, |boot| boot.loader);
    if let Some(boot) = declared {
        let mut keep = |key: &str, value: String, was: Option<String>| {
            record.insert(key.to_string(), BasicRecord { value, was });
        };
        keep(LOADER_KEY, target.name().into(), Some(was.name().into()));
        if let Some(timeout) = boot.timeout {
            keep(TIMEOUT_KEY, timeout.to_string(), None);
        }
        if let Some(default) = &boot.default {
            keep(DEFAULT_KEY, default.clone(), None);
        }
    }
    // A switch to the declared loader boots once as a trial first (bv-1): while it waits, or
    // is the running boot, the plan says so; once it reverted, the switch is tried again.
    let waiting = match &tried {
        Some((trial, status)) if declared.is_some_and(|b| b.loader == trial.to) => {
            notes.push(trial.note(*status));
            *status != Status::Reverted
        }
        _ => false,
    };
    if current != target && !waiting {
        let trial = declared.is_some();
        out.push(switch(gate, &fw, current, target, manifest, lock, trial)?);
        if trial {
            let what = format!("{} -> {}", current.name(), target.name());
            notes.push(format!("= loader {what} ({})", trial::why(Status::Pending)));
        }
    }
    // A trial of a loader no longer declared goes, and the firmware's one-shot with it.
    if let Some((trial, status)) = tried.filter(|(t, _)| declared.is_none_or(|b| b.loader != t.to))
    {
        let mut run = Vec::new();
        if status == Status::Pending && var(&gate.root, "BootNext").is_some() {
            let efibootmgr = Tool::find(gate, "efibootmgr", "[boot] loader")?;
            run.push(efibootmgr.invocation(&["--delete-bootnext"]));
        }
        out.push(BasicAction {
            key: "loader",
            change: Change::Remove,
            what: format!("trial {}", trial.what()),
            detail: format!("{} stays first", trial.from.name()),
            step: Step::Trial {
                writes: vec![forget(SWITCH_STATE)],
                run,
                after: Vec::new(),
            },
        });
    }
    settings(gate, &fw, declared, out)?;
    // systemd-boot's command line, once it is in use; a switch writes it itself. A change boots
    // once as a trial first (bv-1); taking the parameters out is not a trial (B3).
    if current == target && declared.is_some_and(|b| b.loader == Loader::SystemdBoot) {
        let want = command_line(gate, manifest, lock);
        let have = super::basics::read(gate, CMDLINE).unwrap_or_default();
        if have != want.as_bytes() {
            let recorded = recorded(super::kernel::PARAMETERS_KEY)
                .map(|record| record.value.as_str())
                .unwrap_or_default();
            out.extend(cmdline(gate, &fw.esp, manifest, recorded, &want, notes)?);
        }
    }
    Ok(())
}

/// `kernel-install add` for each installed kernel, which writes its entry from
/// `/etc/kernel/cmdline`.
pub fn kernel_installs(gate: &Gate) -> Result<Vec<Invocation>, Diagnostic> {
    let tool = Tool::find(gate, "kernel-install", "[boot] loader")?;
    Ok(kernels(gate)
        .iter()
        .map(|(version, image)| {
            let image = gate.root.join(&image[1..]).display().to_string();
            tool.invocation(&["add", version, &image])
        })
        .collect())
}

/// systemd-boot's command line `want` in place of the one its entries boot.
fn cmdline(
    gate: &Gate,
    esp: &str,
    manifest: &HostManifest,
    recorded: &str,
    want: &str,
    notes: &mut Vec<String>,
) -> Result<Option<BasicAction>, Diagnostic> {
    let parameters = manifest.kernel.parameters.join(" ");
    let what = want.trim_end().to_string();
    if parameters.is_empty() {
        let then = kernel_installs(gate)?;
        let detail = std::iter::once(CMDLINE.to_string())
            .chain(then.iter().map(Invocation::command_line))
            .chain([format!("the entry before it as {}", trial::KNOWN_GOOD)])
            .collect::<Vec<_>>()
            .join("; ");
        let step = trial::sd_new_default(gate, esp, want, true, then)?;
        return Ok(Some(BasicAction {
            key: "cmdline",
            change: Change::Set,
            what,
            detail,
            step,
        }));
    }
    let tried = trial::Trial::read(gate)
        .filter(trial::Trial::is_systemd_boot)
        .map(|trial| {
            let status = trial.status(gate);
            (trial, status)
        });
    if let Some((trial, status)) = &tried {
        if trial.tries_cmdline(want) || *status == Status::Reverted {
            notes.push(trial.note(*status));
        }
        if trial.tries_cmdline(want) && *status != Status::Reverted {
            return Ok(None);
        }
    }
    // What the default boots: a waiting trial's own record of it, else the lock's.
    let confirmed = match &tried {
        Some((trial, _)) => trial.confirmed().unwrap_or_default().to_string(),
        None => recorded.to_string(),
    };
    let (detail, step, trial) = trial::sd_trial(gate, esp, want, &parameters, &confirmed)?;
    notes.push(trial.note(Status::Pending));
    Ok(Some(BasicAction {
        key: "cmdline",
        change: Change::Set,
        what,
        detail,
        step,
    }))
}

/// Carry out one `[boot]` stage.
pub fn perform(gate: &Gate, stage: &Stage) -> Result<(), Diagnostic> {
    match stage {
        Stage::Edit(edit) => {
            let text = super::basics::read(gate, &edit.path)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            let text = with_setting(&text, edit.key, edit.line.as_deref());
            let bytes = (!(edit.owned && text.is_empty())).then(|| text.into_bytes());
            super::basics::apply_write(
                gate,
                &Write {
                    path: edit.path.clone(),
                    bytes,
                    mode: 0o644,
                },
            )?;
            edit.then.iter().try_for_each(pm::run)
        }
        Stage::Switch(switch) => perform_switch(gate, switch),
    }
}

/// Every file under `dir`, with its bytes.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if let Ok(bytes) = std::fs::read(&path) {
                out.insert(path, bytes);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, &mut out);
    out
}

/// Put the files under `dir` back as `saved` holds them.
fn put_back(dir: &Path, saved: &BTreeMap<PathBuf, Vec<u8>>) -> std::io::Result<()> {
    for path in snapshot(dir).into_keys() {
        if !saved.contains_key(&path) {
            std::fs::remove_file(&path)?;
        }
    }
    for (path, bytes) in saved.iter().filter(|(path, _)| path.starts_with(dir)) {
        if std::fs::read(path).ok().as_ref() != Some(bytes) {
            std::fs::create_dir_all(path.parent().unwrap_or(dir))?;
            std::fs::write(path, bytes)?;
        }
    }
    Ok(())
}

/// The disk and partition number the ESP is on, as `efibootmgr` takes them.
fn esp_device(gate: &Gate, esp: &str) -> Option<(String, String)> {
    let mounts = std::fs::read_to_string(gate.root.join("proc/self/mountinfo")).ok()?;
    let target = format!("/{esp}");
    // An automounted ESP has an autofs line first, whose source is no device.
    let name = mounts.lines().find_map(|line| {
        let (head, tail) = line.split_once(" - ")?;
        let source = tail.split(' ').nth(1)?.strip_prefix("/dev/")?;
        (head.split(' ').nth(4)? == target).then(|| source.to_string())
    })?;
    let name = name.as_str();
    let part = std::fs::read_to_string(
        gate.root
            .join("sys/class/block")
            .join(name)
            .join("partition"),
    )
    .ok()?
    .trim()
    .to_string();
    let disk = name.trim_end_matches(|c: char| c.is_ascii_digit());
    let disk = match disk.strip_suffix('p') {
        Some(rest) if rest.ends_with(|c: char| c.is_ascii_digit()) => rest,
        _ => disk,
    };
    Some((format!("/dev/{disk}"), part))
}

/// The entry that starts `loader` from its own file, making one if there is none.
fn entry_for(gate: &Gate, efibootmgr: &Tool, esp: &str, loader: Loader) -> Result<u16, Diagnostic> {
    let find = || {
        let (order, entries) = nvram(&gate.root);
        let mut named: Vec<&Entry> = entries
            .iter()
            .filter(|entry| matches!(entry.target, Target::File(_)))
            .filter(|entry| started_by(&gate.root, esp, entry) == Some(loader))
            .collect();
        named.sort_by_key(|entry| {
            order
                .iter()
                .position(|n| *n == entry.number)
                .unwrap_or(usize::MAX)
        });
        named.first().map(|entry| entry.number)
    };
    if let Some(number) = find() {
        return Ok(number);
    }
    let file = loader_file(&gate.root, esp, loader).ok_or_else(|| {
        no_loader(format!(
            "{} is not on the EFI system partition",
            loader.name()
        ))
    })?;
    let (disk, part) = esp_device(gate, esp)
        .ok_or_else(|| no_loader(format!("cannot tell which disk and partition /{esp} is on")))?;
    let label = match loader {
        Loader::Grub => "GRUB",
        Loader::SystemdBoot => "Linux Boot Manager",
    };
    pm::run(&efibootmgr.invocation(&[
        "--create-only",
        "--disk",
        &disk,
        "--part",
        &part,
        "--label",
        label,
        "--loader",
        &file,
    ]))?;
    find().ok_or_else(|| no_loader(format!("efibootmgr made no entry for {}", loader.name())))
}

fn perform_switch(gate: &Gate, switch: &Switch) -> Result<(), Diagnostic> {
    let esp = gate.root.join(&switch.esp);
    let mut saved = snapshot(&esp);
    for write in &switch.writes {
        let path = gate.root.join(&write.path[1..]);
        if let Ok(bytes) = std::fs::read(&path) {
            saved.insert(path, bytes);
        }
    }
    let (order, entries) = nvram(&gate.root);
    let done = (|| {
        switch.packages.iter().try_for_each(pm::run)?;
        let efibootmgr = Tool::find(gate, "efibootmgr", "[boot] loader")?;
        let esp_arg = esp.display().to_string();
        match switch.to {
            Loader::SystemdBoot => {
                if !esp.join(SD_BOOT).is_file() {
                    let bootctl = Tool::find(gate, "bootctl", "[boot] loader")?;
                    pm::run(&bootctl.invocation(&[&format!("--esp-path={esp_arg}"), "install"]))?;
                }
                for write in &switch.writes {
                    super::basics::apply_write(gate, write)?;
                }
                let install = Tool::find(gate, "kernel-install", "[boot] loader")?;
                for (version, image) in &switch.kernels {
                    let image = gate.root.join(&image[1..]).display().to_string();
                    pm::run(&install.invocation(&["add", version, &image]))?;
                }
                let entries = snapshot(&esp.join("loader/entries"));
                if entries.is_empty() {
                    return Err(no_loader(
                        "systemd-boot was installed with no entry to start".into(),
                    ));
                }
            }
            Loader::Grub => {
                if loader_file(&gate.root, &switch.esp, Loader::Grub).is_none() {
                    let id = match gate.distro {
                        Distro::Debian => "debian",
                        Distro::Ubuntu => "ubuntu",
                        _ => "GRUB",
                    };
                    let grub = Tool::find(gate, "grub-install", "[boot] loader")?;
                    pm::run(&grub.invocation(&[
                        "--target=x86_64-efi",
                        &format!("--efi-directory={esp_arg}"),
                        &format!("--bootloader-id={id}"),
                    ]))?;
                }
                for write in &switch.writes {
                    super::basics::apply_write(gate, write)?;
                }
                switch.mkconfig.iter().try_for_each(pm::run)?;
            }
        }
        // The firmware's removable path keeps starting the loader it started.
        let removable = esp.join(REMOVABLE);
        put_back(&removable, &saved).map_err(|e| {
            Diagnostic::new(
                "E_APPLY",
                format!("cannot put back {}: {e}", removable.display()),
            )
        })?;
        let first = entry_for(gate, &efibootmgr, &switch.esp, switch.to)?;
        let second = entry_for(gate, &efibootmgr, &switch.esp, switch.from)?;
        if !switch.trial {
            set_order(gate, &efibootmgr, first, second)?;
            if nvram(&gate.root).0.first() != Some(&first) {
                return Err(no_loader(format!(
                    "the firmware does not start {} first",
                    switch.to.name()
                )));
            }
            return Ok(());
        }
        // The trial (bv-1): the earlier loader stays first, and the new one starts once.
        set_order(gate, &efibootmgr, second, first)?;
        let number = format!("{first:04X}");
        pm::run(&efibootmgr.invocation(&["--bootnext", &number]))?;
        let next = var(&gate.root, "BootNext").and_then(|d| d.get(4..6).map(words));
        if nvram(&gate.root).0.first() != Some(&second) || next != Some(vec![first]) {
            return Err(no_loader(format!(
                "the firmware does not start {} once, next",
                switch.to.name()
            )));
        }
        let state = format!(
            "loader={}\nfrom={}\nentry={number}\nboot_id={}\n",
            switch.to.name(),
            switch.from.name(),
            trial::boot_id(gate)
        );
        super::basics::apply_write(
            gate,
            &Write {
                path: SWITCH_STATE.to_string(),
                bytes: Some(state.into_bytes()),
                mode: 0o600,
            },
        )
    })();
    let Err(mut error) = done else {
        return Ok(());
    };
    // Put the ESP's files, the boot order and the entries back as they were.
    let mut restored = put_back(&esp, &saved).is_ok();
    for write in &switch.writes {
        let path = gate.root.join(&write.path[1..]);
        restored &= match saved.get(&path) {
            Some(bytes) => std::fs::write(&path, bytes).is_ok(),
            None => !path.exists() || std::fs::remove_file(&path).is_ok(),
        };
    }
    if let Ok(efibootmgr) = Tool::find(gate, "efibootmgr", "[boot] loader") {
        if switch.trial && var(&gate.root, "BootNext").is_some() {
            restored &= pm::run(&efibootmgr.invocation(&["--delete-bootnext"])).is_ok();
        }
        for entry in nvram(&gate.root).1 {
            if !entries.iter().any(|kept| kept.number == entry.number) {
                let number = format!("{:04X}", entry.number);
                restored &=
                    pm::run(&efibootmgr.invocation(&["--delete-bootnum", "--bootnum", &number]))
                        .is_ok();
            }
        }
        if nvram(&gate.root).0 != order {
            let hex: Vec<String> = order.iter().map(|n| format!("{n:04X}")).collect();
            restored &= pm::run(&efibootmgr.invocation(&["--bootorder", &hex.join(",")])).is_ok();
        }
    }
    error.message = if restored {
        format!(
            "{}; the EFI system partition and the boot order were put back, and {} still starts",
            error.message,
            switch.from.name()
        )
    } else {
        format!(
            "{}; the EFI system partition or the boot order could not all be put back",
            error.message
        )
    };
    Err(error)
}
