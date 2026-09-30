# The command line

Every `lodi` command, with an example, its flags and the exit statuses it can end with. For a
task from start to end, read [the guide](GUIDE.md). For what an error code means and what to do
next, read [the error reference](ERRORS.md).

| You want to | Commands |
|---|---|
| see the version or the help | [`lodi --version`](#lodi---version), [`lodi --help`](#lodi---help), [`lodi help`](#lodi-help) |
| set up and run a project | [`lodi init`](#lodi-init), [`lodi lock`](#lodi-lock), [`lodi trust`](#lodi-trust), [`lodi develop`](#lodi-develop), [`lodi run`](#lodi-run), [`lodi info`](#lodi-info) |
| try a tool, find one, free space | [`lodi shell`](#lodi-shell), [`lodi search`](#lodi-search), [`lodi gc`](#lodi-gc) |
| keep a machine and its homes in one repository | [`lodi import`](#lodi-import), [`lodi plan`](#lodi-plan), [`lodi apply`](#lodi-apply), [`lodi update`](#lodi-update) |
| manage your home directory | [`lodi home init`](#lodi-home-init), [`lodi home plan`](#lodi-home-plan), [`lodi home apply`](#lodi-home-apply), [`lodi home status`](#lodi-home-status), [`lodi home import`](#lodi-home-import) |
| manage this machine | [`lodi host arm`](#lodi-host-arm), [`lodi host import`](#lodi-host-import), [`lodi host plan`](#lodi-host-plan), [`lodi host apply`](#lodi-host-apply) |
| pin packages on this machine | [`lodi host versions`](#lodi-host-versions), [`lodi host pin`](#lodi-host-pin), [`lodi host unpin`](#lodi-host-unpin) |

## How to read a command's entry

Each entry has the same parts: an example with its real output, what the command does, a
**Flag** table and an **Exits** table. The flag table lists every flag the command takes. A flag
with an argument names it, and `Required` says whether the command stops without it. Give each
flag at most once. `lodi` reads no flag after `--`, because what follows is your own command.

Any other flag, a flag without its value, or an argument the command does not take is a usage
error. It exits 2, prints no `E_` code, and reads and writes nothing.

The exits table names the statuses a command can end with and the codes behind them. Status 0 is
success, and 2 is a usage error. `lodi` with no command at all prints the six scope lines of
`lodi --help`'s opening, lines 3 to 8 of the same text, and exits 0. Some examples below come
from a Debian 12 test machine, so they name its packages. `~/demo` stands for your project and
`<hostname>` for your machine.

An output line longer than this page is wrapped onto an indented next line.

### Recipes of your own

A tool name resolves through `recipes/NAME.toml` at your repository's root before the built-in
recipe of that name. The repository is the one the command names, as `lodi home apply ~/lodi`
does. A command that names none uses the current directory once you confirm it, else
`LODI_REPO`:

```sh
cd ~/lodi/web && LODI_REPO=~/lodi lodi lock
```

With no repository, only the built-in recipes are used. `lodi lock`, `shell`, `develop`, `run`,
`search`, `info`, `plan`, `apply`, `update` and the home commands all read the folder the same
way. [The recipe guide](../catalogue/README.md#a-recipe-of-your-own) has the format and the rules.

### `lodi --version`

Print the version and exit.

```sh
lodi --version
```

```text
lodi 1.12.2
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
lodi 1.12.2 — environment and system manager for Ubuntu, Debian, Arch and Fedora

Three independent scopes. Start with the one you want. None needs another:
  this machine  sudo lodi host arm      once: allow lodi to manage this machine
                sudo lodi host import   copy its packages and settings, and no /etc file
  your home     lodi home init          write a starting home.toml
  a project     lodi init               write ./lodi.toml here; then lodi lock
Commands without 'host' or 'home' act on the current directory.
```

The rest of the text gives examples, the command groups and a link to this page. It fits in 80
columns and 40 lines, and it lists only commands that work.

`--help` and `-h` may follow any command or scope. `lodi home --help`, `lodi host apply -h` and
`lodi init --name x --help` print the help of `home`, `host apply` and `init`, and run nothing
else. A bare `lodi home` or `lodi host` prints that scope's help too.

The help is about the first word after `lodi`, or for `home` and `host` the command after it.
Nothing after `--` asks for help, and nothing from `lodi run`'s task name on. So
`lodi run build --help` passes `--help` to the task. A `--help` beside a command that does not
exist, such as `lodi frobnicate --help`, is a usage error.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `-h` | — | no | the same request as `--help`, wherever `--help` may go |

| Status | Code | When |
|---|---|---|
| 0 | — | the help, or the part of it asked for, was printed |
| 2 | — | it was asked about a command that does not exist |

### `lodi help`

Show the help of lodi, of one scope or of one command.

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

Details:
  It also adds .lodi/ to ./.gitignore. --base writes a [container] manifest
  instead of host tools, from one of the bases
  arch:rolling, debian:bookworm, fedora:44 or ubuntu:noble. --force replaces
  an existing ./lodi.toml. It works offline: nothing is downloaded, installed
  or run.
```

`lodi help` alone prints all of `lodi --help`. `lodi help COMMAND` prints what
`lodi COMMAND --help` prints. `COMMAND` is a command such as `init`, a scope such as `host`, or a
scope's command such as `host plan`. It works offline.

| Flag | Argument | Required | What it does |
|---|---|---|---|

| Status | Code | When |
|---|---|---|
| 0 | — | the help, or the part of it asked for, was printed |
| 2 | — | `COMMAND` is no command or scope that `lodi --help` lists |

### `lodi init`

Start a project: write a commented `./lodi.toml` in this directory.

```sh
lodi init
```

```text
wrote ./lodi.toml
added .lodi/ to ./.gitignore
next: lodi lock
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

### `lodi lock`

Pin the project's tools and packages in `./lodi.lock`.

```sh
lodi lock
```

```text
wrote lodi.lock (resolved ripgrep): ripgrep 15.2.0
```

Run it again with nothing changed, and it says so:

```text
lodi.lock is up to date: ripgrep 15.2.0
```

It resolves `./lodi.toml` from metadata only: the container base, that distribution's dated
package indexes and the upstream tools in the catalogue. It downloads no tool and installs or runs
nothing. A manifest with no tools and no base prints `wrote lodi.lock: nothing to lock`. A tool
from [a recipe of your own](#recipes-of-your-own) is stale when its file changes, and only that
tool is resolved again.

`lodi lock --check` writes nothing and exits 10 unless the lock matches the manifest:

```sh
lodi lock --check
```

```text
lodi: error E_LOCK_STALE: lodi.lock is out of date with the manifest
   = ripgrep: locked for `15`, the manifest asks for `14`
   = hint: run `lodi lock`
```

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--check` | — | no | exit 10 unless `./lodi.lock` is there, readable and up to date, and never resolve or write |

**Reads:** `./lodi.toml`, `./lodi.lock`, and the package indexes and upstream metadata over
HTTPS. **Writes:** `./lodi.lock`, except with `--check`, and the download cache under
`$LODI_HOME`. That includes `$LODI_HOME/cache/api/`, which keeps GitHub's answers so an
unchanged one is not sent again. lodi still asks each time and uses a kept answer only when GitHub
says it is unchanged.

| Status | Code | When |
|---|---|---|
| 0 | — | the lock is written or already up to date |
| 2 | — | a usage error |
| 3 | `E_NO_MANIFEST`, `E_SYNTAX`, `E_TYPE`, `E_IDENT`, `E_UNKNOWN_ATTR`, `E_UNKNOWN_BLOCK`, `E_UNSUPPORTED`, `E_MODE`, `E_BLOCK_NOT_ALLOWED`, `E_INLINE_NEEDS_EXACT`, `E_EXCLUDED_CONSTRUCT`, `E_VAR_UNSET`, `E_VERSION` | the manifest is missing, unreadable, or asks for something this version of lodi does not do |
| 4 | `E_NO_RECIPE`, `E_NO_MATCH`, `E_UNSUPPORTED_ARCH`, `E_RECIPE_CTX`, `E_RECIPE_INVALID`, `E_REPO_UNREACHABLE`, `E_SNAPSHOT_TOO_OLD`, `E_SNAPSHOT_FUTURE`, `E_LOCK_VERSION` | no recipe, no matching version, or an index or snapshot lodi cannot use |
| 5 | `E_FETCH`, `E_INSECURE_URL`, `E_NO_CHECKSUM`, `E_HASH_MISMATCH` | metadata could not be downloaded, or what arrived is not what was expected |
| 6 | `E_STORE_IO`, `E_STORE_VERSION` | the lock or the cache could not be written, or the store was written by a newer lodi |
| 9 | `E_STORE_PERM` | there is no usable per-user store |
| 10 | `E_LOCK_STALE` | with `--check`: the lock is missing, unreadable or out of date |

### `lodi trust`

Review the project's tasks and allow them to run.

```sh
lodi trust
```

```text
~/demo/lodi.toml declares 1 task:
  task versions:
    | rg --version
script text hash: sha256:6adff068c4f56b8f403019c836686fc1e2c9b3e05c2c2fe59b9e20694b64ab3c
Trusting authorizes only the task text shown above. Files that text runs or sources (a Makefile,
  project scripts, anything on disk or downloaded) are outside this authorization and are not shown
  or hashed by lodi.
trusted.
```

It shows the task text of `./lodi.toml` with its hash and records your trust. A project with no
tasks that declares tools or a base is shown whole and trusted whole, because its tools come first
on `PATH`. Any change to what was trusted asks again. A project with no tasks, tools or base has
nothing to trust.

Running `lodi trust` is the consent, so it asks nothing. When standard input is not a terminal it
also prints `lodi: trust was recorded without a prompt because standard input is not a terminal`
on standard error. `LODI_TRUST=1` allows one run of `lodi develop` or `lodi run` instead.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--revoke` | — | no | remove the trust recorded for this manifest |

**Reads:** `./lodi.toml` and the trust store in your configuration directory. **Writes:** The
trust store.

| Status | Code | When |
|---|---|---|
| 0 | — | trust was recorded or revoked |
| 2 | — | a usage error |
| 3 | `E_NO_MANIFEST`, `E_SYNTAX`, `E_TYPE`, `E_UNKNOWN_ATTR`, `E_UNKNOWN_BLOCK`, `E_UNSUPPORTED`, `E_CONFIG`, `E_VERSION` | the manifest, or there is no usable configuration directory |
| 6 | `E_STORE_IO` | the trust store could not be written |

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

It downloads what `./lodi.lock` pins, then runs the command after `--` and exits with its status.
With nothing after `--` it opens an interactive shell. It never resolves, so a missing or stale lock
stops with `E_LOCK_STALE`. Run `lodi lock` first. The warning `W_SHADOWS_HOST` names each
command that runs in place of one on your `PATH`.

The project must be trusted first. Until `lodi trust` has recorded it, `lodi develop` asks
`[y/N]` on a terminal and otherwise stops:

```text
lodi: error E_TRUST_REQUIRED: ~/demo/lodi.toml declares task text that has not been trusted; nothing
  was realized or run
   = hint: run `lodi trust` to review and record it, or set LODI_TRUST=1 for one invocation
```

While the command runs, lodi passes `SIGTERM` and `SIGHUP` on to it. A command ended by signal
`n` exits `128 + n`, so 143 after `SIGTERM`. Ctrl-C from the terminal reaches the command
directly, and lodi then exits 130.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--no-nest` | — | no | stop when you are already inside another lodi environment |
| `--` | `COMMAND [ARGS…]` | no | run that command instead of a shell, with its arguments unchanged |

**Reads:** `./lodi.toml`, `./lodi.lock`, the pinned downloads over HTTPS, `$LODI_HOME` and the
trust store. **Writes:** `$LODI_HOME`: the download cache, the unpacked tools and a record of the
session. In a container, the project directory and `$HOME` are mounted read-write.

| Status | Code | When |
|---|---|---|
| 0 | — | the command or shell exited cleanly, or else its own status |
| 2 | — | a usage error, including `--` with no command after it |
| 3 | `E_NO_MANIFEST`, `E_SYNTAX`, `E_TYPE`, `E_IDENT`, `E_UNKNOWN_ATTR`, `E_UNKNOWN_BLOCK`, `E_UNSUPPORTED`, `E_MODE`, `E_BLOCK_NOT_ALLOWED`, `E_EXCLUDED_CONSTRUCT`, `E_VAR_UNSET`, `E_VERSION` | the manifest |
| 4 | `E_LOCK_VERSION`, `E_RECIPE_CTX`, `E_RECIPE_INVALID`, `E_UNSUPPORTED_ARCH` | the lock was written by a newer lodi, or a recipe no longer fits |
| 5 | `E_FETCH`, `E_INSECURE_URL`, `E_NO_CHECKSUM`, `E_HASH_MISMATCH` | a pinned download failed, or does not match the lock |
| 6 | `E_STORE_IO`, `E_STORE_VERSION`, `E_ARCHIVE_FORMAT`, `E_ARCHIVE_UNSAFE`, `E_ARCHIVE_EMPTY`, `E_BIN_MISSING`, `E_TREE_UNSUPPORTED`, `E_LAYER_BUILD`, `E_CLOSURE_DRIFT` | a download cannot be unpacked, stored or turned into an image |
| 7 | `E_NO_RUNTIME`, `E_NESTED`, `E_ENTER`, `E_SHELL_NOT_FOUND` | the environment cannot be entered, or Podman is missing |
| 9 | `E_STORE_PERM` | there is no usable per-user store |
| 10 | `E_LOCK_STALE` | `./lodi.lock` is missing, unreadable or does not match the manifest |
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
as `lodi develop` does. A task the manifest does not declare is a usage error, reported before
the lock or trust is read:

```text
lodi: error: lodi.toml has no task `tset`
   = its tasks: versions
```

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--no-nest` | — | no | stop when you are already inside another lodi environment |

**Reads:** As `lodi develop`. **Writes:** As `lodi develop`.

| Status | Code | When |
|---|---|---|
| 0 | — | the task exited cleanly, or else its own status |
| 2 | — | a usage error, including no task, or a task the manifest does not declare |
| 3 | `E_NO_MANIFEST`, `E_SYNTAX`, `E_TYPE`, `E_IDENT`, `E_UNKNOWN_ATTR`, `E_UNKNOWN_BLOCK`, `E_UNSUPPORTED`, `E_MODE`, `E_BLOCK_NOT_ALLOWED`, `E_EXCLUDED_CONSTRUCT`, `E_VAR_UNSET`, `E_VERSION` | the manifest |
| 4 | `E_LOCK_VERSION`, `E_RECIPE_CTX`, `E_RECIPE_INVALID`, `E_UNSUPPORTED_ARCH` | the lock or a recipe, as for `lodi develop` |
| 5 | `E_FETCH`, `E_INSECURE_URL`, `E_NO_CHECKSUM`, `E_HASH_MISMATCH` | a pinned download, as for `lodi develop` |
| 6 | `E_STORE_IO`, `E_STORE_VERSION`, `E_ARCHIVE_FORMAT`, `E_ARCHIVE_UNSAFE`, `E_ARCHIVE_EMPTY`, `E_BIN_MISSING`, `E_TREE_UNSUPPORTED`, `E_LAYER_BUILD`, `E_CLOSURE_DRIFT` | unpacking and storing, as for `lodi develop` |
| 7 | `E_NO_RUNTIME`, `E_NESTED`, `E_ENTER` | the environment cannot be entered |
| 9 | `E_STORE_PERM` | there is no usable per-user store |
| 10 | `E_LOCK_STALE` | the lock is missing, unreadable or out of date |
| 11 | `E_TRUST_REQUIRED`, `E_DECLINED` | the task text is not trusted, or you answered no |

### `lodi info`

Summarize this project, or show what lodi knows about one tool.

```sh
lodi info
```

```text
project: demo
mode:    host tools
tools:
  ripgrep  15          15.2.0  art-33e15bcf1624b25cdd2a55813a47a2f9-ripgrep-15.2.0
lock:    fresh
```

With a tool's name it shows its recipe instead:

```sh
lodi info jq
```

```text
jq
  description:  A lightweight and flexible command-line JSON processor
  homepage:     https://jqlang.org
  strategy:     github_releases
  upstream:     https://github.com/jqlang/jq/releases
  bin:          bin/jq
  path:         bin
  recipe:       jq.toml (sha256:85cb23c0f68bdc3cbaef77a6687e7b17d900919e8f95c052c9c2e6aed8ed2aac)
```

It works offline and writes nothing. With no argument it needs `./lodi.toml`, and the lock is
`fresh`, `stale` or `missing`. A tool with no recipe is marked on its line with the nearest
names, such as ``(no recipe `pyhton` in the catalogue; nearest names: python)``, and the summary
still exits 0.

| Flag | Argument | Required | What it does |
|---|---|---|---|

For a recipe of your own, the `recipe:` line names the file, such as
`~/lodi/recipes/kubectl-convert.toml`, and a `replaces:` line names the built-in recipe it hides.
The file is checked, and a broken one stops with `E_RECIPE_INVALID`.

**Reads:** The built-in catalogue, and with no argument `./lodi.toml` and `./lodi.lock`.
**Writes:** Nothing.

| Status | Code | When |
|---|---|---|
| 0 | — | the summary or the recipe was printed |
| 2 | — | a usage error, including a second tool name and any flag |
| 3 | `E_NO_MANIFEST`, `E_SYNTAX`, `E_TYPE`, `E_UNKNOWN_ATTR`, `E_UNKNOWN_BLOCK`, `E_UNSUPPORTED`, `E_VERSION` | there is no `./lodi.toml`, or it cannot be read |
| 4 | `E_NO_RECIPE` | the catalogue has no recipe of that name |
| 10 | `E_LOCK_STALE` | `./lodi.lock` cannot be read by this version of lodi |

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

Your own recipes are listed under their own heading, `recipes  (~/lodi/recipes)`, after the
catalogue. A built-in recipe your file replaces is not listed. A file that does not read or parse
is skipped with `W_RECIPE_SKIPPED`.

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

### `lodi import`

Copy this machine into a repository you keep, with a `home.toml` for you.

These four commands are tested in 1.5.0 on Debian 12, Ubuntu 24.04 and Arch. The tests ran
import, apply of every home, update, the move to one lock and `git revert` on each. Applying a
`github:` URL was tested against a git server on the same machine, not GitHub.

```sh
sudo lodi import ~/lodi
```

```text
wrote ~/lodi/<hostname>/home/<login>/home.toml
wrote ~/lodi/<hostname>/host.toml: 35 package(s) declared, 0 not captured, no file copied from
  /etc; no package and no file outside ~/lodi/<hostname> changed
read it first: NOT CAPTURED lists what was left out and how to declare it
next: sudo lodi host plan ~/lodi (writes nothing)
W_UNCAPTURED: /etc/sudoers is not captured: it is on lodi's list of paths that are never read,
  because it carries an identity, a credential, a privilege or lodi's own state
```

**Before you share the repository:** `files/` holds this machine's changed configuration files,
copied byte for byte, with any secret lodi does not recognise. See
[`lodi host import`](#lodi-host-import).

It is `lodi host import` into a repository. It writes `~/lodi/<hostname>/`, or
`~/lodi/NAME/` with `--host NAME`, and a commented `home/<login>/home.toml` for you. Every file
belongs to the repository's owner, also under `sudo`. It runs no `git`, writes no lock and
applies nothing.

Every repository command finds its repository in this order:

1. The `SOURCE` you name, a path.
2. Else the current directory, when it holds a repository and you confirm it. For
   `lodi import` an empty directory counts too, hidden entries such as `.git` aside.
3. Else the directory `LODI_REPO` names.
4. Else it stops with `E_NO_MANIFEST`, and the hint names these ways.

A repository holds a `host.toml`, or a directory with one, and no project `lodi.toml`. Without a
terminal lodi cannot ask, so the current directory needs `--yes`. A URL stops with
`E_UNSUPPORTED`: import into a checkout.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--host` | `NAME` | no | the host directory to write instead of the hostname's |
| `--root` | `DIR` | no | the root to read, instead of `/` |
| `--yes` | — | no | use the current directory without asking: its repository, or an empty directory |

| Status | Code | When |
|---|---|---|
| 0 | — | imported |
| 2 | — | a usage error, such as an unknown flag or a second `SOURCE` |
| 3 | `E_NO_MANIFEST`, `E_UNSUPPORTED`, `E_PATH_ESCAPE`, `E_CONFIG` | no repository was found, a URL was given, or as for `host import` |
| 9 | `E_NEED_ROOT`, `E_HOST_NOT_ARMED`, `E_HOST_ROOT_REQUIRED` | as for `host import` |
| 11 | `E_DECLINED` | the current directory was not confirmed, or lodi could not ask |

### `lodi plan`

Show what `lodi apply` would change: the machine, then each home.

```sh
sudo lodi plan ~/lodi
```

It shows the host part as [`lodi host plan`](#lodi-host-plan) does. Then it shows each
`home/<login>/` in login order, planned as that user. It changes nothing and writes no lock.

The repository is found as for [`lodi import`](#lodi-import). It may also be a git URL, or a
`github:`, `gitlab:` or `codeberg:` name. Without a terminal, the current directory needs
`--yes`:

```text
lodi: error E_DECLINED: the current directory ~/lodi holds a repository, and with no terminal `lodi
  plan` cannot ask to use it; nothing was read or written
   = hint: pass it (`lodi plan ~/lodi`), give --yes, or set LODI_REPO
```

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--host` | `NAME` | no | the host of the repository to plan instead of the hostname's |
| `--root` | `DIR` | no | the root to read, instead of `/` |
| `--yes` | — | no | use the current directory's repository without asking |
| `--ref` | `NAME` | no | with a URL: the branch or tag to use instead of `HEAD` |
| `--rev` | `COMMIT` | no | with a URL: the commit, by its 40 hexadecimal digits |
| `--refresh` | — | no | with a URL: look up the branch again instead of the locked commit |

| Status | Code | When |
|---|---|---|
| 0 | — | planned |
| 2 | — | a usage error, including a URL flag without a URL, `--ref` with `--rev`, or `--rev` with `--refresh` |
| any | as for `host plan` | the host part. A home's failure is reported after the others are planned, and the status is the first failure's |
| 11 | `E_DECLINED` | as for `lodi import` |

### `lodi apply`

Make this machine and each home match your repository.

**Before you apply:** on Arch the host part upgrades the whole machine with `pacman -Syu`. There
is no rollback.

```sh
sudo lodi apply ~/lodi
```

```text
nothing to do
home lodi
created   flake-verbs-file          0600
1 file: 1 create
home lodi-flake-b
created   flake-verbs-file          0600
1 file: 1 create
home lodi-flake-gone skipped: the root's /etc/passwd names no lodi-flake-gone
```

It applies the host as root, as [`lodi host apply`](#lodi-host-apply) does. Then it applies each
`home/<login>/` in login order, each in a process that runs as that user.

- lodi reads and checks the host and every home first. A home that cannot be read stops the apply
  with nothing written.
- A login the machine does not know is skipped, with a line that says so.
- A home that fails is reported, and the next home still runs. The exit status is the first
  failure's.
- A home's tools are pinned in its section of the repository's `lodi.lock`. From a git URL that
  lock must be there already, because lodi never writes into a downloaded tree.
- Before any home runs, lodi pins every home's new tools as root, as `lodi update` does.
  `lodi.lock` stays yours.
- An old `pins.lock` or `home.lock` that matches its section of `lodi.lock` is removed.
- The repository's `recipes/` folder is read with everything else, before anything changes. A
  folder that others can write stops the apply with `E_PATH_ESCAPE`.

The repository and the URL flags are those of [`lodi plan`](#lodi-plan). On `/` it needs root.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--host` | `NAME` | no | the host of the repository to apply instead of the hostname's |
| `--root` | `DIR` | no | the root to apply to, instead of `/` |
| `--yes` | — | no | use the current directory's repository without asking |
| `--ref` | `NAME` | no | with a URL: as for `plan` |
| `--rev` | `COMMIT` | no | with a URL: as for `plan` |
| `--refresh` | — | no | with a URL: as for `plan` |

| Status | Code | When |
|---|---|---|
| 0 | — | applied, or nothing to do |
| 2 | — | a usage error, as for `plan` |
| any | as for `host apply` and `home apply` | the host part, then the first home that failed |
| 10 | `E_LOCK_STALE` | a downloaded home's tools are not locked, or `lodi.lock` and a different `pins.lock` both lock one host |
| 11 | `E_DECLINED` | as for `lodi import` |

### `lodi update`

Move your repository's snapshot to the last published day and lock it.

```sh
sudo lodi update ~/lodi
```

```text
[host] snapshot 2025-03-01T00:00:00Z -> 2026-09-25T00:00:00Z
pv: 2025-03-01 kept at 1.6.20-1
migrated ~/lodi/<hostname>/pins.lock into ~/lodi/lodi.lock
wrote ~/lodi/lodi.lock
wrote ~/lodi/<hostname>/host.toml
next: sudo lodi apply ~/lodi
```

It moves the repository's `lodi.lock` forward for this machine's host and its homes, like
`nix flake update`. It applies nothing.

- A `[host] snapshot` moves to the newest published day, and the pins are looked up again.
- A host without a snapshot gets none, and a pin you set in `[packages.pin]` stays.
- Each key from `signed_by_url` is downloaded, checked against `signed_by_sha256` and recorded
  in the lock.
- Each home's tools are locked again. An entry that still matches is kept.
- `host.toml` and `lodi.lock` change together or not at all.
- Old `pins.lock` and `home.lock` files move into `lodi.lock` first, one line each.
- Under `sudo` it writes as the repository's owner.

A URL stops with `E_UNSUPPORTED`: update a checkout, commit and push. The repository is found as
for [`lodi import`](#lodi-import).

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--host` | `NAME` | no | the host of the repository to update instead of the hostname's |
| `--root` | `DIR` | no | the root to read, instead of `/` |
| `--yes` | — | no | use the current directory's repository without asking |

| Status | Code | When |
|---|---|---|
| 0 | — | updated, or already up to date |
| 2 | — | a usage error |
| 3 | `E_NO_MANIFEST`, `E_UNSUPPORTED`, `E_CONFIG` | no repository was found, a URL or the host in place was given, or a project's `lodi.toml` is at the top |
| 4, 5 | as for `host pin` | looking up the pins, or downloading a key (`E_FETCH`, `E_HASH_MISMATCH`) |
| 6 | `E_STORE_IO` | a file could not be written, and nothing changed |
| 9 | `E_SYSTEM_BUSY`, `E_HOST_NOT_ARMED`, `E_HOST_ROOT_REQUIRED` | another command is writing the repository, or as for `host pin` |
| 10 | `E_LOCK_STALE` | `lodi.lock` and a `pins.lock` both lock one host |
| 11 | `E_DECLINED` | as for `lodi import` |

### `lodi home init`

Write a starting `home.toml` for this home directory.

```sh
lodi home init
```

```text
wrote ~/.config/lodi/home.toml; next: uncomment what you want, then lodi home plan
```

`lodi home init` starts the home scope, and `lodi home import` is its 1.0 name. It reads no file
in your home directory and copies nothing. The file has one live table, `[home] version = "1"`,
and everything else is commented out:

- a `[programs.<name>]` stub for each program on your `PATH` that lodi has a module for, with
  example values only
- a `[tools]` block with the catalogue's tools that are on your `PATH`
- a `[home.file."…"]` example
- a list of the files the program modules would write that exist already, checked by name only

It is the same file each time on an unchanged machine: no date, version, user, host or absolute
path. It makes no network request. Planned as it is, it does nothing, so uncomment what you want
first. With `--stdout` it prints the file and writes nothing:

```sh
lodi home init --stdout
```

```text
# Generated by `lodi home import`. It copied nothing and read no file in your home
# directory: every table below is commented out and holds no value from this machine,
# so no setting, secret or address of yours is in it. It names no user, no machine and
# no path outside the home directory, and it carries no date and no lodi version.
```

With `SOURCE`, it writes the file into that home directory, or into your home in a host of that
repository. `--host NAME` picks the host.

**Under `sudo`.** Every home command acts on the home `HOME` names, and under `sudo` that is
root's. When `SUDO_UID` names another user, the command first prints this line on standard error
and then runs as usual:
``lodi: warning W_HOME_SUDO: this runs under sudo as uid 0, so it acts on the home HOME names for
uid 0, not on the home of uid 1000 who ran sudo; the home scope needs no root: run lodi home without
sudo``.
Run home commands without `sudo`.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--out` | `DIR` | no | write `DIR/home.toml` instead. `DIR` is inside your home directory, as a relative path or an absolute path below it |
| `--stdout` | — | no | print the file and write nothing. Not with `--out` or `--force` |
| `--force` | — | no | replace an existing `home.toml` there |
| `--host` | `NAME` | no | with a `SOURCE` that holds several hosts: the host to write into |

**Reads:** No file's content. `PATH`, and whether the files the modules write exist. **Writes:**
`home.toml` at mode 0644 in `~/.config/lodi`, or in `DIR` with `--out`. Nothing with `--stdout`.

| Status | Code | When |
|---|---|---|
| 0 | — | the file was written or printed |
| 2 | — | a usage error, including `--out` without its value, a flag given twice, `--host` without a `SOURCE`, and `--stdout` with `--out` or `--force` |
| 3 | `E_CONFIG` | there is no usable `HOME` |
| 3 | `E_EXISTS` | `home.toml` is already there and `--force` was not given. The message names the file |
| 3 | `E_PATH_ESCAPE` | `--out` is not inside your home directory, or a path on the way cannot be trusted |
| 6 | `E_STORE_IO` | `--out` is inside a directory lodi keeps for itself, or the file could not be written |
| 11 | `E_DECLINED` | with no `SOURCE`, your home was last applied from a `SOURCE`. Name it again |

### `lodi home import`

The 1.0 name of `lodi home init`, which does exactly the same.

```sh
lodi home import --stdout
```

```text
# Generated by `lodi home import`. It copied nothing and read no file in your home
# directory: every table below is commented out and holds no value from this machine,
# so no setting, secret or address of yours is in it. It names no user, no machine and
# no path outside the home directory, and it carries no date and no lodi version.
```

`lodi home import` is its 1.0 name, and it stays for all of 1.x with no warning. It takes the
same flags, writes the same files and exits the same way as
[`lodi home init`](#lodi-home-init). A usage error names the command you typed.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--out` | `DIR` | no | as for `lodi home init` |
| `--stdout` | — | no | as for `lodi home init` |
| `--force` | — | no | as for `lodi home init` |
| `--host` | `NAME` | no | as for `lodi home init` |

| Status | Code | When |
|---|---|---|
| 0 | — | the file was written or printed |
| 2 | — | a usage error, as for `lodi home init` |
| 3 | `E_CONFIG`, `E_EXISTS`, `E_PATH_ESCAPE` | as for `lodi home init` |
| 6 | `E_STORE_IO` | as for `lodi home init` |
| 11 | `E_DECLINED` | as for `lodi home init` |

### `lodi home plan`

Show what `lodi home apply` would change. It changes nothing.

```sh
lodi home plan
```

```text
create    .config/demo/hello.txt    0644
1 file: 1 create
```

It lists what an apply would do to each file `~/.config/lodi/home.toml` declares, in path order
with the mode of each. It writes nothing, not even the configuration directory. Each line starts
with what would happen:

| Word | What an apply would do |
|---|---|
| `create` | write a file that is not there |
| `seed` | write a `state = "seed"` file once, and never compare or rewrite it later |
| `adopt` | record a file that already has the declared bytes and mode, writing nothing |
| `update` | rewrite a file lodi manages with its new declared bytes |
| `replace` | back up a file lodi did not write, and replace it |
| `restore`, `remove`, `keep` | handle a file whose entry you deleted |
| `drift` | nothing: the file was edited by hand, and the apply stops |
| `unchanged` | nothing |

A `[files]` entry, the 1.0 table, shows only `replace`, `unchanged` and the three words for a
deleted entry. The keys of `home.toml` are in [the home scope page](scopes/home.md). An
`XDG_CONFIG_HOME` outside `HOME` stops with `E_CONFIG` before anything is planned.

`lodi home plan SOURCE` plans a home directory you name, or your home in a host of that
repository. With several hosts, `--host NAME` picks one:

```sh
lodi home plan ~/lodi --host laptop
```

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--host` | `NAME` | no | with a `SOURCE` that holds several hosts: the host whose home to plan |

**Reads:** `~/.config/lodi/home.toml`, what lodi recorded about your home, and the declared files.
**Writes:** Nothing.

| Status | Code | When |
|---|---|---|
| 0 | — | the plan was printed |
| 2 | — | a usage error, including a second `SOURCE` and `--host` without a `SOURCE` |
| 3 | `E_NO_MANIFEST`, `E_SYNTAX`, `E_TYPE`, `E_IDENT`, `E_UNKNOWN_ATTR`, `E_UNKNOWN_BLOCK`, `E_UNSUPPORTED`, `E_ATTR_CONFLICT`, `E_DUP_RESOURCE`, `E_EXCLUDED_CONSTRUCT`, `E_VAR_UNSET`, `E_PATH_ESCAPE`, `E_CONFIG`, `E_VERSION` | the manifest, the configuration directory, or a path outside your home |
| 6 | `E_STORE_IO` | a file could not be read |
| 9 | `E_STORE_PERM` | the directory a declared file would go in cannot be written |
| 11 | `E_DECLINED` | with no `SOURCE`, your home was last applied from a `SOURCE`. Name it again |

### `lodi home apply`

Write the files and tools your `home.toml` declares.

```sh
lodi home apply
```

```text
created   .config/demo/hello.txt    0644
1 file: 1 create
```

It creates the declared files with their modes. It keeps one copy of a file that was there before
lodi took the path over. It restores, deletes or keeps a file whose entry you removed. It checks
everything before it writes anything.

It never removes a directory or changes an owner or group. It never edits your shell's files:
`[programs.bash]` and the others write files of lodi's own.

A file edited by hand since lodi wrote it stops the apply, and nothing is written:

```text
lodi: warning W_DRIFT: .config/demo/hello.txt has changed since lodi wrote it (recorded 5891b5b5,
  now 75ecc33b)
lodi: error E_DRIFT: 1 managed file has changed since lodi wrote it; nothing was applied
   = .config/demo/hello.txt
   = hint: run lodi home status, then lodi home apply --overwrite-drift to take them back
```

With `SOURCE` it applies that home, and keeps `home.lock` beside its `home.toml`. Run it as the
user who owns the home.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--overwrite-drift` | — | no | take back a file edited by hand, and keep the edited bytes as a backup |
| `--locked` | — | no | use `home.lock` as it is, and stop if it is missing or out of date |
| `--host` | `NAME` | no | with a `SOURCE` that holds several hosts: the host whose home to apply |

**Reads:** `~/.config/lodi/home.toml` and `home.lock`, what lodi recorded about your home and its
backups, `$LODI_HOME`, and the downloads for `[tools]` over HTTPS. **Writes:** The declared files,
and lodi's own files for your home: `home.lock`, the backups, the `profile/bin` directory,
`profile.sh` and `state.json`. It also writes the unpacked tools under `$LODI_HOME`.

| Status | Code | When |
|---|---|---|
| 0 | — | the apply ran, or there was nothing to do |
| 2 | — | a usage error |
| 3 | `E_NO_MANIFEST`, `E_SYNTAX`, `E_TYPE`, `E_IDENT`, `E_UNKNOWN_ATTR`, `E_UNKNOWN_BLOCK`, `E_UNSUPPORTED`, `E_ATTR_CONFLICT`, `E_DUP_RESOURCE`, `E_EXCLUDED_CONSTRUCT`, `E_VAR_UNSET`, `E_PATH_ESCAPE`, `E_CONFIG`, `E_INLINE_NEEDS_EXACT`, `E_VERSION` | the manifest, the configuration directory, or a path outside your home |
| 4 | `E_NO_RECIPE`, `E_NO_MATCH`, `E_UNSUPPORTED_ARCH`, `E_RECIPE_CTX`, `E_RECIPE_INVALID` | a `[tools]` entry cannot be looked up |
| 5 | `E_FETCH`, `E_INSECURE_URL`, `E_NO_CHECKSUM`, `E_HASH_MISMATCH` | a download failed, or does not match |
| 6 | `E_STORE_IO`, `E_STORE_VERSION`, `E_ARCHIVE_FORMAT`, `E_ARCHIVE_UNSAFE`, `E_ARCHIVE_EMPTY`, `E_BIN_MISSING`, `E_TREE_UNSUPPORTED` | a download cannot be unpacked or stored |
| 8 | `E_DRIFT` | a file was edited by hand and `--overwrite-drift` was not given. Nothing at all was written |
| 9 | `E_STORE_PERM` | a declared file's directory cannot be written, or there is no usable store |
| 10 | `E_LOCK_STALE` | `--locked`, and `home.lock` is missing or out of date |
| 11 | `E_DECLINED` | with no `SOURCE`, your home was last applied from a `SOURCE`. Name it again |

### `lodi home status`

Report whether each managed file still matches `home.toml`.

```sh
lodi home status
```

```text
ok           .config/demo/hello.txt    0644
1 path: 1 ok
```

After the file was edited by hand:

```text
lodi: warning W_DRIFT: .config/demo/hello.txt has changed since lodi wrote it (recorded 5891b5b5,
  now 75ecc33b)
drift        .config/demo/hello.txt    0644    (edited by hand since lodi wrote it)
1 path: 1 drift
```

Each path is `ok`, `drift`, `missing`, `unmanaged` or `stale-backup`. It exits 0 whatever it
finds, and writes nothing. `SOURCE` and `--host NAME` pick the home as for
[`lodi home plan`](#lodi-home-plan).

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--host` | `NAME` | no | with a `SOURCE` that holds several hosts: the host whose home to report on |

**Reads:** `~/.config/lodi/home.toml`, what lodi recorded about your home and its backups, and the
declared files. **Writes:** Nothing.

| Status | Code | When |
|---|---|---|
| 0 | — | the report was printed, whatever it found |
| 2 | — | a usage error, including a second `SOURCE` and `--host` without a `SOURCE` |
| 3 | `E_NO_MANIFEST`, `E_SYNTAX`, `E_TYPE`, `E_IDENT`, `E_UNKNOWN_ATTR`, `E_UNKNOWN_BLOCK`, `E_UNSUPPORTED`, `E_ATTR_CONFLICT`, `E_DUP_RESOURCE`, `E_EXCLUDED_CONSTRUCT`, `E_VAR_UNSET`, `E_PATH_ESCAPE`, `E_CONFIG`, `E_VERSION` | the manifest or the configuration directory |
| 6 | `E_STORE_IO` | a file could not be read |
| 11 | `E_DECLINED` | with no `SOURCE`, your home was last applied from a `SOURCE`. Name it again |

### `lodi host arm`

Allow lodi to manage this machine. Run it once.

```sh
sudo lodi host arm
```

```text
armed /: lodi host plan, apply and import may now act on it; delete /etc/lodi/host-allowed to disarm
  it
```

It creates the empty file `/etc/lodi/host-allowed`, mode 0644, and `/etc/lodi/` if needed.
`lodi host plan`, `apply` and `import` stop with `E_HOST_NOT_ARMED` until that file exists.
Nothing else creates it. To undo it, delete the file.

On `/` it needs root, and it stops with `E_NEED_ROOT` before it looks at anything. With
`--root DIR` you allow lodi to manage a directory tree of your own. It never follows a symbolic
link. A valid file that is already there is left exactly as it is. Anything else at that path
stops with an error and is left as found.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--root` | `DIR` | no | allow lodi to manage this existing directory instead of `/` |

**Reads:** The details of `/etc`, `/etc/lodi` and `/etc/lodi/host-allowed`. **Writes:**
`/etc/lodi/` and `/etc/lodi/host-allowed` when they are missing, and nothing else.

| Status | Code | When |
|---|---|---|
| 0 | — | the file was created, or a valid one was already there |
| 2 | — | a usage error, including any flag `arm` does not take |
| 3 | `E_EXISTS`, `E_PATH_ESCAPE` | something other than a valid file is at that path, or a symbolic link is on the way. It is left as found |
| 6 | `E_STORE_IO` | `--root` is not an existing directory, or the file could not be created |
| 9 | `E_NEED_ROOT` | the root is `/` and you are not root. Nothing was created |
| 9 | `E_HOST_ROOT_REQUIRED` | `LODI_HOST_REQUIRE_ROOT=1` is set and no `--root` was given. Nothing was read |

### `lodi host import`

Copy this machine's packages and settings into a `host.toml`. It copies no file from `/etc`,
and never opens `/etc/shadow`, SSH host keys or private keys.

```sh
sudo lodi host import ~/lodi
```

```text
wrote ~/lodi/<hostname>/home/<login>/home.toml
wrote ~/lodi/<hostname>/host.toml: 34 package(s) declared, 0 not captured, no file copied from
  /etc; no package and no file outside ~/lodi/<hostname> changed
read it first: NOT CAPTURED lists what was left out and how to declare it
next: sudo lodi host plan ~/lodi (writes nothing)
```

It reads the packages you installed from your distribution, your hostname, time zone, locale,
keymap and accounts. It writes them to `host.toml`, which a fresh install can apply. It changes
nothing on the machine.

Packages from elsewhere, such as a PPA, a package file or the AUR, are listed in a `NOT
CAPTURED` block. So is each configuration file your package manager reports as changed, with the
`[etc."PATH"]` table that declares its text. Each one prints one `W_UNCAPTURED` line.

Where it writes:

- With a `SOURCE`, it writes `SOURCE/<hostname>/`, or `SOURCE/NAME/` with `--host NAME`. Under
  `sudo` the files belong to the owner of `SOURCE`. It also writes a starting `home.toml` for
  you, unless you give `--no-home`.
- With no flag, it writes `/etc/lodi/host.toml`, which needs root.
- `--out DIR` writes `DIR/host.toml`. `--stdout` prints the manifest and writes nothing.
- `files/` beside `host.toml` holds only the keyring each commented `[sources]` block names.

Run it again over an existing `host.toml` and it merges what changed on the machine into your
file. It keeps your edits, comments and order, and prints one `W_RECONCILE` line for each
conflict, keeping your side. `--dry-run` shows the merge as a diff and writes nothing:

```sh
sudo lodi host import --dry-run ~/lodi
```

A machine that has never fetched a package index cannot say where its packages came from. lodi
then declares every package it finds and prints `W_UNCHECKED_ORIGIN`. Run `sudo apt-get update`
and import again.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--root` | `DIR` | no | the root to read and write in, instead of `/` |
| `--host` | `NAME` | no | with a `SOURCE` that holds several hosts: the host to write instead of the hostname's |
| `--out` | `DIR` | no | write `DIR/host.toml` and `DIR/files/` instead of `/etc/lodi` |
| `--stdout` | — | no | print the manifest, copy no file and write nothing. Not with `--out` or `--force` |
| `--force` | — | no | replace an existing `host.toml` instead of merging into it, and remove from `files/` what it no longer declares |
| `--no-home` | — | no | with a `SOURCE`: write no `home.toml` for you |
| `--dry-run` | — | no | show what the import would change, as a diff, and write nothing. Not with `--stdout` |

**Reads:** `/etc/lodi/host-allowed`, `/etc/os-release`, the package manager's answers, the
settings files under `/etc` and the keyrings of the apt sources. **Writes:** `host.toml`,
`files/` and, as root, the record `/etc/lodi/host.lock`. Nothing with `--stdout` or
`--dry-run`. Nothing else on the machine.

| Status | Code | When |
|---|---|---|
| 0 | — | the manifest was written, printed or merged. A `W_RECONCILE` conflict does not change this |
| 2 | — | a usage error, including `--stdout` with `--out`, `--force` or `--dry-run`, a `SOURCE` with `--out` or `--stdout`, and `--host` without a `SOURCE` |
| 3 | `E_UNSUPPORTED` | the machine runs a distribution lodi does not manage |
| 3 | `E_PATH_ESCAPE` | a symbolic link at `/etc/lodi`, `host.toml` or `files`, a `SOURCE` lodi cannot trust, or a host name that is not one plain name |
| 3 | `E_NO_MANIFEST` | a `SOURCE` that does not exist, or a directory of hosts and no hostname and no `--host` |
| 4 | `E_LOCK_VERSION` | `/etc/lodi/host.lock` was written by a newer lodi. Nothing was read or written |
| 6 | `E_STORE_IO` | `--root` is not a directory, `--out` is inside a directory lodi keeps for itself, or a file could not be written |
| 7 | `E_NO_RUNTIME` | the package manager is missing, or is not owned by root or is writable by others |
| 8 | `E_APPLY` | reading the package manager failed |
| 9 | `E_HOST_NOT_ARMED` | the machine is not set up with `lodi host arm`. Nothing was opened |
| 9 | `E_NEED_ROOT` | writing into `/etc/lodi` on `/` without root. Nothing was read. `--dry-run` needs no root |
| 9 | `E_HOST_ROOT_REQUIRED` | `LODI_HOST_REQUIRE_ROOT=1` is set and no `--root` was given. Nothing was read |

### `lodi host plan`

Show what `lodi host apply` would change on this machine. It changes nothing.

```sh
sudo lodi host plan
```

```text
= index (private: the transaction reads a dated source set refreshed into /var/lib/lodi/host/pin)
~ package tzdata (pinned 2024b-0+deb12u1 from debian at 2025-03-01T00:00:00Z; downgrade from
  2026b-0+deb12u1; behind latest 2026c-0+deb12u1; not recorded in pins.lock, record it with: sudo
  lodi host pin tzdata --to 2025-03-01)
~ package tzdata (hold)
2 action(s)
```

It shows the package, source and file changes `/etc/lodi/host.toml` declares, and writes nothing.
A change starts with `+` for something added, `~` for something changed and `-` for something
removed. A line that starts with `=` is already right. The machine must be set up once with
[`lodi host arm`](#lodi-host-arm). lodi never calls `sudo` itself.

**Where the host comes from.** With no `SOURCE` it reads `/etc/lodi/host.toml`. With a `SOURCE`
that holds a `host.toml`, it reads that folder. Otherwise `SOURCE` holds several hosts, and lodi
reads `SOURCE/<hostname>/`, or `SOURCE/NAME/` with `--host NAME`:

```sh
sudo lodi host plan ~/lodi
```

Every folder from `/` down to the host must belong to root or to you and must not be writable by
others. A folder that fails this stops with `E_PATH_ESCAPE`, and no flag skips it. When the host
holds your `home/<login>/home.toml`, the plan shows your home after the host. `--no-home` skips it.

**A host from a git URL.** `SOURCE` may be `git+https://HOST/PATH`, or `github:OWNER/REPO`,
`gitlab:OWNER/REPO` or `codeberg:OWNER/REPO`, for a public repository:

```sh
sudo lodi host plan github:OWNER/REPO
```

lodi downloads one commit, checks it and keeps it in a cache only root can read. The apply
records that commit in `/etc/lodi/host.lock`, and later runs use it with no request until you give
`--refresh`. The first line names the commit and whether a request was made.

**Pins.** A pinned package shows the version and date it is pinned to, and whether a newer one
exists. A pin the lock does not record yet is looked up over the network. It is the only case
in which a plan makes a request. See [the host scope page](scopes/host.md) for sources and pins.

`LODI_HOST_REQUIRE_ROOT=1` makes `--root` required for every `lodi host` command. Without it, the
command stops with `E_HOST_ROOT_REQUIRED` before it reads anything.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--root` | `DIR` | no | the root to read, instead of `/` |
| `--no-home` | — | no | plan only the host, and not your home in it |
| `--host` | `NAME` | no | with a `SOURCE` that holds several hosts: the host to read instead of the hostname's. Without a `SOURCE` it is a usage error |
| `--ref` | `NAME` | no | with a URL: the branch or tag to use instead of the default branch |
| `--rev` | `COMMIT` | no | with a URL: the commit to read, by its 40 hexadecimal digits |
| `--refresh` | — | no | with a URL: look up the branch again instead of the locked commit. Not with `--rev` |

**Reads:** `/etc/lodi/host-allowed`, the host's `host.toml` and the files it declares, and the
keyrings of declared sources. It also reads `/etc/lodi/host.lock`, the pin lock, the declared
paths and the machine's apt sources. Over HTTPS, only what an unrecorded pin needs.
**Writes:** Nothing.

| Status | Code | When |
|---|---|---|
| 0 | — | the plan was printed |
| 2 | — | a usage error, including a second `SOURCE`, `--host` without a `SOURCE` or beside a single host, a URL flag without a URL, and `--rev` with `--refresh` |
| 3 | `E_NO_MANIFEST`, `E_SYNTAX`, `E_TYPE`, `E_IDENT`, `E_UNKNOWN_ATTR`, `E_UNKNOWN_BLOCK`, `E_UNSUPPORTED`, `E_ATTR_CONFLICT`, `E_DUP_RESOURCE`, `E_GPGCHECK_OFF`, `E_EXCLUDED_CONSTRUCT`, `E_VAR_UNSET`, `E_UNKNOWN_PACKAGE`, `E_PATH_ESCAPE`, `E_HOST_MISMATCH`, `E_VERSION` | the manifest, a path outside the root, a folder others can write, a managed file with a second hard link, or an apt source that is listed twice or cannot be read |
| 4 | `E_LOCK_VERSION` | `/etc/lodi/host.lock` or the pin lock was written by a newer lodi |
| 4 | `E_SNAPSHOT_TOO_OLD`, `E_SNAPSHOT_FUTURE`, `E_NO_MATCH`, `E_REPO_UNREACHABLE` | a snapshot or pin date outside the archive's range, a pinned version that was never published, or an archive that could not be reached |
| 5 | `E_HASH_MISMATCH` | a source's keyring does not have its `signed_by_sha256`, or a dated index does not match its `Release` |
| 5 | `E_NO_CHECKSUM` | a pinned version is served with no SHA-256 |
| 5 | `E_INSECURE_URL` | a `signed_by_url` is not an `https://` URL, or holds a user name or password |
| 5 | `E_INSECURE_URL`, `E_FETCH`, `E_HASH_MISMATCH` | a URL `SOURCE` that is not `git+https://`, a repository that could not be downloaded or is not public, or a cached tree that does not match the lock |
| 6 | `E_STORE_IO` | `--root` is not a directory, or a file could not be read |
| 7 | `E_NO_RUNTIME` | the package manager, or a program it needs, is missing or not trusted |
| 8 | `E_APPLY` | a declared `owner` or `group` does not exist, or reading the package manager failed |
| 9 | `E_NEED_ROOT` | a URL `SOURCE` planned on `/` without root. Nothing was downloaded |
| 9 | `E_HOST_NOT_ARMED` | the machine is not set up with `lodi host arm`. Nothing was opened |
| 9 | `E_HOST_ROOT_REQUIRED` | `LODI_HOST_REQUIRE_ROOT=1` is set and no `--root` was given. Nothing was read |
| 11 | `E_DECLINED` | under `packages = "exact"`, a removal would also take a declared, base-system or never-recorded package |

### `lodi host apply`

Make this machine match `host.toml`: its packages, package sources and files.

**Before you apply:** on Arch this upgrades the whole machine with `pacman -Syu`. Under
`packages = "exact"`, which `lodi host import` writes, a package whose line you deleted is
removed. There is no rollback. Read `sudo lodi host plan` first.

```sh
sudo lodi host apply ~/lodi
```

```text
journal 20260926T124504Z-3c758b44292db1feba1c4e5a48f2320d
+ package cmatrix
1 action(s) applied
home lodi
created   .config/flake-home/app.conf 0640
created   flake-home-file           0600
2 files: 2 create
```

It applies the plan. It checks everything before the first change, and keeps a journal so an
interrupted apply can continue where it stopped. An interrupted apply lodi cannot sort out stops
with `E_JOURNAL_AMBIGUOUS` until you name its journal with `--resolved`. When the host holds your
home, lodi applies the host as root, then your home as you. `--no-home` skips the home:

```sh
sudo lodi host apply ~/lodi --no-home
```

```text
nothing to do
home skipped (--no-home)
```

- **Exact packages.** Under `packages = "exact"`, what leaves is what the package manager's own
  simulation says. Each package is printed with what happens to it and why.
- **Files.** The first time lodi writes over a file it did not create, it keeps the original.
  `on_remove = "restore"` puts it back. lodi never deletes a file it did not create.
- **Edited by hand.** A managed file or package changed outside lodi stops the apply until you
  give `--overwrite-drift`.
- **Pins.** A pinned package is installed at exactly the planned version and held. Nothing is
  installed when a pin cannot be met.
- **Sources.** A source's keyring from `signed_by_url` is downloaded only when the file on disk
  does not have the declared digest. If the index refresh then fails, the source files are put
  back and nothing is installed.
- **Arch.** `--unsupported-partial-upgrade` installs only the declared packages with
  `pacman -S --needed`. It prints `W_ARCH_PARTIAL` and leaves the machine partly upgraded.

A bare `lodi host apply`, whose last apply read another host folder, stops with `E_DECLINED` and
names that folder. Name the folder you mean to go on. An apply that changes nothing on the
machine but updates `/etc/lodi/host.lock` prints `no machine changes; record updated`.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--root` | `DIR` | no | the root to apply to, instead of `/` |
| `--no-home` | — | no | apply only the host, and not your home in it |
| `--host` | `NAME` | no | with a `SOURCE` that holds several hosts: the host to apply instead of the hostname's |
| `--ref`, `--rev`, `--refresh` | as for `host plan` | no | with a URL: as for [`lodi host plan`](#lodi-host-plan). The apply locks the commit it applied |
| `--overwrite-drift` | — | no | take back a file changed outside lodi, and under `packages = "exact"` remove each package installed by hand since lodi last recorded the machine |
| `--resolved` | `JOURNAL-ID` | no | say that you have sorted out an interrupted apply by hand |
| `--no-update` | — | no | do not refresh the package index |
| `--unsupported-partial-upgrade` | — | no | Arch only: install the declared packages without upgrading the rest of the machine |

**Reads:** What `lodi host plan` reads, and the journal. **Writes:** The declared files, a backup
of each file it replaces, the journal and `/etc/lodi/host.lock`. A declared apt source is written
as `/etc/apt/keyrings/lodi-NAME.gpg` or `.asc` and `/etc/apt/sources.list.d/lodi-NAME.sources`,
and on Fedora as `/etc/pki/rpm-gpg/lodi-NAME.asc` and `/etc/yum.repos.d/lodi-NAME.repo`.
It never writes into a host folder you own.

| Status | Code | When |
|---|---|---|
| 0 | — | the apply finished |
| 2 | — | a usage error |
| 3 | `E_NO_MANIFEST`, `E_SYNTAX`, `E_TYPE`, `E_IDENT`, `E_UNKNOWN_ATTR`, `E_UNKNOWN_BLOCK`, `E_UNSUPPORTED`, `E_ATTR_CONFLICT`, `E_DUP_RESOURCE`, `E_GPGCHECK_OFF`, `E_EXCLUDED_CONSTRUCT`, `E_VAR_UNSET`, `E_UNKNOWN_PACKAGE`, `E_PATH_ESCAPE`, `E_HOST_MISMATCH`, `E_VERSION` | as for `host plan`, or a downloaded keyring that holds a secret key or is not a public keyring. Nothing was written |
| 4 | `E_LOCK_VERSION` | the host lock or the pin lock was written by a newer lodi |
| 4 | `E_PIN_UNSATISFIABLE` | a pinned version is not offered. Nothing was installed |
| 4 | `E_SNAPSHOT_TOO_OLD`, `E_SNAPSHOT_FUTURE`, `E_NO_MATCH`, `E_REPO_UNREACHABLE` | as for `host plan`, or the archive of the pinned date could not be refreshed. Nothing was installed |
| 5 | `E_HASH_MISMATCH` | a keyring does not have its `signed_by_sha256`, or the downloaded index differs from the lock. Nothing was written |
| 5 | `E_NO_CHECKSUM` | a pinned version is served with no SHA-256 |
| 5 | `E_PIN_UNTRUSTED` | an Arch pin is signed by a key the current keyring does not trust. `[packages.arch] archived_keyring = true` checks it against that day's keyring |
| 5 | `E_FETCH`, `E_INSECURE_URL` | a `signed_by_url` keyring could not be downloaded, or is not HTTPS. Nothing was written |
| 6 | `E_STORE_IO` | `--root` is not a directory, or a file could not be written |
| 6 | `E_CLOSURE_DRIFT` | after the transaction a pinned package is not at its version. The record is written |
| 7 | `E_NO_RUNTIME` | the package manager, or a program it needs, is missing or not trusted |
| 8 | `E_DRIFT` | a declared source's keyring or file was edited by hand and `--overwrite-drift` was not given. Nothing was applied |
| 8 | `E_APPLY`, `E_JOURNAL_AMBIGUOUS` | an action failed, including a removal the package manager no longer agrees to, or an interrupted apply cannot be sorted out |
| 9 | `E_HOST_NOT_ARMED`, `E_NEED_ROOT`, `E_SYSTEM_BUSY` | the machine is not set up, you are not root, or another apply is running |
| 9 | `E_HOST_ROOT_REQUIRED` | `LODI_HOST_REQUIRE_ROOT=1` is set and no `--root` was given. Nothing was read |
| 11 | `E_DECLINED` | a file or package was changed outside lodi and the overwrite was not confirmed, an exact removal would take a needed package, or a bare apply's last host was another folder |

### `lodi host versions`

List the versions of a package the archive offers.

```sh
lodi host versions pv ~/lodi
```

```text
version   served      state
1.6.20-1  2026-09-26  installed, pinned, latest

lodi host pin pv --to 2026-09-26T00:00:00Z ~/lodi
```

It lists the versions newest first, then one line to copy. On Ubuntu that is every version the
release published, with the day it arrived. On Debian and Arch it is the version served on the
last full day, the pinned one and the installed one. The answer is kept for a day in
`$LODI_HOME/cache/versions/`. It needs no root and no `host.toml`.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--root` | `DIR` | no | the root to read, instead of `/` |
| `--host` | `NAME` | no | with a `SOURCE` that holds several hosts: the host to read |

**Reads:** `/etc/lodi/host-allowed`, the host's `host.toml` and pin lock when there, the installed
version, the cache, and the archive over HTTPS. **Writes:** The cache.

| Status | Code | When |
|---|---|---|
| 0 | — | the table was printed |
| 2 | — | a usage error: no package, a second one, `--all`, `--to`, or `--host` without a `SOURCE` |
| 3 | `E_UNKNOWN_PACKAGE`, `E_NO_MANIFEST`, `E_SYNTAX`, `E_PATH_ESCAPE` | the archive has no such package, and the hint names the nearest, or the host or its manifest |
| 4 | `E_REPO_UNREACHABLE`, `E_LOCK_VERSION` | no answer from the last day is cached and the archive cannot be reached, or the pin lock is from a newer lodi |
| 5 | `E_HASH_MISMATCH`, `E_NO_CHECKSUM` | a dated index does not match its `Release` |
| 6 | `E_STORE_IO` | `--root` is not a directory, or the cache could not be written |
| 9 | `E_HOST_NOT_ARMED`, `E_HOST_ROOT_REQUIRED` | as for [`host plan`](#lodi-host-plan) |

### `lodi host pin`

Keep a package at one version, or the whole machine at one date.

```sh
lodi host pin git --to 2025-03-01T00:00:00Z ~/lodi
```

```text
pinned git to 2025-03-01T00:00:00Z
git: 1:2.39.5-0+deb12u2 from debian-security at 2025-03-01T00:00:00Z
wrote ~/lodi/lodi.lock
wrote ~/lodi/<hostname>/host.toml
next: sudo lodi host apply ~/lodi
```

`lodi host pin NAME --to VALUE` sets `[packages.pin] NAME` to a version or a date. With no
`--to` it pins the installed version. On Debian and Arch it pins the date whose archive serves
that version. `lodi host pin --all` sets `[host] snapshot`, the date of the whole archive, to
`--to DATE` or to the last full day. It works like `nix flake lock --update-input` and
`nix flake update`.

It looks up every pin before it writes, and writes nothing if one fails. It records the result in
the repository's `lodi.lock`, or in `pins.lock` beside the host in place. Pinning again to the
same value writes nothing. It applies nothing. A folder you own needs no root, and under `sudo`
lodi writes as the folder's owner.

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--to` | `VERSION` or `DATE` | no | what to pin to. With `--all`, a date only |
| `--all` | — | no | pin the whole machine to one date instead of one package |
| `--root` | `DIR` | no | the root to read, instead of `/` |
| `--host` | `NAME` | no | with a `SOURCE` that holds several hosts: the host to pin |

**Reads:** As `lodi host versions`, and the dated archives the new pins need. **Writes:** The lock
and `host.toml`, nothing else.

| Status | Code | When |
|---|---|---|
| 0 | — | pinned, or already pinned |
| 2 | — | a usage error: no package and no `--all`, a second package, `--to` with no value, or `--host` without a `SOURCE` |
| 3 | `E_UNKNOWN_PACKAGE` | the package is not in the list, or with no `--to` it is not installed. The hint names `lodi host versions NAME` |
| 3 | `E_TYPE`, `E_UNSUPPORTED`, `E_NO_MANIFEST`, `E_SYNTAX`, `E_PATH_ESCAPE` | `--all --to` a version, a version on Debian or Arch, where the hint gives the date form, or the host or its manifest |
| 4 | `E_SNAPSHOT_TOO_OLD`, `E_SNAPSHOT_FUTURE`, `E_NO_MATCH`, `E_REPO_UNREACHABLE`, `E_LOCK_VERSION` | as for a pin in [`host plan`](#lodi-host-plan), or no date serves the installed version |
| 5 | `E_HASH_MISMATCH`, `E_NO_CHECKSUM` | as for a pin in `host plan` |
| 6 | `E_STORE_IO` | a file could not be written |
| 9 | `E_NEED_ROOT`, `E_SYSTEM_BUSY`, `E_HOST_NOT_ARMED`, `E_HOST_ROOT_REQUIRED` | the host in place without root, another `pin` or `unpin` is running, or as for `host plan` |

### `lodi host unpin`

Remove a pin, so the package follows `host.toml` again.

```sh
lodi host unpin --all ~/lodi --host <hostname>
```

```text
unpinned every pin
removed ~/lodi/<hostname>/pins.lock
wrote ~/lodi/<hostname>/host.toml
next: sudo lodi host apply ~/lodi --host <hostname>
```

It removes one package's pin, or with `--all` the snapshot, every pin and the pin lock. A package
with no pin exits 0, says so and writes nothing. The next `lodi host apply` releases the holds and
changes no version. Root and locking work as for [`host pin`](#lodi-host-pin).

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--all` | — | no | remove every pin, so every package follows the newest version |
| `--root` | `DIR` | no | the root to read, instead of `/` |
| `--host` | `NAME` | no | with a `SOURCE` that holds several hosts: the host to unpin |

| Status | Code | When |
|---|---|---|
| 0 | — | unpinned, or there was no pin |
| 2 | — | a usage error: no package and no `--all`, `--to`, or `--host` without a `SOURCE` |
| 3 | `E_NO_MANIFEST`, `E_SYNTAX`, `E_PATH_ESCAPE` | the host or its manifest |
| 4 | `E_LOCK_VERSION` | the pin lock is from a newer lodi |
| 6 | `E_STORE_IO` | a file could not be written or removed |
| 9 | `E_NEED_ROOT`, `E_SYSTEM_BUSY`, `E_HOST_NOT_ARMED`, `E_HOST_ROOT_REQUIRED` | as for `host pin` |

### `lodi boot confirm`

Make the trial boot of a boot change the default: new `[kernel] parameters`, or a switch of
`[boot] loader`.

```sh
sudo lodi boot confirm
```

```text
~ parameters lodi.bv1=one (the default entry from now on)
confirmed; the entry that was the default stays as lodi-known-good
```

`sudo lodi apply` never changes the default boot entry for new `[kernel] parameters`. It adds an
entry that boots once, as a trial, and says so:

```text
= parameters lodi.bv1=one (a trial boot follows: reboot, then run `sudo lodi boot confirm`)
journal 20260929T135015Z-7825f64618e4e39a2a6134c2e632fdd8
+ parameters lodi.bv1=one (a trial boot: lodi-trial once, by /boot/efi/EFI/lodi.env)
1 action(s) applied
```

Reboot, then run `sudo lodi boot confirm` in the trial boot. lodi writes the parameters as the
default and keeps the entry that was the default as `lodi: known good` in the boot menu. If you do
not confirm, the next boot is the earlier entry, and the next `sudo lodi plan` or `apply` says the
trial reverted. See
[the host scope](scopes/host.md#try-a-boot-change-once-then-confirm-it-with-lodi-boot-confirm).

A switch of the loader is tried the same way. The firmware starts the new loader once, by
`BootNext`, and the earlier one stays first until you confirm. Here a Debian 12 machine went
back from systemd-boot to GRUB with `parameters = ["lodi.bl1=1"]`. In the trial boot, one
command confirms both:

```text
~ loader grub (the default from now on)
confirmed; systemd-boot stays as the fallback entry
~ parameters lodi.bl1=1 (the default entry from now on)
confirmed; the entry that was the default stays as lodi-known-good
```

| Flag | Argument | Required | What it does |
|---|---|---|---|
| `--root` | `DIR` | no | confirm the trial of the machine at `DIR` instead of `/` |

**Reads:** `/var/lib/lodi/host/boot-trial` and `boot-trial-loader`, `/proc/cmdline`, the boot
id, the firmware's `BootCurrent` and the loader's configuration. **Writes,** only in the trial
boot: `/etc/default/grub` or `/etc/kernel/cmdline`, and lodi's own boot entries. It also writes
`EFI/lodi.env` on the EFI system partition, the loader's configuration and `BootOrder`.

| Status | Code | When |
|---|---|---|
| 0 | — | each trial this boot is is now the default |
| 2 | — | a usage error |
| 7 | `E_BOOT_TRIAL` | no trial waits, or this boot is not the trial. Nothing was changed |
| 9 | `E_HOST_ROOT_REQUIRED` | `LODI_HOST_REQUIRE_ROOT=1` is set and no `--root` was given. Nothing was read |

## Environment variables

| Variable | What it does |
|---|---|
| `LODI_HOME` | where lodi keeps its store, caches and records for your user |
| `LODI_REPO` | the repository folder a command uses when it names none (above) |
| `LODI_TRUST` | `1` allows one run of `lodi develop` or `lodi run` on an untrusted project |
| `LODI_FETCH_REWRITE` | sends downloads to a mirror (below) |
| `LODI_FETCH_ATTEMPTS` | how many times a download that failed in passing is tried, 1 to 10 |

`LODI_FETCH_REWRITE` holds `<prefix>=<replacement>` pairs separated by `;`. A request whose URL
starts with a prefix goes to the replacement instead:

```sh
LODI_FETCH_REWRITE='https://github.com/=http://127.0.0.1:8080/github/' lodi lock
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
| 8 | an apply stopped part way or did not start: an action failed, an interrupted apply is unclear, or a managed file was edited by hand. Nothing is undone. |
| 9 | permission: no usable store, a machine lodi may not manage yet, a missing privilege, another apply running, or a host command without `--root` where one is required |
| 10 | the lock is missing, cannot be read, or is out of date |
| 11 | trust: the project's tasks or manifest are not trusted yet, or you answered no |

## What stays the same in 1.x

- **Commands and flags from 1.0 keep working for all of 1.x.** They keep their names, arguments
  and meanings.
- **A minor release may add commands and flags.** A new flag never changes what a command does
  without it.
- **One change of behaviour: an import with no flag writes its file.** `lodi host import` and
  `lodi home import` with no flag write where `plan` reads, instead of printing. Use `--stdout` to
  print, as in `lodi host import --stdout > host.toml`.
- **Help works at every level.** `lodi help COMMAND`, and `--help` or `-h` after any command,
  print help and exit 0.
- **A bare `lodi` orients.** It prints the six scope lines of the help and exits 0, where it once
  was a usage error.
- **`lodi home init` starts the home scope.** `lodi home import` is its 1.0 name and stays for
  all of 1.x with no warning.
- **A command or flag is removed only after a warning.** For at least one minor release, using
  it prints `W_DEPRECATED` on standard error, with what to use instead.
- **Exit statuses keep their meaning.** A new code may be added to an existing status.
- **A warning never changes the exit status.**

There is no `--json` output yet.

## Deprecations

No command or flag is deprecated. One table of `home.toml` is:

| Surface | Announced in | May be removed in | Use instead |
|---|---|---|---|
| home.toml `[files]` | 1.2 | 2.0 | `[home.file]` (rename `content` to `text`) |

`[files]` in `home.toml` is the 1.0 name of `[home.file]`, with `content` where `[home.file]`
says `text`. It still works. `lodi home plan`, `lodi home apply` and `lodi home status` print
one line on standard error for a `home.toml` that uses it, once per run:

```text
lodi: warning W_DEPRECATED: `[files]` in home.toml is deprecated since 1.2 and may be removed in
  2.0; use `[home.file]` (rename `content` to `text`)
```

The command then runs and exits as it would have. A manifest that cannot be read prints its
errors and not this line. `lodi home init`, `lodi home import`, help and usage errors never print
it.

### How a deprecation is announced

A deprecated command or flag prints one line on standard error, then runs as before:

```text
lodi: warning W_DEPRECATED: `lodi <surface>` is deprecated since <X.Y> and may be removed in <X.Y>;
  use `lodi <replacement>` instead
```

The `Surface` column above names what you typed, in one of these forms:

- the first argument alone, such as `init` or `--help`
- the first two arguments, when the second does not start with `-`, such as `host plan`
- each argument that starts with `-`, alone and after the first argument, such as `--force` and
  `init --force`

Arguments after `--` are your own command and never count. A table of a manifest is written as
the file's name and the table, such as ``home.toml `[files]` ``. The command that reads the file
prints the line.
