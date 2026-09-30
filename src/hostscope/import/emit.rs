//! The emitter: one machine's reading, written as the `host.toml` a person can read.
//!
//! It writes **text**, not a serialized structure: an emitted manifest is the user's own TOML,
//! which `docs/SCHEMAS.md` excludes from `src/schema.rs`'s closed registry by name, so no
//! `Serialize`-deriving type is added here and `tests/schema.rs` stays green.
//!
//! # What the bytes carry, and what they must never carry
//!
//! The emitted file names the distribution and its release **codename**, the packages a person
//! chose, the holds the machine keeps, and the snapshot instant the reading was taken at. It
//! carries no host name, no user name, no home path, no address — except the `https://` URI of a
//! commented `[sources]` block, which is what an apply would arm (LD-367) — no `lodi` version and
//! no package version — a guest's emitted bundle is committed as a fixture, and `AGENTS.md` §1.2
//! forbids the first four from ever entering this repository. The release is the codename and
//! never the number, because a digit-dotted release is indistinguishable from a version of
//! something and the test that enforces the rule should not have to tell them apart.
//!
//! The snapshot is the one date, and it is the owner's decision of 2026-09-22, which supersedes
//! design call D10's "no date at all" forward. It is a value the caller supplies
//! ([`super::Snapshot`]), never a clock this module reads, so the function below is pure: the
//! same machine and the same instant give the same bytes, always.
//!
//! # The comment block is half the feature
//!
//! Everything the emitter leaves out is named at the end, under `NOT CAPTURED`, with the reason.
//! A file that quietly omitted the packages an apply could not install would teach a person that
//! Lodi restores a machine, which it does not.

use std::fmt::Write;

use super::super::pm::Shape;
use super::super::safety::Distro;
use super::baseline::{self, Labelled, Selection};
use super::files::Capture;
use super::{Machine, Snapshot};

/// The whole emitted manifest for one machine, read at one instant.
///
/// `files` is the configuration capture of T-2, and `None` is a manifest that declares packages
/// alone — which is what `lodi host import --stdout` emits, because a declaration whose `source`
/// points at a bundle nobody wrote would be a manifest that cannot apply. A capture is placed
/// **before** the comment block, so that everything the emitter left out stays last.
pub fn manifest(machine: &Machine, snapshot: &Snapshot, files: Option<&Capture>) -> String {
    let selection = baseline::select(machine);
    let mut out = String::new();
    header(&mut out, machine);
    host(&mut out, machine, snapshot);
    packages(&mut out, machine, &selection);
    super::sources::emit(&mut out, machine, files.is_some());
    system(&mut out, machine);
    users(&mut out, machine);
    not_captured(&mut out, machine, &selection, files);
    out
}

/// The typed settings (si-1): `[system]` as the machine has it, and the tables lodi reads no
/// value of, as comments that say what each one declares.
fn system(out: &mut String, machine: &Machine) {
    if !machine.system.is_empty() {
        out.push_str(
            "# The machine's own settings. An apply sets each one that differs, and leaves a\n\
             # line you take out as it is.\n",
        );
        out.push_str("[system]\n");
        for (key, value) in &machine.system {
            let _ = writeln!(out, "{key} = \"{value}\"");
        }
        out.push('\n');
    }
    out.push_str(
        "# Tables lodi does not read off a machine, to write on purpose:\n\
         # [services]\n\
         # \"ssh.service\" = true          # enabled and started; false disables and stops it\n\
         # [firewall]\n\
         # allow = [\"22/tcp\"]             # lodi's own nftables table; the rest is dropped\n\
         # [network]\n\
         # interfaces.eth0 = { dhcp = true }  # put back if the machine is cut off\n\
         # [etc.\"motd\"]\n\
         # text = \"...\"                   # a file under /etc holding exactly this text\n\n",
    );
}

