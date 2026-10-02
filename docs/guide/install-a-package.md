# Install a package on this machine

Add a package to your config, see what changes, and switch. The same edit, a deleted line,
removes a package.

You need a config made by `lodi import`, by default in `~/.config/lodi`. You never type `sudo`:
lodi asks for your password only when the host part has changes.

## Steps

1. Open `~/.config/lodi/host.toml` and add the package to the `common` list under `[packages]`:

   ```toml
   [packages]
   common = [
     "bc",
     "linux-image-amd64",
     "tree",
   ]
   ```

2. See what the switch would change. This changes nothing on the machine:

   ```sh
   lodi switch --dry-run
   ```

   ```text
   + package tree
   host: +1 -0 packages
   ```

   A `+` line adds, `~` changes, `-` removes and `=` leaves a thing as it is.

3. Switch. **On Arch, a switch that installs or removes a package upgrades the whole
   machine first**, with `pacman -Syu`. See [On each distribution](#on-each-distribution).

   ```sh
   lodi switch
   ```

   On a terminal each step keeps one line, and the last line sums it up:

   ```text
   ✓ [1/5] Download packages  0.1s
   ✓ [2/5] Install packages  0.0s
   ✓ [3/5] Tools  0.0s
   ✓ [4/5] Files  1 in 0.0s
   ✓ [5/5] User services  0.0s
   ✓ switched in 2.1s: +1 -0 packages, home 1 file (5798a24)
   ```

   The text in brackets at the end is your config's commit.

4. Commit the change, so `git` can take you back to it:

   ```sh
   git -C ~/.config/lodi commit -qam 'add tree'
   ```

## Check it worked

Switch again. There is nothing left to do:

```sh
lodi switch
```

```text
nothing to switch (5798a24)
```

## Remove a package

**With `packages = "exact"`, deleting a line removes that package.** `lodi import` writes this
setting under `[host]`. Read the preview before every switch, because it names each package
that leaves, and why:

```text
- package cowsay (not in the manifest)
- package perl (needed by nothing once the rest is removed)
~ package librecode3 (a dependency now: needed by fortune-mod; stays)
```

- A package another declared package still needs stays, as a dependency.
- The base system and the running kernel stay, even when you delete their lines.
- A package from a third-party repository, a package file or the AUR that lodi never recorded
  is never removed.
- `packages = "managed"` removes only what lodi recorded installing. Without a `packages` line
  it works the same way.

Delete the line, then run steps 2 to 4 again.

## Keys of `[packages]`

```toml
[packages]
common = ["tree"]
optional = ["fonts-noto-color-emoji"]
```

| Key | What it does |
|---|---|
| `common` | Packages for every distribution |
| `[packages.debian] add` | Debian only. `ubuntu`, `arch` and `fedora` work the same way |
| `hold` | Keep these packages at the version they have |
| `mark_auto` | Mark these as installed only as a dependency |
| `optional` | Leave these out, with a warning, where the package index does not have them |
| `absent` | Remove these wherever they are installed |

A name the package index does not have stops with [`E_UNKNOWN_PACKAGE`](../ERRORS.md), and the
hint names the nearest one. A package name never holds a version. See
[Pin a package to a version](pin-a-package.md) instead.

## On each distribution

**Debian and Ubuntu.** All changes go into one `apt-get install`.

- lodi refreshes the package index first when it is more than six hours old.
- It never runs `apt-get upgrade`, `dist-upgrade` or `autoremove`.
- Say a package ships a new version of a file you changed. Then dpkg keeps your version, and
  puts the new one beside it, normally as `.dpkg-dist`.
- lodi does not report which files that happened to, so compare them yourself after a switch.

**Arch: every switch with package changes upgrades the whole machine.** lodi installs with
`pacman -Syu`, because Arch does not support installing against an old package database. A
`hold` holds a package back from lodi's own `pacman -Syu` only, not from yours.

**Fedora.** lodi installs with `dnf5 install` and removes with `dnf5 remove`. It never installs
a weak dependency. SELinux stays enforcing, and every file lodi writes keeps its label. Fedora
Silverblue and other rpm-ostree systems stop with [`E_HOST_OSTREE`](../ERRORS.md).

## A file or package changed by hand

Say you edited a file lodi wrote, or installed a package with `apt` since the last switch. The
preview names it in a `W_DRIFT` line, and the switch stops before it changes anything:

```text
lodi: error E_DECLINED: /etc/motd was changed since lodi last wrote it
```

To keep your change, put it in `host.toml`: add the package, or put the file's new text in its
entry. Or [import again](preview-a-change.md#import-again-after-you-change-the-machine-by-hand).

**`--overwrite-drift` removes every package the preview lists as installed by hand, and puts
back every file.** Read the preview first. Then:

```sh
lodi switch --overwrite-drift
```

## A switch that stopped part way

Each switch writes a journal under `/var/lib/lodi/host/journal/` before it changes anything.
After a crash or a power cut, the next switch reads it. It carries on when it can tell how far
the last one got. When it cannot, it stops with [`E_JOURNAL_AMBIGUOUS`](../ERRORS.md) and
names the journal.

Then set the machine right yourself. On Debian or Ubuntu that is often
`sudo dpkg --configure -a`. On Arch, read `/var/log/pacman.log`, and remove
`/var/lib/pacman/db.lck` once no pacman runs. Then give the journal's name:

```sh
lodi switch --resolved 20260926T210702Z-4ab970f50ec9cdb778a996441e98dee3
```

## When it fails

- [`E_APPLY`](../ERRORS.md): A step failed. The message names it and the log. Earlier steps stay
  done. Fix it and switch again.
- [`E_SYSTEM_BUSY`](../ERRORS.md): Another switch is running. Wait for it.
- [`E_HOST_NOT_ARMED`](../ERRORS.md): Run `lodi import` once and answer yes.
- [`E_NEED_ROOT`](../ERRORS.md): lodi found no `sudo`, `doas` or `run0`, or no terminal to ask on.
- [`E_HOST_MISMATCH`](../ERRORS.md): `[host] distro` names another distribution.
- [`E_NO_RUNTIME`](../ERRORS.md): The package manager is missing, or not owned by root.
- [`E_SYNTAX`](../ERRORS.md): Fix the line and column the message names.
- [`E_UNKNOWN_BLOCK`](../ERRORS.md): `host.toml` has a table lodi does not know. Fix its name.
- [`E_EXISTS`](../ERRORS.md): `/etc/lodi/may-manage` is not a plain empty file owned by root.
  Remove it, and run `lodi import` again.
- [`E_VERSION`](../ERRORS.md): `min_lodi_version` asks for a newer lodi.
- [`E_VAR_UNSET`](../ERRORS.md): Add the variable to `[vars]` in `host.toml`.
- [`E_ATTR_CONFLICT`](../ERRORS.md): A package is both declared and in `absent`. Keep one.

Every flag of `lodi switch` is in [the command reference](../CLI.md#lodi-switch).
