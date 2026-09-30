//! `[network]`: the interfaces a host declares, configured through the machine's own network
//! stack and staged, with a timed rollback (sd-1, LD-420; S3).
//!
//! lodi drives the stack the machine runs and never switches it (LD-430): netplan when
//! `/etc/netplan` holds a configuration, else NetworkManager or systemd-networkd, whichever is
//! active. Two active, or none of them (ifupdown, say), stops the plan with `E_NETWORK_STACK`.
//! lodi writes files of its own that sort before the distribution's and never edits the
//! distribution's: `/etc/netplan/90-lodi.yaml`, `/etc/systemd/network/00-lodi-NAME.network` or
//! `/etc/NetworkManager/system-connections/lodi-NAME.nmconnection`.
//!
//! A change is staged. Before it, lodi keeps the files it is about to change under
//! `/etc/lodi/network-rollback/` with a script that puts them back, and arms a transient timer,
//! `lodi-network-rollback`, that runs the script if lodi itself is gone. Then it writes the
//! files, has the stack apply them, and pings one address: `[network] check`, else the first
//! declared gateway, else the default gateway the machine has now. An answer within
//! `confirm_within` seconds confirms the change: the timer is stopped and the kept files removed.
//! No answer, and lodi puts the earlier files back, has the stack apply them, and stops with
//! `E_NETWORK_ROLLED_BACK`; the record keeps the last confirmed configuration.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use crate::diag::Diagnostic;

use super::basics::{self, BasicAction, Change, Step, Tool, Write};
use super::lock::{BasicRecord, HostLock};
use super::manifest::{HostManifest, Interface, Network};
use super::pm::{self, Invocation};
use super::safety::Gate;
use super::services::Systemctl;

/// Where a staged change keeps what it replaced, inside the root.
const KEPT: &str = "/etc/lodi/network-rollback";
const TIMER: &str = "lodi-network-rollback";
/// The seconds the timer waits beyond `confirm_within`: lodi puts the files back itself first.
const GRACE: u64 = 30;
const HEADER: &str = "# Written by lodi from host.toml [network]; lodi removes it when [network] \
                      is gone.\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stack {
    Netplan,
    Networkd,
    NetworkManager,
}

impl Stack {
    fn name(self) -> &'static str {
        match self {
            Stack::Netplan => "netplan",
            Stack::Networkd => "systemd-networkd",
            Stack::NetworkManager => "NetworkManager",
        }
    }

    fn tool(self) -> &'static str {
        match self {
            Stack::Netplan => "netplan",
            Stack::Networkd => "networkctl",
            Stack::NetworkManager => "nmcli",
        }
    }

    /// The file lodi writes for one interface.
    fn path(self, interface: &str) -> String {
        match self {
            Stack::Netplan => "/etc/netplan/90-lodi.yaml".to_string(),
            Stack::Networkd => format!("/etc/systemd/network/00-lodi-{interface}.network"),
            Stack::NetworkManager => {
                format!("/etc/NetworkManager/system-connections/lodi-{interface}.nmconnection")
            }
        }
    }

    fn mode(self) -> u32 {
        match self {
            Stack::Networkd => 0o644,
            Stack::Netplan | Stack::NetworkManager => 0o600,
        }
    }
}

/// A staged network change, as the plan built it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    stack: Stack,
    tool: Tool,
    /// Each file with the interface it configures (none for netplan's one file).
    writes: Vec<(Write, Option<String>)>,
    probe: Invocation,
    target: String,
    within: u64,
    arm: Invocation,
    disarm: Invocation,
    /// The restore script the timer runs, at its path inside the root.
    shell: String,
}

impl Stage {
    pub fn invocations(&self) -> Vec<&Invocation> {
        vec![&self.arm, &self.probe, &self.disarm]
    }

    /// What has the stack apply its files, given which of lodi's files exist afterwards.
    fn activate(&self, after: &[(bool, Option<String>)]) -> Vec<Invocation> {
        match self.stack {
            Stack::Netplan => vec![self.tool.invocation(&["apply"])],
            Stack::Networkd => {
                let mut args = vec!["reconfigure", "--"];
                args.extend(after.iter().filter_map(|(_, name)| name.as_deref()));
                vec![
                    self.tool.invocation(&["reload"]),
                    self.tool.invocation(&args),
                ]
            }
            Stack::NetworkManager => {
                let mut out = vec![self.tool.invocation(&["connection", "reload"])];
                for (present, name) in after {
                    let name = name.as_deref().unwrap_or_default();
                    let id = format!("lodi-{name}");
                    out.push(match present {
                        true => self.tool.invocation(&["connection", "up", "id", &id]),
                        false => self.tool.invocation(&["device", "connect", name]),
                    });
                }
                out
            }
        }
    }
}