/// The `[users]` half (su-1, S1): one table per human account, with its UID, supplementary
/// groups, shell and home, and never a password, a hash or a key.
fn users(out: &mut String, machine: &Machine) {
    if machine.users.is_empty() {
        return;
    }
    out.push_str(
        "# The human accounts of the machine that was read: a UID from UID_MIN and a login\n\
         # shell. An apply creates a missing one with a locked password, and never changes a\n\
         # password. No password, hash or key is written here; add public keys with ssh_keys.\n\
         # An account taken out of this file is left on the machine as it is, with its home.\n\n",
    );
    for user in &machine.users {
        let _ = writeln!(out, "[users.{}]", user.name);
        let _ = writeln!(out, "uid = {}", user.uid);
        let groups: Vec<String> = user.groups.iter().map(|g| format!("\"{g}\"")).collect();
        let _ = writeln!(out, "groups = [{}]", groups.join(", "));
        let _ = writeln!(out, "shell = \"{}\"", user.shell);
        if super::super::manifest::normalize_absolute(&user.home).as_deref() == Some(&user.home) {
            let _ = writeln!(out, "home = \"{}\"", user.home);
        }
        out.push('\n');
    }
}

/// The header's claim to name nobody, and what it says instead when the file names accounts.
const NAMES_NOBODY: (&str, &str) = (
    "# had installed and chose, and its settings. It says nothing about who runs it.",
    "# had installed and chose, its settings and its human accounts.",
);
/// What the header says the file does not carry, and the same when it carries accounts.
const NOT_CARRIED: (&str, &str) = (
    "# services, users, or package versions:",
    "# services, passwords, or package versions:",
);

fn header(out: &mut String, machine: &Machine) {
    if machine.users.is_empty() {
        plain_header(out, machine);
        return;
    }
    // The file names the machine's own accounts, so it no longer says it names nobody.
    let mut text = String::new();
    plain_header(&mut text, machine);
    out.push_str(
        &text
            .replace(NAMES_NOBODY.0, NAMES_NOBODY.1)
            .replace(NOT_CARRIED.0, NOT_CARRIED.1),
    );
}

fn plain_header(out: &mut String, machine: &Machine) {
    out.push_str(
        "# Generated by lodi from one machine's own package database. It says what that machine\n\
         # had installed and chose, and its settings. It says nothing about who runs it.\n\
         #\n",
    );
    let _ = writeln!(out, "# distribution: {}", applicability(machine));
    // On both families the dated archive is the source of Lodi's own transaction (LD-396); on
    // Fedora it is the repositories (LD-432).
    out.push_str(
        "#\n\
         # This is a starting point, not a backup. It does not carry anything installed from\n\
         # third-party repositories, foreign and hand-built packages, snaps and flatpaks, data,\n\
         # services, users, or package versions: an apply installs a missing package at what\n",
    );
    out.push_str(if machine.distro == Distro::Fedora {
        "# Fedora's repositories offer. "
    } else {
        "# the dated archive at the snapshot below offers. "
    });
    out.push_str(if machine.distro == Distro::Fedora {
        "A third-party dnf repository this machine\n"
    } else {
        "A third-party apt repository this machine\n"
    });
    out.push_str(
        "\
         # installed from is written as a commented [sources] block, to take on purpose. No\n\
         # file is copied from /etc: a changed configuration file is named at the end, with the\n\
         # [etc.\"PATH\"] table that declares the text you want there instead.\n\
         # The NOT CAPTURED block at the end names what this machine had that is missing from\n\
         # the declarations above it.\n\n",
    );
}

/// What the file applies to: the distribution, and its release by codename where it has one.
fn applicability(machine: &Machine) -> String {
    let name = machine.distro.name();
    if machine.codename.is_empty() {
        name.to_string()
    } else {
        format!("{name} ({})", machine.codename)
    }
}

