# The command line

Every `lodi` command, with an example, its flags and the exit statuses it can end with. For a
task from start to end, read [the guide](GUIDE.md). For what an error code means and what to do
next, read [the error reference](ERRORS.md).

| You want to | Commands |
|---|---|
| keep this machine and your home in one config | [`lodi import`](#lodi-import), [`lodi switch`](#lodi-switch), [`lodi update`](#lodi-update), [`lodi pin`](#lodi-pin), [`lodi unpin`](#lodi-unpin) |
| set up and run a project | [`lodi init`](#lodi-init), [`lodi develop`](#lodi-develop), [`lodi run`](#lodi-run) |
| try a tool, find one, free space | [`lodi shell`](#lodi-shell), [`lodi search`](#lodi-search), [`lodi gc`](#lodi-gc) |
| see the version or the help | [`lodi --version`](#lodi---version), [`lodi --help`](#lodi---help), [`lodi help`](#lodi-help) |

After the commands come the sections several commands share, from
[how a command finds your config](#how-a-command-finds-your-config) to
[what 2.0 promises](#what-20-promises).

## How to read a command's entry

Each entry has the same parts: an example with its real output, what the command does, a
**Flag** table and an **Exits** table. The flag table lists every flag the command takes. A flag
with an argument names it, and `Required` says whether the command stops without it. Give each
flag at most once. `lodi` reads no flag after `--`, because what follows is your own command.

Any other flag, a flag without its value, or an argument the command does not take is a usage
error. It exits 2, prints no `E_` code, and reads and writes nothing.

The exits table names the statuses a command can end with and the codes behind them. Status 0 is
success, and 2 is a usage error. `lodi` with no command at all prints where to start, lines 3
to 6 of `lodi --help`, and exits 0. Some examples below come
from a Debian 12 test machine, so they name its packages. `~/demo` stands for your project and
`<hostname>` for your machine.

An output line longer than this page is wrapped onto an indented next line.

### `lodi --version`

Print the version and exit.

```sh
lodi --version
```

```text
lodi 2.0.0
```

It reads nothing and runs nothing.

| Flag | Argument | Required | What it does |
|---|---|---|---|

| Status | Code | When |
|---|---|---|
| 0 | — | the version was printed |
| 2 | — | any argument beside it |

### `lodi --help`

Print the help and exit.

```sh
lodi --help
```

```text
lodi 2.0.0 — environment and system manager for Ubuntu, Debian, Arch and Fedora

Start here:
  lodi import   copy this host and your home into a config, ~/.config/lodi
  lodi switch   make this host and your home match the config
  lodi init     start a project: write ./lodi.toml in this folder

Examples:
  lodi import --home                copy only your home, no root needed
  lodi switch --dry-run             show what a switch would change
  lodi pin tree                     list the versions of tree you can pin
  lodi init --base debian:bookworm  start a project in a Debian container
  lodi develop -- make              run make in the project's environment
  lodi run build                    run the project's build task

Commands ('lodi help COMMAND' shows one with its examples):
  host and home  lodi import, switch, update, pin, unpin
  projects       lodi init, develop, run, shell, search, gc
  help           lodi help [COMMAND], --help, --version

Anything else is refused with exit status 2.
Reference: https://github.com/stshow/lodi/blob/main/docs/CLI.md
```

It fits in 80 columns and 40 lines, and it lists only commands that work.

`--help` and `-h` may follow any command. `lodi switch --help`, `lodi pin -h` and
`lodi init --name x --help` print the help of `switch`, `pin` and `init`, and run nothing else.
A command's help shows what it does, its usage, examples and its options.

The help is about the first word after `lodi`. Nothing after `--` asks for help, and nothing from
`lodi run`'s task name on. So `lodi run build --help` passes `--help` to the task. A `--help`
beside a command that does not exist, such as `lodi frobnicate --help`, is a usage error. Beside
a [1.x name](#names-20-removed), such as `lodi plan --help`, it prints what replaces that name.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `-h` | — | no | the same request as `--help`, wherever `--help` may go |

| Status | Code | When |
|---|---|---|
| 0 | — | the help, or the part of it asked for, was printed |
| 2 | — | it was asked about a command that does not exist, or a 1.x name |

### `lodi help`

Show the help of lodi, or of one command.

```sh
lodi help init
```

```text
Start a project: write a commented ./lodi.toml in this directory.

Usage:
  lodi init [--name NAME] [--base DISTRO[:RELEASE]] [--force]

Examples:
  lodi init                         a project named after this directory
  lodi init --base debian:bookworm  a project in a Debian container
  lodi init --name hello --force    replace ./lodi.toml, naming it hello

Options:
  --name NAME              the project's name, instead of this folder's
  --base DISTRO[:RELEASE]  a project in a container of that distribution
  --force                  replace an existing ./lodi.toml

Details:
  It also adds .lodi/ to ./.gitignore. --base writes a [container] manifest
  instead of host tools, from one of the bases
  arch:rolling, debian:bookworm, fedora:44 or ubuntu:noble. --force replaces
  an existing ./lodi.toml. It works offline: nothing is downloaded, installed
  or run.
```

`lodi help` alone prints all of `lodi --help`. `lodi help COMMAND` prints what
`lodi COMMAND --help` prints, for each command `lodi --help` lists. It works offline.

| Flag | Argument | Required | What it does |
|---|---|---|---|

| Status | Code | When |
|---|---|---|
| 0 | — | the help, or the part of it asked for, was printed |
| 2 | — | `COMMAND` is no command that `lodi --help` lists |

### `lodi import`

Copy this machine and your home into your config.

```sh
lodi import
```

```text
May lodi manage this host? [y/N] y
[1/7] ask: done, 0.0s (yes)
[2/7] read the host: done, 0.4s
[3/7] write the marker: done, 0.3s
[4/7] record the host: done, 0.0s
[5/7] fill the lock: done, 0.0s
[6/7] write the config: done, 0.1s
[7/7] git init: done, 0.0s
imported in 1.0s: ~/.config/lodi: home.toml, lodi.lock, host.toml
wrote ~/.config/lodi/host.toml: 3 package(s) declared, 0 not captured; read NOT CAPTURED in it first
next: review ~/.config/lodi, commit it, then run lodi switch --dry-run
```

You type it as yourself. lodi asks for your password itself (through `sudo`, `doas` or `run0`)
to read the host, and every file it writes belongs to you. It finds the folder as
[How a command finds your config](#how-a-command-finds-your-config) says. It makes the folder
when it is not there yet, and a folder it makes gets `git init`. It commits nothing.

- Into an empty folder it writes `host.toml`, `home.toml`, `lodi.lock` and `files/host/`.
- Into a config that already has another host, it writes this one to `<hostname>/host.toml`,
  lists both in `config.toml`, and both share your `home.toml`.
- Into a config that already has this host, it merges what changed on the machine into its
  `host.toml`. It keeps your edits, with one warning for each place they disagree.
- A `home.toml` that is there already is never rewritten.

The first time, it asks whether lodi may manage this machine, and a yes leaves a mark under
`/etc/lodi`. It also records what it read there, so that the first `lodi switch` changes only
what you edit. It installs and removes nothing.

Without the network it writes no lock and says so. It stops before reading anything when the
folder is a project, sits inside another config, or holds other files. A URL stops it too.

**Before you share the config:** `files/host/` holds this machine's changed configuration
files, copied byte for byte.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--home` | — | no | write only `home.toml`, and read nothing as root |
| `--yes` | — | no | answer yes to the one question whether lodi may manage this machine |
| `--dry-run` | — | no | show what it would write, and ask, write and elevate nothing |
| `--root` | `DIR` | no | the root to read, instead of `/` |
| `-v` | — | no | print every step |

| Status | Code | When |
|---|---|---|
| 0 | — | imported |
| 2 | — | a usage error, such as an unknown flag or a second `PATH` |
| 3 | `E_NO_MANIFEST`, `E_UNSUPPORTED`, `E_PATH_ESCAPE`, `E_CONFIG` | the remembered folder is gone, a URL was given, the folder is not yours, or it is not a config to import into |
| 6 | `E_STORE_IO` | a write failed; what the run created is removed |
| 9 | `E_NEED_ROOT`, `E_HOST_ROOT_REQUIRED` | the host needs root, or the guard holds it |

### `lodi switch`

Make this machine and your home match your config, the host part first.

```sh
lodi switch
```

```text
- package bc (not in the manifest)
host: +0 -1 packages
0 files: nothing to do
home: no changes
[1/1] Remove packages: done, 0.2s
switched in 2.2s: +0 -1 packages
```

You type it as yourself. lodi asks for your password itself, and only when the host part has
something to change. The home part always runs as you. When nothing differs it says
`nothing to switch`.

It finds the config as [How a command finds your config](#how-a-command-finds-your-config) says,
and locks what `lodi.lock` is missing. On standard error it shows what each part changes and the
config's commit. It warns when the config is not in git or has uncommitted changes.

The config may also be a git URL, such as `lodi switch github:you/lodi`. A fetched config is
never written. The next switch uses the same commit until `--refresh`.

Then it changes the host part and, only if that worked, your home part. The host part stops
with `E_HOST_NOT_ARMED` until `lodi import` allowed lodi to manage this machine.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--ask` | — | no | ask `switch? [y/N]` before changing anything |
| `--dry-run` | — | no | show what would change, and change and write nothing |
| `--host` | — | no | only the host part |
| `--home` | — | no | only your home part; no root needed |
| `--update` | — | no | look every pin up again first, as `lodi update` does |
| `--overwrite-drift` | — | no | take back a home file edited by hand |
| `--resolved` | `ID` | no | go on after an interrupted switch, once you set right what its journal entry `ID` names |
| `--ref` | `NAME` | no | with a URL: the branch or tag to fetch |
| `--rev` | `COMMIT` | no | with a URL: the commit to fetch |
| `--refresh` | — | no | with a URL: look up the branch again instead of the last commit |
| `--root` | `DIR` | no | the root to change, instead of `/` |
| `-v` | — | no | print every step |

| Status | Code | When |
|---|---|---|
| 0 | — | switched, or there was nothing to switch |
| 2 | — | a usage error, including `--host` with `--home`, `--ask` with `--dry-run`, a URL flag without a URL, `--ref` with `--rev`, or `--rev` with `--refresh` |
| 3 | `E_NO_MANIFEST`, `E_UNSUPPORTED`, `E_CONFIG` | the config or a part it needs is missing, or `--update` was given with a URL |
| 5 | `E_FETCH`, `E_INSECURE_URL`, `E_HASH_MISMATCH` | the URL could not be fetched, is not one lodi fetches, or its tree does not match |
| 8 | `E_DRIFT`, `E_APPLY` | a home file was edited by hand and `--overwrite-drift` was not given, or a part failed; a failed host part stops before your home part |
| 9 | `E_NEED_ROOT`, `E_HOST_NOT_ARMED`, `E_HOST_ROOT_REQUIRED` | the host part needs root, or lodi may not manage this machine yet |
| 10 | `E_LOCK_STALE` | a tool's version or recipe changed since `lodi.lock` locked it; `lodi update` locks it |
| 11 | `E_DECLINED` | `--ask` was answered no, or there was no terminal to ask on |

### `lodi update`

Move your config's snapshots to the last published day and lock them.

```sh
lodi update
```

```text
[1/2] look up host.toml
[1/2] look up host.toml: done, 1.2s
[2/2] lock home.toml
[2/2] lock home.toml: done, 0.4s
updated in 1.6s: host.toml snapshot 2026-09-25T00:00:00Z
next: lodi switch
```

It moves the config's `lodi.lock` forward, like `nix flake update`. It changes nothing on the
machine.

- A `[host] snapshot` moves to the newest published day, and the pins are looked up again.
- A host without a snapshot gets none, and a pin you set in `[packages.pin]` stays.
- Each key from `signed_by_url` is downloaded, checked against `signed_by_sha256` and recorded.
- Each home's tools are locked again.
- `host.toml` and `lodi.lock` change together or not at all. Under `sudo` it writes as the
  config's owner.

It finds the config as [How a command finds your config](#how-a-command-finds-your-config) says.
A URL stops with `E_UNSUPPORTED`. One exception: in a folder holding `./lodi.toml`, with no path
typed, it updates that project's `lodi.lock` instead.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--host` | `NAME` | no | the host of a config with several hosts |
| `--root` | `DIR` | no | the root to read, instead of `/` |
| `-v` | — | no | print each step's log as it runs |

| Status | Code | When |
|---|---|---|
| 0 | — | updated, or already up to date |
| 2 | — | a usage error, or `--host` in a project |
| 3 | `E_NO_MANIFEST`, `E_UNSUPPORTED`, `E_CONFIG` | no config was found, a URL was given, or the path typed is a project |
| 4 | `E_REPO_UNREACHABLE`, `E_SNAPSHOT_TOO_OLD`, `E_SNAPSHOT_FUTURE` | the archive could not be read, or a date is outside what it serves; nothing changed |
| 5 | `E_FETCH`, `E_HASH_MISMATCH` | a key or an index could not be downloaded, or does not match |
| 6 | `E_STORE_IO` | a file could not be written, and nothing changed |
| 9 | `E_SYSTEM_BUSY`, `E_HOST_ROOT_REQUIRED` | another command is writing the config |

### `lodi pin`

Pin a package, or with `--all` the host's snapshot, and lock it.

```sh
lodi pin bc --to 2026-09-01
```

```text
pinned bc
bc: 1.07.1-3+b1 from debian at 2026-09-01T00:00:00Z
next: lodi switch
```

With no `--to` it lists the package's versions and the command that pins each, and writes
nothing. `--all` sets `[host] snapshot` to `--to DATE` or to the last full day. It looks up the
pin before it writes, and writes `host.toml` and `lodi.lock` together. Pinning again to the same
value writes nothing. The config is found as for [`lodi update`](#lodi-update).

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--to` | `VERSION` or `DATE` | no | what to pin to. With `--all`, a date only |
| `--all` | — | no | pin the whole host to one date instead of one package |
| `--host` | `NAME` | no | the host of a config with several hosts |
| `--root` | `DIR` | no | the root to read, instead of `/` |

| Status | Code | When |
|---|---|---|
| 0 | — | pinned, already pinned, or the versions listed |
| 2 | — | a usage error: no package and no `--all`, or both |
| 3 | `E_UNKNOWN_PACKAGE`, `E_NO_MANIFEST`, `E_UNSUPPORTED` | the package is not in the list, no config or host was found, or `--all` on Fedora |
| 4 | `E_REPO_UNREACHABLE`, `E_SNAPSHOT_TOO_OLD`, `E_SNAPSHOT_FUTURE` | the archive could not be read, or the date is outside what it serves; nothing changed |
| 5 | `E_FETCH`, `E_HASH_MISMATCH` | an index could not be downloaded, or does not match; nothing changed |
| 9 | `E_SYSTEM_BUSY`, `E_HOST_ROOT_REQUIRED` | another command is writing the config |

### `lodi unpin`

Remove a package's pin, or with `--all` every pin and the snapshot.

```sh
lodi unpin bc
```

```text
unpinned bc
next: lodi switch
```

It writes `host.toml` and `lodi.lock` together. Nothing to remove prints a line and is not an
error. The config is found as for [`lodi update`](#lodi-update).

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--all` | — | no | remove every pin and the snapshot |
| `--host` | `NAME` | no | the host of a config with several hosts |
| `--root` | `DIR` | no | the root to read, instead of `/` |

| Status | Code | When |
|---|---|---|
| 0 | — | unpinned, or nothing to unpin |
| 2 | — | a usage error: no package and no `--all`, or both |
| 3 | `E_NO_MANIFEST`, `E_UNSUPPORTED` | no config or host was found |
| 9 | `E_SYSTEM_BUSY`, `E_HOST_ROOT_REQUIRED` | another command is writing the config |

### `lodi init`

Start a project: write a commented `./lodi.toml` in this directory.

```sh
lodi init
```

```text
wrote ./lodi.toml
added .lodi/ to ./.gitignore
next: lodi develop
```

It writes `./lodi.toml` and adds the line `.lodi/` to `./.gitignore`, creating that file if
needed. It works offline. Nothing is downloaded, installed or run, and nothing outside this
directory is read or written. A second `lodi init` stops with `E_EXISTS` unless you give
`--force`.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--name` | `NAME` | no | the project name to write into `[project]`, instead of the directory's name |
| `--base` | `DISTRO[:RELEASE]` | no | write a `[container]` project on that base, such as `debian:bookworm`, instead of host tools |
| `--force` | — | no | replace an existing `./lodi.toml` |

**Reads:** `./lodi.toml`, to stop before replacing it, and `./.gitignore`. **Writes:**
`./lodi.toml` at mode 0644 and `./.gitignore`.

| Status | Code | When |
|---|---|---|
| 0 | — | the manifest was written |
| 2 | — | a usage error |
| 3 | `E_EXISTS`, `E_IDENT`, `E_UNSUPPORTED` | `./lodi.toml` is already there and `--force` was not given, `--name` has no usable character, or `--base` names a base lodi cannot write |
| 6 | `E_STORE_IO` | `./.gitignore` is not a regular file, or a write failed |

### `lodi develop`

Run a command, or a shell, in the project's environment.

```sh
lodi develop -- rg --version
```

```text
lodi: warning W_SHADOWS_HOST: this environment runs its own `rg` in place of the host command of the
  same name on PATH
lodi: realized art-33e15bcf1624b25cdd2a55813a47a2f9-ripgrep-15.2.0,
  env-57597d9e49c348822144e3effce87266 (2265718 bytes downloaded)
ripgrep 15.2.0 (rev e89fff89ac)

features:+pcre2
simd(compile):+SSE2,-SSSE3,-AVX2
simd(runtime):+SSE2,+SSSE3,+AVX2

PCRE2 10.45 is available (JIT is available)
```

It locks first, and writes a missing `./lodi.lock`. A new tool is added, a removed one is
dropped, and a changed entry is resolved again. Every other entry is kept byte for byte, so no
pinned version moves. A container base keeps its snapshot. A fresh lock is not touched.

When it writes the lock it says so on standard error:

```text
lodi: wrote lodi.lock (resolved ripgrep): ripgrep 15.2.0
```

A lock this lodi cannot read, or one a newer lodi wrote, stops with the file named and is never
replaced. A failed resolution leaves the lock as it was and runs nothing. Then it downloads what
the lock pins, runs the command after `--` and exits with its status. With nothing after `--` it
opens an interactive shell. The warning `W_SHADOWS_HOST` names each command that runs in place of
one on your `PATH`.

The project must be trusted before anything is locked or run. On a terminal it shows the task
text, or the whole manifest when there are no tasks, with its hash, and asks `[y/N]`. `y` or
`yes` records your trust, and any change to what was trusted asks again. Without a terminal it
stops:

```text
lodi: error E_TRUST_REQUIRED: ~/demo/lodi.toml declares task text that has not been trusted; nothing
  was realized or run
   = hint: pass --trust to allow it for this run
```

While the command runs, lodi passes `SIGTERM` and `SIGHUP` on to it. A command ended by signal
`n` exits `128 + n`, so 143 after `SIGTERM`. Ctrl-C from the terminal reaches the command
directly, and lodi then exits 130.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--no-nest` | — | no | stop when you are already inside another lodi environment |
| `--trust` | — | no | allow the project for this run only, as `LODI_TRUST=1` does, and record nothing |
| `--` | `COMMAND [ARGS…]` | no | run that command instead of a shell, with its arguments unchanged |

**Reads:** `./lodi.toml`, `./lodi.lock`, the package indexes and upstream metadata when something
needs locking, the pinned downloads over HTTPS, `$LODI_HOME` and the trust store. **Writes:**
`./lodi.lock` when something needed locking, and the trust store when you answer yes. It also
writes `$LODI_HOME`: the download cache, the unpacked tools and a record of the session. In a
container, the project directory and `$HOME` are mounted read-write.

| Status | Code | When |
|---|---|---|
| 0 | — | the command or shell exited cleanly, or else its own status |
| 2 | — | a usage error, including `--` with no command after it |
| 3 | `E_NO_MANIFEST`, `E_SYNTAX`, `E_TYPE`, `E_IDENT`, `E_UNKNOWN_ATTR`, `E_UNKNOWN_BLOCK`, `E_UNSUPPORTED`, `E_MODE`, `E_BLOCK_NOT_ALLOWED`, `E_EXCLUDED_CONSTRUCT`, `E_VAR_UNSET`, `E_VERSION` | the manifest |
| 4 | `E_LOCK_VERSION`, `E_NO_RECIPE`, `E_NO_MATCH`, `E_RECIPE_CTX`, `E_RECIPE_INVALID`, `E_UNSUPPORTED_ARCH`, `E_REPO_UNREACHABLE`, `E_SNAPSHOT_TOO_OLD`, `E_SNAPSHOT_FUTURE` | the lock was written by a newer lodi, or locking found no recipe, no matching version, or an index or snapshot lodi cannot use |
| 5 | `E_FETCH`, `E_INSECURE_URL`, `E_NO_CHECKSUM`, `E_HASH_MISMATCH` | a pinned download failed, or does not match the lock |
| 6 | `E_STORE_IO`, `E_STORE_VERSION`, `E_ARCHIVE_FORMAT`, `E_ARCHIVE_UNSAFE`, `E_ARCHIVE_EMPTY`, `E_BIN_MISSING`, `E_TREE_UNSUPPORTED`, `E_LAYER_BUILD`, `E_CLOSURE_DRIFT` | a download cannot be unpacked, stored or turned into an image |
| 7 | `E_NO_RUNTIME`, `E_NESTED`, `E_ENTER`, `E_SHELL_NOT_FOUND` | the environment cannot be entered, or Podman is missing |
| 9 | `E_STORE_PERM` | there is no usable per-user store |
| 10 | `E_LOCK_STALE` | `./lodi.lock` cannot be read; it is left as it is |
| 11 | `E_TRUST_REQUIRED`, `E_DECLINED` | the project is not trusted, or you answered no |

### `lodi run`

Run one of the project's tasks.

```sh
lodi run versions
```

```text
lodi: warning W_SHADOWS_HOST: this environment runs its own `rg` in place of the host command of the
  same name on PATH
ripgrep 15.2.0 (rev e89fff89ac)

features:+pcre2
simd(compile):+SSE2,-SSSE3,-AVX2
simd(runtime):+SSE2,+SSSE3,+AVX2

PCRE2 10.45 is available (JIT is available)
```

It runs `[tasks.versions]` of `./lodi.toml` with `/bin/sh -c`, in the environment
`lodi develop` enters. Arguments after the task name go to the task unchanged. It asks for trust
and locks as `lodi develop` does. A task the manifest does not declare is a usage error, reported
before trust is asked or anything is locked:

```text
lodi: error: lodi.toml has no task `tset`
   = its tasks: versions
```

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--no-nest` | — | no | stop when you are already inside another lodi environment |
| `--trust` | — | no | allow the task text for this run only, as `LODI_TRUST=1` does, and record nothing |

**Reads:** As `lodi develop`. **Writes:** As `lodi develop`.

| Status | Code | When |
|---|---|---|
| 0 | — | the task exited cleanly, or else its own status |
| 2 | — | a usage error, including no task, or a task the manifest does not declare |
| 3 | `E_NO_MANIFEST`, `E_SYNTAX`, `E_TYPE`, `E_IDENT`, `E_UNKNOWN_ATTR`, `E_UNKNOWN_BLOCK`, `E_UNSUPPORTED`, `E_MODE`, `E_BLOCK_NOT_ALLOWED`, `E_EXCLUDED_CONSTRUCT`, `E_VAR_UNSET`, `E_VERSION` | the manifest |
| 4 | `E_LOCK_VERSION`, `E_NO_RECIPE`, `E_NO_MATCH`, `E_RECIPE_CTX`, `E_RECIPE_INVALID`, `E_UNSUPPORTED_ARCH`, `E_REPO_UNREACHABLE`, `E_SNAPSHOT_TOO_OLD`, `E_SNAPSHOT_FUTURE` | the lock, locking or a recipe, as for `lodi develop` |
| 5 | `E_FETCH`, `E_INSECURE_URL`, `E_NO_CHECKSUM`, `E_HASH_MISMATCH` | a pinned download, as for `lodi develop` |
| 6 | `E_STORE_IO`, `E_STORE_VERSION`, `E_ARCHIVE_FORMAT`, `E_ARCHIVE_UNSAFE`, `E_ARCHIVE_EMPTY`, `E_BIN_MISSING`, `E_TREE_UNSUPPORTED`, `E_LAYER_BUILD`, `E_CLOSURE_DRIFT` | unpacking and storing, as for `lodi develop` |
| 7 | `E_NO_RUNTIME`, `E_NESTED`, `E_ENTER` | the environment cannot be entered |
| 9 | `E_STORE_PERM` | there is no usable per-user store |
| 10 | `E_LOCK_STALE` | the lock cannot be read, as for `lodi develop` |
| 11 | `E_TRUST_REQUIRED`, `E_DECLINED` | the task text is not trusted, or you answered no |

### `lodi shell`

Open a shell with some tools, without a project.

```sh
lodi shell ripgrep
```

```text
lodi: warning W_SHADOWS_HOST: this environment runs its own `rg` in place of the host command of the
  same name on PATH
lodi: realized env-346aed16ce608804b5b58a1eed00402f (0 bytes downloaded)
```

You are then in a shell with `rg` first on `PATH`. Leave it with `exit`. Give a version as
`python@3.12`. With `--base debian:bookworm`, the names are Debian packages in a Podman
container instead.

It reads no `./lodi.toml` and writes nothing in this directory, so it asks for no trust. The
tools are the ones you typed. The resolution is kept under `$LODI_HOME/cache/shell/`, so the same
command again needs no network.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--no-nest` | — | no | stop when you are already inside another lodi environment |
| `--base` | `DISTRO[:RELEASE]` | no | read the names as distribution packages in a rootless Podman container |

**Reads:** Upstream metadata and downloads over HTTPS, and `$LODI_HOME`. **Writes:**
`$LODI_HOME`: a lock of its own, the download cache, the unpacked tools and a record of the
session.

| Status | Code | When |
|---|---|---|
| 0 | — | the shell exited cleanly, or else its own status |
| 2 | — | a usage error, including no tool at all, or `--base` with no package |
| 3 | `E_UNSUPPORTED`, `E_IDENT`, `E_TYPE` | a base, release or name lodi cannot use |
| 4 | `E_NO_RECIPE`, `E_NO_MATCH`, `E_UNSUPPORTED_ARCH`, `E_RECIPE_CTX`, `E_RECIPE_INVALID`, `E_REPO_UNREACHABLE`, `E_SNAPSHOT_TOO_OLD`, `E_SNAPSHOT_FUTURE` | no recipe, no matching version, or an index lodi cannot use |
| 5 | `E_FETCH`, `E_INSECURE_URL`, `E_NO_CHECKSUM`, `E_HASH_MISMATCH` | a download failed, or does not match |
| 6 | `E_STORE_IO`, `E_STORE_VERSION`, `E_ARCHIVE_FORMAT`, `E_ARCHIVE_UNSAFE`, `E_ARCHIVE_EMPTY`, `E_BIN_MISSING`, `E_TREE_UNSUPPORTED`, `E_LAYER_BUILD`, `E_CLOSURE_DRIFT` | a download cannot be unpacked, stored or turned into an image |
| 7 | `E_NO_RUNTIME`, `E_NESTED`, `E_ENTER`, `E_SHELL_NOT_FOUND` | the environment cannot be entered, or Podman is missing |
| 9 | `E_STORE_PERM` | there is no usable per-user store |

### `lodi search`

Find a tool in the catalogue, or a package in the distribution's index.

```sh
lodi search grep
```

```text
catalogue
  ripgrep  Recursively search directories for a regex pattern, fast  (https://github.com/BurntSushi/ripgrep)
```

It lists the catalogue's tools whose name or description contains your query, in any case.
Then it lists the matching packages of the distribution index `./lodi.lock` pins, from the
download cache. An empty query, `''`, matches everything. It makes no network request and writes
nothing.

A query that is a recipe's name, in any case, shows that recipe's details first. They name its
file and hash, the built-in recipe it replaces, and what `./lodi.lock` pins for it. The
other matches follow. `--registry` shows them too, and `--distro` never does:

```sh
lodi search jq
```

```text
jq
  description:  A lightweight and flexible command-line JSON processor
  homepage:     https://jqlang.org
  strategy:     github_releases
  upstream:     https://github.com/jqlang/jq/releases
  bin:          bin/jq
  path:         bin
  recipe:       jq.toml (sha256:098a17e08bfb4ec88be36ce9a7cba7be9ea07942e08dc45f32ff8ea0eea6628d)
```

When the index is not in the cache it prints `W_NO_INDEX` and still exits 0. You see it with
`--distro`, or when the lock pins a container base whose index is missing:

```sh
lodi search --distro jq
```

```text
lodi: warning W_NO_INDEX: ./lodi.lock pins no container base, so this project has no distro index
no match for "jq"
```

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--registry` | — | no | search the built-in catalogue only |
| `--distro` | — | no | search the distribution index only |

**Reads:** The built-in catalogue, `./lodi.lock` when it is there, and `$LODI_HOME/cache/dl/`.
**Writes:** Nothing.

| Status | Code | When |
|---|---|---|
| 0 | — | the matches were printed, or there were none |
| 2 | — | a usage error, including no query, a second query, or both flags at once |
| 3 | `E_NO_MANIFEST` | `--distro` in a directory with no project |
| 10 | `E_LOCK_STALE` | `./lodi.lock` cannot be read by this version of lodi |

### `lodi gc`

Free disk space: remove what no project or shell still uses.

```sh
lodi gc --dry-run
```

```text
would remove 0 session roots
would remove 5 entries
would remove 0 image records
would remove 0 staging directories
would remove 0 downloads
would remove 1 shell resolution
would free 224460701 bytes
```

Without `--dry-run` it removes them:

```text
removed 0 session roots
removed 5 entries
removed 0 image records
removed 0 staging directories
removed 0 downloads
removed 1 shell resolution
freed 224460701 bytes
```

It removes unpacked tools no project or shell uses, records of sessions that ended, and downloads
no tool needs. It never follows a symbolic link out of the store and never crosses a mount.
Nothing outside `$LODI_HOME` is removed unless you give `--images`.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--dry-run` | — | no | show what would be removed, and remove nothing |
| `--keep-days` | `N` | no | keep downloads newer than `N` whole days, 7 by default |
| `--images` | — | no | also remove unused Podman images tagged `localhost/lodi-env:<32 hex>` |
| `-v` | — | no | name everything it removes |

**Reads:** `$LODI_HOME` and, only when the store records an image, `podman`. **Writes:** It removes
from `$LODI_HOME`, and nothing at all with `--dry-run`.

| Status | Code | When |
|---|---|---|
| 0 | — | it ran, or there was nothing to remove |
| 2 | — | a usage error, including a `--keep-days` that is not a whole number |
| 6 | `E_STORE_IO`, `E_STORE_VERSION` | the store cannot be read or removed from, or was written by a newer lodi |
| 9 | `E_STORE_PERM` | there is no usable per-user store |

## How a command finds your config

`import`, `switch`, `update`, `pin` and `unpin` act on your config, the folder that holds
`host.toml`, `home.toml` and `lodi.lock`. Each takes the first of these:

1. the path or URL you type, such as `lodi switch ~/dotfiles`. A relative path starts at the
   current folder.
2. `LODI_REPO`, when it is set and not empty.
3. the remembered path, in `${XDG_STATE_HOME:-~/.local/state}/lodi/config-path`.
4. `~/.config/lodi`.

```sh
LODI_REPO=~/dotfiles lodi switch --dry-run
```

A path or URL you type is remembered once the command worked, so the next command finds it
without one. Nothing else is remembered. When none of the four is there, the command stops with
`E_NO_MANIFEST` and says how to name one.

The folder must be a config: it holds `lodi.lock`, `config.toml`, `host.toml` or `home.toml`.
A folder holding a project's `lodi.toml` stops the command. So does a path inside a config,
such as `~/dotfiles/files`: the error names the config's root to type instead. The folder and
every folder above it must belong to you or root, and no one else may write to them.

`lodi import` is the one command that may name a folder that does not exist yet. It makes it.
`lodi update` in a project folder, with no path typed, updates that project's `lodi.lock`
instead. A config with several hosts lists them in `config.toml`, as
[the schema reference](SCHEMAS.md#the-config-index) shows.

### Recipes of your own

A tool name resolves through `recipes/NAME.toml` at your config's root before the built-in
recipe of that name, for `switch`, `update`, `pin` and `unpin`. `lodi shell`, `develop`, `run`
and `search` never look for a config, and use only the built-in recipes.
[The recipe guide](../catalogue/README.md#a-recipe-of-your-own) has the format and the rules.

## Where lodi keeps state and logs

| Path | What it holds |
|---|---|
| `${XDG_STATE_HOME:-~/.local/state}/lodi/config-path` | the remembered config path |
| `${XDG_STATE_HOME:-~/.local/state}/lodi/logs/` | one log per run of `import`, `switch` and `update` |
| `$LODI_HOME`, by default `~/.local/share/lodi` | the store, the caches and the records of your home |
| `$LODI_HOME/trust.json` | the projects you trusted |
| `/etc/lodi/` and `/var/lib/lodi/host/` | what a switch did on the machine, under the root it changed |

A log is named for the time and the command, such as `2026-09-25T10-00-00Z-switch.log`. It holds
every step, every command run and every output line. lodi keeps the 20 newest. A failed step
prints its last lines and the path of the full log. [The schema reference](SCHEMAS.md) shows
each record.

## Under sudo or doas

Under `sudo`, lodi acts for the person who typed it, the user `SUDO_UID` names. Under `doas` it
reads `DOAS_USER` the same way, when `SUDO_UID` is not set. That user's home in the password
database replaces `HOME`, and `XDG_STATE_HOME` is not read.

```sh
sudo lodi switch
```

So `sudo lodi switch` finds your config, not root's, and runs your home part as you. Every file
it writes into your config, state or logs belongs to you. A `SUDO_UID` or `DOAS_USER` the
password database does not name stops with `E_CONFIG`.

Without `sudo`, `lodi import` and `lodi switch` ask for root themselves when the host part needs
it. They use `sudo`, `doas` or `run0`, whichever is on `PATH`, and need a terminal to ask on.
Otherwise they stop with `E_NEED_ROOT`. A plain `sudo` drops variables such as `LODI_REPO`, and
`sudo -E` passes them on.

## Progress output

`import`, `switch` and `update` show their steps on standard error as they run. On a terminal,
each finished step keeps one line. The step in progress is one live line with a count and the
item it works on. Without a terminal, or with `TERM=dumb`, each step prints one plain line
when it starts and one when it ends:

```text
[1/2] look up host.toml
[1/2] look up host.toml: done, 1.2s
```

`-v` also prints every output line of the commands a step runs. The log holds
those lines either way.

## Environment variables

| Variable | What it does |
|---|---|
| `LODI_REPO` | the config a command uses when you type no path or URL, [as above](#how-a-command-finds-your-config) |
| `LODI_HOME` | where lodi keeps its store, caches and records for your user, by default `$XDG_DATA_HOME/lodi`, else `~/.local/share/lodi` |
| `LODI_TRUST` | `1` allows one run of `lodi develop` or `lodi run` on an untrusted project, as `--trust` does; any other value is ignored |
| `LODI_FETCH_REWRITE` | sends downloads to a mirror (below) |
| `LODI_FETCH_ATTEMPTS` | how many times a download that failed in passing is tried, 1 to 10 |
| `LODI_HOST_REQUIRE_ROOT` | `1` stops `import`, `switch`, `update`, `pin` and `unpin` with `E_HOST_ROOT_REQUIRED` unless they are given `--root DIR`, for test machines |
| `XDG_STATE_HOME`, `XDG_DATA_HOME`, `XDG_CONFIG_HOME` | move the folders [above](#where-lodi-keeps-state-and-logs) |
| `SUDO_UID`, `DOAS_USER` | the person lodi acts for under `sudo` or `doas` ([above](#under-sudo-or-doas)) |
| `TERM` | `dumb` turns the live progress line off |

`LODI_FETCH_REWRITE` holds `<prefix>=<replacement>` pairs separated by `;`. A request whose URL
starts with a prefix goes to the replacement instead:

```sh
LODI_FETCH_REWRITE='https://github.com/=http://127.0.0.1:8080/github/' lodi develop
```

A prefix must be an HTTPS URL. A replacement must be HTTPS, or plain HTTP to `127.0.0.1`,
`[::1]` or `localhost`. Anything else stops with `E_CONFIG` before any request. Locks keep the
original URLs and every recorded hash is still checked, but a first resolve trusts what the
mirror serves ([SECURITY.md](../SECURITY.md#known-limitations-what-is-trusted-without-a-signature)).
A plain `sudo` drops the variable, and `sudo -E` passes it to the host commands.

## Exit statuses

The same table is in [the error reference](ERRORS.md).

| Status | Meaning |
|---|---|
| 0 | success |
| 2 | usage: an unknown command or flag, a flag without its value, or an argument the command does not take. There is no error code. |
| 3 | the manifest is missing, cannot be read, is not valid TOML, or asks for something this lodi does not support. Nothing was written or run. |
| 4 | resolution: no recipe, no matching version, or a package index or snapshot lodi cannot use |
| 5 | download and integrity: a download failed, or what arrived is not what the lock records |
| 6 | build and store: a download cannot be unpacked, stored or turned into an image |
| 7 | runtime: the environment cannot be entered, or a program it needs is missing |
| 8 | a switch stopped part way or did not start: an action failed, an interrupted switch is unclear, or a managed file was edited by hand. Nothing is undone. |
| 9 | permission: no usable store, a machine lodi may not manage yet, a missing privilege, another switch running, or a host command without `--root` where one is required |
| 10 | the lock is missing, cannot be read, or is out of date |
| 11 | trust: the project's tasks or manifest are not trusted yet, or you answered no |

## Names 2.0 removed

A 1.x command stops with exit status 2 and names what to use instead. It runs nothing:

```sh
lodi host plan
```

```text
lodi: usage: `lodi host plan` is gone in lodi 2.0; use `lodi switch --dry-run`
```

| 1.x | Use instead |
|---|---|
| `lodi plan`, `lodi host plan` | `lodi switch --dry-run` |
| `lodi apply`, `lodi host apply` | `lodi switch` |
| `lodi host arm` | `lodi import`, which asks once whether lodi may manage this machine |
| `lodi host import` | `lodi import`, which copies this machine and your home |
| `lodi host versions PKG` | `lodi pin PKG` |
| `lodi host pin PKG --to V` | `lodi pin PKG --to V` |
| `lodi host unpin PKG` | `lodi unpin PKG` |
| `lodi home init`, `lodi home import` | `lodi import --home` |
| `lodi home plan`, `lodi home status` | `lodi switch --dry-run`, or `lodi switch --home --dry-run` for your home only |
| `lodi home apply` | `lodi switch --home` |
| `lodi boot confirm` | nothing to confirm: `lodi switch` makes a boot change at once |
| `lodi lock`, `lodi lock --check` | `lodi develop` and `lodi run` lock by themselves, and `lodi update` moves a project's pins |
| `lodi trust`, `lodi trust --revoke` | `lodi develop` and `lodi run` ask on first run. Without a terminal, pass `--trust` or set `LODI_TRUST=1` |
| `lodi info TOOL` | `lodi search TOOL` |

Any other word after `host`, `home` or `boot` stops the same way. 2.0 reads no 1.x lock or
record either, as [the schema reference](SCHEMAS.md#when-a-lodi-meets-a-file-it-does-not-read)
says.

## What 2.0 promises

- **Commands and flags of 2.0 keep working for all of 2.x.** They keep their names, arguments
  and meanings.
- **A minor release may add commands and flags.** A new flag never changes what a command does
  without it.
- **Help works at every level.** `lodi help COMMAND`, and `--help` or `-h` after any command,
  print help and exit 0.
- **A bare `lodi` orients.** It prints where to start and exits 0.
- **A command or flag is removed only after a warning.** For at least one minor release, using
  it prints `W_DEPRECATED` on standard error, with what to use instead.
- **Exit statuses keep their meaning.** A new code may be added to an existing status.
- **A warning never changes the exit status.**
- **What 2.0 writes, a later 2.x reads.** [The schema reference](SCHEMAS.md) lists each file.

There is no `--json` output yet.

## Deprecations

No command, flag or manifest table is deprecated.

| Surface | Announced in | May be removed in | Use instead |
|---|---|---|---|

`[files]` in `home.toml` stops with an error (`E_UNKNOWN_BLOCK`). Declare each file in
`[home.file]` instead, with `text` where it said `content`.

### How a deprecation is announced

A deprecated command or flag prints one line on standard error, then runs as before:

```text
lodi: warning W_DEPRECATED: `lodi <surface>` is deprecated since <X.Y> and may be removed in <X.Y>;
  use `lodi <replacement>` instead
```

The `Surface` column above names what you typed, in one of these forms:

- the first argument alone, such as `init` or `--help`
- the first two arguments, when the second does not start with `-`, such as `help init`
- each argument that starts with `-`, alone and after the first argument, such as `--force` and
  `init --force`

Arguments after `--` are your own command and never count. A table of a manifest is written as
the file's name and the table, such as ``home.toml `[files]` ``. The command that reads the file
prints the line.
