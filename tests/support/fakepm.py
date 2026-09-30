#!/usr/bin/env python3
"""A fake machine's package manager, for the host scope's convergence tests (LD-375).

No real package manager ever runs in this repository's tests (AGENTS.md §8). The recorded shims
of `tests/host_apt.rs` and `tests/host_pacman.rs` replay what a real guest printed at fixed
moments; this one keeps a **machine** instead: one JSON file (`machine.json`) holding what is
installed, with its versions, marks, dependencies and provides, and what the repositories offer.
Every read the two backends make is answered from it, and every mutation they make changes it,
so a test can import a machine, edit a manifest, apply, and look at what is left.

It answers as the real programs answer, in the layout the backends parse, and no more:

    pacman -Q | -Qe | -Qi | -Qii | -Qqem | -Si | -Sgq | -Slq | -Sp | -Syu | -S | -Rs | -Rns | -Rsp
    pacman -D                     pacman-conf --repo-list
    dpkg-query -W -f=...          apt-mark showauto|showhold|auto|manual|hold|unhold
    apt-cache policy [names]      apt-cache pkgnames          apt-cache show [names]
    apt-get update                apt-get install [-s] ... -- NAME... NAME-...
    dpkg --audit
    dnf5 repoquery|install|remove|mark|makecache|check    rpm -qa --qf ...|-Va --configfiles
    systemctl [--root=DIR] is-enabled|is-active|enable|disable|preset [--now] -- UNIT

Removal follows each family's own rule: pacman's `-Rs` takes the targets and every dependency
they leave with no reason to stay (installed as a dependency, and required by nothing else), and
refuses when anything outside the set still requires a target; apt's `NAME-` removes the name
and every installed package that would be left with a dependency it cannot satisfy, and names as
"no longer required" what was installed automatically and is reached from nothing installed by
hand through `Depends` or `Recommends` (apt's own default, `APT::AutoRemove::RecommendsImportant`).
apt's `install` also installs what a new package recommends, as apt does by default, unless the
argv says `--no-install-recommends`; and a new package's `conflicts` are removed in the same
transaction, which is how apt resolves a `Conflicts` (LD-375 validator repair). A transaction
whose removals would break a package it installs — one it was asked for, or one it pulls in —
is refused as apt refuses it, exit 100 and "held broken packages", with nothing changed
(LD-412).

A machine may carry `"on": {"upgrade": {...}, "update": {...}}`: package records merged into the
installed set (a `null` removes one) when `pacman -Syu` or `apt-get update` runs. It is how a test
makes the package graph change between the plan and the removal, which is what an apply must
notice. `"on": {"fetch": {...}}` holds offers the machine's index does not know yet: `apt-get
update` adds them to what the repositories offer, the way a machine whose index was never
fetched (a fresh cloud image) learns every name it can install (LD-379 guest run).

A machine may carry `"say": {"apt-get update": "text"}`: the update prints the text on standard
error and still succeeds, the way apt reports a repository it could not fetch as a warning and
exits 0 (LD-365, S9). Every `apt-get update` appends what it found armed to `$STATE/update-saw`.

On Fedora (LD-433) `dnf5 makecache` reads the root's `lodi-*.repo` files, appends each with the
`gpgkey` it names and whether that file is there to `$STATE/makecache-saw`, and refreshes the
repositories `--repo=ID` names, or every one when none is named. A machine may carry
`"repos": {"ID": {NAME: offer}}`, what repository ID offers once it was refreshed while its
`.repo` file is there, and `"fail_repo": {"ID": "message"}`, which fails the refresh of ID.

A machine may carry `"fail": {"PROGRAM OP": "message"}`: that invocation (`pacman -Rs`,
`apt-mark auto`, ...) prints the message and exits 1 without changing anything, the way a package
manager refuses a transaction part way through an apply (LD-377). A simulation is its own
operation (`-Rsp`, `install -s`) and is never failed by an entry for the real one.
`"fail_part": {"PROGRAM OP": "message"}` is the other kind of failure: the invocation carries out
its change for its **first** package name only, keeps that change, then prints the message and
exits 1 — a transaction that changed the machine before it failed (LD-377 validator repair).

An installed record may carry `"conffiles": {"/etc/PATH": "MD5"}`: the configuration files the
package tracks. pacman reports each as `[modified]` under `Backup Files`; dpkg reports the path
and the digest, and the backend compares it with the file below the root (LD-377).

Every invocation is logged, argv with \\x1f between the words and \\x1e after the last, to
`$STATE/log` — the same record the recorded shims keep.

A machine may carry `"units": {"NAME": {"state": "enabled", "active": true, "preset": "disabled",
"package": "NAME"}}`: the systemd units `systemctl` answers for (sc-1). A unit with a `package`
exists only while that package is installed; `preset` is where `systemctl preset` puts it.

A pinned apply (M-Pin) points apt at a private source set with `-o Dir::Etc::SourceParts=…` and
`-o Dir::State::Lists=…` below the root. The fake then reads that set's stanzas: a URI the
machine names under `"archives"` is a dated archive served from a recorded fixture directory
(`tests/fixtures/host/pin/archive/…`), whose `Release` it copies into the private lists under
apt's own list-file name and whose `Packages` it offers; any other URI offers what the machine's
repositories offer. Nothing under the root's `etc/apt` or `var/lib/apt/lists` is touched, and
each private update appends the set it read to `$STATE/private-saw`. `install` then takes
`NAME=VERSION`, refuses a version the set does not offer (`E: Version '…' for '…' was not found`),
a downgrade without `--allow-downgrades` and a held package moved without
`--allow-change-held-packages`, as apt does. `"skew": {"NAME": "VERSION"}` makes the next install
of NAME leave that version instead: a transaction whose read-back does not match.

The shadow tools (su-1) keep the root's own `etc/passwd`, `etc/group` and `etc/shadow`, which is
where the product reads accounts back from: `useradd` writes the account with a locked password
(`!` in the shadow file, as the real one does with no password given), its own group and, with
`--create-home`, its home; `groupadd`, `usermod --shell | --append --groups` and
`gpasswd --members` change the files as the real tools do. Each needs the `--root` the product
passes and refuses a password option, so no test can pass with lodi setting one.

The OS basics (sd-1) get `--root=DIR` from the product under a scratch root, and every one of
these fakes refuses to act without it: `hostnamectl set-hostname` writes the root's
`etc/hostname`, `timedatectl set-timezone` points its `etc/localtime` at a zone file the root
holds, and `localectl` answers `status`, `list-locales`, `set-locale` and `set-x11-keymap` from
`"os": {"locale": ["LANG=…"], "keymap": "us", "locales": [...]}`; with an `etc/default/keyboard`,
as Ubuntu's localed, it reads the layout from that file and refuses `set-x11-keymap`. `nft` keeps
`"os": {"nft": {"FAMILY NAME": "listing"}}`: `list tables`, `list table`, `delete table` and
`-f FILE`, whose tables it stores as written. `netplan`, `networkctl` and `nmcli` only log;
`systemd-run` records the timer it would arm under `"os": {"timers": {}}`, and `systemctl stop`
of that timer removes it. `ping` answers unless a lodi network file below the root holds the
string `"os": {"cut": "…"}` names, which is how a test makes a network change cut the machine off.

The kernel's tools (bc-1) need the same `--root=DIR`. `sysctl -w NAME=VALUE` writes the root's
`proc/sys` file, which must exist; `modprobe -- NAME` makes `sys/module/NAME` unless `"os":
{"absent_modules": [...]}` names it; `grub-mkconfig -o FILE` sources the root's
`etc/default/grub` and writes one entry with that command line; Fedora 44's `grub2-mkconfig`
refuses `--update-bls-cmdline`, as the real one does, and rewrites the `options` line of each
entry under `boot/loader/entries` that is not lodi's with that command line, as the real one did
on the Fedora 44 guest (bv-1). `grubby --update-kernel=ALL --args=WORDS` (or `--remove-args=WORDS`) adds
(or takes out) the words in the `options` line of each such entry and in `etc/kernel/cmdline`,
and, unless `--no-etc-grub-update` follows, rewrites every `GRUB_CMDLINE_LINUX=` line of
`etc/default/grub` to the edited value, as the real one does; that flag first is refused.

The bootloader's tools (bl-1) need it too, and keep the root's firmware variables in efivarfs's
own format under `sys/firmware/efi/efivars` (`BootOrder`, `Boot####`), which is what lodi reads.
`efibootmgr --create-only --disk D --part P --label L --loader PATH` adds an entry,
`--bootorder X,Y` sets the order and `--delete-bootnum --bootnum X` deletes an entry;
`--bootnext X` sets `BootNext` and `--delete-bootnext` takes it out (bv-1).
`bootctl --esp-path=ESP install` writes systemd-boot to `EFI/systemd` and to the removable path
`EFI/BOOT/BOOTX64.EFI`, as the real one does, makes `loader/`, and puts its own entry first;
`bootctl --esp-path=ESP set-oneshot ID` sets systemd-boot's `LoaderEntryOneShot` (bv-1).
`kernel-install add KVER IMAGE` writes `loader/entries/MACHINE-ID-KVER.conf`, with the command
line of the root's `etc/kernel/cmdline`: under `BOOT_ROOT` when `etc/kernel/install.conf` names
one, else on a `/boot` that `proc/self/mountinfo` mounts as other than vfat (the real one finds
an XBOOTLDR partition by its type; Ubuntu 24.04's is ext4), else on the ESP whose `loader/`
exists. `grub-install
--efi-directory=ESP --bootloader-id=ID` writes `EFI/ID/grubx64.efi` and puts an entry for it
first. `grub-mkconfig` also sources `etc/default/grub.d/*.cfg` and writes `set timeout=` and
`set default=` from what it read, then appends what each executable script under the root's
`etc/grub.d` prints, as the real one does. `grub-reboot ENTRY` (bv-1) writes `next_entry=ENTRY`
to the root's `boot/grub/grubenv` (`grub2` on Fedora).
"""

import gzip
import json
import lzma
import os
import shutil
import subprocess
import sys


def load(state):
    with open(os.path.join(state, "machine.json"), encoding="utf-8") as handle:
        return json.load(handle)


def save(state, machine):
    path = os.path.join(state, "machine.json")
    with open(path + ".new", "w", encoding="utf-8") as handle:
        json.dump(machine, handle, indent=1, sort_keys=True)
    os.replace(path + ".new", path)