/// The release that first honours the `[host] snapshot` every imported manifest carries, which
/// every imported manifest names as its `min_lodi_version`. A 1.3 binary reads the snapshot and
/// ignores it, so it would install from the live archive what the file pins to the dated one;
/// from 1.4.0 the file names a lodi that honours it, and 1.3 refuses it by name (M-Pin, LD-398;
/// it was 1.1.1, the release that first reads `packages = "exact"`, LD-375). It is not the
/// version of the lodi that happens to run the import: a later importer writes the same minimum.
/// A bare version would be a prefix under the shared constraint grammar, which 1.4.1 would not
/// satisfy, so it is written as the range `>=1.4.0`.
pub const IMPORT_MINIMUM: &str = "1.4.0";

/// What `[host] snapshot` does on Debian and Ubuntu, above the line the import writes it on: two
/// lines, with no date, version, user, host or path in them (M-Pin, P11).
pub const SNAPSHOT_COMMENT: &str = "# An apply installs a missing package from the dated archive at this instant, UTC;\n\
     # remove the line and it installs from the live archive instead.\n";

fn host(out: &mut String, machine: &Machine, snapshot: &Snapshot) {
    out.push_str("[host]\nversion = \"1\"\n");
    let _ = writeln!(out, "distro = \"{}\"", machine.distro.name());
    out.push_str(PACKAGES_EXACT_COMMENT);
    out.push_str("packages = \"exact\"\n");
    // The lodi that honours `snapshot`: 1.4.0 or a later one (LD-398), so an older lodi refuses
    // the file by name rather than ignoring its pin.
    let _ = writeln!(out, "min_lodi_version = \">={IMPORT_MINIMUM}\"");
    // Fedora has no dated archive this build installs from, so its file carries no snapshot
    // (LD-432); an older lodi refuses `distro = "fedora"` by name.
    if machine.distro == Distro::Fedora {
        out.push('\n');
        return;
    }
    out.push_str(SNAPSHOT_COMMENT);
    // The capture clock stays the test seam; the value written is the last UTC day that had
    // ended, the one Arch has published (LD-396) and whose dated apt index no longer changes
    // (LD-444), leaving every other byte unchanged.
    let instant = crate::util::parse_utc(snapshot.as_str())
        .map(crate::arch::base::latest_published_day)
        .map(crate::util::format_utc)
        .unwrap_or_else(|| snapshot.as_str().to_string());
    let _ = writeln!(out, "snapshot = \"{instant}\"\n");
}

fn packages(out: &mut String, machine: &Machine, selection: &Selection) {
    // Fedora's names are Fedora's own, so they are declared under its table (LD-432).
    let (table, key) = if machine.distro == Distro::Fedora {
        ("[packages.fedora]", "add")
    } else {
        ("[packages]", "common")
    };
    let _ = writeln!(out, "{table}");
    out.push_str(
        "# Every name here was installed and explicitly wanted on the machine that was read,\n\
         # with that distribution's own base system subtracted from it.\n",
    );
    if selection.unchecked {
        out.push_str(
            "#\n\
             # This machine has no package index: nothing has been fetched from any\n\
             # repository, so where each of these names came from could not be checked, and\n\
             # they are declared as read. On a machine whose index has been fetched, a name\n\
             # no repository of this distribution's own serves is named at the end of this\n\
             # file instead of declared here. Fetch the index and import again to have that\n\
             # decided rather than assumed.\n",
        );
    }
    list(out, key, &selection.common);
    out.push('\n');
    if !selection.from_other_suite.is_empty() {
        out.push_str(
            "# Declared above, and installed here from one of this distribution's own suites\n\
             # that a fresh install of the same release does not enable. The name is offered by\n\
             # the release's own repositories, so an apply installs it — at whatever version the\n\
             # default suites carry, which is not the version this machine has:\n",
        );
        for (name, suite) in &selection.from_other_suite {
            let _ = writeln!(out, "#   {name}{}{suite}", padding(name));
        }
        out.push('\n');
    }
    if selection.hold.is_empty() {
        out.push_str("# No hold was read off this machine. ");
        if Shape::of(machine.distro).hold_is_machine_state {
            out.push_str("A hold here is machine state:\n# it outlives lodi.\n");
        } else {
            out.push_str(
                "This distribution keeps no hold of its own:\n\
                 # lodi expresses a hold as an option on its own transactions, so there is\n\
                 # nothing on the machine to read back.\n",
            );
        }
        out.push_str("# hold = [\"NAME\"]\n\n");
    } else {
        out.push_str("# The names this machine keeps at the version it already has.\n");
        list(out, "hold", &selection.hold);
        out.push('\n');
    }
    out.push_str(
        "# Lines lodi did not write, and what each one would do if you did:\n\
         #\n\
         #   mark_auto = [\"NAME\"]  an apply marks NAME automatically installed, so that\n\
         #                         removing whatever pulled it in removes NAME with it.\n\
         #   optional  = [\"NAME\"]  an apply that finds no such package in the index leaves\n\
         #                         NAME out and carries on, rather than stopping.\n\
         #   absent    = [\"NAME\"]  an apply removes NAME wherever it finds it installed.\n\
         #\n\
         # packages = \"exact\" under [host] is why a line taken out of this list is a package\n\
         # taken off the machine: the next apply removes it, and says so.\n\n",
    );
}