/// The stack this machine runs, or why lodi will not drive one.
fn detect(gate: &Gate, systemctl: &Systemctl) -> Result<Stack, Diagnostic> {
    let netplan = std::fs::read_dir(gate.root.join("etc/netplan"))
        .map(|dir| {
            dir.flatten()
                .any(|entry| entry.file_name().to_string_lossy().ends_with(".yaml"))
        })
        .unwrap_or(false);
    if netplan {
        return Ok(Stack::Netplan);
    }
    let active = |unit: &str| -> Result<bool, Diagnostic> {
        Ok(systemctl.observe(unit)?.is_some_and(|(_, running)| running))
    };
    match (
        active("NetworkManager.service")?,
        active("systemd-networkd.service")?,
    ) {
        (true, false) => Ok(Stack::NetworkManager),
        (false, true) => Ok(Stack::Networkd),
        (both, _) => Err(Diagnostic::new(
            "E_NETWORK_STACK",
            if both {
                "NetworkManager and systemd-networkd are both active, so which one owns an \
                 interface is not lodi's to guess"
                    .to_string()
            } else {
                "no netplan, NetworkManager or networkd is running".to_string()
            },
        )
        .hint("nothing changed; run one of them, or take [network] out")),
    }
}

/// The default gateway in the root's `proc/net/route`, if there is one.
fn default_gateway(gate: &Gate) -> Option<String> {
    let table = std::fs::read_to_string(gate.root.join("proc/net/route")).ok()?;
    table.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        (fields.get(1) == Some(&"00000000") && fields.get(7) == Some(&"00000000"))
            .then(|| u32::from_str_radix(fields.get(2)?, 16).ok())
            .flatten()
            .filter(|gateway| *gateway != 0)
            .map(|gateway| std::net::Ipv4Addr::from(gateway.to_le_bytes()).to_string())
    })
}

fn is_v6(address: &str) -> bool {
    address.contains(':')
}

/// Each file lodi writes for `network`, by path, with the interface it configures.
fn render(stack: Stack, network: &Network) -> BTreeMap<String, (String, Option<String>)> {
    let mut out = BTreeMap::new();
    if stack == Stack::Netplan {
        let mut text = format!("{HEADER}network:\n  version: 2\n  ethernets:\n");
        for (name, interface) in &network.interfaces {
            let _ = writeln!(text, "    {name}:\n      dhcp4: {}", interface.dhcp);
            let list = |text: &mut String, key: &str, values: &[String], indent: &str| {
                if !values.is_empty() {
                    let _ = writeln!(text, "{indent}{key}:");
                    for value in values {
                        let _ = writeln!(text, "{indent}  - \"{value}\"");
                    }
                }
            };
            list(&mut text, "addresses", &interface.addresses, "      ");
            if let Some(gateway) = &interface.gateway {
                let _ = writeln!(
                    text,
                    "      routes:\n        - to: default\n          via: \"{gateway}\""
                );
            }
            if !interface.dns.is_empty() {
                text.push_str("      nameservers:\n");
                list(&mut text, "addresses", &interface.dns, "        ");
            }
        }
        out.insert(stack.path(""), (text, None));
        return out;
    }
    for (name, interface) in &network.interfaces {
        let text = match stack {
            Stack::Networkd => networkd(name, interface),
            _ => keyfile(name, interface),
        };
        out.insert(stack.path(name), (text, Some(name.clone())));
    }
    out
}

fn networkd(name: &str, interface: &Interface) -> String {
    let mut text = format!(
        "{HEADER}[Match]\nName={name}\n\n[Network]\nDHCP={}\n",
        if interface.dhcp { "yes" } else { "no" }
    );
    for address in &interface.addresses {
        let _ = writeln!(text, "Address={address}");
    }
    if let Some(gateway) = &interface.gateway {
        let _ = writeln!(text, "Gateway={gateway}");
    }
    for dns in &interface.dns {
        let _ = writeln!(text, "DNS={dns}");
    }
    text
}