def log(state, program, args):
    with open(os.path.join(state, "log"), "a", encoding="utf-8") as handle:
        handle.write(program + "".join("\x1f" + a for a in args) + "\x1e")


def strip_root(args):
    """The root options of design call D2, and the root they name."""
    out, root, skip = [], None, 0
    for index, arg in enumerate(args):
        if skip:
            skip -= 1
            continue
        if arg in ("--root", "--dbpath", "--sysroot") and index + 1 < len(args):
            if arg in ("--root", "--sysroot"):
                root = args[index + 1]
            skip = 1
            continue
        if arg == "-o" and index + 1 < len(args) and args[index + 1].startswith("RootDir="):
            root = args[index + 1][len("RootDir="):]
            skip = 1
            continue
        if arg.startswith("--admindir="):
            continue
        out.append(arg)
    return out, root


def names_of(args):
    return args[args.index("--") + 1:] if "--" in args else []


PRIVATE_KEYS = ("Dir::Etc::SourceList", "Dir::Etc::SourceParts", "Dir::State::Lists",
                "Dir::Cache::pkgcache", "Dir::Cache::srcpkgcache")


def private_of(args):
    """The private source set's options (M-Pin), taken out of the argv: their values by key, or
    None when the invocation reads the machine's own sources."""
    out, found, skip = [], {}, 0
    for index, arg in enumerate(args):
        if skip:
            skip -= 1
            continue
        if arg == "-o" and index + 1 < len(args) and \
                args[index + 1].split("=", 1)[0] in PRIVATE_KEYS:
            key, value = args[index + 1].split("=", 1)
            found[key] = value
            skip = 1
            continue
        out.append(arg)
    return out, (found or None)


def _order(char):
    if char == "~":
        return -1
    if char.isdigit():
        return 0
    if char.isalpha():
        return ord(char)
    return ord(char) + 256


def _verrevcmp(a, b):
    i = j = 0
    while i < len(a) or j < len(b):
        first = 0
        while (i < len(a) and not a[i].isdigit()) or (j < len(b) and not b[j].isdigit()):
            ac = _order(a[i]) if i < len(a) else 0
            bc = _order(b[j]) if j < len(b) else 0
            if ac != bc:
                return ac - bc
            i += 1
            j += 1
        while i < len(a) and a[i] == "0":
            i += 1
        while j < len(b) and b[j] == "0":
            j += 1
        while i < len(a) and a[i].isdigit() and j < len(b) and b[j].isdigit():
            if not first:
                first = ord(a[i]) - ord(b[j])
            i += 1
            j += 1
        if i < len(a) and a[i].isdigit():
            return 1
        if j < len(b) and b[j].isdigit():
            return -1
        if first:
            return first
    return 0


def dpkg_compare(a, b):
    """dpkg's ordering of two versions: epoch, upstream, revision."""
    def split(version):
        epoch = 0
        if ":" in version:
            head, version = version.split(":", 1)
            epoch = int(head)
        upstream, _, revision = version.rpartition("-") if "-" in version else (version, "", "")
        return epoch, upstream, revision
    ea, ua, ra = split(a)
    eb, ub, rb = split(b)
    if ea != eb:
        return ea - eb
    return _verrevcmp(ua, ub) or _verrevcmp(ra, rb)


def list_file_name(uri, suite, name):
    """The name apt gives the list file of one index it downloaded."""
    bare = uri.split("://", 1)[-1].rstrip("/")
    return f"{bare}/dists/{suite}/{name}".replace("/", "_")


def deb822(text):
    stanzas, fields = [], {}
    for line in text.split("\n"):
        if line.startswith("#"):
            continue
        if not line.strip():
            if fields:
                stanzas.append(fields)
            fields = {}
            continue
        key, _, value = line.partition(":")
        fields[key.strip()] = value.strip()
    if fields:
        stanzas.append(fields)
    return stanzas


def private_update(state, machine, root, private):
    """`apt-get update` of a private source set: read its stanzas, copy each dated suite's
    `Release` into its own lists and offer what its `Packages` lists."""
    parts = os.path.join(root, private["Dir::Etc::SourceParts"].lstrip("/"))
    lists = os.path.join(root, private["Dir::State::Lists"].lstrip("/"))
    os.makedirs(lists, exist_ok=True)
    pool, texts = {}, []
    for name in sorted(os.listdir(parts)):
        with open(os.path.join(parts, name), encoding="utf-8") as handle:
            text = handle.read()
        texts.append(text)
        for stanza in deb822(text):
            for uri in stanza.get("URIs", "").split():
                archive = machine.get("archives", {}).get(uri)
                if archive is None:
                    # A URI the test serves no recorded archive for offers what the machine's
                    # repositories offer — what a fetch of them brings included.
                    offered = dict(machine.get("on", {}).get("fetch", {}))
                    offered.update(machine["available"])
                    for package, offer in offered.items():
                        pool.setdefault(package, []).append(dict(offer, repo=uri))
                    continue
                for suite in stanza.get("Suites", "").split():
                    suite_dir = os.path.join(archive, "dists", suite)
                    release = os.path.join(suite_dir, "Release")
                    if not os.path.isfile(release):
                        sys.stderr.write(f"E: Failed to fetch {uri}dists/{suite}/InRelease 404\n")
                        return 100
                    shutil.copyfile(release, os.path.join(lists, list_file_name(uri, suite, "Release")))
                    for component in stanza.get("Components", "").split():
                        base = os.path.join(suite_dir, component, "binary-amd64", "Packages")
                        for suffix, opener in ((".xz", lzma.open), (".gz", gzip.open), ("", open)):
                            if os.path.isfile(base + suffix):
                                with opener(base + suffix, "rt", encoding="utf-8") as handle:
                                    for record in deb822(handle.read()):
                                        if "Package" in record and "Version" in record:
                                            pool.setdefault(record["Package"], []).append(
                                                {"version": record["Version"], "repo": uri})
                                break
    mutate(machine, "update")
    machine["private"] = pool
    with open(os.path.join(state, "private-saw"), "a", encoding="utf-8") as handle:
        handle.write("".join(texts) + "\x1e")
    save(state, machine)
    return 0


def private_install(state, machine, args, names, simulate):
    """`apt-get install` against a private source set: `NAME=VERSION` or `NAME`, apt's own
    refusals, then the same transaction the ordinary install makes — dependencies, what apt
    recommends unless told not to, conflicts and removals — from what the set offers."""
    pool = machine.get("private", {})
    installed = machine["installed"]
    after = json.loads(json.dumps(machine))
    available = {}
    for name, offers in pool.items():
        best = offers[0]
        for offer in offers[1:]:
            if dpkg_compare(offer["version"], best["version"]) > 0:
                best = offer
        available[name] = dict(best)
    after["available"] = available
    wanted = []
    for spec in [n for n in names if not n.endswith("-")]:
        name, _, version = spec.partition("=")
        offers = pool.get(name, [])
        if version:
            chosen = next((o for o in offers if o["version"] == version), None)
            if chosen is None:
                sys.stderr.write(f"E: Version '{version}' for '{name}' was not found\n")
                return 100
        else:
            if not offers:
                sys.stderr.write(f"E: Unable to locate package {name}\n")
                return 100
            chosen = available[name]
        available[name] = dict(chosen)
        now = installed.get(name)
        if now is not None and now["version"] != chosen["version"]:
            if now.get("held") and "--allow-change-held-packages" not in args:
                sys.stderr.write("E: Held packages were changed and -y was used without "
                                 "--allow-change-held-packages.\n")
                return 100
            if dpkg_compare(chosen["version"], now["version"]) < 0 and \
                    "--allow-downgrades" not in args:
                sys.stderr.write("E: Packages were downgraded and -y was used without "
                                 "--allow-downgrades.\n")
                return 100
            after["installed"][name]["version"] = chosen["version"]
            after["installed"][name]["explicit"] = True
        wanted.append(name)
    unwanted = [n[:-1] for n in names if n.endswith("-")]
    fields = ("depends",) if "--no-install-recommends" in args else ("depends", "recommends")
    missing = install(after, wanted, fields)
    if missing:
        sys.stderr.write(f"E: Unable to locate package {missing[0]}\n")
        return 100
    for name, version in machine.get("skew", {}).items():
        if name in after["installed"]:
            after["installed"][name]["version"] = version
    after.pop("skew", None)
    return finish_install(state, machine, after, wanted, unwanted, simulate)


# ------------------------------------------------------------------------------ the graph ---

def clauses(record, fields=("depends",)):
    """Each dependency clause of a record, as a list of alternatives."""
    out = []
    for field in fields:
        for clause in record.get(field, []):
            out.append([alt.strip() for alt in clause.split("|") if alt.strip()])
    return out


def provider(installed, name, exclude=()):
    """An installed package that satisfies `name`: itself, or one that provides it."""
    if name in installed and name not in exclude:
        return name
    for other, record in sorted(installed.items()):
        if other not in exclude and name in record.get("provides", []):
            return other
    return None


def required_by(installed, name):
    """What pacman prints under `Required By`: every installed package with a dependency that
    this package satisfies by its name or by what it provides."""
    record = installed[name]
    satisfies = {name, *record.get("provides", [])}
    out = []
    for other, rec in sorted(installed.items()):
        if other == name:
            continue
        for clause in clauses(rec):
            if any(alt in satisfies for alt in clause):
                out.append(other)
                break
    return out


def pacman_removal(installed, targets):
    """pacman -Rs: the targets, then every dependency left with no reason to stay. Returns the
    set, or the first dependency it would break."""
    remove = set(targets)
    changed = True
    while changed:
        changed = False
        for name in sorted(remove):
            for clause in clauses(installed[name]):
                for alt in clause:
                    dep = provider(installed, alt)
                    if dep is None or dep in remove or installed[dep].get("explicit"):
                        continue
                    if all(r in remove for r in required_by(installed, dep)):
                        remove.add(dep)
                        changed = True
    for name in sorted(remove):
        for other in required_by(installed, name):
            if other not in remove:
                return None, (name, other)
    return remove, None


def reachable(installed, roots, fields):
    seen, queue = set(), list(roots)
    while queue:
        name = queue.pop()
        if name in seen or name not in installed:
            continue
        seen.add(name)
        for clause in clauses(installed[name], fields):
            for alt in clause:
                dep = provider(installed, alt)
                if dep is not None and dep not in seen:
                    queue.append(dep)
    return seen


