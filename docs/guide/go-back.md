# Go back to an earlier config

Undo a change by taking your config back in git, then switch. lodi has no command of its own
for this, and keeps no generations. Your config's git history is the only history.

You need a config made by `lodi import`, by default in `~/.config/lodi`, and each change
committed with `git`. The import makes the folder a git repository when `git` is installed. To
go back to a pinned version, you need the network.

## Steps

1. Find the commit you want to undo:

   ```sh
   git -C ~/.config/lodi log --oneline
   ```

   ```text
   9f9a421 B
   be415f8 A
   7bf352e import
   ```

2. Undo it with a new commit. The machine has not changed yet:

   ```sh
   git -C ~/.config/lodi revert --no-edit HEAD
   git -C ~/.config/lodi log --oneline -1
   ```

   ```text
   5633f61 Revert "B"
   ```

   To drop an edit you have not committed yet, check the file out instead:

   ```sh
   git -C ~/.config/lodi checkout -- host.toml
   ```

3. See what going back changes. On Fedora 44, after a commit that moved a pin, pinned another
   package, and changed a file, the time zone and a service:

   ```sh
   lodi switch --dry-run
   ```

   The preview ends with these lines:

   ```text
   ~ package less (pinned 0:691-2.fc44.x86_64 from fedora; downgrade from 0:702-1.fc44.x86_64; …)
   ~ package less (hold: lodi transactions only)
   ~ package libevent (unhold: no longer pinned or held; 0:2.1.13-1.fc44.x86_64 stays installed)
   ~ file /etc/going-back-note (content)
   ~ timezone Asia/Tokyo -> Europe/Berlin (timedatectl set-timezone -- Europe/Berlin)
   - service fstrim.timer (systemctl disable --now -- fstrim.timer)
   host: +0 -0 ~1 packages, 1 file, 1 service, 2 other changes
   0 files: nothing to do
   home: no changes
   config: 5633f61
   ```

   `less` goes back to the build its pin named before. `libevent` loses its pin and keeps the
   version it has.

4. Switch:

   ```sh
   lodi switch
   ```

   The last line names the commit the machine now matches:

   ```text
   switched in 8.5s: +0 -0 ~1 packages, 1 file, 1 service, 2 other changes (5633f61)
   ```

## What goes back

| Distribution | A package with no pin goes back | A pinned package goes back | From | Not restored |
|---|---|---|---|---|
| Ubuntu 24.04 | No, it keeps the version it has, even with `[host] snapshot` set | Yes, to the version its pin names | The archive at snapshot.ubuntu.com, as it was on the pin's day | Packages from `[sources]`, and the data and state of your programs |
| Debian 12 | No, it keeps the version it has, even with `[host] snapshot` set | Yes, to the version its pin names | The archive at snapshot.debian.org, as it was on the pin's day | Packages from `[sources]`, and the data and state of your programs |
| Arch | No, it keeps the version it has, even with `[host] snapshot` set | Yes, to the version its pin names | The archive at archive.archlinux.org, as it was on the pin's day | Packages from `[sources]`, and the data and state of your programs |
| Fedora 44 | No, it keeps the version it has | Yes, to the exact build its pin names | A configured repository that still serves the build, else Fedora's build system Koji. Fedora has no archive by day | Packages from `[sources]`, and the data and state of your programs |

`[host] snapshot` is the day missing packages are installed from. It never moves a package
that is already installed, so it is not a pin. **To be able to go back to a package's version,
pin it in the commit you may want back.** See [Pin a package to a version](pin-a-package.md).

What else the switch puts back:

- **Files under `/etc`** that your config writes get the earlier text.
- **Services** you declare are enabled or disabled as the earlier commit says.
- **Settings** you declare, such as the time zone, get the earlier value.
- **To start the kernel that booted before**, see
  [the kernel page](kernel-and-boot.md#go-back-to-the-entry-that-booted-before).
- **The commit** is named in brackets at the end of the switch's last line. It is the short
  hash `git log` shows.

The switch warns when git can't take you back to what it applies. With an edit you have not
committed, the hash ends in `+uncommitted`:

```text
nothing to switch (5633f61+uncommitted)
lodi: warning: config has uncommitted changes; git can't take you back to this
```

With a config that is not in git:

```text
nothing to switch
lodi: warning: config isn't in git; nothing to go back to
```

Commit your edit, or drop it with `git checkout`, and the warning goes away.

## Check it worked

1. Compare the hash in brackets with the commit you are on:

   ```sh
   git -C ~/.config/lodi rev-parse --short HEAD
   ```

   ```text
   5633f61
   ```

2. Switch again. Nothing is left to do:

   ```sh
   lodi switch
   ```

   ```text
   nothing to switch (5633f61)
   ```

3. Check a pinned package's version. On Fedora:

   ```sh
   rpm -q less
   ```

   ```text
   less-691-2.fc44.x86_64
   ```

   On Debian and Ubuntu, use `dpkg-query -W tzdata`. On Arch, use `pacman -Q pv`.
