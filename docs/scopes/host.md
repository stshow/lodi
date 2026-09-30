# Manage this machine's packages and files

With the host scope you can copy this machine's packages and changed `/etc` files into a folder
you own. You can then read that folder, edit it without root, and apply it to this machine or a
new one. You can also pin packages to a version or a date.

This is the one scope that changes the machine, so every command here runs as root. The other two
scopes, [a project](project.md) and [your home](home.md), never need root.

The examples below show your folder as `~/lodi`, your machine as `<hostname>` and your login
name as `<login>`. Your output names your own paths.

## Allow lodi to manage this machine with `lodi host arm`

Lodi does nothing to a machine until you allow it once:

```sh
sudo lodi host arm
```

```text
armed /: lodi host plan, apply and import may now act on it; delete /etc/lodi/host-allowed to disarm
  it
```

This writes one empty file, `/etc/lodi/host-allowed`, owned by root. Nothing else writes it. No
package install does it, and neither do `plan`, `apply` or `import`. Running the command again
leaves the file exactly as it is. To take the permission back, delete the file.

Until the file is there, every host command stops before it reads anything:

```text
lodi: error E_HOST_NOT_ARMED: / is not armed for the host scope: /etc/lodi/host-allowed does not
  exist
   = hint: arm this machine once, as root: sudo lodi host arm (it writes /etc/lodi/host-allowed, an
     empty file)
```

- Run as an ordinary user, the command stops with [`E_NEED_ROOT`](../ERRORS.md) and creates
  nothing. Lodi never runs `sudo` or `doas` for you.
- A marker that is not a plain empty file owned by root stops with [`E_EXISTS`](../ERRORS.md).
  A link there stops with an error too. Lodi never repairs it. Remove it and run the command again.
- `/`, `/etc` and `/etc/lodi` must be owned by root and writable by nobody else. The same holds
  for `host.toml` and `host.lock`. If not, lodi stops with [`E_PATH_ESCAPE`](../ERRORS.md).
- `--root DIR` points every host command at another directory tree instead of `/`. Use it for a
  tree of your own. Manage the packages of a real machine against `/`.

## Copy this machine into a folder with `lodi host import`

The import copies no file from `/etc`. It writes your settings instead, and it never opens
`/etc/shadow`, `/etc/gshadow`, SSH host keys or anything under `/etc/ssl/private`.

```sh
mkdir -p ~/lodi
sudo lodi host import ~/lodi
```

```text
W_UNCAPTURED: lodi-hand-built is not declared: its installed version is known only to this machine's
  own package database: a package file installed by hand, or a source this machine no longer has
wrote ~/lodi/<hostname>/home/<login>/home.toml
wrote ~/lodi/<hostname>/host.toml: 7 package(s) declared, 1 not captured, no file copied from
  /etc; no package and no file outside ~/lodi/<hostname> changed
read it first: NOT CAPTURED lists what was left out and how to declare it
next: sudo lodi host plan ~/lodi (writes nothing)
```

It changes nothing on the machine. You get this folder, owned by you:

```text
~/lodi/
└── <hostname>/
    ├── host.toml                 the packages and settings of this machine
    ├── files/etc/apt/keyrings/   the key of each commented [sources] block, if any
    └── home/<login>/home.toml    a commented starting point for your home
```

The usage line:

```text
lodi host import [--root DIR] [--out DIR | --stdout] [--force] [--dry-run]
                 [--no-home] [SOURCE [--host NAME]]
```

### What the import copies

- **Packages.** The packages you installed on purpose, from the distribution's own
  repositories. The distribution's base system, kernels and firmware are left out.
- **Settings.** Your hostname, time zone, locale and keymap, as `[system]`, and your
  accounts, as `[users]`. Commented examples of `[services]`, `[firewall]`, `[network]` and
  `[etc."PATH"]` follow, to fill in yourself.
- **No files.** A configuration file your package manager reports as changed is listed under
  `NOT CAPTURED`, with the `[etc."motd"]` style table you write to declare its text.