def autoremovable(installed):
    manual = [n for n, r in installed.items() if r.get("explicit")]
    kept = reachable(installed, manual, ("depends", "recommends"))
    return sorted(n for n, r in installed.items() if not r.get("explicit") and n not in kept)


def satisfied(installed, record, gone):
    for clause in clauses(record):
        if not any(provider(installed, alt, exclude=gone) for alt in clause):
            return False
    return True


def apt_removal(installed, targets):
    """apt's `NAME-`: the names, and every installed package a removal leaves broken."""
    remove = set(t for t in targets if t in installed)
    changed = True
    while changed:
        changed = False
        for name, record in sorted(installed.items()):
            if name in remove:
                continue
            if not satisfied(installed, record, remove):
                remove.add(name)
                changed = True
    return remove


def install(machine, names, with_deps_fields=("depends",)):
    """Install each name from what the repositories offer, and the dependencies it needs."""
    installed, available = machine["installed"], machine["available"]
    missing = [n for n in names if n not in available and n not in installed]
    if missing:
        return missing
    queue = [(n, True) for n in names]
    while queue:
        name, explicit = queue.pop(0)
        if name in installed:
            if explicit:
                installed[name]["explicit"] = True
            continue
        offer = available.get(name)
        if offer is None:
            continue
        record = {k: v for k, v in offer.items() if k != "repo"}
        record["explicit"] = explicit
        installed[name] = record
        for clause in clauses(offer, with_deps_fields):
            if not any(provider(installed, alt) for alt in clause):
                queue.append((clause[0], False))
    return []


def mutate(machine, event):
    for name, record in machine.get("on", {}).pop(event, {}).items():
        if record is None:
            machine["installed"].pop(name, None)
        else:
            machine["installed"][name] = record


# ----------------------------------------------------------------------------- pacman ---

def pacman_record(name, record, full=False, installed=None):
    lines = [f"Name            : {name}", f"Version         : {record['version']}"]
    provides = record.get("provides", [])
    lines.append("Provides        : " + ("  ".join(provides) if provides else "None"))
    depends = record.get("depends", [])
    lines.append("Depends On      : " + ("  ".join(depends) if depends else "None"))
    if installed is not None:
        req = required_by(installed, name)
        lines.append("Required By     : " + ("  ".join(req) if req else "None"))
        reason = "Explicitly installed" if record.get("explicit") else \
            "Installed as a dependency for another package"
        lines.append(f"Install Reason  : {reason}")
    if full:
        conffiles = sorted(record.get("conffiles", {}))
        if conffiles:
            lines.append(f"Backup Files    : {conffiles[0]} [modified]")
            lines.extend(f"                  {path} [modified]" for path in conffiles[1:])
        else:
            lines.append("Backup Files    : None")
    return "\n".join(lines) + "\n\n"


def arch_sync(machine, root, args):
    """Read Lodi's dated config, copy the measured .db bytes and parse its own desc records.

    The fake's available set remains the live archive; only a -Syu with --config reads dated.
    """
    if "--config" not in args:
        return None
    config = args[args.index("--config") + 1]
    with open(config, encoding="utf-8") as handle:
        text = handle.read()
    lines = text.splitlines()
    if lines.count("SigLevel = Required DatabaseOptional") != 1 or \
            lines.count("LocalFileSigLevel = Required") != 1:
        raise RuntimeError("dated pin.conf must require local package signatures")
    import tarfile
    base = os.path.join(root, "var/lib/pacman/sync")
    os.makedirs(base, exist_ok=True)
    found = {}
    repo = None
    for line in text.splitlines():
        if line.startswith("[") and line.endswith("]"):
            repo = line[1:-1]
        elif line.startswith("Server = ") and repo != "options":
            url = line[len("Server = "):]
            source = machine.get("archives", {}).get(url)
            if source is None:
                raise RuntimeError("no measured archive for the dated pacman Server")
            db = os.path.join(source, repo + ".db")
            staged = os.path.join(base, repo + ".db")
            shutil.copyfile(db, staged)
            if machine.get("pin_corrupt_db") and repo == "extra":
                with open(staged, "r+b") as handle:
                    handle.seek(12)
                    byte = handle.read(1)
                    handle.seek(12)
                    handle.write(bytes([byte[0] ^ 1]))
            with tarfile.open(db, "r:gz") as index:
                for entry in index:
                    if not entry.name.endswith("/desc"):
                        continue
                    blocks = index.extractfile(entry).read().decode().split("\n\n")
                    fields = dict(block.split("\n", 1) for block in blocks if "\n" in block)
                    name = fields["%NAME%"].strip()
                    found[name] = {"version": fields["%VERSION%"].strip(),
                                   "repo": repo, "depends": [], "explicit": True}
    return found


def pacman(state, machine, args):
    args, _root = strip_root(args)
    installed, available = machine["installed"], machine["available"]
    op = args[0] if args else ""
    names = names_of(args)
    out = sys.stdout
    if op == "-Q":
        for name in sorted(installed):
            out.write(f"{name} {installed[name]['version']}\n")
    elif op == "-Qe":
        for name in sorted(installed):
            if installed[name].get("explicit"):
                out.write(f"{name} {installed[name]['version']}\n")
    elif op in ("-Qi", "-Qii"):
        for name in sorted(installed):
            out.write(pacman_record(name, installed[name], op == "-Qii", installed))
    elif op == "-Qqem":
        foreign = [n for n in sorted(installed)
                   if installed[n].get("explicit") and n not in available]
        for name in foreign:
            out.write(name + "\n")
        return 0 if foreign else 1
    elif op == "-Si":
        code = 0
        for name in names:
            offers = [available[name]] if name in available else []
            offers += [o for o in machine.get("also", {}).get(name, [])]
            if not offers:
                sys.stderr.write(f"error: package '{name}' was not found\n")
                code = 1
                continue
            for offer in offers:
                out.write(f"Repository      : {offer['repo']}\n")
                out.write(pacman_record(name, offer))
        return code
    elif op in ("-Sg", "-Sgq"):
        found = False
        for name in names:
            for member in machine.get("groups", {}).get(name, []):
                found = True
                out.write(member + "\n" if op == "-Sgq" else f"{name} {member}\n")
        return 0 if found else 1
    elif op == "-Slq":
        for name in sorted(available):
            out.write(name + "\n")
    elif op == "-Sp":
        # What a sync of these names would install, printed and not done (`--print-format %n`).
        after = json.loads(json.dumps(machine))
        missing = install(after, names)
        if missing:
            sys.stderr.write(f"error: target not found: {missing[0]}\n")
            return 1
        for name in sorted(after["installed"]):
            if name not in installed:
                out.write(name + "\n")
    elif op == "-Syy":
        failures = machine.get("sync_failures") or []
        if "--config" in args and failures:
            # libalpm's words for a download that failed, then pacman's for the whole sync.
            why = failures.pop(0)
            save(state, machine)
            sys.stderr.write("error: failed retrieving file 'core.db' from "
                             f"archive.archlinux.org : {why}\n"
                             "error: failed to synchronize all databases "
                             "(download library error)\n")
            return 1
        arch_sync(machine, _root, args)
        save(state, machine)
    elif op in ("-Syu", "-S"):
        dated = arch_sync(machine, _root, args) if "--config" in args else None
        mutate(machine, "upgrade")
        live = machine["available"]
        if dated is not None:
            machine["available"] = {**live, **dated}
        missing = install(machine, names)
        machine["available"] = live
        if missing:
            sys.stderr.write(f"error: target not found: {missing[0]}\n")
            return 1
        save(state, machine)
    elif op in ("-Up", "-U"):
        unmet = machine.get("pin_unsatisfiable")
        if op == "-Up" and unmet:
            # pacman's own wording for a local file whose dependency no repository satisfies.
            target = unmet if isinstance(unmet, str) else "tree"
            sys.stderr.write("error: failed to prepare transaction (could not satisfy dependencies)\n"
                             f":: unable to satisfy dependency 'libmissing' required by {target}\n")
            return 1
        for path in names:
            if not os.path.isfile(path):
                sys.stderr.write(f"error: failed to open file: {path}\n")
                return 1
            # A local package needs its detached signature whatever configuration pacman read:
            # this check does not depend on `--config`, so a lost `.sig` fails on its own (#172).
            if not os.path.isfile(path + ".sig"):
                sys.stderr.write("error: missing required package signature\n")
                return 1
            if machine.get("keyring") and signer(path + ".sig") not in keys(gpgdir(_root, args)):
                sys.stderr.write("error: required key missing from keyring\n"
                                 f"error: '{path}': unexpected error\n")
                return 1
            # The file name is read as pacman reads a local package's identity. It was obtained
            # from a real index by the fixture recorder, not invented by this fake.
            stem = os.path.basename(path).removesuffix(".pkg.tar.zst")
            for name in machine["available"]:
                if stem.startswith(name + "-"):
                    version = stem[len(name)+1:].rsplit("-", 1)[0]
                    if op == "-Up":
                        out.write(name + "\n")
                    else:
                        entry = dict(machine["available"][name])
                        entry["version"] = version
                        entry["explicit"] = True
                        machine["installed"][name] = entry
                    break
            else:
                sys.stderr.write(f"error: package file not in index: {path}\n")
                return 1
        if op == "-U":
            for name, version in machine.pop("skew", {}).items():
                if name in machine["installed"]:
                    machine["installed"][name]["version"] = version
            save(state, machine)
    elif op in ("-Rs", "-Rns", "-Rsp"):
        unknown = [n for n in names if n not in installed]
        if unknown:
            sys.stderr.write(f"error: target not found: {unknown[0]}\n")
            return 1
        remove, broken = pacman_removal(installed, names)
        if broken:
            sys.stderr.write(
                "error: failed to prepare transaction (could not satisfy dependencies)\n"
                f":: removing {broken[0]} breaks dependency '{broken[0]}' "
                f"required by {broken[1]}\n")
            return 1
        if op == "-Rsp":
            for name in sorted(remove):
                out.write(name + "\n")
            return 0
        for name in remove:
            del installed[name]
        save(state, machine)
    elif op == "-D":
        explicit = "--asexplicit" in args
        for name in names:
            if name in installed:
                installed[name]["explicit"] = explicit
        save(state, machine)
    return 0


def pacman_conf(machine, args):
    for repo in machine.get("repositories", []):
        sys.stdout.write(repo + "\n")
    return 0


# -------------------------------------------------------------------------------- apt ---

def apt_status(record):
    return "hi " if record.get("held") else "ii "


