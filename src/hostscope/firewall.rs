//! `[firewall]`: one nftables table, `inet lodi`, that lodi owns and nothing else (sd-1, LD-420).
//!
//! The table has one input chain: what is established, the loopback interface and ICMP come in,
//! each `allow` entry comes in, and everything else is dropped (or, with `input = "accept"`,
//! let in). lodi writes the table as `/etc/lodi/firewall.nft` and loads it with `nft -f`; the
//! file creates the table, deletes it and declares it again, which nftables does as one atomic
//! transaction, and it names no other table and never flushes the ruleset. A unit of lodi's own,
//! `lodi-firewall.service`, loads the same file at boot. The table carries the digest of what it
//! was written from as its comment, which is how a plan knows it is as declared.
//!
//! Never two firewalls (S6): while `ufw` (enabled in `/etc/ufw/ufw.conf`) or `firewalld` is
//! active, a declared `[firewall]` stops the plan with `E_FIREWALL_CONFLICT`, before anything
//! changes. Once `[firewall]` is gone, the apply disables the unit, removes both files and
//! deletes the table.

use std::collections::BTreeMap;

use crate::diag::Diagnostic;
use crate::util::sha256_hex;

use super::basics::{BasicAction, Change, Step, Tool, Write};
use super::lock::{BasicRecord, HostLock};
use super::manifest::{Firewall, HostManifest};
use super::safety::Gate;
use super::services::Systemctl;

/// The table lodi owns, as `nft` names it.
pub const TABLE: &str = "inet lodi";
const SCRIPT: &str = "/etc/lodi/firewall.nft";
const UNIT_NAME: &str = "lodi-firewall.service";
const UNIT: &str = "/etc/systemd/system/lodi-firewall.service";
/// The firewalls lodi never runs beside (S6).
const CONFLICTS: &[&str] = &["ufw.service", "firewalld.service"];

/// The file `nft -f` loads, and the digest its table carries.
fn script(firewall: &Firewall) -> (String, String) {
    let policy = if firewall.drop { "drop" } else { "accept" };
    let mut chain = format!(
        "\tchain input {{\n\t\ttype filter hook input priority filter; policy {policy};\n\
         \t\tct state established,related accept\n\t\tiif \"lo\" accept\n\
         \t\tmeta l4proto {{ icmp, ipv6-icmp }} accept\n"
    );
    for (proto, ports) in &firewall.allow {
        chain.push_str(&format!("\t\t{proto} dport {ports} accept\n"));
    }
    chain.push_str("\t}\n");
    let digest = sha256_hex(chain.as_bytes());
    let text = format!(
        "# Written by lodi from host.toml [firewall]: lodi's own table, and no other.\n\
         table {TABLE}\ndelete table {TABLE}\ntable {TABLE} {{\n\
         \tcomment \"lodi sha256:{digest}\"\n{chain}}}\n"
    );
    (text, digest)
}

/// The unit that loads the table at boot.
fn unit(nft: &Tool) -> String {
    format!(
        "# Written by lodi from host.toml [firewall]: loads lodi's own nftables table at boot.\n\
         [Unit]\nDescription=lodi's nftables table ({TABLE})\nDefaultDependencies=no\n\
         Wants=network-pre.target\nBefore=network-pre.target shutdown.target\n\
         Conflicts=shutdown.target\n\n[Service]\nType=oneshot\nRemainAfterExit=yes\n\
         ExecStart={} -f {SCRIPT}\n\n[Install]\nWantedBy=sysinit.target\n",
        nft.path.display()
    )
}

/// The digest `nft list table inet lodi` shows in the table's comment.
fn listed_digest(listing: &str) -> Option<&str> {
    listing.lines().find_map(|line| {
        line.trim()
            .strip_prefix("comment \"lodi sha256:")?
            .strip_suffix('"')
    })
}

type Planned = Option<(BasicAction, Option<BasicRecord>)>;

