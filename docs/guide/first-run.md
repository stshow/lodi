# Your first switch: import, edit, switch

In about ten minutes you copy this machine into a config and add a package and a home file.
Then `lodi switch` makes the machine match it.

You need lodi installed, as the [README](../../README.md#1-install) shows, and `git`. You never
type `sudo` for lodi. It asks for your password when it needs root.

## 1. Import this machine: `lodi import`

```sh
lodi import
```

lodi asks once whether it may manage this host. Answer `y`:

```text
May lodi manage this host? [y/N] y
✓ [1/7] ask  0.0s (yes)
✓ [2/7] read the host  0.4s
✓ [3/7] write the marker  0.0s
✓ [4/7] record the host  0.0s
✓ [5/7] fill the lock  0.0s
✓ [6/7] write the config  0.0s
✓ [7/7] git init  0.0s
✓ imported in 0.9s: ~/.config/lodi: home.toml, lodi.lock, host.toml
wrote ~/.config/lodi/host.toml: 2 package(s) declared, 0 not captured; read NOT CAPTURED in it first
next: review ~/.config/lodi, commit it, then run lodi switch --dry-run
```

Nothing on the machine changed. The import only wrote your config.

## 2. Look at your config

```sh
ls -A ~/.config/lodi
```

```text
.git  home.toml  host.toml  lodi.lock
```

- `host.toml` lists the packages you installed and the host's settings.
- `home.toml` is your home: your files, programs and tools.
- `lodi.lock` holds the exact versions, so a switch on another day installs the same ones.
- A `files` folder appears when the host has files that `host.toml` points to.

Read the `NOT CAPTURED` block at the end of `host.toml`. It lists what the import left out.

## 3. Commit it

```sh
git -C ~/.config/lodi add -A
git -C ~/.config/lodi commit -m "import this machine"
```

From now on, git is how you go back: revert a commit, then switch again.

## 4. Add a package and a home file

Add `tree` to the `common` list in `~/.config/lodi/host.toml`:

```toml
[packages]
common = [
  "bc",
  "tree",
]
```

Add this file to `~/.config/lodi/home.toml`:

```toml
[home.file.".inputrc"]
text = "set bell-style none\n"
```

## 5. Preview the switch

```sh
lodi switch --dry-run
```

```text
+ package tree
host: +1 -0 packages
create    .inputrc                  0644
1 file: 1 create
home: 1 file
config: 5798a24
```

The preview changes nothing and never asks for a password.

## 6. Switch: `lodi switch`

**Warning:** on Arch a switch runs `pacman -Syu`, which upgrades the whole machine.

```sh
lodi switch
```

```text
✓ [1/5] Download packages  0.1s
✓ [2/5] Install packages  0.0s
✓ [3/5] Tools  0.0s
✓ [4/5] Files  1 in 0.0s
✓ [5/5] User services  0.0s
✓ switched in 2.1s: +1 -0 packages, home 1 file (5798a24)
```

lodi asked for root once, for the host part. Your home part ran as you.

## 7. Check it worked

```sh
command -v tree
cat ~/.inputrc
git -C ~/.config/lodi add -A
git -C ~/.config/lodi commit -m "add tree and .inputrc"
lodi switch
```

`tree` is installed and `~/.inputrc` holds `set bell-style none`. The last switch has nothing to
do:

```text
nothing to switch (9115cb6)
```

## Next

- [Install a package on this machine](install-a-package.md)
- [Preview a change before you switch](preview-a-change.md)
- [Manage the files in your home](manage-your-home.md)
- [All tasks](../GUIDE.md)