def dpkg_query(machine, args):
    args, _root = strip_root(args)
    fmt = next((a for a in args if a.startswith("-f=")), "")
    for name, record in sorted(machine["installed"].items()):
        if "${Conffiles}" in fmt:
            for path, md5 in sorted(record.get("conffiles", {}).items()):
                sys.stdout.write(f" {path} {md5}\n")
            sys.stdout.write("\n")
        elif "${Priority}" in fmt:
            sys.stdout.write("\t".join([
                name, apt_status(record), record.get("priority", "optional"),
                record.get("section", "utils"), ", ".join(record.get("depends", [])),
                ", ".join(record.get("recommends", []))]) + "\n")
        else:
            sys.stdout.write(f"{name}\t{record['version']}\t{apt_status(record)}\n")
    return 0


def apt_mark(state, machine, args):
    args, _root = strip_root(args)
    installed = machine["installed"]
    verb = args[0] if args else ""
    if verb == "showauto":
        for name in sorted(installed):
            if not installed[name].get("explicit"):
                sys.stdout.write(name + "\n")
        return 0
    if verb == "showhold":
        for name in sorted(installed):
            if installed[name].get("held"):
                sys.stdout.write(name + "\n")
        return 0
    for name in names_of(args):
        if name not in installed:
            continue
        if verb in ("auto", "manual"):
            installed[name]["explicit"] = verb == "manual"
        elif verb in ("hold", "unhold"):
            installed[name]["held"] = verb == "hold"
    save(state, machine)
    return 0


STATUS = "/var/lib/dpkg/status"


def apt_cache(machine, args):
    args, _root = strip_root(args)
    installed, available = machine["installed"], machine["available"]
    sources = machine.get("sources", {})
    if args[:1] == ["pkgnames"]:
        prefix = (names_of(args) or [""])[0]
        for name in sorted(available):
            if name.startswith(prefix):
                sys.stdout.write(name + "\n")
        return 0
    names = names_of(args)
    if args[:1] == ["show"]:
        # `--no-all-versions`: the candidate's record, in the layout apt prints it.
        found = False
        for name in names:
            offer = available.get(name)
            if offer is None:
                sys.stderr.write(f"N: Unable to locate package {name}\n")
                continue
            found = True
            lines = [f"Package: {name}", f"Version: {offer['version']}",
                     f"Priority: {offer.get('priority', 'optional')}",
                     f"Section: {offer.get('section', 'utils')}"]
            for field in ("depends", "recommends"):
                if offer.get(field):
                    lines.append(f"{field.capitalize()}: " + ", ".join(offer[field]))
            lines += ["Description: a package of the fake machine", " kept by fakepm.py"]
            sys.stdout.write("\n".join(lines) + "\n\n")
        if not found:
            sys.stderr.write("E: No packages found\n")
            return 100
        return 0
    if not names:
        sys.stdout.write("Package files:\n 100 /var/lib/dpkg/status\n     release a=now\n")
        for key, release in sorted(sources.items()):
            sys.stdout.write(f" 500 {release['key']}\n     release {release['release']}\n")
        return 0
    for name in names:
        record, offer = installed.get(name), available.get(name)
        if record is None and offer is None:
            continue
        have = record["version"] if record else "(none)"
        candidate = offer["version"] if offer else have
        sys.stdout.write(f"{name}:\n  Installed: {have}\n  Candidate: {candidate}\n"
                         "  Version table:\n")
        if offer is not None:
            mark = "***" if record and record["version"] == offer["version"] else "   "
            key = sources[offer["repo"]]["key"]
            sys.stdout.write(f" {mark} {offer['version']} 500\n        500 {key}\n")
            if mark == "***":
                sys.stdout.write(f"        100 {STATUS}\n")
        if record is not None and (offer is None or record["version"] != offer["version"]):
            sys.stdout.write(f" *** {record['version']} 100\n        100 {STATUS}\n")
    return 0


def saw(state, root):
    """What this `apt-get update` found armed: each `lodi-*.sources` stanza under the root's
    `sources.list.d`, the keyring its `Signed-By:` names, and whether that keyring is there.
    Appended to `$STATE/update-saw`, one line per stanza, so that a test can prove no refresh
    ever saw a stanza whose keyring was missing (LD-365, S8)."""
    directory = os.path.join(root, "etc/apt/sources.list.d")
    lines = []
    for name in sorted(os.listdir(directory)) if os.path.isdir(directory) else []:
        if not (name.startswith("lodi-") and name.endswith(".sources")):
            continue
        signed = None
        with open(os.path.join(directory, name), encoding="utf-8") as handle:
            for line in handle:
                if line.startswith("Signed-By:"):
                    signed = line.split(":", 1)[1].strip()
        there = signed is not None and os.path.isfile(os.path.join(root, signed.lstrip("/")))
        lines.append(f"{name} {signed} {'present' if there else 'missing'}\n")
    with open(os.path.join(state, "update-saw"), "a", encoding="utf-8") as handle:
        handle.write("".join(lines) or "nothing\n")


def apt_get(state, machine, args):
    args, root = strip_root(args)
    args, private = private_of(args)
    verb = args[0] if args else ""
    if verb == "update" and private:
        return private_update(state, machine, root, private)
    if verb == "install" and private:
        return private_install(state, machine, args, names_of(args), "-s" in args)
    if verb == "update":
        if root:
            saw(state, root)
        said = machine.get("say", {}).get("apt-get update")
        if said is not None:
            sys.stderr.write(said + "\n")
        mutate(machine, "update")
        machine["available"].update(machine.get("on", {}).pop("fetch", {}))
        if root:
            lists = os.path.join(root, "var/lib/apt/lists")
            os.makedirs(lists, exist_ok=True)
            with open(os.path.join(lists, "fake_Packages"), "w", encoding="utf-8") as handle:
                handle.write("fetched\n")
        save(state, machine)
        return 0
    if verb != "install":
        sys.stderr.write(f"E: the fake apt-get does not do {verb!r}\n")
        return 100
    simulate = "-s" in args
    names = names_of(args)
    wanted = [n for n in names if not n.endswith("-")]
    unwanted = [n[:-1] for n in names if n.endswith("-")]
    after = json.loads(json.dumps(machine))
    fields = ("depends",) if "--no-install-recommends" in args else ("depends", "recommends")
    missing = install(after, wanted, fields)
    if missing:
        sys.stderr.write(f"E: Unable to locate package {missing[0]}\n")
        return 100
    return finish_install(state, machine, after, wanted, unwanted, simulate)


def finish_install(state, machine, after, wanted, unwanted, simulate):
    """What one `apt-get install` does once its installs are placed: conflicts and removals,
    what it prints, and — unless it simulates — the machine it leaves. `wanted` are the names
    it was asked for, which its removals may not break (LD-412)."""
    for name, record in after["installed"].items():
        if name not in machine["installed"]:
            unwanted += [c for c in record.get("conflicts", []) if c in after["installed"]]
    remove = apt_removal(after["installed"], unwanted)
    out = sys.stdout
    out.write("Reading package lists...\nBuilding dependency tree...\n"
              "Reading state information...\n")
    # apt never removes what the same transaction installs: a removal that would leave a name
    # it was asked for, or one it pulls in, with a dependency it cannot satisfy is refused, and
    # nothing is simulated or done.
    broken = sorted(n for n in remove if n in wanted or n not in machine["installed"])
    if broken:
        out.write("Some packages could not be installed. This may mean that you have\n"
                  "requested an impossible situation or if you are using the unstable\n"
                  "distribution that some required packages have not yet been created\n"
                  "or been moved out of Incoming.\n"
                  "The following information may help to resolve the situation:\n\n"
                  "The following packages have unmet dependencies:\n")
        for name in broken:
            for clause in clauses(after["installed"][name]):
                if not any(provider(after["installed"], alt, exclude=remove) for alt in clause):
                    out.write(f" {name} : Depends: {clause[0]} but it is not going to be "
                              "installed\n")
                    break
        sys.stderr.write("E: Unable to correct problems, you have held broken packages.\n")
        return 100
    for name in remove:
        del after["installed"][name]
    orphans = autoremovable(after["installed"])
    if orphans:
        out.write("The following packages were automatically installed and are no longer "
                  "required:\n")
        for start in range(0, len(orphans), 4):
            out.write("  " + " ".join(orphans[start:start + 4]) + "\n")
        out.write("Use 'apt autoremove' to remove them.\n")
    before = machine["installed"]
    added = sorted(n for n in after["installed"] if n not in before)
    if remove:
        out.write("The following packages will be REMOVED:\n  " + " ".join(sorted(remove)) + "\n")
    out.write(f"0 upgraded, {len(added)} newly installed, {len(remove)} to remove and "
              "0 not upgraded.\n")
    if simulate:
        for name in sorted(remove):
            out.write(f"Remv {name} [{before[name]['version']}]\n")
        for name in added:
            out.write(f"Inst {name} ({after['installed'][name]['version']} Fake [amd64])\n")
        for name in sorted(n for n in after["installed"] if n in before):
            was, now = before[name]["version"], after["installed"][name]["version"]
            if was != now:
                out.write(f"Inst {name} [{was}] ({now} Fake [amd64])\n")
        return 0
    machine["installed"] = after["installed"]
    machine.pop("skew", None)
    save(state, machine)
    return 0


# -------------------------------------------------------------------------------- dnf ---
# dnf5 and rpm of Fedora 44 (LD-432), in the layouts a Fedora 44 guest printed: the
# `--assumeno` transaction table (a heading at the first column, a row per package indented by
# one space), `Operation aborted by the user.` and exit 1 when it stops, `No match for argument`
# for an unknown name, and rpm's `[%{=NAME}\t%{TAG}\n]` rows. A version is `EPOCH:VERSION-RELEASE`
# and every package here is x86_64 unless its record says `arch`.

def evr(record):
    version = record["version"]
    return version if ":" in version else "0:" + version


def dnf_split(args):
    """The global options, the command, and what follows it: its options and its names."""
    out = [a for a in args if not a.startswith("--installroot=")]
    root = next((a.split("=", 1)[1] for a in args if a.startswith("--installroot=")), None)
    commands = ("repoquery", "install", "remove", "mark", "makecache", "check", "download")
    at = next(i for i, a in enumerate(out) if a in commands)
    rest, options, names, skip = out[at + 1:], [], [], False
    for index, arg in enumerate(rest):
        if skip:
            skip = False
            continue
        if arg in ("--qf", "--destdir"):
            options.append(arg + "=" + rest[index + 1])
            skip = True
        elif arg.startswith("-"):
            options.append(arg)
        else:
            names.append(arg)
    return out[:at], out[at], options, names, root