- **Pins.** A `[host] snapshot` line with the last full UTC day, so a new machine installs the
  same versions. See [Pin a package to a version](#pin-a-package-to-a-version).
- **Extra apt repositories.** Each third-party one you installed from becomes a `[sources]`
  block with every line commented out. Uncomment it on purpose, on the new machine.
- **Extra suites.** Ubuntu's `backports` or `proposed`, or Debian's `backports`, becomes a
  commented `[sources]` block too, signed by your distribution's own key.

Everything the import leaves out is listed at the end of `host.toml`, under `NOT CAPTURED`,
with the reason. Each one is also a `W_UNCAPTURED` line on the screen. This includes packages
from a PPA, a package file, the AUR, snaps and flatpaks.

The import never reads a secret. It does not open `/etc/shadow`, SSH host keys, `.key` and
`.pem` files, `/etc/sudoers.d/` or `/etc/netrc`, even when a package says it changed them.

### Other places to write it

```sh
sudo lodi host import                   # into /etc/lodi/host.toml, where a bare plan reads it
sudo lodi host import --out /srv/box    # into /srv/box/host.toml
sudo lodi host import --stdout          # print host.toml only, and write nothing
```

`--out` into `/etc/lodi` or `/var/lib/lodi` stops with [`E_STORE_IO`](../ERRORS.md). A link
planted in the destination stops the import, and what it points at is left alone.

### Import again after you change the machine by hand

Installed or removed packages with `apt` or `pacman` since the last import? Import again. Lodi
adds what changed on the machine to `host.toml` and keeps your own edits. See it first:

```sh
sudo lodi host import --dry-run ~/lodi
```

The output, shortened:

```text
--- ~/lodi/<hostname>/host.toml
+++ ~/lodi/<hostname>/host.toml (reconciled)
@@ -34,6 +34,7 @@
   "git",
   "linux-image-amd64",
   "ripgrep",
+  "sqlite3",
   "task-english",
   "task-ssh-server",
 ]
would reconcile ~/lodi/<hostname>/host.toml: 2 added, 0 removed, 0 kept, 0 conflict(s); nothing was
  written (--dry-run)
```

- A package installed by hand is added, and one removed by hand leaves every list it is in.
- A line you deleted stays deleted. Your comments, blank lines and list order stay too.
- A configuration file changed on the machine is named in a `W_RECONCILE` line, and never copied.
- When both sides changed the same thing, your side wins and lodi prints a `W_RECONCILE` line.
- `--force` replaces `host.toml` instead of merging.

The merge needs the record the last apply or import wrote in `/etc/lodi/host.lock`. With no
record, lodi prints a report and writes nothing. Run `sudo lodi host apply ~/lodi` once to write it.

## See what would change with `lodi host plan`

```text
lodi host plan [--root DIR] [--no-home] [SOURCE [--host NAME]]
```

```sh
sudo lodi host plan ~/lodi
```

```text
+ file /etc/issue.net (mode 0644, owner root:root)
= file /etc/ssh/sshd_config (unchanged)
1 action(s)
home <login>
0 files: nothing to do
```

The plan changes nothing on the machine and writes no file. It runs every check an apply runs
first, so an error shows up here before anything changes. `+` adds, `~` changes, `-` removes and
`=` leaves as it is.

Lodi picks the host from `SOURCE`. If `SOURCE/host.toml` exists, that is the host. Otherwise
it is `SOURCE/<hostname>/`, or `SOURCE/NAME/` with `--host NAME`. With no `SOURCE`, lodi reads
`/etc/lodi/host.toml`. A host that is not there stops with [`E_NO_MANIFEST`](../ERRORS.md), and
the hint lists the hosts the folder has.

## Make the machine match with `lodi host apply`

```text
lodi host apply [--root DIR] [--overwrite-drift] [--resolved JOURNAL-ID]
                [--no-update] [--unsupported-partial-upgrade] [--no-home]
                [SOURCE [--host NAME]]
```

**Before you apply, read the plan.** An apply can remove packages. On Arch it also upgrades the
whole machine. There is no rollback: to go back, put the old `host.toml` back and apply again.

```sh
sudo lodi host apply ~/lodi
```

```text
journal 20260926T210702Z-4ab970f50ec9cdb778a996441e98dee3
+ file /etc/issue.net (mode 0644, owner root:root)
1 action(s) applied
home <login>
nothing to do
```

Run it again and there is nothing to do:

```text
nothing to do
home <login>
nothing to do
```

The apply makes the changes the plan shows, and then writes `/etc/lodi/host.lock`. That file
records what was applied. It is never a pin.

### Packages on Debian and Ubuntu

All the changes go into one `apt-get install`, so apt solves them together. Lodi refreshes the
package index first when it is more than six hours old. `--no-update` skips that. Lodi never runs
`apt-get upgrade`, `dist-upgrade` or `autoremove`.

That invocation carries `-o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold`,
on the real machine exactly as under `--root`. So no package can stop an apply to ask about a
configuration file.

Say a package ships a new version of a file you changed. Then dpkg **keeps the
machine's version** and leaves the package's new one beside it (normally as `.dpkg-dist`).
Lodi does not report which files that happened to. Compare them yourself after an apply.

### Packages on Arch

**An apply on Arch upgrades the whole machine.** Lodi installs with `pacman -Syu`, because Arch
does not support installing against an old package database. `--no-update` has nothing to skip.

- `--unsupported-partial-upgrade` installs with `pacman -S --needed` and upgrades nothing. It
  prints `W_ARCH_PARTIAL`. Arch does not support the state it leaves. On Debian or Ubuntu the
  flag stops with [`E_UNSUPPORTED`](../ERRORS.md).
- A `hold` only holds back lodi's own `pacman -Syu`. Your own `pacman -Syu` still upgrades the
  package, and the plan says so: `~ package NAME (hold: lodi transactions only)`.

### Packages on Fedora

Lodi manages Fedora 44 with dnf5 and rpm, and SELinux stays enforcing. The import writes your
packages under their own table:

```toml
[packages.fedora]
add = [
  "htop",
  "tree",
]
```

Delete `tree` from that list and add `figlet`, and the plan names the exact build dnf5 installs
or removes:

```sh
sudo lodi host plan
```

```text
+ package figlet 0:2.2.5-39.fc44.x86_64
- package tree 0:2.2.1-4.fc44.x86_64 (not in the manifest)
2 action(s)
```

- Lodi installs with `dnf5 install` and removes with `dnf5 remove`. It never installs a weak
  dependency, and it refreshes the package index with `dnf5 makecache` when it is more than six
  hours old.
- A `hold` only holds back lodi's own `dnf5 install`. Your own `dnf5 upgrade` still upgrades it.
- Every file lodi writes keeps the SELinux label its place had. Lodi never switches SELinux off
  and never relabels your files.
- dnf has no metapackages section. If you installed a package that another listed package needs,
  the import lists both. On Debian, Ubuntu and Arch a metapackage stands for what it needs.
- Lodi never writes Fedora's keys, repositories, dnf variables or SELinux policy settings
  (`/etc/pki`, `/etc/yum.repos.d`, `/etc/dnf/vars`, `/etc/selinux`). A `[files]` entry there
  stops with [`E_PROTECTED_PATH`](../ERRORS.md), and the import never copies one.
- The only files lodi writes there are the `lodi-` ones of a
  [repository you add](#add-a-third-party-dnf-repository-on-fedora).
- Fedora Silverblue and other rpm-ostree systems are not supported. Every host command stops
  there with [`E_HOST_OSTREE`](../ERRORS.md) before it reads or changes anything.
- A machine that only says it is like Fedora, in `ID_LIKE`, is not managed.
- A Fedora pin keeps one exact build. See
  [Pin a Fedora package to one build](#pin-a-fedora-package-to-one-build). `[host] snapshot`
  and date pins stop with [`E_UNSUPPORTED`](../ERRORS.md), because Fedora keeps no copy of its
  repositories as they were on each date.

### A file changed by hand

A file you edited since lodi wrote it shows up in the plan, and the apply stops:

```text
W_DRIFT: /etc/motd was changed since lodi last wrote it
~ file /etc/motd (drift, content)
```

```text
lodi: error E_DECLINED: /etc/motd was changed since lodi last wrote it
   = hint: run the apply again with --overwrite-drift to replace it, or fold the change into the
     manifest
```

Nothing was changed. To keep your edit, put its text in the entry. To replace it:

```sh
sudo lodi host apply --overwrite-drift ~/lodi
```

### A file lodi did not write

The first time lodi writes over a file it did not create, it keeps a copy. When you later take
the entry out of `host.toml`, `on_remove` decides what happens:

- `restore`, the default, puts the original back.
- `delete` removes the file.
- `keep` leaves it as it is.

A file lodi did not create is never deleted by `restore`. With `backup = false` there is no copy,
so the file is left in place when its entry goes.

### An apply that stopped part way

Each apply writes a journal under `/var/lib/lodi/host/journal/` before it changes anything. After
a crash or a power cut, the next apply reads it and does one of three things:

| What lodi finds | What it does |
|---|---|
| The interrupted step finished | Carries on with the rest |
| The step never started | Runs it again |
| Neither | Stops with [`E_JOURNAL_AMBIGUOUS`](../ERRORS.md) and names the journal |

In the last case, read the journal and put the machine in the state you want. On Debian or Ubuntu
that is often `sudo dpkg --configure -a`. On Arch, check `/var/log/pacman.log` and remove
`/var/lib/pacman/db.lck` once no pacman is running. Then name the journal:

```sh
sudo lodi host apply --resolved 20260926T210702Z-4ab970f50ec9cdb778a996441e98dee3 ~/lodi
```

A step that failed stops the apply with [`E_APPLY`](../ERRORS.md). The message names each step
that ran and each that did not. The steps that ran stay done, and the next plan shows what is
left. Only one apply runs at a time. A second one stops with [`E_SYSTEM_BUSY`](../ERRORS.md).

## Write `host.toml` by hand

```toml
[host]
distro = "debian"
packages = "exact"

[etc."issue.net"]
text = "Authorized use only.\n"

[packages]
common = ["tree", "curl"]
hold = ["curl"]
optional = ["a-name-not-every-release-has"]

[packages.debian]
add = ["zip"]
```

| Key | What it does |
|---|---|
| `[host] distro` | The distribution the file is for. On another one lodi stops with [`E_HOST_MISMATCH`](../ERRORS.md) |
| `[host] min_lodi_version` | The oldest lodi that can read the file, such as `">=1.4.0"`. An older lodi stops with [`E_VERSION`](../ERRORS.md) |
| `[host] packages` | `"exact"` or `"managed"`. See [Remove the packages you delete from the list](#remove-the-packages-you-delete-from-the-list) |
| `[packages] common` | Packages for every distribution |
| `[packages.debian] add` | Packages for Debian only. `[packages.ubuntu]` and `[packages.arch]` work the same way |
| `hold` | Keep these packages at the version they have |
| `mark_auto` | Mark these as installed only as a dependency |
| `optional` | Skip these, with `W_OPTIONAL_SKIPPED`, where the index does not have them |
| `absent` | Remove these wherever they are installed |
| `[vars]` | Values you use as `${vars.NAME}` in other strings. An unset one stops with [`E_VAR_UNSET`](../ERRORS.md) |

## Write a file under `/etc` with `[etc."PATH"]`

Put the text you want in the file, keyed by its path below `/etc`:

```toml
[etc."motd"]
text = """
Welcome to the build box.
"""
```

```sh
sudo lodi plan ~/lodi
```

On a fresh Debian 12, just after the import, the plan says:

```text
~ file /etc/motd (content)
1 action(s)
home <login>
0 files: nothing to do
W_REPLACED_UNMANAGED: /etc/motd already exists; its original bytes will be kept
```

`sudo lodi apply ~/lodi` writes exactly that text to `/etc/motd`, and nothing else under
`/etc`. Lodi writes no path under `/etc` that `host.toml` does not name. The first time it
writes over a file, it keeps the old one under `/etc/lodi/originals/`.

| Key | Default | What it does |
|---|---|---|
| `text` | none | The file's text |
| `mode` | `"0644"` | The file's mode |
| `owner`, `group` | `"root"` | The file's owner and group |

A path outside `/etc` stops with [`E_PATH_ESCAPE`](../ERRORS.md). A path that holds a secret,
such as `shadow` or `ssh/ssh_host_ed25519_key`, stops with [`E_PROTECTED_PATH`](../ERRORS.md).

### Move from `[files]`

A `host.toml` with the old `[files]` table still applies as before. The plan and the apply print
one line:

```text
W_LEGACY_FILES: host.toml declares [files]; it applies as before, and [etc."PATH"] with the file's
  text is the form that replaces it
```

Move each entry under `/etc` to `[etc."PATH"]`. Put the file's text in `text`, and keep `mode`,
`owner` and `group`:

```toml
# before
[files."/etc/motd"]
source = "files/etc/motd"

# after
[etc."motd"]
text = "Welcome to the build box.\n"
```

Each `[files."PATH"]` entry takes these keys:

| Key | Default | What it does |
|---|---|---|
| `content` or `source` | none | The file's text, or a path under `files/`. Give exactly one |
| `mode` | `"0644"` | The file's mode |
| `owner`, `group` | `"root"` | The file's owner and group |
| `backup` | `true` | Keep a copy of a file lodi did not write |
| `on_remove` | `"restore"` | `restore`, `delete` or `keep`, when the entry leaves the file |

A mode with the set-uid or set-gid bit, or one anyone can write, is applied as written. The plan
warns about it first with `W_FILE_MODE`.

Lodi checks the whole file before it changes anything, and names the line and column of each
mistake. A name the index does not have stops with [`E_UNKNOWN_PACKAGE`](../ERRORS.md), which
lists the nearest names. A package name never carries a version. Pin it instead, as below.

## Remove the packages you delete from the list

**With `packages = "exact"`, deleting a line removes the package.** The import writes this
setting. Read the plan before each apply, because it names every package that leaves:

```text
- package cowsay (not in the manifest)
- package db5.3 (needed by nothing once the rest is removed)
- package perl (needed by nothing once the rest is removed)
= file /etc/default/grub (unchanged)
= file /etc/locale.gen (unchanged)
= file /etc/shells (unchanged)
1 action(s)
```

- A package another declared package still needs stays, marked as a dependency:
  `~ package librecode3 (a dependency now: needed by fortune-mod; stays)`.
- The base system and the kernel stay, even when you delete their lines:
  `= package libtext-charwidth-perl (no longer needed, but part of the base system; not removed)`.
- A package from a PPA, a package file or the AUR that lodi never recorded is never touched.
- A package installed by hand since lodi last recorded the machine is a `W_DRIFT` line. The
  apply stops with [`E_DECLINED`](../ERRORS.md) until you give `--overwrite-drift`.
- `--overwrite-drift` removes **every** package the plan lists as installed by hand. Read the
  plan before you give it.

Before it removes anything, lodi asks the package manager what would go, with a command that
changes nothing. It asks again right before the removal. A different answer stops the apply with
nothing removed. A removal that would take a declared or base-system package stops before anything
changes.

`packages = "managed"` removes only what lodi recorded installing, and never a package you
installed by hand. A file with no `packages` line works the same way. It also prints one
`W_UNDECLARED` line for each package the file does not declare.

## Turn services on and off with `[services]`

Declare each systemd unit you care about, `enabled` or `disabled`, in `host.toml`:

```toml
[services]
"fstrim.timer" = "enabled"
"logrotate.timer" = "disabled"
```

`sudo lodi plan` lists each change as `systemctl` would run it, and changes nothing. Here, on
Debian 12, `fstrim.timer` was disabled and `logrotate.timer` running:

```text
+ service fstrim.timer (systemctl enable --now -- fstrim.timer)
- service logrotate.timer (systemctl disable --now -- logrotate.timer)
2 action(s)
```

Then `sudo lodi apply` runs those commands, and a second apply has nothing to do.

- `enabled` means enabled and running now, and `disabled` means disabled and stopped. A unit you
  started or stopped by hand is put back the way `host.toml` says.
- Services come after packages and files. A unit whose package the same apply installs is
  enabled once the package is there, and the plan says `after the package transaction`.
- **Delete a unit's line and the next apply runs `systemctl preset` for it.** The unit is
  enabled or disabled as your distribution ships it. Whether it runs now is left as it is.
- **A unit you never declared is never touched.** lodi does not even ask about it.
- A unit this machine does not have stops the plan and the apply with
  [`E_UNKNOWN_UNIT`](../ERRORS.md) before anything changes. So does a static or masked unit.
- `git revert` of a change to `[services]`, then `sudo lodi apply`, brings back the states you
  declared before.
- Name each unit in full, ending in `.service`, `.socket`, `.timer` or `.path`. Package exact mode
  never enables, disables or presets a unit, and `lodi import` keeps the `[services]` you wrote.

## Add users and groups

Declare an account, and the group it works in, in `host.toml`:

```toml
[users.alice]
uid = 1001
groups = ["sudo"]
shell = "/bin/bash"
home = "/home/alice"
ssh_keys = ["ssh-ed25519 AAAAC3Nza... alice-laptop"]

[groups.devs]
gid = 3000
members = ["alice"]
```

Then `sudo lodi plan` shows what the apply would do:

```text
+ group devs (gid 3000)
+ user alice (uid 1001, locked password)
~ group devs (members alice)
+ ssh keys alice (1 key(s))
4 action(s)
```

`sudo lodi apply` creates the account with a **locked password** and puts the key in
`~/.ssh/authorized_keys` in alice's home, which alice owns. Log in with the key, or set a
password on the machine with `sudo passwd alice`. `host.toml` has no key for a password or a
hash, so none can reach your repository.

- A group gets exactly the members you list, and every account whose `groups` names it.
- An existing account keeps its password. lodi sets its shell and adds it to the groups you
  list, and adds a key that is not in its `authorized_keys` yet. It removes no key.
- An account whose UID or home differs from `host.toml`, or a group whose GID does, stops with
  [`E_IDENTITY_CONFLICT`](../ERRORS.md). lodi never changes a UID, GID or home.
- A private key, or anything that is not one public key, in `ssh_keys` stops with
  [`E_SSH_KEY`](../ERRORS.md).

| Key | What it does |
|---|---|
| `[users.NAME] uid` | The account's UID, from 1. Optional: the distribution picks one |
| `[users.NAME] groups` | Groups the account joins. Each exists or is declared |
| `[users.NAME] shell` | The login shell, an absolute path. Optional |
| `[users.NAME] home` | The home, `/home/NAME` when left out |
| `[users.NAME] ssh_keys` | Public keys, one line each, for `authorized_keys` |
| `[groups.NAME] gid` | The group's GID, from 1. Optional |
| `[groups.NAME] members` | Exactly the group's members |

A name is lowercase letters, digits, `_` and `-`, or it stops with [`E_IDENT`](../ERRORS.md).

**Deleting an account or a group from `host.toml` leaves it on the machine**, with its home and
its files. The next plan and apply say so once:

```text
= user alice (no longer managed: left as it is, with its home)
= group devs (no longer managed: left as it is)
```

So `git revert` of the commit that added an account, then `sudo lodi apply`, removes nothing.
Remove an account yourself with `sudo userdel alice` when you mean to.

`sudo lodi import` writes your own accounts: those with a UID from 1000, or `UID_MIN` in
`/etc/login.defs`, and a login shell that `/etc/shells` lists. System accounts, `nobody` and
accounts with `nologin` or `false` are left out. The import never reads `/etc/shadow`, and
copies no key and no file from a home. Importing again keeps the `[users]` and `[groups]` you
wrote.

## Set the hostname, time zone, locale, keymap, firewall and network

Declare the basics of the machine in `host.toml`:

```toml
[system]
hostname = "sd1-basics"
timezone = "Europe/Berlin"
locale = "en_GB.UTF-8"
keymap = "de"

[firewall]
allow = ["22/tcp"]

[network]
confirm_within = 30

[network.interfaces.enp0s2]
dhcp = true
addresses = ["10.0.2.50/24"]
```

`sudo lodi plan` lists each change as the tool would make it, and changes nothing. Here is the
plan on a freshly installed Debian 12 machine:

```text
~ hostname lodi-debian -> sd1-basics (hostnamectl set-hostname -- sd1-basics)
~ timezone Etc/UTC -> Europe/Berlin (timedatectl set-timezone -- Europe/Berlin)
~ locale LANG=C.UTF-8 -> LANG=en_GB.UTF-8 (localectl set-locale -- LANG=en_GB.UTF-8)
~ keymap us -> de (XKBLAYOUT in /etc/default/keyboard)
+ firewall table inet lodi (nft -f /etc/lodi/firewall.nft)
+ network enp0s2 (netplan /etc/netplan/90-lodi.yaml; ping 10.0.2.2 within 30 s, else put back)
6 action(s)
home lodi
0 files: nothing to do
```

Then `sudo lodi apply` makes each change, and a second apply has nothing to do.

- **Each basic you leave out is never touched.** A hostname is set only when `[system]` names
  one. A `host.toml` without these tables plans and applies exactly as before.
- lodi sets them with the distribution's own tools: `hostnamectl`, `timedatectl` and
  `localectl`. `keymap` is the keyboard layout, and `localectl` sets the console keymap too.
  Where `/etc/default/keyboard` exists, as on Ubuntu, lodi sets its `XKBLAYOUT` line instead.
- A time zone or locale this machine does not have stops the plan with
  [`E_UNKNOWN_SETTING`](../ERRORS.md) before anything changes. `localectl list-locales` lists
  the locales you can declare. On Debian, install `locales-all` for more of them.
- lodi picks the host folder by hostname. After a new hostname, pass the old one with
  `--host OLD`, or rename the folder to the new one.
- **Delete a line, or `git revert` the change, and the next apply puts back what the machine had
  before lodi first set it.**

### The firewall

`[firewall]` is one nftables table, `inet lodi`, and lodi never touches another table. Incoming
traffic is dropped, except what `allow` names, what answers your own connections, the loopback
interface and ICMP. To let everything in, write:

```toml
[firewall]
input = "accept"
```

lodi keeps the table in `/etc/lodi/firewall.nft`, and `lodi-firewall.service` loads it at boot.
Delete `[firewall]` and the next apply removes the table, the file and the unit.

lodi needs `nft`: on Debian and Ubuntu, add `nftables` to `[packages]` and apply that first. While
`ufw` or `firewalld` is active, a `[firewall]` stops the plan with
[`E_FIREWALL_CONFLICT`](../ERRORS.md) and nothing changes. lodi never runs beside another firewall.

### The network

lodi configures the network through the stack the machine already runs: netplan, NetworkManager
or systemd-networkd. It writes files of its own, and never edits yours:
`/etc/netplan/90-lodi.yaml`, `/etc/NetworkManager/system-connections/lodi-NAME.nmconnection` or
`/etc/systemd/network/00-lodi-NAME.network`. With no stack it drives, or two at once, the plan
stops with [`E_NETWORK_STACK`](../ERRORS.md). Each interface takes `dhcp`, `addresses`, `gateway`
and `dns`.

**A network change rolls back by itself when the machine cannot be reached.** Before the change,
lodi keeps the files it replaces and starts a timer. After the change it pings one address:
`[network] check`, else the gateway you declared, else the gateway the machine had.

When the address answers within `confirm_within` seconds, 90 unless you set it, the change stays.
When it does not, lodi puts the earlier files back and stops with
[`E_NETWORK_ROLLED_BACK`](../ERRORS.md). If lodi is gone with your SSH session, the timer puts
them back about 30 seconds later:

```toml
[network]
check = "192.168.1.1"
confirm_within = 60
```

Package exact mode never removes the network stack or the firewall tools, and `lodi import` keeps
the `[system]`, `[firewall]` and `[network]` tables you wrote.

## Set kernel parameters, modules and sysctl

Declare them in `host.toml`:

```toml
[kernel]
parameters = ["mitigations=off"]
modules = ["dummy"]
blacklist = ["pcspkr"]

[sysctl]
"net.ipv4.ip_forward" = "1"
```

`sudo lodi plan` lists each change, and changes nothing. Here is the plan on a Debian 12
machine:

```text
+ parameters mitigations=off (a trial boot: lodi-trial once, by /boot/efi/EFI/lodi.env)
+ modules dummy (/etc/modules-load.d/lodi.conf; modprobe -- dummy)
+ blacklist pcspkr (/etc/modprobe.d/lodi-blacklist.conf)
~ sysctl net.ipv4.ip_forward 0 -> 1 (sysctl -w net.ipv4.ip_forward=1)
= parameters mitigations=off (a trial boot follows: reboot, then run `sudo lodi boot confirm`)
4 action(s)
```

Then `sudo lodi apply` makes each change, and a second apply has nothing to do.

- **A parameter change boots once, as a trial, before it becomes the default.** See
  [the trial boot](#try-a-boot-change-once-then-confirm-it-with-lodi-boot-confirm).
- Once you confirm it, lodi adds one line of its own to `/etc/default/grub`. Then it runs
  `grub-mkconfig`. On Fedora it runs `grubby`, which also sets the parameter for kernels you
  install later.
- Without `/etc/default/grub`, the plan stops with [`E_BOOT_LOADER`](../ERRORS.md) and nothing
  changes.
- **A module in `modules` is loaded now and at every boot**, from
  `/etc/modules-load.d/lodi.conf`. A module in `blacklist` is kept from loading from the next
  boot on, by `/etc/modprobe.d/lodi-blacklist.conf`. lodi never unloads a running module.
- **A sysctl value is set now and at every boot**, from `/etc/sysctl.d/90-lodi.conf`. A name
  this machine does not have stops the plan with [`E_UNKNOWN_SETTING`](../ERRORS.md). Write a
  value as a string, or a number as a number.
- **Delete a line, or `git revert` the change, and the next apply puts back the distribution's
  own settings.** lodi removes its own line or file, and nothing else. A parameter or a module
  goes back at the next boot.
- A sysctl value goes back at once, to what it was before lodi first set it.

Package exact mode never removes the running kernel or the boot loader, and `lodi import` keeps
the `[kernel]` and `[sysctl]` tables you wrote.

## Choose the bootloader: GRUB or systemd-boot

Declare the loader in `host.toml`, with its timeout and default entry if you want to set them:

```toml
[kernel]
parameters = ["lodi.bl1=1"]

[boot]
loader = "systemd-boot"
timeout = 4
default = "1d3e396edf92427e9514064b96fc9cd0-*"
```

Here `default` matches this machine's own entries, which start with its machine id.
`sudo lodi plan` lists each change, and changes nothing. Here is the plan on a Debian 12
machine that starts GRUB:

```text
~ loader grub -> systemd-boot (…; a trial boot: systemd-boot once, by BootNext; grub stays …)
+ timeout 4 (/boot/efi/loader/loader.conf)
+ default 1d3e396edf92427e9514064b96fc9cd0-* (/boot/efi/loader/loader.conf)
= loader grub -> systemd-boot (a trial boot follows: reboot, then run `sudo lodi boot confirm`)
3 action(s)
home lodi
0 files: nothing to do
```

The loader's line is shortened here. It also names the packages, `bootctl install` and
`kernel-install`. It says GRUB stays first until you confirm, then as the fallback entry.

Then `sudo lodi apply` installs systemd-boot, and a second apply has nothing to do. Reboot: the
firmware starts systemd-boot once, as a trial. `bootctl status` shows systemd-boot, and
`cat /proc/cmdline` shows your parameters. Run `sudo lodi boot confirm` to keep it:

```text
~ loader systemd-boot (the default from now on)
confirmed; grub stays as the fallback entry
```

Write `loader = "grub"` and apply again to go back to GRUB, the same way.

- **A switch boots once as a trial.** The apply keeps the earlier loader first in the
  firmware's boot order. The firmware starts the new one once, by `BootNext`. `sudo lodi boot
  confirm` in that boot puts the new loader first.
- **Without a confirmation, the next boot starts the earlier loader again.** The next `sudo
  lodi plan` says the trial reverted. See
  [Try a boot change once](#try-a-boot-change-once-then-confirm-it-with-lodi-boot-confirm).
- **The earlier loader stays as the fallback entry.** Once you confirm, the new loader comes
  first, and the earlier one right after it. lodi removes neither. If the new loader cannot
  start, the firmware starts the old one.
- To start the earlier loader yourself, pick it in the firmware's boot menu. Or run
  `sudo efibootmgr --bootnext NUMBER` with its number from `efibootmgr`, and reboot.
- **A switch that fails changes nothing.** lodi keeps a copy of the EFI system partition and
  the boot order first, and puts both back if any step fails. The machine boots as before.
- **lodi manages only a UEFI bootloader.** On a machine started the older way, `[boot]` stops
  the plan with [`E_BOOT_NOT_UEFI`](../ERRORS.md) and nothing changes. If lodi cannot find the
  EFI system partition or the loader in use, the plan stops with
  [`E_BOOT_LOADER`](../ERRORS.md).
- `timeout` is in seconds, 0 to 600. `default` is written as the loader takes it. For GRUB
  that is `0` or `saved`. For systemd-boot it is an entry name, or a pattern such as `debian-*`
  or `@saved`.
- The kernel command line is `[kernel] parameters`. systemd-boot gets the machine's own command
  line with your parameters, from `/etc/kernel/cmdline`. GRUB, the fallback, keeps the command
  line it booted.
- On Ubuntu 24.04, `/boot` is a partition the firmware cannot read. There lodi also writes
  `/etc/kernel/install.conf`, so each kernel goes to the EFI system partition. A switch back to
  GRUB removes that file.
- **Delete `[boot]`, or `git revert` the change, and the next apply puts your distribution's
  loader back first.** It removes lodi's timeout and default. Both loaders stay installed.
  This is not a trial, and a switch that waits is forgotten.

Package exact mode never removes either loader's packages.

## Try a boot change once, then confirm it with `lodi boot confirm`

A change to `[kernel] parameters` could stop your machine from booting. So `sudo lodi apply`
does not change the default boot entry. It adds one entry, `lodi-trial`, with the new
parameters, and sets a flag in `EFI/lodi.env` on the EFI system partition.

GRUB clears that flag as it starts, and only then boots the trial, so the trial boots once. Here
is the apply on a Debian 12 machine, for `parameters = ["lodi.bv1=one"]`:

```text
= parameters lodi.bv1=one (a trial boot follows: reboot, then run `sudo lodi boot confirm`)
journal 20260929T135015Z-7825f64618e4e39a2a6134c2e632fdd8
+ parameters lodi.bv1=one (a trial boot: lodi-trial once, by /boot/efi/EFI/lodi.env)
1 action(s) applied
```

1. Reboot. The machine boots the trial entry once.
2. Check it, then run `sudo lodi boot confirm` in that boot:

   ```text
   ~ parameters lodi.bv1=one (the default entry from now on)
   confirmed; the entry that was the default stays as lodi-known-good
   ```

   The change is now the default. The entry that was the default before stays in the boot menu
   as `lodi: known good`.
3. **If you do not confirm it**, the next boot is the earlier entry again, with nothing to
   do. A machine that hangs in the trial boots the old way after a reset.

   The next `sudo lodi plan` says so. Here a second change, `lodi.bv1=two`, was not confirmed:

   ```text
   ~ parameters lodi.bv1=two (a trial boot: lodi-trial once, by /boot/efi/EFI/lodi.env)
   = parameters lodi.bv1=two (not confirmed: the trial reverted to the earlier entry)
   = parameters lodi.bv1=two (a trial boot follows: reboot, then run `sudo lodi boot confirm`)
   1 action(s)
   ```

   `sudo lodi apply` sets the trial again. Change `host.toml` first if the trial did not boot.

Until you reboot, `sudo lodi plan` says the trial follows, and a second apply has nothing to do.
`lodi boot confirm` in any other boot stops with [`E_BOOT_TRIAL`](../ERRORS.md) and changes
nothing. A machine without an EFI system partition at `/efi`, `/boot/efi` or `/boot` stops the
plan with [`E_BOOT_LOADER`](../ERRORS.md), and nothing changes.

Under systemd-boot (see [Choose the bootloader](#choose-the-bootloader-grub-or-systemd-boot)),
a change is tried the same way, with systemd-boot's own one-shot. lodi writes the entry
`lodi-trial.conf` on the EFI system partition. It is a copy of your default entry with the new
command line. Then lodi runs `bootctl set-oneshot`, which systemd-boot forgets as it boots the
entry once. Here is the apply on an Ubuntu 24.04 machine that runs systemd-boot, for
`parameters = ["lodi.bl1=2"]` in place of a confirmed `lodi.bl1=1`:

```text
= parameters lodi.bl1=2 (a trial boot follows: reboot, then run `sudo lodi boot confirm`)
journal 20260929T152627Z-e5564a910a989f1bded4108d66766ad4
~ cmdline root=UUID=… ro console=tty1 console=ttyS0 lodi.bl1=2 (a trial boot: lodi-trial once, …)
1 action(s) applied
```

The line is shortened here. It names `bootctl --esp-path=/boot/efi set-oneshot lodi-trial.conf`.
`sudo lodi boot confirm` in the trial writes `/etc/kernel/cmdline` and each kernel's entry. The
entry that was the default stays as `lodi-known-good.conf`. Unconfirmed, the next boot is your
default entry with `lodi.bl1=1` again.

Delete `parameters`, or `git revert` the change, and the next apply puts back your
distribution's own command line as the default. The last entry you confirmed stays as
`lodi: known good`, so you can still pick it from the boot menu. Modules and sysctl values take
effect without a reboot and are never a trial.

## Add a third-party apt repository

Declare the repository and its signing key, and lodi adds it before it installs from it:

```toml
[host]
distro = "debian"
min_lodi_version = ">=1.3.0"

[sources.docker]
uris = ["https://download.docker.com/linux/debian"]
suites = ["bookworm"]
components = ["stable"]
signed_by = "files/etc/apt/keyrings/docker.asc"
signed_by_sha256 = "<the sha256sum of that file>"

[packages]
common = ["docker-ce"]
```

The plan shows each repository first, then the index refresh. For a repository named
`lodigate` on a test machine, it printed:

```text
+ source lodigate (keyring, sources)
~ index (apt-get update -o APT::Update::Error-Mode=any)
2 action(s)
```

The apply writes `/etc/apt/keyrings/lodi-docker.asc` and
`/etc/apt/sources.list.d/lodi-docker.sources`, then refreshes the index. Take the table out and
the next apply removes both files.

| Key | Needed | What it takes |
|---|---|---|
| `types` | no, `["deb"]` | `deb` or `deb-src` |
| `uris` | yes | `https://` addresses only. Another scheme, a user name or a password stops with an error |
| `suites` | yes | Suite names. One that ends in `/` is a flat repository |
| `components` | yes, unless every suite is flat | Component names |
| `architectures` | no | dpkg architecture names such as `amd64` |
| `signed_by` | this or `signed_by_url` | The keyring's path, next to `host.toml` |
| `signed_by_url` | this or `signed_by` | The keyring's `https://` address. Only the apply downloads it |
| `signed_by_sha256` | yes | The keyring's SHA-256 digest |

- **Lodi checks the keyring's digest before it writes anything.** A key the publisher replaced
  stops the plan and the apply with [`E_HASH_MISMATCH`](../ERRORS.md). Check the new key through a
  channel you trust, then write its digest.
- **The keyring must hold public keys only.** One with a private key, or that is not a keyring,
  stops with [`E_TYPE`](../ERRORS.md). Every key in the file is trusted for that repository.
- **One repository, one entry.** If `/etc/apt` already lists the same repository, lodi stops with
  [`E_DUP_RESOURCE`](../ERRORS.md) and names that file. Disable that entry first.
- **A failed refresh puts things back.** If apt cannot reach the repository or check its key, the
  apply restores the source files it changed and stops with nothing installed.
- **An edit by hand** of either file stops the apply with [`E_DRIFT`](../ERRORS.md) until you
  give `--overwrite-drift`.

A key the table does not know stops with [`E_UNKNOWN_ATTR`](../ERRORS.md). Apt options such as
`trusted` are not supported. On Arch, a `[sources]` table stops with an error, and lodi never
edits `pacman.conf`.

## Add a third-party dnf repository on Fedora

On Fedora 44 the same table adds a dnf repository and its signing key. Download the key, check
it with the publisher, and put it next to `host.toml`:

```toml
[host]
distro = "fedora"

[sources.ghcli]
uris = ["https://cli.github.com/packages/rpm"]
signed_by = "files/ghcli.asc"
signed_by_sha256 = "cec6e9ed82d3949ca5f4428cc968b41ef5e7416cb3653cdfc2a421977663bbfd"

[packages.fedora]
add = ["gh"]
```

```sh
sudo lodi host plan
```

On a test machine it printed the repository first, then the refresh of that repository alone:

```text
+ source ghcli (key, repo)
~ index (dnf5 --refresh --repo=lodi-ghcli makecache)
+ package gh 0:2.97.0-2.fc44.x86_64
3 action(s)
```

The plan reads the index as it was before the refresh, so it named Fedora's own `gh` here. The
apply installed the newer `gh` from the repository you added. If the index is more than six
hours old, the refresh is `dnf5 --refresh makecache`, for every repository.

`sudo lodi host apply` writes `/etc/pki/rpm-gpg/lodi-ghcli.asc` and
`/etc/yum.repos.d/lodi-ghcli.repo`, with `gpgcheck=1` and `gpgkey` naming that key. Both files
keep the SELinux label of their directory. Take the table out, and the next apply removes both
files and nothing else:

```text
- source ghcli (repo: remove /etc/yum.repos.d/lodi-ghcli.repo, key: remove …/lodi-ghcli.asc)
1 action(s)
```

| Key | Needed | What it takes |
|---|---|---|
| `uris` | yes | The repository's `baseurl`, `https://` only |
| `signed_by` | this or `signed_by_url` | The key's path, next to `host.toml`. It must be ASCII-armored |
| `signed_by_url` | this or `signed_by` | The key's `https://` address. Only the apply downloads it |
| `signed_by_sha256` | yes | The key's SHA-256 digest |
| `gpgcheck` | no | Only `true`. Every package from the repository is checked anyway |
| `repo_gpgcheck` | no | `true` also checks the repository's own metadata signature |

Each of these stops the plan and the apply before anything is written:

- **A key that is not the one you checked.** On the test machine, one extra byte in the key
  gave:

  ```text
  lodi: error E_HASH_MISMATCH: [sources.ghcli]: the keyring files/ghcli.asc has
  sha256:9078892d…, and the manifest declares sha256:cec6e9ed…
  ```

- **A plain `http://` address.** [`E_UNSUPPORTED`](../ERRORS.md): `is not an https:// URI`. A
  `signed_by_url` that redirects to `http://` stops with [`E_INSECURE_URL`](../ERRORS.md).
- **A signature check switched off.** `gpgcheck = false` or `repo_gpgcheck = false` stops with
  [`E_GPGCHECK_OFF`](../ERRORS.md).
- **A repository the machine already has.** If an enabled repository in `/etc/yum.repos.d`
  has the same `baseurl`, [`E_DUP_RESOURCE`](../ERRORS.md) names its file. Set `enabled=0`
  there first.
- **A binary key.** [`E_TYPE`](../ERRORS.md): dnf reads an ASCII-armored key only. Export it
  with `gpg --armor --export`.

If dnf cannot fetch the repository, the apply puts both files back as they were, installs
nothing, and names the repository in its error. `sudo lodi host import` writes each
third-party repository it can check as a commented `[sources]` block, and copies its key under
`files/etc/pki/rpm-gpg/`. Remove the `# ` to take it. dnf keeps the key it imported into rpm's
own keyring when you remove the repository. `sudo rpmkeys --delete` removes it.

## Pin a package to a version

Pins make a new machine install the same versions as this one. They live in `host.toml` and a
lock file beside it, so they travel with your folder.

```toml
[host]
distro = "debian"
snapshot = "2026-09-18T14:00:00Z"   # install from the archive as it was at this time

[packages]
common = ["tree", "curl", "git"]

[packages.pin]
curl = "2026-01-04"                  # the version the archive had that day, held

[packages.debian.pin]
git = "2026-09-01T00:00:00Z"         # a per-distribution pin wins over [packages.pin]
```

- **`[host] snapshot`** installs every missing package from the distribution's snapshot archive
  at that time. A package already installed keeps its version. Delete the line to install from
  the live archive again.
- **`[packages.pin]`** keeps one package at one version, or at the version of one day. Lodi
  installs it and holds it. A version moved by hand is a `W_DRIFT` line until you give
  `--overwrite-drift`.
- **Which pin wins:** `[packages.<distro>.pin]`, then `[packages.pin]`, then `[host] snapshot`,
  then the live archive.

The snapshot archives are snapshot.debian.org, snapshot.ubuntu.com and archive.archlinux.org.
Lodi checks every index it downloads against its SHA-256 digest. A date before the archive begins
stops with [`E_SNAPSHOT_TOO_OLD`](../ERRORS.md). A date in the future stops with
[`E_SNAPSHOT_FUTURE`](../ERRORS.md).

| Distribution | A version pin | A date pin |
|---|---|---|
| Ubuntu | Yes. One never published stops with [`E_NO_MATCH`](../ERRORS.md), and one with no digest with [`E_NO_CHECKSUM`](../ERRORS.md) | Yes |
| Debian | No, the archive gives no SHA-256 per package. Use a date | Yes |
| Arch | No, use a date | Yes |
| Fedora | Yes, one exact build. See [Pin a Fedora package to one build](#pin-a-fedora-package-to-one-build) | No |

A pinned version the archive does not have stops the apply with
[`E_PIN_UNSATISFIABLE`](../ERRORS.md), before anything is installed. If the machine does not show
the pinned version after the apply, lodi stops with [`E_CLOSURE_DRIFT`](../ERRORS.md). A package
from a `[sources]` repository takes a version pin but not a date pin.

Lodi never writes a pin into `/etc/apt` or `/etc/pacman.conf`. It installs pinned packages
through its own source list under `/var/lib/lodi/host/pin/`, and removes that when the apply ends.

### Old Arch packages signed by a retired key

An old Arch package may be signed by a key Arch has since removed. Lodi then stops with
[`E_PIN_UNTRUSTED`](../ERRORS.md) and installs nothing, because it cannot prove the package is
genuine. Installing it anyway would run unchecked code as root. To check it against the Arch
keyring of the pin's own day, add:

```toml
[packages.arch]
archived_keyring = true
```

Lodi then downloads that day's `archlinux-keyring` and checks it against the machine's own keys.
It uses it for this apply only, and never writes the machine's keyring.

### Pin a Fedora package to one build

On Fedora a pin keeps one exact build, such as `0:1.10.4-1.fc44.x86_64`. Pin the build
installed now:

```sh
sudo lodi host pin pv ~/lodi
```

```text
pinned pv to 0:1.10.4-1.fc44.x86_64
pv: 0:1.10.4-1.fc44.x86_64 from fedora
wrote ~/lodi/lodi.lock
wrote ~/lodi/host.toml
next: sudo lodi host apply ~/lodi
```

`~/lodi/lodi.lock` now records the build's epoch, version, release, arch, the repository that
served it, the file's SHA-256, and Fedora's signed copy of that file. Commit both files.

Move the pin to a newer build, lock it and apply it:

```sh
sudo lodi host pin pv --to 0:1.10.5-1.fc44.x86_64 ~/lodi
lodi update ~/lodi
git -C ~/lodi commit -qam 'pv 1.10.5'
sudo lodi apply ~/lodi
```

To go back, revert the commit and apply again:

```sh
git -C ~/lodi revert --no-edit HEAD
sudo lodi apply ~/lodi
```

```text
journal 20260928T030305Z-06c6d96ca1a5cd52fc48c34ea623afb1
~ package pv (pinned 0:1.10.4-1.fc44.x86_64 from fedora; downgrade from 0:1.10.5-1.fc44.x86_64)
~ package pv (hold: lodi transactions only)
1 action(s) applied
```

Where the older build comes from:

- Your configured Fedora mirror, if it still has those exact bytes.
- Otherwise Fedora's signed copy on its build server, `kojipkgs.fedoraproject.org`, over HTTPS.

Lodi checks the file's SHA-256 against the lock before dnf5 installs it, and dnf5 checks
Fedora's signature. Lodi never installs another build, an unsigned file or a file signed by
another key in its place.

Fedora may delete old builds from its build server. When neither place has the bytes, the apply
stops with [`E_PIN_UNAVAILABLE`](../ERRORS.md) and changes nothing. A copy with another SHA-256
stops with [`E_HASH_MISMATCH`](../ERRORS.md). An unsigned copy stops with
[`E_PIN_UNTRUSTED`](../ERRORS.md). A build whose dependencies the mirror no longer offers stops
with [`E_PIN_UNSATISFIABLE`](../ERRORS.md).

`lodi host versions pv ~/lodi` lists every build your mirrors offer, newest first.
`sudo lodi host unpin pv ~/lodi` removes the pin, and the next install takes the newest build.

### List the versions with `lodi host versions`

```sh
lodi host versions tree ~/lodi
```

It lists the versions of `tree`, newest first, marks the installed, pinned and latest ones, and
ends with the `lodi host pin` line to copy. On Debian and Arch it lists the version the archive
has now, the pinned one and the installed one. It needs the network.

### Pin a package with `lodi host pin`

```sh
lodi host pin tree ~/lodi                   # keep tree at the version installed now
lodi host pin tree --to 2026-01-04 ~/lodi   # or at the version of that day
lodi host pin --all ~/lodi                  # move [host] snapshot to now
```

It writes the pin into `host.toml` and what it resolves to into `~/lodi/lodi.lock`. It checks
every pin first, and writes nothing if one fails. A folder you own needs no root. The next
`sudo lodi host apply ~/lodi` installs the pinned versions.

### Remove a pin with `lodi host unpin`

```sh
lodi host unpin --all ~/lodi
```

```text
unpinned every pin
removed ~/lodi/lodi.lock
wrote ~/lodi/<hostname>/host.toml
next: sudo lodi host apply ~/lodi
```

`lodi host unpin tree ~/lodi` removes one pin. The next apply releases the holds and changes no
version. A package with no pin prints `tree has no pin; nothing written`.

### Set up a new machine with the same versions

```sh
sudo lodi host import ~/lodi
lodi host pin --all ~/lodi
git -C ~/lodi init -q && git -C ~/lodi add -A && git -C ~/lodi commit -qm 'this machine'
```

On the new machine of the same distribution, clone the folder and apply it:

```sh
sudo lodi host apply ~/lodi --host <hostname>
```

Every pinned package arrives at its pinned version. Every other declared package arrives at the
version its `[host] snapshot` had. Importing again never moves a pin.

## Keep the host in a folder you own

The host folder is yours. You edit it without root and apply it as root:

```text
~/lodi/                   a folder of hosts, one per machine
  <hostname>/             one host: host.toml, files/ and home/
    host.toml
    files/etc/...
  laptop/
```

**Whoever can write the folder decides what root does to the machine.** So lodi stops with
[`E_PATH_ESCAPE`](../ERRORS.md) when any folder or file on the way is:

- a link,
- owned by anyone but root or you,
- writable by the group or by others,
- on NFS or SMB.

No flag skips this check. A `sudoers` rule that lets someone run `lodi host apply` without a
password gives that person full root.

Lodi reads `host.toml` and its files once, and applies exactly what it read. The record,
`/etc/lodi/host.lock`, stays on the machine and never goes into the folder. It names the folder
the last apply read. A bare `sudo lodi host apply` then stops with [`E_DECLINED`](../ERRORS.md),
so an old `/etc/lodi/host.toml` cannot quietly take the machine back. Name the folder instead.

To move an existing `/etc/lodi/host.toml` into a folder you own:

```sh
mkdir -p ~/lodi/"$(head -n1 /etc/hostname)"
sudo cp -r /etc/lodi/host.toml /etc/lodi/files ~/lodi/"$(head -n1 /etc/hostname)"/
sudo chown -R "$(id -un):" ~/lodi
sudo lodi host apply ~/lodi    # nothing to do, and the record now names ~/lodi/<hostname>
```

Never copy `/etc/lodi/host-allowed`. A new machine needs its own `sudo lodi host arm`.

## Apply your home with the host

**Tested in 1.5.0** on Debian 12, Ubuntu 24.04, Ubuntu 26.04 and Arch.

A host folder can hold your home too, in `home/<login>/home.toml`. The import writes a commented
one for you. `sudo lodi host apply ~/lodi` then applies the host as root, and your home as you:

```text
~/lodi/<hostname>/host.toml                 the machine, applied as root
~/lodi/<hostname>/home/<login>/home.toml    your home, applied as you
```

- Lodi reads and checks the home before the host changes. A home that does not parse stops the
  whole apply with nothing written.
- After the host, lodi gives up root for good and applies the home as you. Root writes nothing
  in your home.
- If the home then fails, the host stays applied. Fix the home and apply again.
- `--no-home` applies the host alone:

```sh
sudo lodi host plan --no-home ~/lodi
```

```text
= file /etc/issue.net (unchanged)
= file /etc/ssh/sshd_config (unchanged)
nothing to do
home skipped (--no-home)
```

A `home/` with no `/etc/passwd` entry for you stops with [`E_CONFIG`](../ERRORS.md). After this,
a bare `lodi home apply` stops until you name the folder. See
[the home scope](home.md#a-home-in-the-host-directory).

## Apply a host from a git repository

**Tested in 1.5.0** on Debian 12, Ubuntu 24.04, Ubuntu 26.04 and Arch. The tests used a git
server on the same machine, with its HTTPS address rewritten to it, so without TLS. One plan that
changed nothing read a public GitHub repository over HTTPS. No GitLab or Codeberg URL was tested.

`lodi host plan` and `lodi host apply` also take a public git repository:

```sh
sudo lodi host plan github:OWNER/REPO
sudo lodi host apply git+https://example.org/me/hosts.git --host <hostname>
```

`github:`, `gitlab:` and `codeberg:` expand to that site's `https://` address. The plan's first
line names the source, the host, the ref and the commit.

- **Whoever can push to the repository controls root's files and packages.** Apply only a
  repository whose writers you would give root.
- **Only `https://` works.** `http://`, `ssh://` and addresses with a password stop with
  [`E_INSECURE_URL`](../ERRORS.md). A private repository works from a local clone.
- **The commit is locked.** Later plans and applies use the same commit with no request. `--ref
  NAME` picks a branch or tag, `--rev COMMIT` one commit, and `--refresh` the newest one.
- **Offline**, anything that needs a new commit stops with [`E_FETCH`](../ERRORS.md) and changes
  nothing. A download that does not match its lock stops with [`E_HASH_MISMATCH`](../ERRORS.md).
- **Lodi never writes into a downloaded repository.** Pin and import in a clone, commit, push,
  and apply with `--refresh`. A home in it needs its lock committed, or lodi stops with
  [`E_LOCK_STALE`](../ERRORS.md).

A link or a submodule in the repository stops with an error.

## Manage a repository with `lodi import`, `lodi plan`, `lodi apply` and `lodi update`

**Tested in 1.5.0** on Debian 12, Ubuntu 24.04, Ubuntu 26.04 and Arch. The tests ran the import,
the apply of the host and every home, and `update`, which moved 1.4's locks into one. They also
applied a repository to a new machine and went back with `git revert`.

These four commands work on a whole repository: every host in it and every home under each host.

```sh
sudo lodi import ~/lodi     # like lodi host import ~/lodi
sudo lodi plan ~/lodi       # the host, then every home; writes nothing
sudo lodi apply ~/lodi      # the host as root, then each home as its own user
lodi update ~/lodi          # move the snapshot to the last published day, and lock it
```

`lodi update` prints what it moved:

```text
[host] snapshot 2026-09-25T00:00:00Z -> 2026-09-25T00:00:00Z
wrote ~/lodi/lodi.lock
next: sudo lodi apply ~/lodi
```

- **Which repository.** The one you name, a path or a URL. Else the current directory once you
  answer yes, or with `--yes`. Else `LODI_REPO`. A folder with a project `lodi.toml` is never one.
- **Every home.** Lodi checks every `home/<login>/` before the host changes. After the host, each
  home runs as its own user. A login the machine does not have is skipped.
- **One lock.** `lodi.lock` at the top of the repository holds every pin and every home's tools.
  An older repository with `pins.lock` or `home.lock` files moves them in at the first write.
  Mixing both for one folder stops with [`E_LOCK_STALE`](../ERRORS.md).
- **Your own recipes.** `recipes/NAME.toml` at the top gives every home a tool lodi does not
  carry. It is read with the rest, before the host changes. See
  [the recipe guide](../../catalogue/README.md#a-recipe-of-your-own).
- **`lodi update`** keeps every pin you set. It writes `host.toml` and `lodi.lock` together or not
  at all. It cannot update a URL or `/etc/lodi`.

### The same loop on Fedora 44

**Tested in 1.7.0** on a fresh Fedora 44 machine with SELinux enforcing. The import writes your
packages under `[packages.fedora]` and a home file for you, all owned by you:

```sh
sudo lodi import ~/lodi
```

```text
wrote ~/lodi/<hostname>/home/<login>/home.toml
wrote ~/lodi/<hostname>/host.toml: 69 package(s) declared, 0 not captured, 0 configuration file(s)
  under ~/lodi/<hostname>/files; no package and no file outside ~/lodi/<hostname> changed
```

Delete `tree` from `host.toml`, pin `pv` with `sudo lodi host pin pv ~/lodi`, commit, and apply.
dnf5 removes `tree`, then your home is applied as you:

```text
- package tree 0:2.2.1-4.fc44.x86_64 (not in the manifest)
1 action(s) applied
home <login>
created   .config/ff1/note          0644
```

To move a pin, change its build in `host.toml` and run `lodi update ~/lodi`, without `sudo`.
It records the new build in `lodi.lock`:

```text
pv: 0:1.10.5-1.fc44.x86_64 kept at 0:1.10.5-1.fc44.x86_64
wrote ~/lodi/lodi.lock
```

Commit, apply, and the newer build is installed. `git revert HEAD` and `sudo lodi apply ~/lodi`
put the older build back, from the lock of the earlier commit:

```text
~ package pv (pinned 0:1.10.4-1.fc44.x86_64 from fedora; downgrade from 0:1.10.5-1.fc44.x86_64)
1 action(s) applied
home <login>
nothing to do
```

`sudo lodi apply github:OWNER/lodi` applies a Fedora folder from a public repository the same
way. [Your own repositories](#add-a-third-party-dnf-repository-on-fedora) work on Fedora too. A
project on the Fedora 44 base is [the project scope](project.md#run-a-project-in-a-container).

## Files lodi keeps on the machine

| Path | What it is |
|---|---|
| `/etc/lodi/host-allowed` | The empty file that allows lodi to manage the machine |
| `/etc/lodi/host.toml` and `files/` | The host, when you keep it in `/etc/lodi` |
| `/etc/lodi/host.lock` | What the last apply did. Never a pin, and never copied to another machine |
| `/etc/lodi/originals/` | Copies of files lodi took over, for `on_remove = "restore"` |
| `/etc/modules-load.d/lodi.conf`, `/etc/modprobe.d/lodi-blacklist.conf` | `[kernel]` modules |
| `/etc/sysctl.d/90-lodi.conf` | `[sysctl]` values |
| `/etc/default/grub.d/99-lodi-boot.cfg`, `/etc/kernel/cmdline` | `[boot]` for GRUB, and systemd-boot's command line |
| `/etc/kernel/install.conf` | `[boot]` for systemd-boot, where `/boot` is a partition the firmware cannot read |
| `/var/lib/lodi/host/journal/` | One journal per apply, so an interrupted apply can continue |
| `/var/lib/lodi/host/git/` | The locked commit of a git repository you applied |
| `/var/lib/lodi/host/pin/` | The source list of a pinned apply, removed when the apply ends |
| `lodi.lock` in your folder | Your pins and your homes' tools |

With `--root DIR`, every path above is under `DIR`. [The file formats](../SCHEMAS.md) describes
the lock and the journal. A file of a newer format stops with [`E_LOCK_VERSION`](../ERRORS.md).

## What this scope will not do

- **It does not allow itself.** Only `sudo lodi host arm` does.
- **It does not run `sudo`.** You type the privilege yourself.
- **It does not undo.** There is no rollback. To go back, apply the old `host.toml`.
- **It does not upgrade Debian or Ubuntu.** No `apt-get upgrade`, `dist-upgrade` or
  `autoremove`. On Arch every apply upgrades the whole machine, because Arch supports no other way.
- **It does not remove a package you did not ask it to.** Under `exact`, a deleted line removes
  its package, and `--overwrite-drift` removes **every** package the plan lists under `W_DRIFT`.
- **It does not change your package manager's settings.** No pin goes into `/etc/apt`, no hold
  into `/etc/pacman.conf`, and no key into pacman's keyring.
- **It does not remove `db.lck` for you**, or guess after an interrupted apply.
- **It does not switch SELinux off or relabel your files.**
- **It does not touch a service you did not declare**, and it does not change a bootloader
  unless `host.toml` has `[boot]`. It never removes a loader, and never manages one on a
  machine that did not start through UEFI.
- **It does not delete an account, a group or a home**, and it never writes a password.
- **It does not hide secrets in a copied file.** It skips a file that looks like one, and copies
  the rest byte for byte. Read `files/` before the folder leaves the machine.
- **It does not add a repository you did not ask for.** The import writes each third-party apt
  repository as a commented block. Nothing adds it until you uncomment it.

## Where to look when it fails

This is always safe to run, and changes nothing:

```sh
sudo lodi host plan ~/lodi
```

The journal under
`/var/lib/lodi/host/journal/` shows what an apply did and how far it got. Each code has an entry
in [the error reference](../ERRORS.md).

| What you see | What to do |
|---|---|
| Exit 2, no code | A typing mistake in the command. Run `lodi help host` |
| [`E_HOST_NOT_ARMED`](../ERRORS.md) | Run `sudo lodi host arm` once |
| [`E_NEED_ROOT`](../ERRORS.md) | Run the command with `sudo` |
| [`E_HOST_ROOT_REQUIRED`](../ERRORS.md) | `LODI_HOST_REQUIRE_ROOT=1` is set. Name a scratch tree with `--root DIR`, or run where the variable is not set |
| [`E_NO_MANIFEST`](../ERRORS.md) | There is no `host.toml`. Run `sudo lodi host import ~/lodi` |
| [`E_HOST_MISMATCH`](../ERRORS.md) | `[host] distro` names another distribution. Nothing was read further |
| [`E_SYNTAX`](../ERRORS.md), [`E_TYPE`](../ERRORS.md), [`E_IDENT`](../ERRORS.md), [`E_UNKNOWN_ATTR`](../ERRORS.md), [`E_UNKNOWN_BLOCK`](../ERRORS.md) | Fix the line and column the message names |
| [`E_ATTR_CONFLICT`](../ERRORS.md), [`E_DUP_RESOURCE`](../ERRORS.md), [`E_VAR_UNSET`](../ERRORS.md), [`E_VERSION`](../ERRORS.md) | Fix the key the message names |
| [`E_PATH_ESCAPE`](../ERRORS.md) | A link, or a folder or file others can write. Fix the owner or mode it names, such as `chmod go-w` |
| [`E_EXISTS`](../ERRORS.md) | `/etc/lodi/host-allowed` is not a plain empty file. Remove it and run `sudo lodi host arm` |
| [`E_UNKNOWN_PACKAGE`](../ERRORS.md) | The index has no such package. Pick one of the names it lists |
| [`E_UNSUPPORTED`](../ERRORS.md) | This lodi cannot do what the file asks. The message says what to write instead |
| [`E_STORE_IO`](../ERRORS.md) | A path lodi could not read or write, such as a `--root` that does not exist |
| [`E_NO_RUNTIME`](../ERRORS.md) | `apt-get`, `dpkg-query` or `pacman` is missing, or not owned by root |
| [`E_HASH_MISMATCH`](../ERRORS.md) | A key or an index does not match its digest. Nothing was installed |
| [`E_INSECURE_URL`](../ERRORS.md), [`E_FETCH`](../ERRORS.md) | Use an `https://` address, and check the network |
| [`E_SNAPSHOT_TOO_OLD`](../ERRORS.md), [`E_SNAPSHOT_FUTURE`](../ERRORS.md), [`E_NO_MATCH`](../ERRORS.md), [`E_NO_CHECKSUM`](../ERRORS.md) | Pick another date or version. Nothing was installed |
| [`E_PIN_UNSATISFIABLE`](../ERRORS.md) | The archive does not have that version. Pin another one |
| [`E_PIN_UNTRUSTED`](../ERRORS.md) | See [Old Arch packages signed by a retired key](#old-arch-packages-signed-by-a-retired-key) |
| [`E_CLOSURE_DRIFT`](../ERRORS.md) | A pinned package is not at its version after the apply. Read the package manager's log, then apply again |
| [`E_LOCK_VERSION`](../ERRORS.md) | A lock of a newer format. Update lodi |
| [`E_LOCK_STALE`](../ERRORS.md) | The lock does not match `host.toml`. Run `lodi update ~/lodi` and commit |
| [`E_DECLINED`](../ERRORS.md) | Something changed by hand. Read the plan, then apply with `--overwrite-drift` |
| [`E_DRIFT`](../ERRORS.md) | A repository file lodi wrote was edited. Apply with `--overwrite-drift` |
| [`E_APPLY`](../ERRORS.md) | A step failed. The message names it and the journal. Earlier steps stay done |
| [`E_JOURNAL_AMBIGUOUS`](../ERRORS.md) | See [An apply that stopped part way](#an-apply-that-stopped-part-way) |
| [`E_SYSTEM_BUSY`](../ERRORS.md) | Another apply is running. Wait for it |
| [`E_CONFIG`](../ERRORS.md) | Your login is not in `/etc/passwd`. Add it, or use `--no-home` |

The codes of the other scopes are in [the project scope](project.md) and
[the home scope](home.md).