pub fn plan(
    gate: &Gate,
    manifest: &HostManifest,
    lock: Option<&HostLock>,
) -> Result<Planned, Diagnostic> {
    let declared = manifest.firewall.as_ref();
    let recorded = lock.and_then(|lock| lock.basics.get("firewall"));
    if declared.is_none() && recorded.is_none() {
        return Ok(None);
    }
    let systemctl = Systemctl::find(gate)?;
    if declared.is_some() {
        let mut active = Vec::new();
        for name in CONFLICTS {
            // Ubuntu runs ufw's unit whether ufw is on or not: its own file says which.
            let on = *name != "ufw.service"
                || std::fs::read_to_string(gate.root.join("etc/ufw/ufw.conf"))
                    .is_ok_and(|conf| conf.lines().any(|line| line.trim() == "ENABLED=yes"));
            if on && systemctl.observe(name)?.is_some_and(|(_, running)| running) {
                active.push(*name);
            }
        }
        if !active.is_empty() {
            // Never two firewalls (S6).
            return Err(Diagnostic::new(
                "E_FIREWALL_CONFLICT",
                format!("{} is active beside [firewall]", active.join(" and ")),
            )
            .hint("nothing changed; disable it, or take [firewall] out"));
        }
    }
    let nft = Tool::find(gate, "nft", "[firewall]")?;
    let listing = nft.read(&["list", "table", "inet", "lodi"])?;
    let script_now = super::basics::read(gate, SCRIPT);
    let unit_now = super::basics::read(gate, UNIT);
    let script_path = gate.root.join(&SCRIPT[1..]).display().to_string();
    let Some(firewall) = declared else {
        if listing.is_none() && script_now.is_none() && unit_now.is_none() {
            return Ok(None);
        }
        let delete = nft.invocation(&["delete", "table", "inet", "lodi"]);
        let removes = [SCRIPT, UNIT].map(|path| Write {
            path: path.to_string(),
            bytes: None,
            mode: 0o644,
        });
        return Ok(Some((
            BasicAction {
                key: "firewall",
                change: Change::Remove,
                what: format!("table {TABLE}"),
                detail: delete.command_line(),
                step: Step::Run {
                    first: unit_now
                        .iter()
                        .map(|_| systemctl.invocation(&["disable"], UNIT_NAME))
                        .collect(),
                    writes: removes.into(),
                    then: listing.iter().map(|_| delete.clone()).collect(),
                },
            },
            None,
        )));
    };
    let (text, digest) = script(firewall);
    let unit_text = unit(&nft);
    let enabled = systemctl
        .observe(UNIT_NAME)?
        .is_some_and(|(state, _)| state == "enabled");
    let same = listing.as_deref().and_then(listed_digest) == Some(digest.as_str())
        && script_now.as_deref() == Some(text.as_bytes())
        && unit_now.as_deref() == Some(unit_text.as_bytes())
        && enabled;
    let kept = BasicRecord {
        value: format!("sha256:{digest}"),
        was: None,
    };
    let load = nft.invocation(&["-f", &script_path]);
    let action = BasicAction {
        key: "firewall",
        change: match (same, listing.is_none() && recorded.is_none()) {
            (true, _) => Change::Unchanged,
            (false, true) => Change::Add,
            (false, false) => Change::Set,
        },
        what: format!("table {TABLE}"),
        detail: load.command_line(),
        step: if same {
            Step::None
        } else {
            let writes = BTreeMap::from([(SCRIPT, text), (UNIT, unit_text)])
                .into_iter()
                .map(|(path, text)| Write {
                    path: path.to_string(),
                    bytes: Some(text.into_bytes()),
                    mode: 0o644,
                })
                .collect();
            Step::Run {
                first: Vec::new(),
                writes,
                then: vec![load, systemctl.invocation(&["enable"], UNIT_NAME)],
            }
        },
    };
    Ok(Some((action, Some(kept))))
}