def qf_render(fmt, fields):
    text = fmt.replace("\\n", "\n").replace("\\t", "\t")
    for key, value in fields.items():
        text = text.replace("%{" + key + "}", value)
    return text


def dnf_fields(name, record, repo=None):
    epoch, rest = evr(record).split(":", 1)
    version, release = rest.rsplit("-", 1) if "-" in rest else (rest, "1")
    arch = record.get("arch", "x86_64")
    return {
        "name": name, "epoch": epoch, "version": version, "release": release, "arch": arch,
        "evr": rest if epoch == "0" else evr(record),
        "repoid": repo or record.get("repo", ""),
        "reason": "User" if record.get("explicit") else "Dependency",
        "sourcerpm": record.get("sourcerpm", f"{name}-{version}-{release}.src.rpm"),
        "from_repo": record.get("from_repo", "fedora"),
        "requires": "\n".join(record.get("depends", [])),
        "provides": "\n".join([f"{name} = {rest}"] + record.get("provides", [])),
    }


def dnf_table(sections):
    lines = ["Package     Arch   Version         Repository      Size"]
    for heading, rows in sections:
        if rows:
            lines.append(heading)
            lines.extend(f" {name} x86_64 {version} {repo} 1.0 KiB" for name, version, repo in rows)
    lines += ["", "Transaction Summary:"]
    return "\n".join(lines) + "\n"


def dnf_removal(installed, targets):
    """dnf5 remove: the targets, what cannot stand without them, then the dependencies nothing
    left needs any more (clean_requirements_on_remove)."""
    before = set(autoremovable(installed))
    dependents = apt_removal(installed, targets) - set(targets)
    gone = set(targets) | dependents
    after = {n: r for n, r in installed.items() if n not in gone}
    unused = set(autoremovable(after)) - before
    return sorted(dependents), sorted(unused)


def dnf_saw(state, root):
    """What this `dnf5 makecache` found armed: each `lodi-*.repo` under the root's
    `yum.repos.d`, the `gpgkey` it names, and whether that key file is there, appended to
    `$STATE/makecache-saw` one line per file (LD-433). Returns the ids of the repositories those
    files define."""
    directory = os.path.join(root, "etc/yum.repos.d")
    lines, ids = [], set()
    for name in sorted(os.listdir(directory)) if os.path.isdir(directory) else []:
        if not (name.startswith("lodi-") and name.endswith(".repo")):
            continue
        key = None
        with open(os.path.join(directory, name), encoding="utf-8") as handle:
            for line in handle:
                if line.startswith("[") and line.rstrip().endswith("]"):
                    ids.add(line.strip()[1:-1])
                if line.startswith("gpgkey="):
                    key = line.split("=", 1)[1].strip()
        path = key[len("file://"):] if key and key.startswith("file://") else None
        there = path is not None and os.path.isfile(os.path.join(root, path.lstrip("/")))
        lines.append(f"{name} {key} {'present' if there else 'missing'}\n")
    if lines:
        with open(os.path.join(state, "makecache-saw"), "a", encoding="utf-8") as handle:
            handle.write("".join(lines))
    return ids


def dnf5(state, machine, args):
    _globals, command, options, names, root = dnf_split(args)
    installed, available = machine["installed"], machine["available"]
    out = sys.stdout
    if command == "makecache":
        asked = [a.split("=", 1)[1] for a in args if a.startswith("--repo=")]
        armed = dnf_saw(state, root or "/")
        for repo in asked or sorted(armed):
            failure = machine.get("fail_repo", {}).get(repo)
            if failure is not None:
                sys.stderr.write(f"{failure}\n>>> Failed to download metadata for repo '{repo}'\n")
                return 1
        for repo in asked or sorted(armed):
            if repo in armed:
                machine["available"].update(machine.get("repos", {}).get(repo, {}))
        if asked:
            save(state, machine)
            return 0
        mutate(machine, "update")
        machine["available"].update(machine.get("on", {}).pop("fetch", {}))
        save(state, machine)
        cache = os.path.join(root or "/", "var/cache/libdnf5/fedora-0/repodata")
        os.makedirs(cache, exist_ok=True)
        with open(os.path.join(cache, "repomd.xml"), "w", encoding="utf-8") as handle:
            handle.write("metadata\n")
        return 0
    if command == "check":
        for line in machine.get("unclean", []):
            out.write(line + "\n")
        return 1 if machine.get("unclean") else 0
    if command == "repoquery":
        fmt = next(o.split("=", 1)[1] for o in options if o.startswith("--qf="))
        if "--installed" in options:
            for name in sorted(installed):
                out.write(qf_render(fmt, dnf_fields(name, installed[name])))
            return 0
        for name in sorted(available) if not names else names:
            offers = [available[name]] if name in available else []
            offers += machine.get("also", {}).get(name, [])
            for offer in offers:
                out.write(qf_render(fmt, dnf_fields(name, offer, offer.get("repo"))))
        return 0
    if command == "mark":
        reason, names = names[0], names[1:]
        for name in names:
            if name in installed:
                installed[name]["explicit"] = reason == "user"
        save(state, machine)
        return 0
    if command == "download":
        # fk-1: the configured repositories' copy of one build, by its NEVRA, when the machine's
        # `serves` table names a file for it (a real package file of tests/fixtures).
        destdir = next(o.split("=", 1)[1] for o in options if o.startswith("--destdir="))
        for nevra in names:
            source = machine.get("serves", {}).get(nevra)
            if source is None:
                sys.stderr.write(f'No package "{nevra}" available; You might want to use '
                                 "--skip-unavailable option.\n")
                return 1
            with open(source, "rb") as src, \
                    open(os.path.join(destdir, os.path.basename(source)), "wb") as dst:
                dst.write(src.read())
        return 0
    stop = "--assumeno" in options
    files = [n for n in names if n.startswith("/")]
    names = [n for n in names if not n.startswith("/")]
    if command == "install" and files:
        # fk-1: a package file named by its path, installed or moved to exactly its build; the
        # build is read from the file name, the epoch from the machine's `epochs`.
        after = json.loads(json.dumps(machine))
        rows = []
        for path in files:
            stem = os.path.basename(path)[:-len(".rpm")]
            rest, arch = stem.rsplit(".", 1)
            name, version, release = rest.rsplit("-", 2)
            epoch = machine.get("epochs", {}).get(name, "0")
            was = after["installed"].get(name) or {
                k: v for k, v in available.get(name, {}).items() if k != "repo"}
            record = dict(was, version=f"{epoch}:{version}-{release}", arch=arch,
                          from_repo="@commandline")
            record.setdefault("explicit", True)
            for need in record.get("depends", []):
                if need not in after["installed"] and need not in available:
                    sys.stderr.write("Failed to resolve the transaction:\n"
                                     f"Problem: nothing provides {need} needed by {stem}\n")
                    return 1
            after["installed"][name] = record
            rows.append((name, record["version"], "@commandline"))
        missing = install(after, names, ("depends",))
        if missing:
            sys.stderr.write("Failed to resolve the transaction:\n"
                             f"No match for argument: {missing[0]}\n")
            return 1
        if stop:
            out.write(dnf_table([("Installing:", rows)]))
            sys.stderr.write("Operation aborted by the user.\n")
            return 1
        save(state, after)
        return 0
    if command == "install":
        after = json.loads(json.dumps(machine))
        weak = "--setopt=install_weak_deps=False" not in args
        missing = install(after, names, ("depends", "recommends") if weak else ("depends",))
        if missing:
            sys.stderr.write("Failed to resolve the transaction:\n"
                             f"No match for argument: {missing[0]}\n")
            return 1
        new = sorted(n for n in after["installed"] if n not in installed)
        if stop:
            row = lambda n: (n, evr(after["installed"][n]), available.get(n, {}).get("repo", ""))
            out.write(dnf_table([("Installing:", [row(n) for n in new if n in names]),
                                 ("Installing dependencies:",
                                  [row(n) for n in new if n not in names])]))
            sys.stderr.write("Operation aborted by the user.\n")
            return 1
        for name in new:
            after["installed"][name]["from_repo"] = available.get(name, {}).get("repo", "")
        save(state, after)
        return 0
    if command == "remove":
        unknown = [n for n in names if n not in installed]
        if unknown:
            sys.stderr.write("Failed to resolve the transaction:\n"
                             f"No packages to remove for argument: {unknown[0]}\n")
            return 1
        dependents, unused = dnf_removal(installed, names)
        if stop:
            row = lambda n: (n, evr(installed[n]), "@System")
            out.write(dnf_table([("Removing:", [row(n) for n in names]),
                                 ("Removing dependent packages:", [row(n) for n in dependents]),
                                 ("Removing unused dependencies:", [row(n) for n in unused])]))
            sys.stderr.write("Operation aborted by the user.\n")
            return 1
        for name in set(names) | set(dependents) | set(unused):
            del installed[name]
        save(state, machine)
        return 0
    return 0


def rpm(machine, args):
    args, _root = strip_root(args)
    installed = machine["installed"]
    out = sys.stdout
    if args[:1] == ["-Va"]:
        changed = False
        for name in sorted(installed):
            for path in sorted(installed[name].get("conffiles", {})):
                out.write(f"S.5....T.  c {path}\n")
                changed = True
        return 1 if changed else 0
    fmt = args[args.index("--qf") + 1]
    for name in sorted(installed):
        record = installed[name]
        fields = dnf_fields(name, record)
        if "%{REQUIRENAME}" in fmt:
            for need in ["rpmlib(CompressedFileNames)"] + record.get("depends", []):
                out.write(f"{name}\t{need}\n")
        elif "%{PROVIDENAME}" in fmt:
            for gives in [name, f"{name}(x86-64)"] + record.get("provides", []):
                out.write(f"{name}\t{gives}\n")
        elif "%{FILENAMES}" in fmt:
            for path in record.get("files", []):
                out.write(f"{name}\t{path}\n")
        else:
            out.write(f"{name} {fields['epoch']}:{fields['version']}-{fields['release']} "
                      f"{fields['arch']}\n")
    return 0