/// What `packages = "exact"` means, said above the key the import writes (LD-375).
pub const PACKAGES_EXACT_COMMENT: &str = "\
# packages = \"exact\": an apply makes this machine's packages what this file declares, plus
# the distribution's own base system. A package whose line you delete is removed by the next
# apply, and one a declared package still needs is kept as a dependency; each is named in the
# plan with the reason. A package installed by hand since lodi last recorded this machine is
# not removed unless the apply is given --overwrite-drift. \"managed\" removes only what lodi
# recorded installing.
";

/// One TOML array, one name per line, in the order the selection holds them — which is sorted,
/// because every list this module builds is walked out of an ordered set.
fn list(out: &mut String, key: &str, names: &[String]) {
    if names.is_empty() {
        let _ = writeln!(out, "{key} = []");
        return;
    }
    let _ = writeln!(out, "{key} = [");
    for name in names {
        let _ = writeln!(out, "  \"{name}\",");
    }
    out.push_str("]\n");
}

/// The heading of each class of left-out package, in the order the block names them. It is one
/// text, used by the counts line and by the heading above the names, so the two can never drift
/// apart — and it is what `docs/scopes/host.md` and `docs/CLI.md` quote.
///
/// `{distro}` is the distribution this file applies to, spelled as `[host] distro` spells it.
const CLASSES: [&str; 3] = [
    "from a repository that is not {distro}'s own",
    "installed from a package file, or from a source this machine no longer has",
    "from no repository this package manager knows",
];

fn class(index: usize, machine: &Machine) -> String {
    CLASSES[index].replace("{distro}", machine.distro.name())
}

/// The `# NOT CAPTURED` block alone, from its first line to the end of the file: what a reconcile
/// regenerates in a manifest that still has it (LD-378).
pub fn not_captured_block(
    machine: &Machine,
    selection: &Selection,
    files: Option<&Capture>,
) -> String {
    let mut out = String::new();
    not_captured(&mut out, machine, selection, files);
    out
}

