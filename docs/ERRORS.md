# Error codes

Look up an error or a warning that lodi printed, and find the command that gets you going again.
Every error has a code, the file and line where one applies, and a hint:

```text
lodi: error E_UNKNOWN_ATTR: unknown key `nmae` in [project]
  --> ./lodi.toml:2:1
   = hint: did you mean `name`?
```

Each entry below gives the code, the exit status, the message as lodi prints it and what to do.
Names in capitals, such as PATH or NAME, stand for the file or package lodi names.
Warnings never change the exit status. They are listed at the [end of this page](#warnings).

## Exit statuses

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

## The four you meet first

| Code | Exit | What lodi prints | What to do |
|---|---|---|---|
| `E_NO_MANIFEST` | 3 | `no manifest at ./lodi.toml`. For your home it is `there is no home manifest at ~/.config/lodi/home.toml`, and for a machine it names the missing `host.toml`. | Run `lodi init` in the project folder. For your home, run `lodi import --home`. For this machine, run `sudo lodi import`. |
| `E_LOCK_STALE` | 10 | `lodi.lock is out of date with the manifest`, followed by the entries that changed. A missing `lodi.lock` is no error: `lodi develop` and `lodi run` write it. | `lodi develop` and `lodi run` resolve what changed. |
| `E_NO_RUNTIME` | 7 | For a `[container]` project: `podman` is not on PATH. For a machine: `apt-get` or `pacman` is not on PATH. For services in home.toml: systemctl is missing, or your own systemd user manager is not running. Nothing was downloaded or run. | Install rootless Podman once from your distribution, then run `lodi develop` again. lodi installs no package manager. For services, log in as that user, or set linger to true on a service and run `sudo lodi switch`. |
| `E_TRUST_REQUIRED` | 11 | `declares task text that has not been trusted; nothing was realized or run`. A manifest without tasks is trusted as a whole file. | Run `lodi develop` or `lodi run` on a terminal to review and allow it, or pass `--trust` (or set `LODI_TRUST=1`) for one run. |

## The manifest (exit 3)

| Code | Exit | What lodi prints | What to do |
|---|---|---|---|
| `E_EXISTS` | 3 | `./lodi.toml already exists`. Nothing was written. | Keep the file, or run `lodi init --force` to replace it. |
| `E_SYNTAX` | 3 | `manifest is not valid UTF-8`, or the TOML parser's own message, with the line and column. A file over 1 MiB stops here too. | Fix the text at the line and column shown. |
| `E_TYPE` | 3 | `` `version` in [project] must be a string, found an integer ``. A value outside the set a key takes stops here too. | Give the key the type and value the hint names. |
| `E_IDENT` | 3 | tool name `Bad Name` must match the name rules. A name is lower case letters, digits, `.`, `_`, `+` and `-`. | Rename it, or pass `lodi init --name NAME` with at least one usable character. |
| `E_UNKNOWN_ATTR` | 3 | unknown key `nmae` in [project]. The hint suggests the nearest key lodi knows. | Correct the key, or delete it. |
| `E_UNKNOWN_BLOCK` | 3 | unknown table `contaienr` in the project manifest. | Correct the table name, or delete it. |
| `E_UNSUPPORTED` | 3 | `long-running services are deferred`, and other keys or values this lodi does not support. See [Something this lodi does not support](#something-this-lodi-does-not-support). | Use a value this lodi supports. Nothing is replaced silently. |
| `E_MODE` | 3 | distro packages need a `[container]` table. | Add `[container]` with `distro` and `release`, or move the entries to `[tools]`. |
| `E_BLOCK_NOT_ALLOWED` | 3 | `hold` is a host-scope package key, not allowed in a project manifest. | Remove `hold`. It belongs in a machine's `host.toml`. |
| `E_INLINE_NEEDS_EXACT` | 3 | `python` declares `url`, so its version must be an exact version, not `3.12`. | Write the version the URL points at, such as `version = "1.7.1"`. |
| `E_EXCLUDED_CONSTRUCT` | 3 | `${a + b}` is not a substitution name. lodi has no expressions. | Write the value itself, or `$${` for a literal `${`. |
| `E_VAR_UNSET` | 3 | `${vars.absent}` names a variable that [vars] does not declare. Only `host.toml` reads `[vars]`. | Add it to `[vars]` in `host.toml`, or write the value itself. |
| `E_VERSION` | 3 | [host] min_lodi_version = ">=99.0.0" is not satisfied by lodi 1.5.0. Nothing was resolved or planned. | Install a newer lodi, or lower `min_lodi_version`. `lodi --version` prints the one you have. |
| `E_DUP_RESOURCE` | 3 | `/etc/./sub/../x.conf` and `/etc/x.conf` are the same file, /etc/x.conf. Two package source tables for one repository stop here too. | Declare each file and each repository once. |
| `E_ATTR_CONFLICT` | 3 | [files."/etc/x.conf"] declares both `content` and `source`. A package both declared and in `absent` stops here too. | Keep `content` for the text itself, or `source` to copy a file. |
| `E_PATH_ESCAPE` | 3 | `in [files] is not an absolute path inside the root`, and other paths that leave where they belong. See [A path outside where it belongs](#a-path-outside-where-it-belongs). | Fix the path or the permissions the message names. |
| `E_UNKNOWN_PACKAGE` | 3 | `curl` is in `hold` but not in the package list. A name the distribution does not offer stops here too, with the nearest names. | Add the name to `[packages]`, or correct it. Put a package some machines lack in `optional`. |
| `E_UNKNOWN_UNIT` | 3 | `[services]` names a unit that is not `on this machine`, or one that is static, masked or an alias, `which systemctl cannot enable or disable`. In home.toml it says no user unit NAME. Nothing was changed. | Correct the name, name the unit an alias points to, or add the package that ships the unit to `[packages]`: lodi looks again once that package is installed. In home.toml you can also write the unit yourself with unit. |
| `E_UNKNOWN_SETTING` | 3 | `[system]` names a time zone or locale the machine does not have: `no time zone Mars/Olympus in /usr/share/zoneinfo`. Nothing was changed. | Correct the name. `timedatectl list-timezones` and `localectl list-locales` list what the machine has. For another locale, install the distribution's locale data, such as `locales-all` or a `glibc-langpack`. |
| `E_HOST_MISMATCH` | 3 | host.toml asserts `[host] distro = "ubuntu"`, but this root is `debian`. Nothing was read further. | Apply it on the machine it was written for, or change `distro` in `[host]`. |
| `E_HOST_OSTREE` | 3 | The root `runs from rpm-ostree`, such as Fedora Silverblue, which lodi does not manage. Nothing was read or changed. | Manage its packages with rpm-ostree. lodi manages Fedora installed with dnf. |
| `E_PROTECTED_PATH` | 3 | A `[files]` entry `is a Fedora package setting`: its keys, repositories, dnf variables or SELinux policy. | Take the entry out. lodi never writes these files. |
| `E_GPGCHECK_OFF` | 3 | [sources.NAME] turns the signature check `gpgcheck` off. `repo_gpgcheck = false` stops here too. Nothing was changed. | Leave the key out, or write it `true`. lodi never adds a repository with a signature check off. |
| `E_IDENTITY_CONFLICT` | 3 | An account or group in `[users]` or `[groups]` differs from the machine: `alice has UID 1001, host.toml says 1500`. A group or account a table names that neither exists nor is declared stops here too. Nothing was changed. | Declare what the machine has, which `lodi import` writes, or change the machine by hand first. lodi never changes an existing UID, GID or home. |
| `E_SSH_KEY` | 3 | An entry of `ssh_keys` `is a private key`, spans more than one line, or is not a public key. Nothing was read further. | Write each public key as the one line of its `.pub` file. A private key never belongs in `host.toml`. |
| `E_CONFIG` | 3 | `HOME is not set, and lodi never looks a home directory up in the password database`. A bad `LODI_FETCH_ATTEMPTS` or home `source` stops here too, and so do services in home.toml run by root. | Set `HOME` to the directory lodi should manage, and run `lodi switch --home` without `sudo`. |

### Common manifest mistakes

Each keeps the code and exit status above. The hint names the fix.

| Mistake | Code | What lodi prints | Hint |
|---|---|---|---|
| A version in a host package name | `E_UNSUPPORTED` | `curl=8.5` in `common` of [packages] carries a version | write `curl` alone, or pin a version in [packages.pin]: `curl = "VERSION"` |
| A host table lodi does not manage | `E_UNSUPPORTED` | `[sysctls]` is not a resource kind this build manages | remove the table: lodi manages the packages, package sources, files, services, users and groups of a machine |
| `[services]` in `lodi.toml` | `E_UNSUPPORTED` | long-running services are deferred, `[services]` is not supported | remove `[services]`: run the service yourself, such as from a task |
| `[vars]`, `[inputs]` or `[modules]` in `home.toml` | `E_UNSUPPORTED` | `[inputs]` is not supported by this version of lodi | remove `[inputs]` from home.toml: lodi does not support inputs fetched from elsewhere |
| `registries` under `[home]` | `E_UNSUPPORTED` | `registries` is not supported by this version of lodi | remove it: lodi uses only its built-in recipes |
| A host key in `lodi.toml` | `E_BLOCK_NOT_ALLOWED` | `hold` is a host-scope package key, not allowed in a project manifest | remove `hold`: it belongs in a machine's host.toml |
| An expression in `lodi.toml` or `home.toml` | `E_EXCLUDED_CONSTRUCT` | `${a + b}` is not a substitution name | there are no expressions: write the value, or `$${` for a literal `${` |
| An expression in `host.toml` | `E_EXCLUDED_CONSTRUCT` | `${foo.bar}` is not a substitution name | put one name inside `${ }`, such as `${host.arch}`, or write `$${` for a literal `${` |
| An architecture table inside another | `E_UNKNOWN_BLOCK` | `x86_64` in [tools.jq.aarch64] is already an architecture table | architecture tables do not nest: declare `x86_64` as its own table under the tool |
| A tool for another architecture only | `E_UNSUPPORTED_ARCH` | `jq` is declared only for aarch64, this build resolves x86_64 | declare the tool for x86_64 too, or mark it `optional = true` |
| A `remove` name not in `common` | `E_UNKNOWN_PACKAGE` | `b` is in `[packages.debian] remove` but not in `common` | take `b` out of this `remove`, or add it to `common` |
| An `on_remove` lodi does not know | `E_TYPE` | `on_remove = "zap"` in [files."/etc/a"] is not a value lodi knows | `on_remove` is `restore`, `delete` or `keep` |
| Both `content` and `source` for a host file | `E_ATTR_CONFLICT` | [files."/etc/a"] declares both `content` and `source` | keep `content` for the text itself, or `source` to copy a file |
| One file declared twice | `E_DUP_RESOURCE` | `/etc//a` and `/etc/a` are the same file, /etc/a | declare each file once: remove one of the two entries |

### Something this lodi does not support

Each situation below prints `E_UNSUPPORTED` and exits 3. Nothing was written.

| Situation | What lodi prints | What to do |
|---|---|---|
| A key or table this lodi does not implement | `` `KEY` in [TABLE] is not supported by this build `` | Remove the key. |
| A container distribution or release it does not build | `container release "RELEASE" is not supported by this build`, and the hint lists the releases it supports | Choose a release from the hint. Arch takes only `rolling`. |
| An archive format it cannot unpack, such as `zip` | `` the artifact of `TOOL` is `FORMAT`, which this build cannot decode `` | Use a `tar.gz`, `tar.xz` or plain binary download. |
| A version in a host package name | `` `curl=8.5` in `common` of [packages] carries a version `` | Write the name alone, and pin the version in `[packages.pin]`. |
| `"latest"` as a pin | `` `NAME = "latest"` is not a pin this build reads `` | Remove the pin to follow the newest version. |
| A version pin where only a date pin works, such as on Arch | the message names the package, and the hint gives the date form | Pin a date, such as `tree = "2026-09-01"`. |
| A host source that is not a well-formed URL or alias | `github:a: an alias names exactly OWNER/REPO, and this names 1 segment(s); nothing was fetched` | Write `github:OWNER/REPO`, or a `git+https://` URL with a plain path. |
| A URL given to `lodi import` | `URL is a URL, and lodi never imports into a checkout` | Import into a folder of yours, then commit and push it. |
| A URL given to `lodi update`, `pin` or `unpin` | `URL is a fetched config, which lodi never writes` | Update a checkout of it instead, commit and push. |

### A path outside where it belongs

Each situation below prints `E_PATH_ESCAPE` and exits 3. Nothing was written. lodi never follows a
symbolic link inside a folder it writes.

| Situation | What lodi prints | What to do |
|---|---|---|
| A `[files]` key that is not absolute, or leaves the root with `..` | `` `etc/x.conf` in [files] is not an absolute path inside the root `` | Write an absolute path, such as `/etc/x.conf`. |
| A `source` outside the manifest's folder | `` `source = "VALUE"` in PLACE leaves the manifest's own directory `` | Move the file next to `host.toml`, and name it by a relative path. |
| A symbolic link on the way to a home file | `` `COMPONENT` of `PATH` is a symbolic link, and lodi does not follow one `` | Move the link aside, or choose another path. |
| A home path with `..`, `~` or an empty part | `` `PATH` is not a path inside the root `` | Write the path relative to your home, without `..` or `~`. |
| Something that is not a file where lodi writes one | `PATH exists and is not a regular file` | Move what is in the way, then run `lodi switch --dry-run` again. |
| A folder others can write, on the way to a file lodi manages | `PATH: DIR is not a trusted directory: WHY` | Give it the owner the message names, and run `chmod go-w DIR`. |
| A folder lodi could not inspect | `PATH: DIR is not a trusted directory: it could not be inspected` | Run the command as the folder's owner, or give back its read and search bits. |
| The host configuration is a link or writable by others | `PATH is not a file the host scope reads: WHY` | Replace the link with the file, and run `chmod go-w` on it. |
| A host folder that is not safe to read | `PATH is not trusted as a host: WHY` | Give it and every folder above it a safe owner and `chmod go-w`, or move it off a network file system. |
| A managed file with a second hard link | `PATH has N hard links, and lodi changes the owner or mode only of a file with one` | Remove the other link, or replace the file with a copy of itself. |
| A package configuration file outside the root | `PATH: dpkg lists this configuration file below ROOT, and it does not stay inside that root` | Check the package database of that root, then import again. |
| A login name that is not a safe folder name | `passwd login "NAME" is not one safe component` | Give the user a plain login name. |

lodi checks each part of a path and then uses it. A link created in between would be followed.
lodi only writes below folders that no one else can write, so no one else can make that swap.

## Resolution (exit 4)

| Code | Exit | What lodi prints | What to do |
|---|---|---|---|
| `E_NO_RECIPE` | 4 | no recipe for tool `pyton` in the built-in catalogue, with the nearest names and where a recipe of your own goes. | Use a name `lodi search` lists, declare `url` and `sha256` for the tool, or write `recipes/NAME.toml` at your config's root. |
| `E_NO_MATCH` | 4 | no widget version upstream matches `2`, followed by the versions that exist. | Widen the constraint in `[tools]`, or pick a version from the hint. |
| `E_UNSUPPORTED_ARCH` | 4 | the widget recipe `has no build for` aarch64. lodi runs on x86_64. | Run it on x86_64, or mark the tool `optional = true`. |
| `E_RECIPE_CTX` | 4 | tool `widget` has no artifacts. Upstream returned something the recipe does not expect. | Run the command again. If it repeats, report it with the tool name. |
| `E_RECIPE_INVALID` | 4 | catalogue recipe widget.toml: `exclude` entry `/absolute` in [asset] must be an exact relative file path. | For a recipe lodi ships, this is a fault: report it with the code and the recipe name. For your own, fix the `recipes/NAME.toml` the message names. |
| `E_REPO_UNREACHABLE` | 4 | `https://mirror.invalid/ubuntu/dists/noble/Release: HTTP status 404`. See [A package index lodi cannot use](#a-package-index-lodi-cannot-use). | Check the network, then run the command again. |
| `E_SNAPSHOT_TOO_OLD` | 4 | snapshot 2024-04-30T23:59:59Z `is before` 2024-05-01T00:00:00Z. The archive serves nothing older. | Remove the snapshot so the next lock pins a current one, or set a newer date. |
| `E_SNAPSHOT_FUTURE` | 4 | snapshot 2026-09-22T00:00:00Z `is in the future`. On Arch, today's date is too new as well. | Set the snapshot to today or earlier. For Arch, use yesterday or earlier. |
| `E_LOCK_VERSION` | 4 | lodi.lock has lock version 9, this lodi reads version 1. | Use the lodi that wrote it; `this lodi never replaces it`. |
| `E_PIN_UNAVAILABLE` | 4 | pv 0:1.10.4-1.fc44.x86_64 `is pinned, and not served`: `neither a configured repository nor Fedora's signed Koji copy serves those exact bytes`. Fedora may delete old builds. | Nothing was installed or removed. Pin another build, which `lodi pin NAME` lists, or remove the pin. |
| `E_PIN_UNSATISFIABLE` | 4 | `tzdata` is pinned to 2024b-0+deb12u9 from debian at 2025-03-01T00:00:00Z, and the source set this switch reads does not offer that version. | `nothing was installed or removed`. Pin a version or date the repository offers, or remove the pin. |

### A package index lodi cannot use

Each situation below prints `E_REPO_UNREACHABLE` and exits 4. Nothing was installed.

| Situation | What lodi prints | What to do |
|---|---|---|
| The index or its `Release` file could not be downloaded | `URL: HTTP status 404` | Check the network and the mirror, then run the command again. |
| The download kept failing | the reason, ending `; gave up after 10 attempts` | Wait, then run the command again. |
| The `Release` file is for another suite | `` URL is for `SUITE` (suite `SUITE`), not `SUITE` `` | Check the release name in `[container]`. |
| The `Release` file lacks a checksum lodi needs | `URL lists no SHA-256 for FILE` | Try again later. The mirror may be part way through an update. |
| Fedora metadata expands past lodi's cap, or `repomd.xml` says it will | `URL: decompressed size exceeds the 536870912-byte limit` | Check the mirror. lodi reads no index larger than 512 MiB. |
| No base image directory for the date | `URL lists no dated base directory` | Choose another snapshot date. |
| The archive for a pinned date could not be read before a switch | `the dated source set could not be refreshed: REASON` | Check the network, then run the switch again. |
| Arch databases for a pinned date could not be downloaded | `the dated Arch databases could not be synchronised: REASON` | Check the network, then run the switch again. |
| The versions of a package could not be listed | `` cannot read which versions of `NAME` the DISTRO archive offers `` | Check the network, then run `lodi pin NAME` again. |

## Download and integrity (exit 5)

| Code | Exit | What lodi prints | What to do |
|---|---|---|---|
| `E_BUILD_UNAVAILABLE` | 5 | NAME-EPOCH:VERSION-RELEASE.ARCH `is not served any more`, and the URL the lock names. No other build is used. It comes from `lodi develop` or `lodi run` on a Fedora base, before anything is built or entered. | Run `lodi update`, then enter again to pin a build the repository serves. Nothing was installed. |
| `E_FETCH` | 5 | `https://index.invalid/versions.txt: HTTP status 404`. See [When a download fails](#when-a-download-fails). | Check the network, then run the command again. Nothing half downloaded is kept. |
| `E_INSECURE_URL` | 5 | `http://dl.invalid/widget-9.8.7 is not an HTTPS URL`. lodi downloads over HTTPS only. A host source must be a `git+https://` URL or an alias such as `github:`. | Use an HTTPS URL or mirror. |
| `E_NO_CHECKSUM` | 5 | `https://dl.invalid/widget-9.8.7.sha256 is not published (HTTP 404)`. lodi downloads nothing it cannot verify. | Run the command again later. For a tool with a `url`, declare its `sha256`. |
| `E_HASH_MISMATCH` | 5 | URL: `expected` one digest, `got` another. The bytes are thrown away. | Run `lodi update` if the version is meant to move. Otherwise treat the file as tampered with. |
| `E_PIN_UNTRUSTED` | 5 | `a dated Arch pin is signed by a key not trusted here`, followed by the package and the key. Nothing was installed. | Usually Arch retired the key. Set `archived_keyring = true` in `[packages.arch]`, or pin another date. |

lodi does not check repository signatures. It checks each download against the digest the lock
records, over HTTPS.

### When a download fails

Each situation below prints `E_FETCH` and exits 5.

| Situation | What lodi prints | What to do |
|---|---|---|
| The server answered with an error | `URL: HTTP status 404` | Check the URL or the recipe's version. |
| The server sent no answer in time | `no response headers within 60 s` | Check the network, then try again. |
| The download stopped sending data | `stalled: no data for 60 s after`, and how many bytes arrived | Try again. lodi starts the download from the first byte. |
| The download was too slow | `too slow: under 1024 bytes/s for 60 s after`, and the bytes so far | Try again on a faster connection. |
| Too many redirects | `more than 5 redirects` | Check the URL. |
| The answer was larger than lodi allows | `body larger than N bytes` | Check the URL points at the file you mean. |
| It kept failing after retries | the reason, ending `; gave up after 10 attempts` | Wait, then try again. Set `LODI_FETCH_ATTEMPTS` to retry fewer times. |
| GitHub limited the requests | the status, with a hint naming `GITHUB_TOKEN` | Set `GITHUB_TOKEN`, or wait for the time in the `x-ratelimit-reset` header. |
| A key named by `signed_by_url` could not be downloaded | `[sources.NAME] signed_by_url: REASON` | Check the URL. Nothing was written. |
| A config URL could not be fetched | the URL and the reason, with a hint that only public ones are fetched | For a private config, clone it and switch from the checkout. |

A failure that may pass is retried up to 10 times, with a wait that grows from 2 to 60 seconds.
lodi prints one line for each retry:

```text
lodi: snapshot.ubuntu.com answered HTTP status 503; retrying in 8 s (attempt 4 of 10)
```

## Build and store (exit 6)

| Code | Exit | What lodi prints | What to do |
|---|---|---|---|
| `E_STORE_IO` | 6 | `cannot write ./.gitignore: Too many levels of symbolic links (os error 40)`, or the same for a rename or removal. | Check the file's type, free space and permissions, then run the command again. |
| `E_STORE_VERSION` | 6 | the store at DIR has layout version 99, this lodi knows layout version 2. | `install a newer lodi to use this store; nothing in it was read, changed or removed`. Or point `LODI_HOME` elsewhere. |
| `E_ARCHIVE_FORMAT` | 6 | archive format `zip` is not supported (tar.gz, tar.xz). A cut-off download stops here too. | Run the command again. A repeat means upstream changed the file. |
| `E_ARCHIVE_UNSAFE` | 6 | entry path `../evil` contains `..`. Nothing was stored. | Treat the download as tampered with, and report it with the tool name. |
| `E_ARCHIVE_EMPTY` | 6 | the archive has no entries after `strip_components = 1`. | The recipe does not fit the download. Report it with the tool and version. |
| `E_BIN_MISSING` | 6 | `bin/python3-config` of python 3.12.14 is not an executable file in its artifact. | The recipe does not fit the download. Report it with the tool and version. |
| `E_TREE_UNSUPPORTED` | 6 | PATH `is a special file`, such as a socket or a device. Nothing was stored. | Report it with the tool and version. |
| `E_LAYER_BUILD` | 6 | `the container image build failed (exit status 125); nothing was recorded`, with the path of the full log. | Read the log the hint names, then run the command again. |
| `E_CLOSURE_DRIFT` | 6 | `the installed packages differ from the lock's closure`, followed by each difference. The image was thrown away. | Run `lodi update` to resolve it again, then enter again. |

## Runtime (exit 7)

For `E_NO_RUNTIME`, see [The four you meet first](#the-four-you-meet-first).

| Code | Exit | What lodi prints | What to do |
|---|---|---|---|
| `E_NESTED` | 7 | cannot enter a host environment from inside a container environment. | `leave the outer environment first`, then run the command again. |
| `E_ENTER` | 7 | /projects/a:b contains `:` or `,`, which Podman's bind syntax cannot express. It also says when a command could not start. | `move the project, or set LODI_HOME, to a path without them`. Check that rootless Podman works. |
| `E_BOOT_LOADER` | 7 | `no /etc/default/grub to set [kernel] parameters in`, or `no EFI system partition at /efi, /boot/efi or /boot` to try a parameter change once, or no systemd-boot entry to try one on, or `[boot]` finds no EFI system partition, no loader it knows in use, or no kernel for systemd-boot. Nothing was changed. | Take `parameters` out of `[kernel]`, or `[boot]` out of `host.toml`. lodi sets kernel parameters through GRUB or systemd-boot and tries each change once, and switches only between GRUB and systemd-boot. |
| `E_BOOT_NOT_UEFI` | 7 | `this machine did not start through UEFI`: `host.toml` declares `[boot]`. Nothing was changed. | Take `[boot]` out of `host.toml`. lodi manages only a UEFI bootloader. |
| `E_FIREWALL_CONFLICT` | 7 | `ufw.service is active beside [firewall]`. lodi never runs beside another firewall, and an active `firewalld.service` stops it too. Nothing was changed. | Stop and disable that firewall, or take `[firewall]` out. |
| `E_NETWORK_STACK` | 7 | `no netplan, NetworkManager or networkd is running`. Two active at once, nothing to confirm a change with, or an earlier change still waiting stop here too. Nothing was changed. | Run one of the three, or take `[network]` out. Name an address that answers as `[network] check`. |
| `E_SHELL_NOT_FOUND` | 7 | `no usable interactive shell: $SHELL (unset) and /bin/sh cannot be run`. | Set `SHELL` to the full path of a shell. lodi installs no shell. |

## A switch stopped (exit 8)

lodi has no rollback, except that a network change it cannot confirm is put back. The next switch
shows and changes what remains.

| Code | Exit | What lodi prints | What to do |
|---|---|---|---|
| `E_DRIFT` | 8 | `1 managed file has changed since lodi wrote it; nothing was applied`, followed by each path. | `lodi switch --home --overwrite-drift takes them back`. The edited bytes are kept first. |
| `E_APPLY` | 8 | `` `owner = "nobodyatall"` of /etc/x.conf names no user of this root ``. After a failed action it lists what ran, what failed and what did not run. | Fix the cause the message names, then run `sudo lodi switch --dry-run` again. |
| `E_NETWORK_ROLLED_BACK` | 8 | `192.168.77.1: no answer in 90 s`: the address that confirms a network change did not answer in time, so lodi kept nothing and `the earlier network configuration is back`. The settings before it stay applied. | Correct `[network]`, or name an address that answers as `[network] check`, then switch again. |
| `E_JOURNAL_AMBIGUOUS` | 8 | An interrupted switch left the machine in neither state, and the message names the journal. lodi does not guess. | Put what it names right by hand, then run `lodi switch` again with `--resolved` and the journal's id. |

For a package database left part way, run `sudo dpkg --configure -a` on Debian and Ubuntu. On
Arch, read the end of `/var/log/pacman.log`, then remove `/var/lib/pacman/db.lck` yourself.

## Permission (exit 9)

| Code | Exit | What lodi prints | What to do |
|---|---|---|---|
| `E_STORE_PERM` | 9 | the store location `relative/store` is not absolute. A home path you cannot write stops here too. | Set `LODI_HOME` to an absolute folder you can write. |
| `E_HOST_NOT_ARMED` | 9 | `lodi may not manage` DIR yet: DIR/etc/lodi/may-manage is missing or not trusted. Nothing below it was opened. | Run `lodi import` once and answer yes to managing this host. |
| `E_HOST_ROOT_REQUIRED` | 9 | `LODI_HOST_REQUIRE_ROOT=1 is set`, so the command `needs --root DIR and was given none; nothing was read`. | Name a scratch root with `--root DIR`, or run it where `LODI_HOST_REQUIRE_ROOT` is not set. |
| `E_NEED_ROOT` | 9 | /etc/x.conf declares `owner = "root"`, which this process cannot set. Nothing was written. | Run it again with `sudo`. |
| `E_SYSTEM_BUSY` | 9 | `cannot take` the lock, or another command holds it. A second command never waits. | Wait for the other command, then run it again. If the lock path is named, `check the permissions of that path`. |

## Trust (exit 11)

| Code | Exit | What lodi prints | What to do |
|---|---|---|---|
| `E_DECLINED` | 11 | `/etc/drift.conf was changed since lodi last wrote it`, or you answered no. Nothing was run or written. | `run lodi switch again with --overwrite-drift to replace it, or fold the change into the manifest`. |

`E_DECLINED` also stops a plain `sudo lodi switch` when the last switch used another config.
Name that config, such as `sudo lodi switch ~/dotfiles`, to go on.

## Warnings

A warning goes to standard error, and the command carries on with the exit status it would have
had. These two have an exit status of 0 and a fixed message:

| Code | Exit | What lodi prints | What to do |
|---|---|---|---|
| `W_PROGRAM_NOT_FOUND` | 0 | [programs.git] is declared, but `git` is not on PATH. The files are written anyway. | `install it with [tools], the host scope's [packages] or the distribution`, or remove the table. |
| `W_SHADOWED` | 0 | .shaderc exists, and shade `reads it before or alongside` the file lodi writes. | `move its content into [programs.shade] and delete it` yourself. lodi never touches that file. |

The other warnings:

| What happened | Code | What lodi prints |
|---|---|---|
| You entered an environment from inside another | `W_NESTED` | `entering DIR inside another environment` |
| A tool in the environment hides a command on your PATH | `W_SHADOWS_HOST` | `this environment runs its own make in place of the host command` |
| The task text changed since you trusted it | `W_TRUST_CHANGED` | `the task text of FILE changed since it was trusted` |
| `lodi search` has no package index to search | `W_NO_INDEX` | `there is no ./lodi.lock, so no distro index is named` |
| An optional tool or package is not available here | `W_OPTIONAL_SKIPPED` | `` `NAME` is optional and has no aarch64 build; it is left out of the lock `` |
| A file lodi manages was edited by hand | `W_DRIFT` | `PATH has changed since lodi wrote it`, with both digests |
| A file lodi did not write is replaced | `W_REPLACED_UNMANAGED` | `PATH was not written by lodi; the copy it had is kept as NAME` |
| A `restore` file has no backup | `W_MISSING_REF` | `PATH has no backup to restore` |
| A home command runs under `sudo` | `W_HOME_SUDO` | `this runs under sudo as uid 0` and names both users |
| A host file gets a mode anyone can write, or set-uid | `W_FILE_MODE` | `PATH declares mode 4755, which ...; it is applied as declared` |
| Your `host.toml` uses the old `[files]` table | `W_LEGACY_FILES` | `host.toml declares [files]; it applies as before`, and it names `[etc."PATH"]` |
| A package is installed that `host.toml` does not declare | `W_UNDECLARED` | `NAME is explicitly installed and host.toml does not declare it, so lodi switch leaves it` |
| An Arch pin is installed from a package file | `W_ARCH_PARTIAL` | `a per-entry Arch pin is installed from a verified local package file with pacman -U` |
| `lodi import` left a package or file out | `W_UNCAPTURED` | `NAME is not declared: REASON`, or `PATH is not captured: REASON` |
| `lodi import` found no package index | `W_UNCHECKED_ORIGIN` | `this machine has no package index`. Run `sudo apt-get update`, then import again. |
| `lodi import` over an existing `host.toml` | `W_RECONCILE` | one line per change it leaves to you, such as `package NAME was installed by hand` |
| Two tools give the same command | `W_BIN_CONFLICT` | `` `NAME` is provided by TOOL and TOOL; TOOL wins `` |
| Your shell is fish and no `[programs.fish]` is declared | `W_FISH_HOOKS` | `fish does not read the POSIX shell profile` |
| `lodi gc` cannot read a root | `W_ROOT_UNREADABLE` | `the home root DIR cannot be read; nothing in the store is swept this run` |
| Your `recipes/` has a file with a built-in recipe's name | `W_RECIPE_SHADOWED` | `recipes/jq.toml is used in place of the built-in recipe jq` |
| Your `recipes/` has a link, a folder or a name that is not `NAME.toml` | `W_RECIPE_SKIPPED` | `recipes/notes.txt is not NAME.toml with NAME a package name; it is not read as a recipe` |
| `lodi gc` finds Podman missing | `W_NO_RUNTIME` | `` `podman` is not on PATH, so the environment images ... were not inspected `` |

`lodi import` never changes a configuration file to make it fit. A file with a secret in it,
such as a line with `token` or `secret`, is left out with `W_UNCAPTURED`.