def main():
    program = os.path.basename(sys.argv[1])
    args = sys.argv[2:]
    state = os.environ["LODI_FAKEPM_STATE"]
    log(state, program, args)
    machine = load(state)
    plain, _root = strip_root(args)
    plain = [a for a in plain if not a.startswith(("--installroot=", "--root="))]
    key = f"{program} {plain[0]}" if plain else program
    if program == "apt-get" and "-s" in plain:
        # `install -s` is a simulation, its own operation: an entry for the real one never fails it.
        key = f"{key} -s"
    failure = machine.get("fail", {}).get(key)
    if failure is not None:
        sys.stderr.write(failure + "\n")
        return 1
    part = machine.get("fail_part", {}).get(key)
    if part is not None:
        if "--" in args:
            args = args[:args.index("--") + 2]
        dispatch(state, machine, program, args)
        sys.stderr.write(part + "\n")
        return 1
    return dispatch(state, machine, program, args)


def signer(path):
    """The issuer fingerprint or key ID of a detached v4 OpenPGP signature, as gpg names it."""
    with open(path, "rb") as handle:
        sig = handle.read()
    if not sig:
        return None
    if sig[0] & 0x40:
        size, at = sig[1], 2
    else:
        width = (1, 2, 4)[sig[0] & 3]
        size, at = int.from_bytes(sig[1:1 + width], "big"), 1 + width
    body = sig[at:at + size]
    hashed = int.from_bytes(body[4:6], "big")
    unhashed = int.from_bytes(body[6 + hashed:8 + hashed], "big")
    for area in (body[6:6 + hashed], body[8 + hashed:8 + hashed + unhashed]):
        i = 0
        while i < len(area):
            n, i = area[i], i + 1
            packet, i = area[i:i + n], i + n
            if packet[:2] == bytes([33, 4]):
                return packet[2:].hex().upper()
            if packet[:1] == bytes([16]):
                return packet[1:].hex().upper()
    return None


def gpgdir(root, args):
    """pacman's keyring: `GPGDir` of the configuration it was given, else the machine's own."""
    if "--gpgdir" in args:
        return args[args.index("--gpgdir") + 1]
    if "--config" in args:
        with open(args[args.index("--config") + 1], encoding="utf-8") as handle:
            for line in handle:
                if line.startswith("GPGDir = "):
                    return line[len("GPGDir = "):].strip()
    return os.path.join(root or "/", "etc/pacman.d/gnupg")


def keys(directory):
    """The fake keyring is `pubring.kbx` holding one trusted fingerprint per line."""
    try:
        with open(os.path.join(directory, "pubring.kbx"), encoding="utf-8") as handle:
            return {line.strip() for line in handle if line.strip()}
    except OSError:
        return set()


def pacman_key(args):
    """`pacman-key` on a private keyring only: it never falls back to the machine's own."""
    if "--gpgdir" not in args:
        sys.stderr.write("fakepm: pacman-key without --gpgdir\n")
        return 1
    home = args[args.index("--gpgdir") + 1]
    if "--verify" in args:
        sig, _file = args[args.index("--verify") + 1:args.index("--verify") + 3]
        if signer(sig) in keys(home):
            return 0
        sys.stderr.write(f"==> ERROR: The signature identified by {sig} could not be verified.\n")
        return 1
    if "--init" in args:
        os.makedirs(home, exist_ok=True)
        for name in ("pubring.kbx", "trustdb.gpg"):
            open(os.path.join(home, name), "a", encoding="utf-8").close()
        return 0
    if "--populate" in args:
        source = args[args.index("--populate-from") + 1]
        found = {}
        for name in ("archlinux.gpg", "archlinux-trusted", "archlinux-revoked"):
            try:
                with open(os.path.join(source, name), encoding="utf-8") as handle:
                    found[name] = {line.split(":")[0].strip() for line in handle if line.strip()}
            except OSError:
                sys.stderr.write(f"==> ERROR: missing {name}\n")
                return 1
        with open(os.path.join(home, "pubring.kbx"), "a", encoding="utf-8") as handle:
            for key in sorted(found["archlinux.gpg"] - found["archlinux-revoked"]):
                handle.write(key + "\n")
        return 0
    return 1


def bsdtar(args):
    """`bsdtar -xOf PACKAGE MEMBER`: the fake keyring package is a plain tar."""
    import tarfile
    package, member = args[args.index("-xOf") + 1], args[-1]
    try:
        with tarfile.open(package) as archive:
            data = archive.extractfile(member).read()
    except (OSError, KeyError, AttributeError, tarfile.TarError):
        sys.stderr.write(f"bsdtar: {member}: Not found in archive\n")
        return 1
    sys.stdout.buffer.write(data)
    return 0


# ---------------------------------------------------------------------------- systemctl ---

def systemctl(state, machine, args):
    """`systemctl [--root=DIR] VERB [--now] -- UNIT`, one unit, as systemd 252 answers."""
    words = [a for a in args if not a.startswith("--root=") and a not in ("--now", "--")]
    verb, names = words[0], words[1:]
    units = machine.setdefault("units", {})
    roots = [a[len("--root="):] for a in args if a.startswith("--root=")]
    if verb == "daemon-reload":
        return 0
    if verb == "stop":
        timers = machine.get("os", {}).get("timers", {})
        for name in names:
            timers.pop(name.removesuffix(".timer"), None)
        save(state, machine)
        return 0
    for name in names:
        # A unit file lodi wrote below the root is a unit systemctl knows (sd-1).
        if name not in units and roots and os.path.exists(
                os.path.join(roots[0], "etc/systemd/system", name)):
            units[name] = {"state": "disabled", "active": False, "preset": "disabled"}

    def there(name):
        unit = units.get(name)
        return unit is not None and unit.get("package", "") in ("", *machine["installed"])

    status = 0
    for name in names:
        unit = units.get(name) if there(name) else None
        if verb == "is-enabled":
            if unit is None:
                sys.stderr.write(f"Failed to get unit file state for {name}: No such file or "
                                 "directory\n")
                status = 1
                continue
            print(unit["state"])
            status = status or (0 if unit["state"] == "enabled" else 1)
        elif verb == "is-active":
            print("active" if unit and unit.get("active") else "inactive")
            status = status or (0 if unit and unit.get("active") else 3)
        elif unit is None:
            sys.stderr.write(f"Failed to {verb} unit: Unit file {name} does not exist.\n")
            return 1
        elif verb in ("enable", "disable"):
            if unit["state"] in ("enabled", "disabled"):
                unit["state"] = verb + "d"
            if "--now" in args:
                unit["active"] = verb == "enable"
        elif verb == "preset":
            unit["state"] = unit.get("preset", "enabled")
        else:
            sys.stderr.write(f"fakepm: systemctl {verb} is not faked\n")
            return 1
    save(state, machine)
    return status


def account_files(root):
    etc = os.path.join(root, "etc")
    return (os.path.join(etc, "passwd"), os.path.join(etc, "group"), os.path.join(etc, "shadow"))


def read_rows(path):
    if not os.path.exists(path):
        return []
    with open(path, encoding="utf-8") as handle:
        return [line.rstrip("\n").split(":") for line in handle if line.strip()]


def write_rows(path, rows):
    with open(path + ".new", "w", encoding="utf-8") as handle:
        handle.write("".join(":".join(row) + "\n" for row in rows))
    os.replace(path + ".new", path)


def shadow_options(args):
    """The options and the one name after `--`, or an exit status for an argv lodi must never
    build: no root, no `--`, or a password."""
    if "--" not in args or args.index("--") != len(args) - 2:
        sys.stderr.write("fakepm: a shadow tool takes exactly one name after --\n")
        return None, None, 2
    if any(a in ("-p", "--password") or a.startswith("--password=") for a in args):
        sys.stderr.write("fakepm: lodi never sets a password\n")
        return None, None, 2
    options, name = args[:args.index("--")], args[-1]
    values, index = {}, 0
    while index < len(options):
        key = options[index]
        if key in ("--create-home", "--append", "--system"):
            values[key] = True
            index += 1
        else:
            values[key] = options[index + 1]
            index += 2
    if "--root" not in values:
        sys.stderr.write("fakepm: a shadow tool of a scratch root needs --root\n")
        return None, None, 2
    return values, name, None


def set_members(groups, group, members):
    for row in groups:
        if row[0] == group:
            row[3] = ",".join(members)


def shadow_tool(program, args):
    values, name, status = shadow_options(args)
    if status is not None:
        return status
    passwd_path, group_path, shadow_path = account_files(values["--root"])
    passwd, groups, shadow = (read_rows(passwd_path), read_rows(group_path),
                              read_rows(shadow_path))
    users = {row[0]: row for row in passwd}
    by_group = {row[0]: row for row in groups}
    if program == "groupadd":
        if name in by_group:
            sys.stderr.write(f"groupadd: group '{name}' already exists\n")
            return 9
        gid = values.get("--gid") or str(max([999] + [int(r[2]) for r in groups]) + 1)
        groups.append([name, "x", gid, ""])
        write_rows(group_path, groups)
        return 0
    if program == "gpasswd":
        if name not in by_group:
            sys.stderr.write(f"gpasswd: group '{name}' does not exist\n")
            return 3
        members = [m for m in values["--members"].split(",") if m]
        set_members(groups, name, members)
        write_rows(group_path, groups)
        return 0
    wanted = [g for g in values.get("--groups", "").split(",") if g]
    for group in wanted:
        if group not in by_group:
            sys.stderr.write(f"{program}: group '{group}' does not exist\n")
            return 6
    if program == "useradd":
        if name in users:
            sys.stderr.write(f"useradd: user '{name}' already exists\n")
            return 9
        uid = values["--uid"]
        # An unprivileged test can give a file only to a group it is in, so the account's own
        # group takes the gid of the scratch root's owner, as long as no group has it.
        gid = str(os.stat(values["--root"]).st_gid)
        if gid in {r[2] for r in groups}:
            gid = str(max([999] + [int(r[2]) for r in groups]) + 1)
        groups.append([name, "x", gid, ""])
        home = values["--home-dir"]
        passwd.append([name, "x", uid, gid, "", home, values["--shell"]])
        shadow.append([name, "!", "20000", "0", "99999", "7", "", "", ""])
        if values.get("--create-home"):
            os.makedirs(os.path.join(values["--root"], home.lstrip("/")), mode=0o700)
    elif program == "usermod":
        if name not in users:
            sys.stderr.write(f"usermod: user '{name}' does not exist\n")
            return 6
        if "--shell" in values:
            users[name][6] = values["--shell"]
    for group in wanted:
        row = next(r for r in groups if r[0] == group)
        members = [m for m in row[3].split(",") if m]
        if name not in members:
            set_members(groups, group, members + [name])
    write_rows(passwd_path, passwd)
    write_rows(group_path, groups)
    write_rows(shadow_path, shadow)
    return 0