fn keyfile(name: &str, interface: &Interface) -> String {
    let mut text = format!(
        "{HEADER}[connection]\nid=lodi-{name}\ntype=ethernet\ninterface-name={name}\n\
         autoconnect-priority=999\n"
    );
    for (family, v6) in [("ipv4", false), ("ipv6", true)] {
        let addresses: Vec<&String> = interface
            .addresses
            .iter()
            .filter(|a| is_v6(a) == v6)
            .collect();
        let method = match (interface.dhcp, addresses.is_empty()) {
            (true, _) => "auto",
            (false, false) => "manual",
            (false, true) if v6 => "ignore",
            (false, true) => "disabled",
        };
        let _ = writeln!(text, "\n[{family}]\nmethod={method}");
        for (index, address) in addresses.iter().enumerate() {
            let _ = writeln!(text, "address{}={address}", index + 1);
        }
        if let Some(gateway) = interface.gateway.as_ref().filter(|g| is_v6(g) == v6) {
            let _ = writeln!(text, "gateway={gateway}");
        }
        let dns: Vec<&String> = interface.dns.iter().filter(|d| is_v6(d) == v6).collect();
        if !dns.is_empty() {
            let joined: Vec<&str> = dns.iter().map(|d| d.as_str()).collect();
            let _ = writeln!(text, "dns={};", joined.join(";"));
        }
    }
    text
}

type Planned = Option<(BasicAction, Option<BasicRecord>)>;

pub fn plan(
    gate: &Gate,
    manifest: &HostManifest,
    lock: Option<&HostLock>,
) -> Result<Planned, Diagnostic> {
    let declared = manifest.network.as_ref();
    let recorded: Vec<String> = lock
        .and_then(|lock| lock.basics.get("network"))
        .map(|record| record.value.split(' ').map(str::to_string).collect())
        .unwrap_or_default();
    if declared.is_none() && recorded.is_empty() {
        return Ok(None);
    }
    if gate.root.join(&KEPT[1..]).exists() {
        return Err(Diagnostic::new(
            "E_NETWORK_STACK",
            format!("an earlier network change still waits for its confirmation: {KEPT} is there"),
        )
        .hint(format!(
            "nothing changed; the timer {TIMER} puts that change back by itself: wait for it, or \
             remove {KEPT} once the network is as it should be"
        )));
    }
    let systemctl = Systemctl::find(gate)?;
    let stack = detect(gate, &systemctl)?;
    let desired = declared
        .map(|network| render(stack, network))
        .unwrap_or_default();
    let mut owned: BTreeMap<String, Option<String>> = desired
        .iter()
        .map(|(path, (_, name))| (path.clone(), name.clone()))
        .collect();
    for name in &recorded {
        let path = stack.path(name);
        owned
            .entry(path)
            .or_insert_with(|| (stack != Stack::Netplan).then(|| name.clone()));
    }
    let mut writes = Vec::new();
    let mut fresh = true;
    for (path, name) in owned {
        let now = basics::read(gate, &path);
        fresh &= now.is_none();
        let want = desired.get(&path).map(|(text, _)| text.as_bytes().to_vec());
        if now != want {
            let write = Write {
                path,
                bytes: want,
                mode: stack.mode(),
            };
            writes.push((write, name));
        }
    }
    let names = declared
        .map(|network| network.interfaces.keys().cloned().collect::<Vec<_>>())
        .unwrap_or(recorded);
    let kept = declared.map(|_| BasicRecord {
        value: names.join(" "),
        was: None,
    });
    let what = names.join(", ");
    if writes.is_empty() {
        return Ok(declared.map(|_| {
            let action = BasicAction {
                key: "network",
                change: Change::Unchanged,
                what,
                detail: String::new(),
                step: Step::None,
            };
            (action, kept)
        }));
    }
    let within = declared.map_or(super::manifest::CONFIRM_WITHIN, |n| n.confirm_within);
    let target = declared
        .and_then(|network| {
            network.check.clone().or_else(|| {
                network
                    .interfaces
                    .values()
                    .find_map(|interface| interface.gateway.clone())
            })
        })
        .or_else(|| default_gateway(gate))
        .ok_or_else(|| {
            Diagnostic::new(
                "E_NETWORK_STACK",
                "nothing can confirm this network change: no [network] check, no gateway \
                 declared, and no default route",
            )
            .hint(
                "nothing changed; name an address that answers once the change works, as \
                   [network] check",
            )
        })?;
    let tool = Tool::find(gate, stack.tool(), "[network]")?;
    let ping = Tool::find(gate, "ping", "[network]")?;
    let run = Tool::find(gate, "systemd-run", "[network]")?;
    let script = format!("{KEPT}/restore.sh");
    let on_active = format!("--on-active={}s", within + GRACE);
    let unit = format!("--unit={TIMER}");
    let files: Vec<&str> = writes
        .iter()
        .map(|(write, _)| write.path.as_str())
        .collect();
    let detail = format!(
        "{} {}; ping {target} within {within} s, else put back",
        stack.name(),
        files.join(", ")
    );
    let stage = Stage {
        stack,
        tool,
        probe: ping.invocation(&["-c", "1", "-W", "1", "--", &target]),
        arm: run.invocation(&[
            &unit,
            &on_active,
            "--timer-property=AccuracySec=1s",
            "--",
            "/bin/sh",
            &script,
        ]),
        disarm: systemctl.invocation(&["stop"], &format!("{TIMER}.timer")),
        writes,
        target,
        within,
        shell: script,
    };
    let change = match (declared, fresh) {
        (None, _) => Change::Remove,
        (Some(_), true) => Change::Add,
        (Some(_), false) => Change::Set,
    };
    Ok(Some((
        BasicAction {
            key: "network",
            change,
            what,
            detail,
            step: Step::Network(Box::new(stage)),
        },
        kept,
    )))
}

