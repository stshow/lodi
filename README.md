# Lodi

Import your machine into plain TOML files you own, then make it, or a fresh install, match
them with one command.

Status: the commands are `lodi import`, `lodi switch`, `lodi update`, `lodi pin`, `lodi unpin`,
`lodi init`, `lodi develop`, `lodi run`, `lodi shell`, `lodi search` and `lodi gc`, with
`lodi help`, `lodi --help` and `lodi --version`. [Every command and flag](docs/CLI.md).

## Quick start
### 1. Install

On x86_64 Ubuntu 24.04, Debian 12, Arch or Fedora 44, get `SHA256SUMS` and your package from the
[2.0.0 release](https://github.com/stshow/lodi/releases/tag/v2.0.0). Check for `OK`, then install:

```sh
sha256sum -c --ignore-missing SHA256SUMS
sudo apt install ./lodi_2.0.0_amd64.deb                  # Debian, Ubuntu
sudo pacman -U lodi-2.0.0-1-x86_64.pkg.tar.zst           # Arch
sudo dnf install ./lodi-2.0.0-1.x86_64.rpm               # Fedora
```

The files are not signed, so checksums catch damage, not forgery. `lodi --version` prints
`lodi 2.0.0`. Other distributions: [the guide](docs/GUIDE.md#install-from-a-release).

### 2. Import this machine

You never type `sudo` for lodi. The import asks for your password to read the host, and asks
once whether lodi may manage this host:

```sh
lodi import
```

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

Your config is `~/.config/lodi`: `host.toml` lists this host's packages and settings, `home.toml`
your home, and `lodi.lock` the versions. Nothing on the machine changed. Commit it now, so that
git can take you back later: `git -C ~/.config/lodi add -A`, then
`git -C ~/.config/lodi commit -m import`.

### 3. Switch

Add `"tree",` to `common` in `~/.config/lodi/host.toml`. Preview the change, then switch.
**Warning:** on Arch a switch runs `pacman -Syu`, which **upgrades the whole machine**.

```sh
lodi switch --dry-run
lodi switch
```

```text
+ package tree
host: +1 -0 packages
0 files: nothing to do
home: no changes
lodi: warning: config has uncommitted changes; git can't take you back to this
config: 1eeb68d+uncommitted
✓ [1/2] Download packages  0.1s
✓ [2/2] Install packages  0.0s
✓ switched in 2.1s: +1 -0 packages (1eeb68d+uncommitted)
```

lodi asks for root only when the host part has changes. A second `lodi switch` prints
`nothing to switch`. To go back, revert the commit with git and switch again. [More](docs/GUIDE.md).

### 4. A first project

In a new, empty directory, run these. **Warning:** the last nine lines add a file and a tool
to `~/.config/lodi/home.toml` and append to `~/.profile`. Skip them to keep yours.

```sh
lodi init --name hello          # writes the ./lodi.toml below and adds .lodi/ to .gitignore
printf '\n[tasks.versions]\nrun = "python3 --version"\n' >> lodi.toml
lodi develop -- python3 --version   # ask to allow the tasks, write ./lodi.lock, run the command
lodi run versions               # run the [tasks.versions] entry with /bin/sh -c
lodi gc --dry-run               # name what a collection would remove, and change nothing
lodi gc                         # remove it, and report the bytes reclaimed
mkdir -p ~/.config/lodi         # your home: your own files and your own tools
printf '\n[home.file.".lodi-hello.conf"]\ntext = "hello from lodi"\n' >> ~/.config/lodi/home.toml
printf 'mode = "0640"\non_remove = "delete"\n\n[tools]\njq = "latest"\n' >> ~/.config/lodi/home.toml
lodi switch --home --dry-run    # what a switch would do to your home directory, writing nothing
lodi switch --home              # do it, and print the one line that puts the tools on PATH
lodi switch --home              # again: nothing to do, and no managed file's mtime moves
printf '\n. "$HOME/.local/share/lodi/home-scope/profile.sh"\n' >> ~/.profile   # the printed line, added by you
env -i HOME="$HOME" SHELL=/bin/sh sh -lc 'command -v jq'   # a new login shell finds the tool
```

`lodi init` writes:

```toml
# lodi.toml: this project's environment, written by `lodi init`. `lodi develop` pins
# what it asks for in ./lodi.lock, and `lodi develop -- COMMAND` runs a command in it.

# The project's name. `version` is the format of this file, and "1" is the only one.
[project]
version = "1"
name = "hello"

# Upstream tools, such as python and nodejs, that lodi downloads and unpacks. A version
# is a prefix ("3.12"), "latest", or a range such as ">=3.11, <3.13".
[tools]
python = "3.12"

# Tasks run with /bin/sh -c in that environment, once you allow them on first use:
#   lodi run versions
# [tasks.versions]
# run = "python3 --version"

# Variables set for the commands you run. A value may use ${var}, with no expressions.
# [env]
# PYTHONDONTWRITEBYTECODE = "1"
```

## Release files
| File | What it is |
|---|---|
| `lodi-<version>-x86_64-linux-musl` | the static `x86_64-unknown-linux-musl` binary |
| `lodi-<version>-aarch64-linux-musl` | the static `aarch64-unknown-linux-musl` binary |
| `lodi_<version>_amd64.deb` | Debian/Ubuntu package, with no dependencies, `/usr/bin/lodi` |
| `lodi_<version>_arm64.deb` | the same for `arm64` |
| `lodi-<version>-1-x86_64.pkg.tar.zst` | Arch package |
| `lodi-<version>-1-aarch64.pkg.tar.zst` | the same for `aarch64` |
| `lodi-<version>-1.x86_64.rpm` | Fedora package, no dependencies, `/usr/bin/lodi`, from 1.7.0 |
| `lodi-<version>-1.aarch64.rpm` | the same for `aarch64` |
| `lodi-<version>-src.tar.gz` | the source |
| `SHA256SUMS` | the checksums of the release's other files |

**The aarch64 artifacts are cross-built on an x86_64 machine and are not tested by this project.**

## About Lodi

I built Lodi to keep the parts of NixOS, Nix and home-manager that I can't live without. It is
GPL-3.0-or-later ([license text](LICENSE)), which covers Lodi, not your manifests or locks. See
[contributing](CONTRIBUTING.md) and [security](SECURITY.md).
The name is a pun on the Creedence Clearwater Revival song "Lodi": setting a machine up yet again
feels like being stuck in Lodi again. With Lodi, the next time starts from files you already
have: `host.toml`, `home.toml` and each project's `lodi.lock`.