def dispatch(state, machine, program, args):
    if program == "pacman-key":
        return pacman_key(args)
    if program == "bsdtar":
        return bsdtar(args)
    if program == "pacman":
        return pacman(state, machine, args)
    if program == "pacman-conf":
        return pacman_conf(machine, args)
    if program == "dpkg-query":
        return dpkg_query(machine, args)
    if program == "apt-mark":
        return apt_mark(state, machine, args)
    if program == "apt-cache":
        return apt_cache(machine, args)
    if program == "apt-get":
        return apt_get(state, machine, args)
    if program == "dpkg":
        return 0
    if program == "dnf5":
        return dnf5(state, machine, args)
    if program == "rpm":
        return rpm(machine, args)
    if program == "systemctl":
        return systemctl(state, machine, args)
    if program in ("useradd", "groupadd", "usermod", "gpasswd"):
        return shadow_tool(program, args)
    if program in BASICS:
        return basics(state, machine, program, args)
    sys.stderr.write(f"fakepm: no such program {program}\n")
    return 127


BASICS = ("hostnamectl", "timedatectl", "localectl", "nft", "netplan", "networkctl", "nmcli",
          "ping", "systemd-run", "sysctl", "modprobe", "grub-mkconfig", "grub2-mkconfig",
          "grubby", "efibootmgr", "bootctl", "kernel-install", "grub-install", "grub-reboot",
          "grub2-reboot")

# The files a lodi network change writes, below the root: what `ping` reads a cut from.
NETWORK_FILES = ("etc/netplan/90-lodi.yaml", "etc/systemd/network", "etc/NetworkManager")


def cut_off(root, cut):
    for rel in NETWORK_FILES:
        path = os.path.join(root, rel)
        paths = [path] if os.path.isfile(path) else [
            os.path.join(d, f) for d, _, fs in os.walk(path) for f in fs if "lodi" in f]
        for one in paths:
            with open(one, encoding="utf-8") as handle:
                if cut in handle.read():
                    return True
    return False


def nft_file(tables, path):
    """`nft -f FILE`: `table F N`, `delete table F N` and `table F N { ... }`, in order."""
    with open(path, encoding="utf-8") as handle:
        lines = handle.read().splitlines()
    index = 0
    while index < len(lines):
        line = lines[index].strip()
        index += 1
        if not line or line.startswith("#"):
            continue
        words = line.split()
        if words[:2] == ["delete", "table"]:
            if tables.pop(" ".join(words[2:4]), None) is None:
                sys.stderr.write(f"Error: No such file or directory\n{line}\n")
                return 1
        elif words[0] == "table" and line.endswith("{"):
            block, depth = [line], 1
            while depth:
                text = lines[index]
                index += 1
                depth += text.count("{") - text.count("}")
                block.append(text)
            tables[" ".join(words[1:3])] = "\n".join(block)
        elif words[0] == "table":
            tables.setdefault(" ".join(words[1:3]), f"table {words[1]} {words[2]} {{\n}}")
        else:
            sys.stderr.write(f"fakepm: nft cannot read {line!r}\n")
            return 1
    return 0


def grub_mkconfig(root, words, program="grub-mkconfig"):
    """`grub-mkconfig -o FILE`: one entry whose command line is what the root's
    `etc/default/grub` gives, as the real one sources it, with `etc/default/grub.d/*.cfg`,
    and writes its `set timeout=` and `set default=` too. `grub2-mkconfig` also writes that
    command line into each loader entry's `options`."""
    script = ('. "$1"; for f in "$1.d"/*.cfg; do [ -f "$f" ] && . "$f"; done; '
              'printf "%s %s\\n%s\\n%s" "$GRUB_CMDLINE_LINUX" "$GRUB_CMDLINE_LINUX_DEFAULT" '
              '"${GRUB_TIMEOUT:-5}" "${GRUB_DEFAULT:-0}"')
    given, timeout, default = subprocess.run(
        ["sh", "-c", script, "sh", os.path.join(root, "etc/default/grub")],
        capture_output=True, text=True, check=True).stdout.split("\n")
    cmdline = " ".join(["root=/dev/vda1", "ro"] + given.split())
    with open(words[words.index("-o") + 1], "w", encoding="utf-8") as handle:
        handle.write(f"set timeout={timeout}\nset default=\"{default}\"\n"
                     f"menuentry 'Linux' {{\n\tlinux /vmlinuz {cmdline}\n}}\n")
        scripts = os.path.join(root, "etc/grub.d")
        for name in sorted(os.listdir(scripts)) if os.path.isdir(scripts) else []:
            path = os.path.join(scripts, name)
            if os.access(path, os.X_OK):
                handle.write(subprocess.run(["sh", path], capture_output=True, text=True,
                                            check=True).stdout)
    entries = os.path.join(root, "boot/loader/entries")
    for name in sorted(os.listdir(entries)) if program == "grub2-mkconfig" and os.path.isdir(
            entries) else []:
        path = os.path.join(entries, name)
        if name.startswith("lodi-") or not name.endswith(".conf"):
            continue
        with open(path, encoding="utf-8") as handle:
            lines = handle.read().splitlines()
        lines = [f"options {cmdline}" if line.startswith("options ") else line for line in lines]
        with open(path, "w", encoding="utf-8") as handle:
            handle.write("\n".join(lines) + "\n")


def grubby(root, words):
    """`grubby --update-kernel=ALL --args=WORDS` or `--remove-args=WORDS`: each word added to (or
    taken out of) every BLS entry's `options` line and `etc/kernel/cmdline`, and the GRUB file's
    `GRUB_CMDLINE_LINUX` unless `--no-etc-grub-update` follows, as Fedora 44's does."""
    if words[:1] == ["--no-etc-grub-update"]:
        # Fedora 44's getopt loop takes the next word as the flag's: `invalid option "ALL"`.
        sys.stderr.write('grubby: invalid option "ALL"\n')
        return 1
    keep = words[2:] == ["--no-etc-grub-update"]
    if words[:1] != ["--update-kernel=ALL"] or len(words) != 2 + keep:
        sys.stderr.write(f"fakepm: grubby {words} is not faked\n")
        return 1
    flag, _, given = words[1].partition("=")
    if flag not in ("--args", "--remove-args"):
        sys.stderr.write(f"fakepm: grubby {words} is not faked\n")
        return 1
    given = given.split()

    def edited(line):
        have = line.split()
        if flag == "--args":
            return " ".join(have + [w for w in given if w not in have])
        return " ".join(w for w in have if w not in given)

    entries = os.path.join(root, "boot/loader/entries")
    paths = [os.path.join(entries, n) for n in sorted(os.listdir(entries))] \
        if os.path.isdir(entries) else []
    for path in paths:
        with open(path, encoding="utf-8") as handle:
            lines = [line if not line.startswith("options ")
                     else f"options {edited(line[len('options '):])}\n" for line in handle]
        with open(path, "w", encoding="utf-8") as handle:
            handle.writelines(lines)
    grub = os.path.join(root, "etc/default/grub")
    if not keep and os.path.isfile(grub):
        script = '. "$1"; printf "%s" "$GRUB_CMDLINE_LINUX"'
        old = subprocess.run(["sh", "-c", script, "sh", grub], capture_output=True, text=True,
                             check=True).stdout
        if old:
            with open(grub, encoding="utf-8") as handle:
                lines = [f'GRUB_CMDLINE_LINUX="{edited(old)}"\n'
                         if line.startswith("GRUB_CMDLINE_LINUX=") else line for line in handle]
            with open(grub, "w", encoding="utf-8") as handle:
                handle.writelines(lines)
    later = os.path.join(root, "etc/kernel/cmdline")
    if os.path.isfile(later):
        with open(later, encoding="utf-8") as handle:
            text = handle.read()
        with open(later, "w", encoding="utf-8") as handle:
            handle.write(edited(text) + "\n")
    return 0


EFI_GLOBAL = "8be4df61-93ca-11d2-aa0d-00e098032b8c"
# What a systemd-boot image carries, which is how bootctl, and lodi, tell it from another loader.
SD_BOOT = b"#### LoaderInfo: systemd-boot 255 ####"


#: systemd-boot's vendor GUID, whose variables `bootctl` sets.
SD_VENDOR = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"


def efivar(root, name):
    return os.path.join(root, "sys/firmware/efi/efivars", f"{name}-{EFI_GLOBAL}")


def boot_order(root):
    try:
        with open(efivar(root, "BootOrder"), "rb") as handle:
            data = handle.read()[4:]
    except FileNotFoundError:
        return []
    return [int.from_bytes(data[i:i + 2], "little") for i in range(0, len(data), 2)]


def set_boot_order(root, order):
    with open(efivar(root, "BootOrder"), "wb") as handle:
        handle.write((7).to_bytes(4, "little") + b"".join(n.to_bytes(2, "little") for n in order))


def boot_entry_bytes(label, loader):
    """A `Boot####` variable: attributes, then an active load option whose device path is one
    file path node, as efivarfs shows it."""
    path = (loader + "\0").encode("utf-16-le")
    node = bytes([4, 4]) + (4 + len(path)).to_bytes(2, "little") + path
    dp = node + bytes([0x7F, 0xFF, 4, 0])
    return ((7).to_bytes(4, "little") + (1).to_bytes(4, "little") + len(dp).to_bytes(2, "little")
            + (label + "\0").encode("utf-16-le") + dp)


def boot_entries(root):
    folder = os.path.dirname(efivar(root, "BootOrder"))
    found = {}
    for name in os.listdir(folder) if os.path.isdir(folder) else []:
        head = name.split("-", 1)[0]
        if len(head) == 8 and head.startswith("Boot") and head != "BootOrder":
            with open(os.path.join(folder, name), "rb") as handle:
                data = handle.read()[4:]
            desc = data[6:].split(b"\0\0\0")[0] + b"\0"
            found[int(head[4:], 16)] = (desc.decode("utf-16-le").rstrip("\0"), data)
    return found


def new_boot_entry(root, label, loader, first):
    taken = boot_entries(root)
    number = next(n for n in range(0x10000) if n not in taken)
    with open(efivar(root, f"Boot{number:04X}"), "wb") as handle:
        handle.write(boot_entry_bytes(label, loader))
    if first:
        set_boot_order(root, [number] + [n for n in boot_order(root) if n != number])
    return number


