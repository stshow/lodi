# Run a project with its own tools

Give a project the exact tools and packages it needs, and run its commands with them. You declare
them in `./lodi.toml`, pin their versions in `./lodi.lock`, and run commands inside:

```sh
lodi init
lodi lock
lodi trust
lodi develop -- make
```

Nothing is installed on the machine. The tools are unpacked into a store in your home, and only
the command you run sees them. Anyone with the same `lodi.toml` and `lodi.lock` gets the same
versions. See also [the home scope](home.md), [the host scope](host.md),
[the file formats](../SCHEMAS.md) and [what it costs in disk and time](../COSTS.md).

## Start a project: `lodi init`

```sh
lodi init --name hello
```

```text
wrote ./lodi.toml
added .lodi/ to ./.gitignore
next: lodi lock
```

The `lodi.toml` it writes explains each table in a comment. `lodi init` needs no network and
writes nothing outside the current folder. It adds the one line `.lodi/` to `./.gitignore` and
changes no other line of it.

| Option | What it does |
|---|---|
| `--name NAME` | sets the project's name. Without it, the folder's name is used. |
| `--base DISTRO[:RELEASE]` | writes a `[container]` project for that distribution, such as `debian:bookworm` |
| `--force` | replaces an existing `./lodi.toml` |

Without `--force`, an existing file stops `lodi init` with [`E_EXISTS`](../ERRORS.md):
`lodi: error E_EXISTS: ./lodi.toml already exists`, and the hint `use --force to overwrite`.

A `./.gitignore` that is a symbolic link stops with [`E_STORE_IO`](../ERRORS.md) before
`./lodi.toml` is written.

## Declare what the project needs

```toml
[project]
version = "1"
name = "hello"

[tools]
jq = "1.8"

[tasks.hello]
run = "echo hello"
```

| Table | What it declares |
|---|---|
| `[project]` | `name`, and `min_lodi_version`, the oldest lodi that may use the project |
| `[tools]` | upstream tools, each with a version or a table of keys |
| `[container]` | `distro`, `release` and an optional `snapshot`, to run in a container |
| `[packages]` | `common`, the distribution packages of a `[container]` project |
| `[tasks]` | named commands that `lodi run` runs with `/bin/sh -c` |
| `[env]` | variables set only for the commands you run |

A tool's version is a prefix such as `"3.12"`, `"latest"`, or a range. As a table, a tool takes
`version`, `name`, `optional`, `env`, `path` and `bin`. For a tool the catalogue does not know,
give its download as `url` with its `sha256`, and a `format` of `tar.gz`, `tar.xz` or `binary`.
`strip_components` and `subdir` pick the folder to use. A `[tools.<name>.x86_64]` or
`.aarch64` table changes any of these for one architecture.

lodi lists every mistake in the file in one run, each with its line and column:

```text
lodi: error E_MODE: distro packages need a `[container]` table
  --> ./lodi.toml:8:2
   = hint: add [container] with distro and release, or move the tools to [tools]
```

## Pin the versions: `lodi lock`

```sh
lodi lock
```

```text
wrote lodi.lock: nothing to lock
```