/// A shell word for the restore script: every argument lodi puts there is a fixed word, a path
/// lodi chose or a name the manifest grammar held to, and is quoted all the same.
fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Stage the change, confirm it, and put the earlier files back when it is not confirmed.
pub fn perform(gate: &Gate, stage: &Stage) -> Result<(), Diagnostic> {
    let before: Vec<(Write, Option<String>)> = stage
        .writes
        .iter()
        .map(|(write, name)| {
            let write = Write {
                bytes: basics::read(gate, &write.path),
                ..write.clone()
            };
            (write, name.clone())
        })
        .collect();
    let after_of = |writes: &[(Write, Option<String>)]| -> Vec<(bool, Option<String>)> {
        writes
            .iter()
            .map(|(write, name)| (write.bytes.is_some(), name.clone()))
            .collect()
    };
    let forward = stage.activate(&after_of(&stage.writes));
    let back = stage.activate(&after_of(&before));

    // What puts the earlier files back if lodi is gone: the kept files and a script.
    let mut script = String::from(
        "#!/bin/sh\n# Written by lodi: puts back the network configuration a change replaced, \
         when lodi did not confirm the change.\n",
    );
    for (index, (write, _)) in before.iter().enumerate() {
        match &write.bytes {
            Some(bytes) => {
                let kept = format!("{KEPT}/{}", index + 1);
                super::files::write_owned(&gate.root, &kept, bytes, write.mode)?;
                let _ = writeln!(script, "cp -p {} {}", quote(&kept), quote(&write.path));
            }
            None => {
                let _ = writeln!(script, "rm -f {}", quote(&write.path));
            }
        }
    }
    for invocation in &back {
        let mut line = quote(&invocation.path.display().to_string());
        for arg in &invocation.args {
            line.push(' ');
            line.push_str(&quote(arg));
        }
        let _ = writeln!(script, "{line}");
    }
    let _ = writeln!(script, "rm -rf {}", quote(KEPT));
    super::files::write_owned(&gate.root, &stage.shell, script.as_bytes(), 0o700)?;
    if let Err(error) = pm::run(&stage.arm) {
        forget(gate);
        return Err(error);
    }

    for (write, _) in &stage.writes {
        basics::apply_write(gate, write)?;
    }
    let applied = forward.iter().try_for_each(pm::run);
    let confirmed = applied.is_ok() && confirm(stage);
    if !confirmed {
        for (write, _) in &before {
            basics::apply_write(gate, write)?;
        }
        back.iter().try_for_each(pm::run)?;
    }
    let _ = pm::run(&stage.disarm);
    forget(gate);
    if confirmed {
        return Ok(());
    }
    let why = match applied {
        Err(error) => error.message,
        Ok(()) => format!("{}: no answer in {} s", stage.target, stage.within),
    };
    Err(
        Diagnostic::new("E_NETWORK_ROLLED_BACK", format!("{why}; nothing kept"))
            .hint("the earlier network configuration is back; fix [network], apply again"),
    )
}

/// Ping the target until it answers or `within` seconds have passed.
fn confirm(stage: &Stage) -> bool {
    let deadline = Instant::now() + Duration::from_secs(stage.within);
    loop {
        // The stack settles before each probe, so the old configuration never answers for it.
        std::thread::sleep(Duration::from_secs(1));
        let answered = stage
            .probe
            .command()
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if answered {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
    }
}

/// Remove the kept files once they are not needed.
fn forget(gate: &Gate) {
    let _ = std::fs::remove_dir_all(gate.root.join(&KEPT[1..]));
}
