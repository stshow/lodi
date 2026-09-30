# The lodi guide

With lodi you can:

- copy the packages and changed `/etc` files of this machine into a folder you own, and put
  them back on a new machine,
- keep the files and tools of your home directory the way you wrote them down,
- give a project its own tools, or its own Debian, Ubuntu, Arch or Fedora container.

This page walks through each of those in order. The [README](../README.md) is the short
version. Every command and flag is in [the command reference](CLI.md), and every error code in
[the error reference](ERRORS.md).

## Terms

- **scope**: what one file manages. The *host* is this machine's packages and root-owned
  files. Your *home* is your own files and tools. A *project* is one directory's tools and tasks.
- **manifest**: the TOML file that says what a scope should hold. It is `host.toml`,
  `home.toml` or `lodi.toml`.
- **lock**: the file beside a manifest that records exact versions and downloads, so the next
  run uses the same bytes.
- **plan** and **apply**: a plan prints what would change and changes nothing. An apply makes
  those changes.
- **host folder**: a folder you own, such as `~/lodi`, with one `<hostname>/host.toml` per
  machine.

## Quick start

Install lodi from a release, or build it from source.

### Install from a release

Download the file for your distribution and `SHA256SUMS` from the project's Releases page,
`https://github.com/stshow/lodi/releases`. Put them in one folder, then check and install:

```sh
sha256sum -c --ignore-missing SHA256SUMS
sudo apt install ./lodi_1.12.2_amd64.deb                  # Debian, Ubuntu
sudo pacman -U lodi-1.12.2-1-x86_64.pkg.tar.zst           # Arch
sudo dnf install ./lodi-1.12.2-1.x86_64.rpm               # Fedora
install -Dm755 lodi-1.12.2-x86_64-linux-musl ~/.local/bin/lodi   # any distribution, no root
```

Run one of the four install lines. The three packages put the same static binary at
`/usr/bin/lodi`. The last line puts it in your own `~/.local/bin`, which works on other
distributions too. Installing lodi changes no other package and no setting.

The aarch64 files, `lodi_1.12.2_arm64.deb`, `lodi-1.12.2-1-aarch64.pkg.tar.zst` and
`lodi-1.12.2-aarch64-linux-musl`, are built but never run by the project. They may not work.

### Build from source

You need x86_64 Linux, `python3`, and either `rustup` or Nix with flakes. Both read the Rust
version from the repository. With Nix, first open its shell as
[Build lodi from source](BUILDING.md) shows. Then run this in a clone of the repository:

```sh
cargo build --locked --release --target x86_64-unknown-linux-musl   # static build
install -Dm755 target/x86_64-unknown-linux-musl/release/lodi ~/.local/bin/lodi
lodi --version                                                      # lodi 1.12.2
lodi --help
```

The binary is static. It needs neither Rust nor Nix to run.

## Allow lodi to manage this machine

`lodi host plan`, `lodi host apply` and `lodi host import` work only on a machine you have
allowed. Run this once, as root:

```sh
sudo lodi host arm
```

```text
armed /: lodi host plan, apply and import may now act on it; delete /etc/lodi/host-allowed to disarm
  it
```

It writes one empty file, `/etc/lodi/host-allowed`. Nothing else writes it, and lodi never runs
`sudo` for you. Delete the file to take the permission back.

Without it, those commands stop with `E_HOST_NOT_ARMED` before they read anything.

## Copy this machine into a folder

Make a folder you own. It must not be writable by your group or others:

```sh
mkdir -pm 755 ~/lodi
```

`lodi host import` copies no file from `/etc`. It never opens `/etc/shadow`, SSH host keys or
private keys. It writes your settings instead, and names each changed configuration file.

```sh
sudo lodi host import ~/lodi
```

The last lines look like this, with your folder, hostname, login and counts:

```text
wrote ~/lodi/<hostname>/home/<login>/home.toml
wrote ~/lodi/<hostname>/host.toml: 0 package(s) declared, 0 not captured, no file copied from
  /etc; no package and no file outside ~/lodi/<hostname> changed
read it first: NOT CAPTURED lists what was left out and how to declare it
next: sudo lodi host plan ~/lodi (writes nothing)
```