fn not_captured(
    out: &mut String,
    machine: &Machine,
    selection: &Selection,
    files: Option<&Capture>,
) {
    let counts = [
        selection.third_party.len(),
        selection.local_only.len(),
        selection.foreign.len(),
    ];
    let total = selection.not_captured();
    out.push_str("# NOT CAPTURED\n#\n");
    let _ = writeln!(
        out,
        "# {total} package(s) installed and chosen on this machine are NOT declared above,\n\
         # because an apply of this file on a fresh {} machine could not install them:",
        machine.distro.name()
    );
    for (index, count) in counts.iter().enumerate() {
        let _ = writeln!(out, "#   {count} {}", class(index, machine));
    }
    let _ = writeln!(
        out,
        "# {} snap(s) and {} system flatpak(s) are installed; the host scope manages neither.",
        machine.snaps, machine.flatpaks
    );
    out.push_str("#\n");
    // The note is wrapped where it is written, not where it is printed: the emitted file is
    // compared byte for byte, and a wrap computed at print time would move with the class name.
    // On apt the source is declared by taking its commented [sources] block above; the
    // repositories part of this block says, per repository, why there is none (LD-367).
    let note = if machine.repositories.read {
        "take its commented [sources] block\n\
         # above, where one is written (the repositories below say why not), and add the name\n\
         # to [packages]; without the source an apply of this file stops at\n\
         # E_UNKNOWN_PACKAGE before it installs anything"
    } else {
        "add that source on the new machine\n\
         # first, or an apply of this file stops at E_UNKNOWN_PACKAGE before it installs\n\
         # anything"
    };
    labelled(out, &class(0, machine), &selection.third_party, Some(note));
    plain(out, &class(1, machine), &selection.local_only);
    plain(out, &class(2, machine), &selection.foreign);
    if let Some(capture) = files {
        uncaptured(out, capture);
    }
    super::sources::not_captured(out, machine);
}

/// One class heading and the names under it, each with the label its class carries. The heading
/// is written whether or not the class has names, because a block that drops its empty classes
/// makes a person wonder which question was asked.
fn labelled(out: &mut String, heading: &str, names: &[Labelled], note: Option<&str>) {
    if names.is_empty() {
        let _ = writeln!(out, "# {}: none", sentence_case(heading));
        return;
    }
    match note {
        Some(note) => {
            let _ = writeln!(out, "# {} — {note}:", sentence_case(heading));
        }
        None => {
            let _ = writeln!(out, "# {}:", sentence_case(heading));
        }
    }
    for (name, label) in names {
        let _ = writeln!(out, "#   {name}{}{label}", padding(name));
    }
}

/// The same, for a class whose heading is the whole of what there is to say about each name.
fn plain(out: &mut String, heading: &str, names: &[String]) {
    if names.is_empty() {
        let _ = writeln!(out, "# {}: none", sentence_case(heading));
        return;
    }
    let _ = writeln!(out, "# {}:", sentence_case(heading));
    for name in names {
        let _ = writeln!(out, "#   {name}");
    }
}

/// The spaces between a name and its label. A name longer than the column simply takes two
/// spaces, so the block never reflows and two imports of one machine stay byte identical.
fn padding(name: &str) -> String {
    const COLUMN: usize = 24;
    " ".repeat(COLUMN.saturating_sub(name.chars().count()).max(2))
}

/// A class heading with its first letter capitalised, so that it reads as a sentence under the
/// counts line that reads it as a phrase.
fn sentence_case(heading: &str) -> String {
    let mut chars = heading.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The configuration half of the comment block: every changed file, with the `[etc."PATH"]`
/// that declares it or why it is never read, and the two counts that say how much of `/etc`
/// the import never looked at.
fn uncaptured(out: &mut String, capture: &Capture) {
    out.push_str("#\n");
    if capture.refused.is_empty() {
        out.push_str("# No configuration file this machine tracks is reported as changed.\n");
    } else {
        out.push_str(
            "# These configuration files were reported as changed; lodi copies none of them:\n",
        );
        for refusal in &capture.refused {
            let _ = writeln!(out, "#   {}", refusal.path);
            match refusal.reason.table(&refusal.path) {
                Some(table) => {
                    let _ = writeln!(out, "#     declare its text as {table}");
                }
                None => {
                    let _ = writeln!(out, "#     {}", refusal.why());
                }
            }
        }
    }
    let _ = writeln!(
        out,
        "# configuration files the package manager tracks and reports unchanged: {}",
        capture.unchanged
    );
    let at_least = if capture.untracked_is_a_floor {
        "at least "
    } else {
        ""
    };
    let _ = writeln!(
        out,
        "# files under /etc that no package tracks as configuration: {at_least}{}",
        capture.untracked
    );
}