def esp_of(root):
    for rel in ("efi", "boot/efi", "boot"):
        if os.path.isdir(os.path.join(root, rel, "loader")):
            return os.path.join(root, rel)
    return None


def boot_root(root):
    """Where `kernel-install` writes: `BOOT_ROOT` of `etc/kernel/install.conf`, an XBOOTLDR
    `/boot`, or the ESP."""
    try:
        with open(os.path.join(root, "etc/kernel/install.conf"), encoding="utf-8") as handle:
            for line in handle:
                if line.startswith("BOOT_ROOT="):
                    return os.path.join(root, line.split("=", 1)[1].strip().lstrip("/"))
    except FileNotFoundError:
        pass
    try:
        with open(os.path.join(root, "proc/self/mountinfo"), encoding="utf-8") as handle:
            for line in handle:
                head, _, tail = line.partition(" - ")
                if head.split(" ")[4:5] == ["/boot"] and tail.split(" ")[0] != "vfat":
                    return os.path.join(root, "boot")
    except FileNotFoundError:
        pass
    return esp_of(root)


def write_bytes(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as handle:
        handle.write(data)


def boot_tools(root, program, words):
    """The bootloader's tools (bl-1); 0, or the exit status of a refusal."""
    if program == "efibootmgr":
        value = lambda flag: words[words.index(flag) + 1]
        if "--create-only" in words:
            new_boot_entry(root, value("--label"), value("--loader"), first=False)
        elif "--bootorder" in words:
            set_boot_order(root, [int(n, 16) for n in value("--bootorder").split(",")])
        elif "--delete-bootnum" in words:
            number = int(value("--bootnum"), 16)
            os.remove(efivar(root, f"Boot{number:04X}"))
            set_boot_order(root, [n for n in boot_order(root) if n != number])
        elif "--bootnext" in words:
            with open(efivar(root, "BootNext"), "wb") as handle:
                handle.write((7).to_bytes(4, "little")
                             + int(value("--bootnext"), 16).to_bytes(2, "little"))
        elif "--delete-bootnext" in words:
            if os.path.exists(efivar(root, "BootNext")):
                os.remove(efivar(root, "BootNext"))
        else:
            sys.stderr.write(f"fakepm: efibootmgr {words} is not faked\n")
            return 1
    elif program == "bootctl" and words[-1] == "install":
        esp = next(w.split("=", 1)[1] for w in words if w.startswith("--esp-path="))
        write_bytes(os.path.join(esp, "EFI/systemd/systemd-bootx64.efi"), SD_BOOT)
        write_bytes(os.path.join(esp, "EFI/BOOT/BOOTX64.EFI"), SD_BOOT)
        write_bytes(os.path.join(esp, "loader/loader.conf"), b"#timeout 3\n#console-mode keep\n")
        os.makedirs(os.path.join(esp, "loader/entries"), exist_ok=True)
        new_boot_entry(root, "Linux Boot Manager", "\\EFI\\systemd\\systemd-bootx64.efi",
                       first=True)
    elif program == "bootctl" and words[-2:-1] == ["set-oneshot"]:
        # systemd-boot's own variable, in the loader's vendor namespace.
        name = f"LoaderEntryOneShot-{SD_VENDOR}"
        write_bytes(os.path.join(root, "sys/firmware/efi/efivars", name),
                    (7).to_bytes(4, "little") + (words[-1] + "\0").encode("utf-16-le"))
    elif program == "kernel-install" and words[0] == "add":
        esp = boot_root(root)
        if esp is None:
            sys.stderr.write("kernel-install: no loader/ on any ESP\n")
            return 1
        with open(os.path.join(root, "etc/kernel/cmdline"), encoding="utf-8") as handle:
            cmdline = handle.read().strip()
        with open(os.path.join(root, "etc/machine-id"), encoding="utf-8") as handle:
            token = handle.read().strip()
        write_bytes(os.path.join(esp, f"loader/entries/{token}-{words[1]}.conf"),
                    f"title Linux\nversion {words[1]}\noptions {cmdline}\n"
                    f"linux /{token}/{words[1]}/linux\n".encode())
    elif program == "grub-install":
        esp = next(w.split("=", 1)[1] for w in words if w.startswith("--efi-directory="))
        ident = next(w.split("=", 1)[1] for w in words if w.startswith("--bootloader-id="))
        write_bytes(os.path.join(esp, f"EFI/{ident}/grubx64.efi"), b"GRUB")
        new_boot_entry(root, ident, f"\\EFI\\{ident}\\grubx64.efi", first=True)
    else:
        sys.stderr.write(f"fakepm: {program} {words} is not faked\n")
        return 1
    return 0


def basics(state, machine, program, args):
    roots = [a[len("--root="):] for a in args if a.startswith("--root=")]
    if not roots:
        sys.stderr.write(f"fakepm: {program} without --root= would reach the running system\n")
        return 2
    root = roots[0]
    words = [a for a in args if not a.startswith("--root=") and a != "--"]
    system = machine.setdefault("os", {})
    status = 0
    if program == "hostnamectl" and words[0] == "set-hostname":
        with open(os.path.join(root, "etc/hostname"), "w", encoding="utf-8") as handle:
            handle.write(words[1] + "\n")
    elif program == "timedatectl" and words[0] == "set-timezone":
        if not os.path.isfile(os.path.join(root, "usr/share/zoneinfo", words[1])):
            sys.stderr.write(f"Failed to set time zone: Invalid or not installed time zone "
                             f"'{words[1]}'\n")
            return 1
        link = os.path.join(root, "etc/localtime")
        if os.path.lexists(link):
            os.remove(link)
        os.symlink("../usr/share/zoneinfo/" + words[1], link)
    elif program == "localectl" and words[0] == "status":
        # A localed with no locale of its own says `n/a`, as Debian's and Arch's do.
        first, *rest = system.get("locale", ["LANG=C.UTF-8"]) or ["n/a"]
        print(f"System Locale: {first}")
        for more in rest:
            print(f"               {more}")
        keymap = system.get("keymap") or "(unset)"
        keyboard = os.path.join(root, "etc/default/keyboard")
        if os.path.isfile(keyboard) and not system.get("localed_ignores_keyboard"):
            # Debian's localed reads the layout from the keyboard file; the Debian and Ubuntu
            # guests' localed may not, and reports `(unset)` (#585).
            with open(keyboard, encoding="utf-8") as handle:
                for line in handle:
                    if line.startswith("XKBLAYOUT="):
                        keymap = line.split("=", 1)[1].strip().strip('"') or "(unset)"
        print(f"    VC Keymap: {keymap}\n   X11 Layout: {keymap}")
    elif program == "localectl" and words[0] == "list-locales":
        for name in system.get("locales", ["C.UTF-8"]):
            print(name)
    elif program == "localectl" and words[0] == "set-locale":
        wanted = words[1].split("=", 1)[1]
        if wanted not in system.get("locales", ["C.UTF-8"]):
            sys.stderr.write(f"Failed to issue method call: Locale {wanted} not installed, "
                             "refusing.\n")
            return 1
        system["locale"] = words[1:]
    elif program == "localectl" and words[0] == "set-x11-keymap":
        if os.path.isfile(os.path.join(root, "etc/default/keyboard")):
            sys.stderr.write("Setting X11 and console keymaps is not supported in Debian.\n")
            return 1
        system["keymap"] = words[1]
    elif program == "nft":
        tables = system.setdefault("nft", {})
        if words[:2] == ["list", "tables"]:
            for name in tables:
                print(f"table {name}")
        elif words[:2] == ["list", "table"]:
            listing = tables.get(" ".join(words[2:4]))
            if listing is None:
                sys.stderr.write("Error: No such file or directory\n")
                return 1
            print(listing)
        elif words[:2] == ["delete", "table"]:
            if tables.pop(" ".join(words[2:4]), None) is None:
                sys.stderr.write("Error: No such file or directory\n")
                return 1
        elif words[0] == "-f":
            status = nft_file(tables, words[1])
        else:
            sys.stderr.write(f"fakepm: nft {words} is not faked\n")
            return 1
    elif program == "ping":
        target = words[-1]
        cut = system.get("cut")
        if (cut and cut_off(root, cut)) or target in system.get("silent", []):
            sys.stderr.write(f"PING {target}: 1 packets transmitted, 0 received\n")
            return 1
    elif program == "systemd-run":
        unit = next(a.split("=", 1)[1] for a in words if a.startswith("--unit="))
        system.setdefault("timers", {})[unit] = words
    elif program in ("netplan", "networkctl", "nmcli"):
        pass
    elif program == "sysctl" and words[0] == "-w" and "=" in words[1]:
        name, value = words[1].split("=", 1)
        path = os.path.join(root, "proc/sys", name.replace(".", "/"))
        if not os.path.isfile(path):
            sys.stderr.write(f"sysctl: cannot stat /proc/sys/{name.replace('.', '/')}: "
                             "No such file or directory\n")
            return 255
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(value + "\n")
    elif program == "modprobe" and len(words) == 1:
        if words[0] in system.get("absent_modules", []):
            sys.stderr.write(f"modprobe: FATAL: Module {words[0]} not found\n")
            return 1
        os.makedirs(os.path.join(root, "sys/module", words[0].replace("-", "_")), exist_ok=True)
    elif program == "grub2-mkconfig" and "--update-bls-cmdline" in words:
        sys.stderr.write("Unrecognized option `--update-bls-cmdline'\n")
        return 1
    elif program in ("grub-mkconfig", "grub2-mkconfig") and "-o" in words:
        grub_mkconfig(root, words, program)
    elif program == "grubby":
        status = grubby(root, words)
        if status:
            return status
    elif program in ("efibootmgr", "bootctl", "kernel-install", "grub-install"):
        status = boot_tools(root, program, words)
        if status:
            return status
    elif program in ("grub-reboot", "grub2-reboot") and len(words) == 1:
        folder = os.path.join(root, "boot", program.split("-")[0])
        os.makedirs(folder, exist_ok=True)
        with open(os.path.join(folder, "grubenv"), "w", encoding="utf-8") as handle:
            handle.write(f"next_entry={words[0]}\n")
    else:
        sys.stderr.write(f"fakepm: {program} {words} is not faked\n")
        return 1
    save(state, machine)
    return status


if __name__ == "__main__":
    sys.exit(main())
