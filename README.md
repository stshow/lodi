# Lodi

Snapshot your machine into a repository you own, and apply it here or on a fresh install.

## Three commands to get up and running

With lodi [installed](#1-install), on Ubuntu, Debian, Arch or Fedora. **Warning:** on Arch the apply
runs `pacman -Syu`, which upgrades the whole machine:

```sh
sudo lodi host arm                                # once: allow lodi to manage this machine
mkdir -pm 755 ~/lodi && sudo lodi import ~/lodi   # copy it into ~/lodi/<hostname>/, yours
sudo lodi apply ~/lodi                            # the host as root, then each home as its user
```

The import copies this machine into `~/lodi/<hostname>/` and records it in
`/etc/lodi/host.lock`. It says what it wrote, and that nothing on the machine changed:

```text
wrote ~/lodi/<hostname>/home/<login>/home.toml
wrote ~/lodi/<hostname>/host.toml: 35 package(s) declared, 0 not captured, no file copied from
  /etc; no package and no file outside ~/lodi/<hostname> changed
read it first: NOT CAPTURED lists what was left out and how to declare it
next: sudo lodi host plan ~/lodi (writes nothing)
```

Status: the commands are `lodi --version`, `lodi --help`, `lodi help`, `lodi import`, `lodi plan`,
`lodi apply`, `lodi update`, `lodi init`, `lodi lock`, `lodi shell`, `lodi develop`, `lodi run`,
`lodi trust`, `lodi gc`, `lodi search`, `lodi info` and `lodi boot confirm`. Home and host have
`lodi home init`, `lodi home plan`, `lodi home apply`, `lodi home status`, `lodi host arm`,
`lodi host import`, `lodi host plan`, `lodi host apply`, `lodi host versions`, `lodi host pin` and
`lodi host unpin`. `sudo lodi plan ~/lodi` previews it: [every flag](docs/CLI.md).

## Quick start
### 1. Install

On x86_64 Ubuntu 24.04, Debian 12, Arch or Fedora 44, get `SHA256SUMS` and your package from the
[1.12.2 release](https://github.com/stshow/lodi/releases/tag/v1.12.2). Check for `OK`, then install:

```sh
sha256sum -c --ignore-missing SHA256SUMS
sudo apt install ./lodi_1.12.2_amd64.deb                  # Debian, Ubuntu
sudo pacman -U lodi-1.12.2-1-x86_64.pkg.tar.zst           # Arch
sudo dnf install ./lodi-1.12.2-1.x86_64.rpm               # Fedora
```

The files are not signed, so checksums catch damage, not forgery. Run `sudo lodi host arm` once to
let lodi manage the machine. `lodi --version` prints `lodi 1.12.2`. A `[container]` project needs
rootless Podman. Other distributions: [the guide](docs/GUIDE.md#install-from-a-release).

### 2. Snapshot the machine you are on

1. Copy the machine into a folder you own, and read the plan. No file is copied from `/etc`.

   ```sh
   mkdir -pm 755 ~/lodi
   sudo lodi import ~/lodi
   sudo lodi plan ~/lodi
   ```

2. Add packages to `common` in `host.toml`, then apply. **Warning:** on Arch this runs
   `pacman -Syu`, which **upgrades the whole machine**. There is no rollback. `--overwrite-drift`
   removes **every** package the plan lists under `W_DRIFT`, hand-installed ones too.

   ```sh
   sudo lodi apply ~/lodi
   ```

Commit `~/lodi/` to git. On a fresh machine of the same distro, run `sudo lodi host arm`, copy
`~/lodi/` across and apply. Never copy `/etc/lodi/host-allowed`. [More](docs/GUIDE.md).

### 3. A first project

In a new, empty directory, run these. **Warning:** the last eight lines replace
`~/.config/lodi/home.toml` and append to `~/.profile`. Skip them to keep yours.

```sh
lodi init --name hello          # writes the ./lodi.toml below and adds .lodi/ to .gitignore
printf '\n[tasks.versions]\nrun = "python3 --version"\n' >> lodi.toml
lodi lock                       # resolve ./lodi.toml into ./lodi.lock from upstream metadata only
lodi trust                      # show the task text of ./lodi.toml and record trust in it
lodi develop -- python3 --version   # run a command in the environment of the lock
lodi run versions               # run the [tasks.versions] entry with /bin/sh -c
lodi lock --check               # exit 10 unless the lock is present, valid and fresh
lodi gc --dry-run               # name what a collection would remove, and change nothing
lodi gc                         # remove it, and report the bytes reclaimed
mkdir -p ~/.config/lodi         # the home scope: your own files and your own tools
printf '[home.file.".lodi-hello.conf"]\ntext = "hello from the lodi home scope"\nmode = "0640"\non_remove = "delete"\n\n[tools]\njq = "latest"\n' > ~/.config/lodi/home.toml
lodi home plan                  # what an apply would do to your home directory, writing nothing
lodi home apply                 # do it, and print the one line that puts the tools on PATH
lodi home apply                 # again: nothing to do, and no managed file's mtime moves
lodi home status                # ok, drift, missing, unmanaged or stale-backup, for each path
printf '\n. "$HOME/.local/share/lodi/home-scope/profile.sh"\n' >> ~/.profile   # the printed line, added by you
env -i HOME="$HOME" SHELL=/bin/sh sh -lc 'command -v jq'   # a new login shell finds the tool
```

`lodi home import` still works: `lodi home init` is the same command, and `lodi home import` is
its 1.0 name. `lodi init` writes:

```toml
# lodi.toml: this project's environment, written by `lodi init`. `lodi lock` pins
# what it asks for in ./lodi.lock, and `lodi develop -- COMMAND` runs a command in it.

# The project's name. `version` is the format of this file, and "1" is the only one.
[project]
version = "1"
name = "hello"

# Upstream tools, such as python and nodejs, that lodi downloads and unpacks. A version
# is a prefix ("3.12"), "latest", or a range such as ">=3.11, <3.13".
[tools]
python = "3.12"

# Tasks run with /bin/sh -c in that environment, and only after `lodi trust`:
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

### About the name

Lodi is a pun on the Creedence Clearwater Revival song "Lodi": setting a machine up yet again
feels like being stuck in Lodi again. With Lodi, the next time starts from files you already
have: `host.toml`, `home.toml` and each project's `lodi.lock`.