`lodi lock` picks the exact version of each tool and package and writes it to `./lodi.lock`, with
its SHA-256. It reads only the upstream lists of versions, and downloads, installs and runs nothing.
A tool with no recipe, built in or [your own](../../catalogue/README.md#a-recipe-of-your-own), stops
the lock with [`E_NO_RECIPE`](../ERRORS.md), and the hint suggests the nearest names. A tool with
`optional = true` and no build for this machine prints `W_OPTIONAL_SKIPPED` and is left out.

To check that the lock still matches `lodi.toml`, without changing it:

```sh
lodi lock --check
```

```text
lodi: error E_LOCK_STALE: lodi.lock does not exist
   = hint: run `lodi lock`
```

It exits 0 when the lock is up to date, and 10 when it is missing or out of date.

`lodi lock` connects over HTTPS only, and follows at most five redirects. It matches a checksum
file's entry by its full name. It limits how many packages one closure may pull in.

### What the lock promises, and what it does not

The lock promises that every later run uses these bytes and no others. Each download is checked
against its SHA-256 before it is unpacked. A difference stops with
[`E_HASH_MISMATCH`](../ERRORS.md), and nothing is added to the store. For a container, the same
check runs through the distribution's own index files.

The lock does not promise that upstream still publishes the files. lodi checks no repository
signature for any distribution. It trusts HTTPS and the SHA-256 it saw when you ran
`lodi lock`. The lock compiles nothing and knows nothing about your own code's needs.

## Allow the project to run: `lodi trust`

A task is code, and a tool runs in place of your own commands. So lodi runs nothing from a new
project until you trust it. `lodi trust` shows what you are allowing and records it:

```sh
lodi trust
```

```text
~/hello/lodi.toml declares 1 task:
  task hello:
    | echo hello
script text hash: sha256:344f561fd0a19dfa46b99a34393e5cfe091b5f1b9ce42268707a41ebf5b195b1
Trusting authorizes only the task text shown above. Files that text runs or sources (a Makefile,
  project scripts, anything on disk or downloaded) are outside this authorization and are not shown
  or hashed by lodi.
trusted.
```

What is trusted depends on the file:

- **With tasks**, lodi trusts the task text. A change to the tools or the base does not ask
  again.
- **With tools or a base and no tasks**, lodi trusts the whole file. Any change, even to a
  comment, prints `W_TRUST_CHANGED` and asks again.
- **With neither**, there is nothing to trust.

`lodi develop` asks on a terminal, and declining stops with [`E_DECLINED`](../ERRORS.md).
`LODI_TRUST=1` allows one run without recording anything. `lodi trust --revoke` removes the
record from `~/.config/lodi/trust.json`. What a trusted tool does, and the files a task runs,
are yours to check.

## Run a command in the environment: `lodi develop`

```sh
lodi develop -- echo inside
```

```text
inside
```

`lodi develop -- COMMAND` runs the command with the project's tools first on `PATH`, and exits
with its status. With no `-- COMMAND`, it opens a shell. It uses `lodi.lock` as it is, and stops
with [`E_LOCK_STALE`](../ERRORS.md) when there is none. The first run downloads the locked
files, checks them and unpacks them into your store. A project tool with the same name as a
command on your `PATH` prints `W_SHADOWS_HOST` once, and lodi goes on.

A `[container]` project runs in a rootless Podman container that shares the host's network and
can read and write your home ([details](#what-a-container-entry-shares-with-your-machine)).
Without `podman` on your `PATH`, it stops with [`E_NO_RUNTIME`](../ERRORS.md). lodi does not
install Podman and changes no setting on the machine.

Ctrl-C reaches the command directly. lodi passes `SIGTERM` and `SIGHUP` on to it and exits with
`128 + n` for signal `n`. Inside another lodi environment, `--no-nest` stops with
[`E_NESTED`](../ERRORS.md). So does a container inside a host-tool environment.

## Run a task: `lodi run`

```sh
lodi run hello
```

```text
lodi: realized env-46f2732d12a4f894b489a00527a32e40 (0 bytes downloaded)
hello
```

`lodi run TASK [ARGS...]` runs a `[tasks]` entry with `/bin/sh -c`, in the same environment as
`lodi develop`, and exits with its status. It needs `lodi trust` first. A task the file does not
declare stops at once with exit 2, and lodi lists the tasks there are:

```text
lodi: error: lodi.toml has no task `nope`
   = its tasks: hello
```

## Try tools without a project: `lodi shell`

```sh
lodi shell jq
jq -n 1+1
```

```text
lodi: warning W_SHADOWS_HOST: this environment runs its own `jq` in place of the host command of the
  same name on PATH
2
```

Here `jq` is on the machine too, so lodi warns that its own comes first.

`lodi shell TOOL[@VERSION]...` never reads `./lodi.toml`, so there is nothing to trust, and it
writes nothing in the current folder. A second identical `lodi shell` needs no network.
`lodi shell --base DISTRO[:RELEASE] PACKAGE...` does the same in a rootless Podman container, with
those distribution packages. To run one command, use `lodi develop -- COMMAND` instead.

## See what a project uses: `lodi info`

```sh
lodi info
```

```text
project: tools
mode:    host tools
tools:
  jq     1.8         (not locked)
  nodez  1           (not locked)  (no recipe `nodez` in the catalogue; nearest names: nodejs)
lock:    missing (run `lodi lock` to write ./lodi.lock)
```

`lodi info` shows the project's name, whether it uses a container, each tool with its locked
version, and whether the lock is `fresh`, `stale` or `missing`. It needs no network, and works
with an outdated lock. `lodi info jq` shows one catalogue entry, and needs no project.

## Search the catalogue: `lodi search`

```sh
lodi search jq
```

```text
catalogue
  jq  A lightweight and flexible command-line JSON processor  (https://jqlang.org)
```

`lodi search QUERY` lists the catalogue tools whose name or description contains `QUERY`. It
then lists matching packages from the distribution indexes that `./lodi.lock` names, if they are
already downloaded. It needs no network and writes nothing. `--registry` searches the catalogue
only, and `--distro` the packages only. lodi does not download package indexes for search, so a
container project usually gets a `W_NO_INDEX` note instead of packages, and exit 0.

## Free disk space: `lodi gc`

```sh
lodi gc --dry-run
```

```text
would remove 0 session roots
would remove 2 entries
would remove 0 image records
would remove 0 staging directories
would remove 0 downloads
would remove 0 shell resolutions
would free 1250 bytes
```

`--dry-run` shows what `lodi gc` would remove, and removes nothing. `lodi gc` removes the store
entries nothing uses, and downloads older than `--keep-days` days, 7 by default. It keeps what
an open shell or session uses, and your home's tools. `-v` names each thing with the reason.

`--images` also removes the Podman images lodi built that no container uses. What `lodi gc`
removes is downloaded again from the lock when you need it.

## Run a project in a container

Before you use a container: **a `[container]` project shares the host's network, and can read
and write your home.** It is a way to use a distribution's packages, not a sandbox. Every setting
is in [What a container entry shares](#what-a-container-entry-shares-with-your-machine).

```sh
lodi init --base debian:bookworm
```

| `--base` | Packages come from | The system image is pinned by |
|---|---|---|
| `debian:bookworm`, `debian:12` | `snapshot.debian.org`: `bookworm`, `bookworm-updates` and `bookworm-security`, component `main` | the git commit and image digest of the Debian image |
| `ubuntu:noble`, `ubuntu:24.04` | `snapshot.ubuntu.com`: `noble`, `noble-updates` and `noble-security`, components `main` and `universe` | its SHA-256 in Canonical's `SHA256SUMS`, from the newest image folder at or before the snapshot |
| `arch:rolling`, `arch` | `archive.archlinux.org`: `core` and `extra` on the snapshot's day | its SHA-256 in `sha256sums.txt`, from the newest image folder at or before the snapshot |
| `fedora:44`, `fedora` | `dl.fedoraproject.org`: the Fedora 44 release's own `Everything` repository | its SHA-256 and size in the release's `CHECKSUM`, and the image digest lodi carries |

Any other distribution or release stops with [`E_UNSUPPORTED`](../ERRORS.md), which lists the
releases you can use. `[packages] common` is one list. Name the packages as the distribution you
chose publishes them.

With no `snapshot`, `lodi lock` pins the current hour for Debian and Ubuntu. For Arch it pins
the previous day at midnight UTC, because the Arch archive publishes a day once it is over. An
Arch `snapshot` within today stops with [`E_SNAPSHOT_FUTURE`](../ERRORS.md), which names the
latest date you can use. Fedora 44 has no snapshot: its release repository and image never
change, its updates are not used, and a `snapshot` stops with [`E_UNSUPPORTED`](../ERRORS.md).

### Work in a container: the develop route

```sh
lodi lock
lodi trust
lodi develop -- make
```

`lodi lock` pins every package to an exact version. The packages then exist only inside the
container `lodi develop` or `lodi run` starts. **Nothing is installed on the machine.** lodi
builds the image with no network, then reuses it, so a second run downloads nothing. The
installed packages must be the ones in the lock, or the build stops with
[`E_CLOSURE_DRIFT`](../ERRORS.md) and no image is kept.

The same `lodi.toml` and `lodi.lock` give the same versions on another machine, for as long as
the archive still serves them:

| Base | How long a lock keeps working |
|---|---|
| Debian | for good: `snapshot.debian.org` keeps every snapshot |
| Ubuntu | about a month, while Canonical keeps its dated images. After that, a machine without the image stops with [`E_FETCH`](../ERRORS.md). |
| Arch | as far back as the archive keeps its images. Older stops with [`E_SNAPSHOT_TOO_OLD`](../ERRORS.md). |
| Fedora 44 | while `dl.fedoraproject.org` serves the Fedora 44 release. A build it no longer serves stops with [`E_BUILD_UNAVAILABLE`](../ERRORS.md) before anything is built, and no other build is used. |

## What a container entry shares with your machine

`lodi develop` or `lodi run` in a `[container]` project, and `lodi shell --base`, start a
container with fixed settings. `lodi.toml` cannot change them:

- **The host's network** (`--network=host`). The command shares your machine's network,
  including services that listen only on `localhost`.
- **Your home, read-write.** The caller's `$HOME` is mounted read-write at its own path.
- **The project folder, read-write**, or the current folder for `lodi shell --base`. The store
  is mounted read-only.
- **Your own user** (`--userns=keep-id`). The command runs with your uid and gid.
- **Its own processes** (`--pid=private`). It cannot see the machine's other processes.
- **No SELinux label** (`--security-opt label=disable`). SELinux does not keep the container out
  of the folders above.

What the command could reach without lodi, it can reach inside the container too. No display
socket and no device is passed in.

## Files lodi keeps

| Path | What it is |
|---|---|
| `./lodi.toml` | the file you edit |
| `./lodi.lock` | the pinned versions. Commit it. |
| `./.gitignore` | gets the one line `.lodi/` from `lodi init` |
| `$LODI_HOME` | your store: `$XDG_DATA_HOME/lodi`, or `~/.local/share/lodi` |
| `$LODI_HOME/store/` | the unpacked tools and the records of built images |
| `$LODI_HOME/cache/` | checked downloads and answers, reused until `lodi gc` removes them |
| `~/.config/lodi/trust.json` | what you trusted |

## What this scope will not do

- **It does not install anything on the machine.** It changes no host package, setting or shell
  file. Podman is yours to install.
- **It does not pick new versions behind your back.** Only `lodi lock` does.
- **It does not test the aarch64 build** on that architecture before a release.
- **It does not undo.** To go back, use the earlier `lodi.toml` and `lodi.lock`.
- **It does not check repository signatures**, for any distribution.
- **It does not build your project**, or manage your language's own package manager.
- **It does not isolate a container** from your network or your home.
- **It does not run tasks, or put tools first on `PATH`, until you trust them.**
- **It has no `--json` output.**

## Where to look when it fails

Every error prints its code, the line and column when there is one, and the next command to run.
[The error reference](../ERRORS.md) explains each code.

| What you see | What to do |
|---|---|
| exit 2, no code | fix the command. `lodi --help` lists what lodi accepts. |
| [`E_SYNTAX`](../ERRORS.md), [`E_TYPE`](../ERRORS.md), [`E_IDENT`](../ERRORS.md), [`E_UNKNOWN_ATTR`](../ERRORS.md), [`E_UNKNOWN_BLOCK`](../ERRORS.md), [`E_UNSUPPORTED`](../ERRORS.md), [`E_MODE`](../ERRORS.md), exit 3 | fix `lodi.toml` at the line shown. `lodi info` shows what lodi read. |
| [`E_BLOCK_NOT_ALLOWED`](../ERRORS.md), [`E_ATTR_CONFLICT`](../ERRORS.md), [`E_DUP_RESOURCE`](../ERRORS.md), [`E_PATH_ESCAPE`](../ERRORS.md), [`E_INLINE_NEEDS_EXACT`](../ERRORS.md), [`E_EXCLUDED_CONSTRUCT`](../ERRORS.md), [`E_VAR_UNSET`](../ERRORS.md), exit 3 | fix `lodi.toml` at the line shown |
| [`E_VERSION`](../ERRORS.md), exit 3 | install the newer lodi that `min_lodi_version` asks for |
| [`E_NO_MANIFEST`](../ERRORS.md), [`E_EXISTS`](../ERRORS.md), exit 3 | run `lodi init`, or pass `--force` to replace the file |
| [`E_NO_RECIPE`](../ERRORS.md), [`E_NO_MATCH`](../ERRORS.md), [`E_UNSUPPORTED_ARCH`](../ERRORS.md), [`E_RECIPE_CTX`](../ERRORS.md), [`E_RECIPE_INVALID`](../ERRORS.md), exit 4 | change the tool or version. The hint names the nearest names. |
| [`E_REPO_UNREACHABLE`](../ERRORS.md), [`E_SNAPSHOT_TOO_OLD`](../ERRORS.md), [`E_SNAPSHOT_FUTURE`](../ERRORS.md), [`E_LOCK_VERSION`](../ERRORS.md), exit 4 | run `lodi lock` again, with the `snapshot` the message gives |
| [`E_FETCH`](../ERRORS.md), [`E_INSECURE_URL`](../ERRORS.md), [`E_NO_CHECKSUM`](../ERRORS.md), [`E_HASH_MISMATCH`](../ERRORS.md), exit 5 | check the address and your network, then run the command again |
| [`E_ARCHIVE_FORMAT`](../ERRORS.md), [`E_ARCHIVE_UNSAFE`](../ERRORS.md), [`E_ARCHIVE_EMPTY`](../ERRORS.md), [`E_BIN_MISSING`](../ERRORS.md), [`E_TREE_UNSUPPORTED`](../ERRORS.md), exit 6 | a download is not what its catalogue entry says. Report it with the tool and version. |
| [`E_LAYER_BUILD`](../ERRORS.md), [`E_CLOSURE_DRIFT`](../ERRORS.md), exit 6 | read the Podman log the message names, then run the command again |
| [`E_STORE_IO`](../ERRORS.md), [`E_STORE_VERSION`](../ERRORS.md), exit 6 | read the path and the system error named. `lodi gc` clears old entries. |
| [`E_NO_RUNTIME`](../ERRORS.md), exit 7 | install rootless Podman |
| [`E_NESTED`](../ERRORS.md), [`E_ENTER`](../ERRORS.md), [`E_SHELL_NOT_FOUND`](../ERRORS.md), exit 7 | leave the other lodi environment, or check that the command and its shell exist |
| [`E_STORE_PERM`](../ERRORS.md), exit 9 | make the store folder named writable for you |
| [`E_LOCK_STALE`](../ERRORS.md), exit 10 | run `lodi lock` |
| [`E_TRUST_REQUIRED`](../ERRORS.md), [`E_DECLINED`](../ERRORS.md), exit 11 | run `lodi trust` to review and record the project |