The import changes nothing on the machine. The files it writes belong to you, even under
`sudo`, so you can edit them without root. You get:

- `host.toml`, with the distribution packages you chose to install, minus the base system,
- your hostname, time zone, locale and keymap under `[system]`, and your accounts under `[users]`,
- `home/<login>/home.toml`, a commented starting file for your home.

A copy is a starting point, not a backup. It leaves out data, services, snaps, flatpaks,
packages you built by hand and the text of your configuration files. The `NOT CAPTURED` list
at the end of `host.toml` names what it left out.

A package from a third-party apt repository comes as a commented `[sources]` block. Remove the
`# ` from its lines to take it on. The details are in [the host scope](scopes/host.md).

To see what a new import would change in a folder you already have, and write nothing:

```sh
sudo lodi host import --dry-run ~/lodi
```

`lodi home import` is different. It reads none of your files, so it copies no secret. See
[The home scope](#the-home-scope).

## Put the machine back

On the new machine, install lodi, allow it with `sudo lodi host arm`, and copy or clone your
folder. It must run the same distribution. See what an apply would do first:

```sh
sudo lodi host plan ~/lodi
```

Each line is one change. Here, one file would be written:

```text
+ file /etc/motd (mode 0644, owner root:root)
1 action(s)
```

**Read this before you apply.**

- **On Arch, the apply upgrades the whole machine** with `pacman -Syu`, because Arch does not
  support a partial upgrade.
- **There is no rollback.** lodi has no undo. To go back, put the older `host.toml` back and
  apply again.
- An imported `host.toml` says `packages = "exact"`. The apply then removes a package whose line
  you deleted. The plan names each removal first.

```sh
sudo lodi host apply ~/lodi
```

The apply makes exactly the changes the plan showed. Then it applies
`home/<login>/home.toml` as you, with root given up. Add `--no-home` to skip the home. A second
apply prints `nothing to do`.

If the apply stops halfway, run it again. lodi keeps a record of each step and continues.

To use another machine's folder, name it with `--host NAME`. Anyone who can write the folder
decides what root installs, so keep it yours.

### Apply straight from a git repository

A host folder in a public git repository can be applied without a clone:

```sh
sudo lodi host plan github:OWNER/hosts
sudo lodi host apply github:OWNER/hosts
sudo lodi host apply github:OWNER/hosts --refresh
```

The first apply records the commit. Later applies use that commit and need no network. Add
`--refresh` to take the newest commit. Anyone who can push to the repository controls this
machine's packages and root files.

### Host packages

List the packages you want in `host.toml`. One list works on every distribution, and a
distribution's own table adds to it:

```toml
[packages]
common = ["tree", "ed"]
hold = ["ed"]

[packages.debian]
add = ["zip"]
```

On Debian and Ubuntu, lodi never upgrades what is installed. It refreshes the package index when
it is over six hours old. Add `--no-update` to skip that. On Arch, see the warning in
[Put the machine back](#put-the-machine-back).

To keep a package at one version, pin it in the folder, then commit the folder:

```sh
lodi host versions tree ~/lodi
lodi host pin tree ~/lodi
```

A fresh machine that applies the folder gets the same version. To undo a pin, `git revert` the
commit and apply again. [The host scope](scopes/host.md) lists every key.

### Host sources

A package from a vendor's apt repository needs that repository first. Declare it with the key
that signs it:

```toml
[sources.docker]
uris = ["https://download.docker.com/linux/debian"]
suites = ["${host.codename}"]
components = ["stable"]
signed_by = "files/etc/apt/keyrings/lodi-docker.asc"
signed_by_sha256 = "…the SHA-256 of that file's bytes…"
```

The apply checks the key file against `signed_by_sha256`, then adds the repository before it
installs anything. apt checks the signatures with that key. Arch has no `[sources]`. Versions from
the repository change when the repository changes, like the distribution's own.

### Your repository

`lodi import`, `lodi plan`, `lodi apply` and `lodi update` do the same for the machine and each
home in one step:

```sh
sudo lodi import ~/lodi     # copy this machine into ~/lodi/<hostname>/
sudo lodi plan ~/lodi       # what an apply would change, host first, then each home
sudo lodi apply ~/lodi      # the host as root, then every home/<login>/ as that user
lodi update ~/lodi          # move the snapshot to the last published day
```

`lodi update` writes the one lock of the folder:

```text
[host] snapshot 2026-09-25T00:00:00Z -> 2026-09-25T00:00:00Z
wrote ~/lodi/lodi.lock
next: lodi apply ~/lodi
```

Commit `lodi.lock`. Inside `~/lodi` you can leave the path out, and lodi asks before it uses the
current folder. `--yes` answers for you. `LODI_REPO` names the folder from anywhere else.

### On Fedora 44

The same four commands work on Fedora 44, with SELinux enforcing. The import puts your packages
under `[packages.fedora]`. Fedora keeps no archive as it was on each date, so there is no
snapshot. Pin a package to one build instead, and `lodi update` records the build you name.

```sh
sudo lodi import ~/lodi
sudo lodi host pin pv ~/lodi    # the installed build, recorded in lodi.lock
# edit pv's build in ~/lodi/<hostname>/host.toml, then:
lodi update ~/lodi
```

```text
[host] has no snapshot: it floats, and none is added
pv: 0:1.10.5-1.fc44.x86_64 kept at 0:1.10.5-1.fc44.x86_64
wrote ~/lodi/lodi.lock
next: sudo lodi apply ~/lodi
```

Commit and apply. To go back, `git revert HEAD` and apply again: the older build returns, from
Fedora's build server if your mirror dropped it. On Fedora Silverblue it stops with an error, and
nothing changes. See [the host scope](scopes/host.md#the-same-loop-on-fedora-44).

Everything else in this guide works on Fedora 44 as on Debian, Ubuntu and Arch. That covers
your home and its programs, exact packages, and changes made outside lodi. It covers a second
import, a host folder you own, file owners, metapackages, pins and recipes. You upgrade lodi
with `sudo dnf install` of the new `.rpm`. Three things work Fedora's own way:

- A package source is a dnf repository and its key, not an apt suite or keyring. See
  [a third-party dnf repository](scopes/host.md#add-a-third-party-dnf-repository-on-fedora).
- A pin keeps one exact build, not the archive of a date, as above.
- A project's container can use the Fedora 44 base. See
  [container environments](#container-environments).

## A first project

In an empty folder, write the starting `lodi.toml`:

```sh
lodi init --name hello
```

```text
wrote ./lodi.toml
added .lodi/ to ./.gitignore
next: lodi lock
```

It asks for `python = "3.12"` and downloads nothing. Add a task, then lock the versions:

```sh
printf '\n[tasks.versions]\nrun = "python3 --version"\n' >> lodi.toml
lodi lock
```

```text
wrote lodi.lock (resolved python): python 3.12.14
```

A task runs only after you read and allow it:

```sh
lodi trust
```

It prints the task text and its hash, then asks you to confirm. Answer `y`.

Now run a command or the task in the project's environment:

```sh
lodi develop -- python3 --version
lodi run versions
```

The first run downloads Python into `~/.local/share/lodi` and prints a line like this before the
command's own output:

```text
lodi: realized art-5eae8cf79dd47fc2496a4fccc892936b-python-3.12.14,
  env-66f983536f56e0ee26c5518634712383 (66890910 bytes downloaded)
```

Nothing is installed on your machine. The tools are on `PATH` only for that command.
`lodi develop` with no command opens a shell. `lodi shell python` does the same with no
project at all.

Check that the lock still matches `lodi.toml`, and see what the project uses:

```sh
lodi lock --check
lodi info
```

```text
lodi.lock is up to date: python 3.12.14
project: hello
mode:    host tools
tools:
  python  3.12        3.12.14  art-5eae8cf79dd47fc2496a4fccc892936b-python-3.12.14
lock:    fresh
```

`lodi lock --check` exits 10 when the lock is missing or out of date. Every key of `lodi.toml`
is in [the project scope](scopes/project.md).

### Free the space downloads use

```sh
lodi gc --dry-run
lodi gc
```

```text
removed 0 session roots
removed 2 entries
removed 0 image records
removed 0 staging directories
removed 0 downloads
removed 0 shell resolutions
freed 222188998 bytes
```

`--dry-run` prints the same list with `would` and removes nothing. A shell that is open keeps
what it uses. The next `lodi develop` downloads it again.

## Find a tool

`lodi search` and `lodi info TOOL` look in the list of tools lodi knows. They use no network and
write nothing:

```sh
lodi search ripgrep
lodi info jq
```

```text
catalogue
  ripgrep  Recursively search directories for a regex pattern, fast  (https://github.com/BurntSushi/ripgrep)
jq
  description:  A lightweight and flexible command-line JSON processor
  homepage:     https://jqlang.org
  strategy:     github_releases
  upstream:     https://github.com/jqlang/jq/releases
  bin:          bin/jq
  path:         bin
  recipe:       jq.toml (sha256:85cb23c0f68bdc3cbaef77a6687e7b17d900919e8f95c052c9c2e6aed8ed2aac)
```

Run `lodi search ''` to list every tool.

## Supported and tested platforms

| Platform | Architecture | What the project runs |
|---|---|---|
| Ubuntu 24.04, Debian 12, Arch, Fedora 44 | x86_64 | each release is installed on a fresh virtual machine and the quick start is run, before it ships |
| Ubuntu 26.04 | x86_64 | the same, but a failure there does not stop the release |
| other Linux | x86_64 | nothing. The static binary may work, but no one has tried it |
| every distribution | aarch64 | nothing. The aarch64 files are built and never run |

Each release's notes say what it was tried on. A `[container]` project also needs rootless
Podman, which lodi never installs.

## The home scope

The home scope keeps the files in your home directory the way you wrote them down. It has
four commands: `lodi home init` writes a first `home.toml`, `lodi home plan` shows what an apply
would do, `lodi home apply` does it, and `lodi home status` reports what is applied.
`lodi home import` is its 1.0 name, and does the same.

Start the file. It reads none of your files, so it holds no setting or secret of yours:

```sh
lodi home init
```

```text
wrote ~/.config/lodi/home.toml; next: uncomment what you want, then lodi home plan
```

Run it as yourself, never with `sudo`. Uncomment what you want, or add files of your own:

```toml
[home]
version = "1"

[home.file.".inputrc"]
text = "set editing-mode vi\n"

[home.xdg_config."git/ignore"]
text = "*.swp\n"
```

See what an apply would do. This writes nothing:

```sh
lodi home plan
```

```text
create    .config/git/ignore        0644
create    .inputrc                  0644    (backup: first unmanaged copy kept)
2 files: 2 create
```

Apply it:

```sh
lodi home apply
```

```text
lodi: warning W_REPLACED_UNMANAGED: .inputrc was not written by lodi; the copy it had is kept as
  94029cdb18d5c02ff6fa90c605076e260835fc9a0be378a38847a3f5817192f5-283382f3da0d1465
created   .config/git/ignore        0644
created   .inputrc                  0644    (backup: first unmanaged copy kept)
2 files: 2 create
```

Your old `.inputrc` is kept once under `~/.local/share/lodi/home-scope/backups/`. A second
apply prints `nothing to do`.

### When you edit a file by hand

If you change a file lodi wrote, the next apply stops and changes nothing:

```text
lodi: warning W_DRIFT: .config/git/ignore has changed since lodi wrote it (recorded 8d92fb00, now
  b6b185a6)
lodi: error E_DRIFT: 1 managed file has changed since lodi wrote it; nothing was applied
   = .config/git/ignore
   = hint: run lodi home status, then lodi home apply --overwrite-drift to take them back
```

See which files changed:

```sh
lodi home status
```

```text
drift        .config/git/ignore        0644    (edited by hand since lodi wrote it)
ok           .inputrc                  0644
2 paths: 1 ok, 1 drift
```

Copy your edit into `home.toml`, or run `lodi home apply --overwrite-drift`. That keeps a copy
of your edit, then writes the file again.

### Tools for your whole account

Add a `[tools]` table to `home.toml`, as in a project:

```toml
[tools]
jq = "latest"
```

Thirty tools come with lodi as built-in recipes:

```text
actionlint  bat  delta  eza  fd  fzf  gh  go  helm  hyperfine
jdk  jq  just  k9s  kubectl  lazygit  neovim  nodejs  opentofu  python
ripgrep  ruff  rust  shellcheck  shfmt  sops  starship  uv  yq  zoxide
```

List them with what each one is:

```sh
lodi search --registry ''
```

`neovim` installs the `nvim` command and `opentofu` installs `tofu`. `kubectl` and `helm` find
only the release their upstream currently calls latest, so `kubectl = "latest"` works and a
constraint on an older release finds nothing. None of them needs root.

`lodi home apply` downloads them and prints the line that puts them on `PATH`:

```text
add this line to your shell rc file (lodi never edits it):
  . "$HOME/.local/share/lodi/home-scope/profile.sh"
```

Add that line to your `~/.profile` yourself. lodi never edits a shell file on its own.
Program settings, such as `[programs.git]`, and every other key are in
[the home scope](scopes/home.md).

### Install a tool lodi does not carry

Write a recipe for it in the `recipes/` folder at your repository's root. Then name the tool in
a home's `[tools]`, as you would a built-in one:

```sh
$EDITOR ~/lodi/recipes/kubectl-convert.toml
cd ~/lodi/laptop/home/alice
echo 'kubectl-convert = "latest"' >> home.toml
sudo lodi apply ~/lodi
```

That user gets `kubectl-convert` on `PATH`. `lodi info kubectl-convert`, run in `~/lodi`, names
your file. A file with a built-in recipe's name, such as `recipes/jq.toml`, replaces the built-in
`jq`, and lodi says so once with `W_RECIPE_SHADOWED`. Without a repository, lodi uses only its
built-in recipes. [The recipe guide](../catalogue/README.md#a-recipe-of-your-own) has an example
and the rules.

## Container environments

Start a project that runs in a Debian, Ubuntu, Arch or Fedora container:

```sh
lodi init --base debian:bookworm
```

```text
wrote ./lodi.toml
added .lodi/ to ./.gitignore
next: lodi lock
```

The file has `[container]` with the distribution and an empty `[packages] common` list. Add the
packages you need, then run `lodi lock` and `lodi develop`. The bases are `debian:bookworm`,
`ubuntu:noble`, `arch:rolling` and `fedora:44`.

`lodi lock` records exact package versions from the archive as it was on one date. So the same
two files give the same versions on another machine. Nothing is installed on your machine: the
packages exist only inside the container.

On Fedora 44 the lock names the builds Fedora 44 shipped with:

```sh
lodi init --base fedora:44
# put ["jq", "zip"] in [packages] common, then:
lodi lock
lodi trust
lodi develop -- rpm -q --qf '%{NAME} %{VERSION}-%{RELEASE}\n' jq zip
```

`lodi lock` prints `wrote lodi.lock (resolved base): fedora 44 snapshot 2026-04-22T13:34:32Z`
and `(150 packages, 4 to install)` on one line: the image's 146 builds, and jq, oniguruma, unzip
and zip, which dnf would add. The first `lodi develop` downloads about 71 MB and says
`building the container image (150 locked packages, 4 to install; no network)`. Every run then
prints:

```text
jq 1.8.1-2.fc44
zip 3.0-45.fc44
```

You need rootless Podman, so `podman info` works without root. lodi never installs it. The
container can read and write your home and the project folder, and shares the machine's network.
[The project scope](scopes/project.md) has the details and the limits of each base.

## License

lodi is free software under the GNU General Public License, version 3 or any later version.
The full text is in [LICENSE](../LICENSE). It comes with no warranty.

The license covers lodi itself. It does not cover your `lodi.toml`, your lock files, the
environments lodi builds or what you run in them. The licenses of the libraries in the binary
are listed in [the dependency list](licenses/DEPENDENCIES.md).
