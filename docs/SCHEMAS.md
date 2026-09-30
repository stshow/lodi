# On-disk schemas

lodi keeps files you rarely read: a lock beside your manifest, a store of tools, and a record
of what it wrote. This page lists each file, shows an example, and says which versions read it.

**What lodi promises.** lodi reads every schema version it has ever written. A file it cannot
read stops the command with an error. The error names the file's schema version, the versions
this lodi reads and the lodi that wrote the file. lodi never guesses, never half-reads a file
and never rewrites it to a shape you did not ask for.

## Every file lodi writes

| Artifact | Path | Root | Writes | Reads | Version field | Writer field | Example |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `project-lock` | `lodi.lock` | the project folder | 1 | 1 | `version` | `generatedBy` | [lock](#locks) |
| `home-lock` | `lodi/home.lock` | the config root | 1 | 1 | `version` | `generatedBy` | [lock](#locks) |
| `shell-lock` | `cache/shell/<h32>.lock` | the store root | 1 | 1 | `version` | `generatedBy` | [lock](#locks) |
| `api-cache` | `cache/api/<h32>.json` | the store root | 1 | 1 | `version` | `lodiVersion` | [cache](#caches) |
| `versions-cache` | `cache/versions/<distro>/<name>.json` | the store root | 1 | 1 | `version` | `lodiVersion` | [cache](#caches) |
| `home-state` | `home-scope/state.json` | the data root | 4 | 1, 2, 3, 4 | `version` | `lodiVersion` | [home](#home-records) |
| `home-backup-index` | `home-scope/backups/index.json` | the data root | 1 | 1 | `version` | `lodiVersion` | [home](#home-records) |
| `home-gc-root` | `gcroots/home` | the store root | 1 | 1 | `version` | `lodiVersion` | [store](#store-records) |
| `session-root` | `gcroots/sessions/<pid>` | the store root | 1 | 1 | `version` | `lodiVersion` | [store](#store-records) |
| `host-lock` | `etc/lodi/host.lock` | the host root | 4, 3, or 2 | 1, 2, 3, 4 | `version` | `generatedBy` | [host](#host-records) |
| `host-pins` | `<host folder>/pins.lock` | the host folder | 2, or 1 | 1, 2 | `version` | `generatedBy` | [host](#host-records) |
| `repository-lock` | `<repository>/lodi.lock` | the repository | 2, or 1 | 1, 2 | `version` | `generatedBy` | [host](#host-records) |
| `host-journal` | `var/lib/lodi/host/journal/<id>.json` | the host root | 1 | 1 | `version` | `lodiVersion` | [host](#host-records) |
| `store-sidecar` | `store/.meta/<entry>.json` | the store root | 1 | 1 | `version` | `lodiVersion` | [store](#store-records) |
| `image-record` | `store/.meta/img-<h32>.json` | the store root | 1 | 1 | `version` | `lodiVersion` | [store](#store-records) |
| `environment-plan` | `store/env-<h32>/env.json` | the store root | 1 | 1 | `schema` | `lodiVersion` | [store](#store-records) |
| `store-layout` | `.layout.json` | the store root | 1 | 1 | `layout` | `lodiVersion` | [layout](#the-store-layout) |
| `trust-store` | `lodi/trust.json` | the config root | 1 | 1 | `version` | `lodiVersion` | [trust](#the-trust-store) |

The store root and the data root are `$LODI_HOME`, by default `~/.local/share/lodi`. The config
root is `~/.config`. The [host records](#host-records) say which version a host lock is.
lodi does not version your `lodi.toml`, backup copies, downloads, build logs or its shell scripts.

The examples come from real runs of lodi 1.5.0 in an empty home, unless they say otherwise.

## Locks

`lodi lock` wrote this `project-lock` for a project with `ripgrep = "15"` under `[tools]`:

```json
{
  "base": null, "format": "lodi-spike-lock/1", "generatedBy": "lodi 1.5.0",
  "manifestHash": "sha256:45592b22312e32fd1fe2d05c315d818fcf82a3340116e718a24a6556c29009b6",
  "packages": {
    "ripgrep": {
      "artifacts": [{
        "format": "tar.gz",
        "sha256": "sha256:33e15bcf1624b25cdd2a55813a47a2f95dbe126268203e76aa6a585d1e7b149c",
        "size": 2265718, "stripComponents": 1, "subdir": "",
        "url": "https://github.com/BurntSushi/ripgrep/releases/download/15.2.0/ripgrep-15.2.0-x86_64-unknown-linux-musl.tar.gz"
      }],
      "provider": "binary",
      "recipe": { "input": "builtin", "path": "ripgrep.toml",
        "sha256": "sha256:9e4e5e9fe39e1bb9e5ffc199ae8543d51a6f2b362ff0d27cc0a21a331e0aaf4e" },
      "request": { "constraint": "15", "name": "ripgrep", "provider": "binary" },
      "spec": { "arch": "x86_64", "bin": ["rg"], "env": {}, "path": ["."] },
      "tag": "15.2.0", "version": "15.2.0"
    }
  },
  "profiles": { "default": { "packages": ["ripgrep"] } }, "version": 1
}
```

`lodi home apply` writes `home.lock` for the `[tools]` of your `home.toml`, and `lodi shell ripgrep`
writes a shell lock. Both have this shape. Every lock ever written has the `format` string
`lodi-spike-lock/1`, and lodi knows a lock by it. A tool from `recipes/NAME.toml` in your repository
records `"input": "user"`, the file's name and its SHA-256.

## Store records

`lodi run` and `lodi develop` download and unpack into the store. Each entry gets a small
record beside it, the `store-sidecar`:

```json
{
  "complete": true, "created": "2026-09-26T21:09:14Z",
  "identity": "sha256:33e15bcf1624b25cdd2a55813a47a2f95dbe126268203e76aa6a585d1e7b149c",
  "lodiVersion": "1.5.0", "name": "art-33e15bcf1624b25cdd2a55813a47a2f9-ripgrep-15.2.0",
  "references": [], "type": "art", "version": 1,
  "treeHash": "sha256:f12d25ff22887ec63c52a1fe15feb9d15a6440bc93f278c063ea51f1ca71bd86"
}
```

The `environment-plan`, `env.json`, describes one environment. Its hash is the name of the store
entry that holds it:

```json
{
  "arch": "x86_64", "env": [], "format": "lodi-spike-plan/1",
  "lodiMajor": 0, "lodiVersion": "1.5.0", "mode": "host",
  "packages": [{
    "artifacts": ["sha256:33e15bcf1624b25cdd2a55813a47a2f95dbe126268203e76aa6a585d1e7b149c"],
    "entry": "art-33e15bcf1624b25cdd2a55813a47a2f9-ripgrep-15.2.0",
    "label": "ripgrep", "provider": "binary",
    "spec": { "bin": ["rg"], "env": {}, "path": ["."] },
    "version": "15.2.0"
  }],
  "profile": "default", "schema": 1, "tasks": { "hello": { "run": "rg --version" } }
}
```

lodi adds `lodiVersion` to the plan file, never to the value it hashes. So the same manifest
gives the same store entry under every lodi version.

The `home-gc-root` keeps the entries of your home's tools from `lodi gc`:

```json
{
  "entries": ["env-346aed16ce608804b5b58a1eed00402f",
              "art-33e15bcf1624b25cdd2a55813a47a2f9-ripgrep-15.2.0"],
  "lodiVersion": "1.5.0", "version": 1
}
```

A `session-root` does the same while `lodi develop`, `lodi run` or `lodi shell` runs, and goes
when it ends. lodi 1.4.1 wrote this one, in the same version 1 shape:

```json
{
  "activation": {
    "envhash": "sha256:5a0c047285a3d4a97b7eb59185d170f84b7c3236161a5afc074ada74330d4a9a",
    "id": "sha256:61bafc0c42d69496eb1960eb2dcc098195655b2a74314cb4883928282a5de0e3",
    "profile": "default", "projectRoot": "/tmp/work141/project", "runtime": "host"
  },
  "entries": ["env-5a0c047285a3d4a97b7eb59185d170f8"], "lodiVersion": "1.4.1", "pid": 7040,
  "version": 1
}
```

An `image-record` is written after lodi builds a container image. It holds `version`, `name`,
`type`, `identity`, `tag`, `imageId`, `closureHash`, `packages`, `created`, `complete` and
`lodiVersion`. Build one with `lodi develop` in a `[container]` project.

## Home records

`lodi home apply` records each file it wrote in `home-state`. This is version 3:

```json
{
  "directories": [], "files": {
    ".inputrc": {
      "backup": "94029cdb18d5c02ff6fa90c605076e260835fc9a0be378a38847a3f5817192f5-283382f3da0d1465",
      "mode": "0644", "onRemove": "restore",
      "origin": { "kind": "content",
        "sha256": "6148f023f95309538bdd2a4239af85f825e068d0ccde2bcaff8c8158e6bc6a3f" },
      "path": ".inputrc",
      "sha256": "6148f023f95309538bdd2a4239af85f825e068d0ccde2bcaff8c8158e6bc6a3f"
    }
  }, "lodiVersion": "1.5.0", "source": "~/.config/lodi", "version": 3
}
```

A home with services is version 4. It adds `services`, each user unit lodi manages and the state
it goes back to, and `"linger": true` if lodi turned lingering on. lodi 1.11 stops on it.

`.inputrc` was already there, so lodi kept a copy. The `home-backup-index` lists it:

```json
{
  "backups": {
    "94029cdb18d5c02ff6fa90c605076e260835fc9a0be378a38847a3f5817192f5-283382f3da0d1465": {
      "kind": "original", "mode": "0644", "path": ".inputrc",
      "sha256": "283382f3da0d14654f172e5d1cebec0d37646729db778e5e01acd54e9401cf61"
    }
  }, "lodiVersion": "1.5.0", "version": 1
}
```

### Home state records the folder it was applied from

Version 3 adds `source`, the home folder whose manifest was applied. A plain `lodi home apply`
stops with `E_DECLINED` when the record names another folder. Name that folder to go ahead.
Records from before version 3 count as `~/.config/lodi`.

lodi 1.4 stops on version 3 and names the version. An apply with nothing to do leaves a
version 2 file alone.

### Home state records seed files and program files

Version 2 allows `"state": "seed"` on a file lodi writes once and never compares again. It also
allows an origin of `"kind": "program"` with the `"module"` that wrote the file. lodi 1.0 stops
on version 2 by its version number, and names it.

## Caches

The `api-cache` keeps a GitHub answer that came with an `ETag`. The next request sends
`If-None-Match`, and only a `304` answer is read from the file. The body is cut short here:

```json
{
  "body": "…", "lodiVersion": "1.5.0", "version": 1,
  "etag": "W/\"75ad8f6b02901541f10e3b0827fd97ed1063331273de1db4f792a9a749cfb9e1\"",
  "request": ["accept: application/vnd.github+json"],
  "sha256": "00d11e4f9f481ebad8549c0c17c8e66b93566d8840db130b7d6df5f06749b5ef",
  "url": "https://api.github.com/repos/BurntSushi/ripgrep/releases?per_page=30"
}
```

- It never holds your `Authorization` header or `GITHUB_TOKEN`.
- lodi asks GitHub again for a file of another version, a wrong digest, or not privately yours.
- `cache/api/` is created at mode 0700 the first time GitHub answers with a tag.
- `lodi gc` leaves it alone.

The `versions-cache` keeps what the archive offered for one package name. `lodi host versions
tree ~/lodi` writes it on a machine of that distribution. It holds `distro`, `name`, `fetched`,
`lodiVersion`, `version` and `versions`, a list of `version` and `arrived` pairs.

- A file less than one day old is used with no request. An older one, or one that does not
  match, is fetched again. With the archive out of reach too, it stops with `E_REPO_UNREACHABLE`.
- It holds no machine name, user or path.

## The trust store

`lodi trust` records the task text you allowed in `~/.config/lodi/trust.json`. `<project>`
stands for your project's folder:

```json
{
  "entries": {
    "<project>/lodi.toml": { "trusted": "2026-09-26T21:09:13Z",
      "hash": "sha256:6fe0ca9a161bb2a7571e340625ae145ddaf502fa5da3afc0b9e2942f6a662512" }
  },
  "lodiVersion": "1.5.0", "version": 1
}
```

## Host records

`lodi host apply` records what it did in the `host-lock`. Here lodi 1.4.0 installed `tree` and
wrote two files, as this version does. It shows one file, with placeholders for the machine:

```json
{
  "version": 2, "format": "lodi-host-lock/2", "generatedBy": "lodi 1.4.0",
  "appliedAt": "<time>", "distro": "debian", "distroVersion": "12",
  "packages": { "tree": { "version": "1.0-1", "mark": "manual", "held": false } },
  "files": {
    "/etc/app.conf": {
      "digest": "sha256:a23c9e491c7f53f7ce9ea426ce849525c4373091b668c3c8606ad8859a3b4670",
      "mode": "0600", "owner": "<uid>", "group": "<gid>",
      "backup": "/etc/app.conf.lodi-backup-<stamp>", "onRemove": "restore"
    }
  },
  "baseline": { "explicit": ["tree"], "holds": {}, "files": {
    "/etc/app.conf": "sha256:a23c9e491c7f53f7ce9ea426ce849525c4373091b668c3c8606ad8859a3b4670" } }
}
```

The same apply wrote a `host-journal`, one JSON line per step, so an interrupted apply can
continue. Its first line lists every action and command. Then each action gets a `begin` and
an `end` line, such as `{"record":"end","action":"p1","at":"<time>"}`, and the last line is
`{"record":"finished","at":"<time>"}`.

`lodi host pin` writes `host-pins`, the `pins.lock` beside `host.toml`. lodi wrote this one for
two pins against a recorded Debian archive. The copy shows one pin and one of three digests:

```json
{
  "arch": "x86_64", "distro": "debian", "format": "lodi-host-pins/1", "release": "bookworm",
  "pins": {"tzdata": {
    "filename": "pool/main/t/tzdata/tzdata_2024b-0+deb12u1_all.deb",
    "policy": "date", "repository": "debian", "requested": "2025-03-01",
    "sha256": "sha256:9426e641cdcea0b890fa270930e2577c71ce2085c445e058b08870b2adee554d",
    "snapshot": "2025-03-01T00:00:00Z", "version": "2024b-0+deb12u1"}},
  "snapshot": {"indexes": {"debian/dists/bookworm/Release":
      "sha256:779667c1a82441486a16b1bdc1da288fb90d58ef2bf4fec42609e8e5ab483815"},
    "instant": "2026-09-01T00:00:00Z", "requested": "2026-09-01T00:00:00Z"},
  "version": 1
}
```

`lodi update ~/lodi` writes the `repository-lock`, `~/lodi/lodi.lock`: `format`, `version`,
`hosts` and `homes`, and no writer field.

### The host lock remembers the machine at the last sync

Version 2 adds `baseline`, the machine as lodi last saw it at an import or an apply.
`lodi host import` merges your edits against it. `explicit` lists the packages you installed from
the distribution's own repositories. `holds` lists each held package with its version, none on
Arch, and `files` each file's path and the digest of its bytes, never its content.

Version 2 also adds `source`, the folder whose manifest was read: `/etc/lodi`, or the host folder
you named. A plain `lodi host apply` stops with `E_DECLINED` when it is another folder.

lodi reads versions 1 to 5. A version 1 lock has no baseline. Until the first apply writes
version 2, `lodi host plan` prints `no machine changes; apply would update the record`.

### A host read from a git URL records its commit

Version 3 adds `git`, and only a host read from a git URL has it. Other hosts are still written as
version 2, byte for byte.

- `url` is the full `git+https://` URL. A short form such as `github:OWNER/REPO` is written out.
- `ref` is the ref you asked for, `HEAD` unless you gave `--ref`.
- `rev` is the 40-digit commit applied, and `narHash` is `sha256:` of its tree.

lodi 1.4 stops on version 3 with `E_LOCK_VERSION`. Apply the host folder again with this lodi to
write version 2, which 1.4 reads.

Version 4 adds `services`, such as `{"fstrim.timer": "enabled"}`, for a host that declares
`[services]`. The next apply presets a unit it names that `host.toml` no longer declares. In
`host.toml`, each `[services]` key is a unit name ending in `.service`, `.socket`, `.timer` or
`.path`, and each value is `"enabled"` or `"disabled"`. Anything else stops with `E_TYPE`.

Version 5 adds `basics` for `[system]`, `[firewall]`, `[network]`, `[kernel]` and `[sysctl]`. A
`[system]` or `[sysctl]` basic keeps what it `was` before lodi set it. An apply puts that back
once the line is gone. `firewall` is a digest, and `kernel.parameters` what GRUB last got.

### Pins get a lock file of their own

`host-pins` version 1 is the lock of a host folder, like `flake.lock` beside a flake. Commit it
with the folder. `/etc/lodi/host.lock` stays the record of what was applied.

- It holds no machine name, user, path, lodi version or clock time, so it has no writer field.
- `snapshot` holds your `[host] snapshot`, the instant it matched and each index digest. On Arch
  the indexes are `core.db` and `extra.db`.
- A pin from a `[sources]` repository has no `snapshot`, because it has no dated copy.
- `lodi host plan` and `lodi host apply` read it. `lodi host pin` and `lodi host unpin` write it.

### One lock for a whole repository

`repository-lock` version 1 records what every host and home of a repository locked to. It sits
at the folder you name as `SOURCE`. lodi never searches upward for it.

- `hosts` is keyed by the host folder's path from the root, `.` for the root itself. Each holds
  a `host-pins` body, plus `keys` with each `signed_by_url` key's digest.
- `homes` is keyed by folder, such as `laptop/home/<login>`, with the home lock's `packages` as
  `tools`.
- Older `pins.lock` and `home.lock` files are read as they are. The next write moves them in.
- `lodi update`, `lodi host pin`, `lodi host unpin` and a home's tools write it, whole, each time.
- A folder with both a section here and an older lock file stops with `E_LOCK_STALE`.
- lodi 1.4 does not read it. To go back, use `git revert`.
- On Fedora both lock files are version 2, and only there. lodi 1.6 stops on it with
  `E_LOCK_VERSION`. A Fedora pin is one build, and `source` is Fedora's signed copy of its file:
```json
"htop": {"arch": "x86_64", "epoch": "0", "policy": "version", "release": "3.fc44",
  "repository": "fedora", "requested": "0:3.4.1-3.fc44.x86_64", "version": "3.4.1",
  "sha256": "sha256:8e8567dcf04e1b7244bef11eca5fca4e995424246822f79c53c5b7a73ae4d5a2",
  "filename": "htop-3.4.1-3.fc44.x86_64.rpm", "source": "https://kojipkgs.fedoraproject.org/packages/htop/3.4.1/3.fc44/data/signed/6d9f90a6/x86_64/htop-3.4.1-3.fc44.x86_64.rpm"}
```

## The store layout

`$LODI_HOME/.layout.json` records the shape of the whole store:

```json
{ "layout": 1, "lodiVersion": "1.5.0", "written": "2026-09-26T21:09:13Z" }
```

| Layout | Written by | What it is |
| --- | --- | --- |
| 0 | lodi 0.3.0 and earlier | the store with no `.layout.json` |
| 1 | lodi 1.0 and later | the same folders and files, with `.layout.json` |

A store with no `.layout.json` is layout 0. lodi never guesses the layout from the files present.

- `lodi develop`, `lodi run`, `lodi shell`, `lodi home apply` and `lodi gc` move a store from
  layout 0 to 1. They write `.layout.json` and nothing else.
- If another lodi holds the store, lodi prints `lodi: waiting for the store lock` and waits.
- `lodi gc --dry-run`, `lodi search`, `lodi info` and `lodi home status` never change the layout.
- A newer layout stops the command with `E_STORE_VERSION`, exit 6, and changes nothing.
- A `.layout.json` that is not JSON stops with `E_STORE_IO`, exit 6.

## When an older lodi meets a newer file

Run the newest lodi you have against a shared store, config or host root. An older lodi stops on
a field it does not know, instead of misreading the file.

- **Locks.** lodi 1.0 added no field to the project, home and shell locks.
- **Store records.** lodi 0.3.0 and earlier treat a store record with `version` as missing. They
  download and unpack again. Nothing is lost.
- **Trust store.** lodi 0.3.0 and earlier stop with `E_CONFIG`. Move the file aside and trust
  again, or use the newer lodi.
- **Host lock.** A lock of version 2 or 3 stops an older lodi with `E_LOCK_VERSION`, which names
  the lodi that wrote it.

A store record with no `version` is version 1. A file with no writer field is read normally.
Reading a file never rewrites it.

## Require a lodi version in a manifest

```toml
[host]
version = "1"
min_lodi_version = ">=1.3.0"
```

`min_lodi_version` works in `[project]`, `[home]` and `[host]`. It takes a constraint such as
`"0.3"` or `">=0.3.0"`, and an empty string sets no minimum.

When the running lodi does not match, or the value is not a constraint, the command stops with
`E_VERSION`, exit 3. It names both versions, before anything is fetched or written.

An older lodi stops at a table it does not know with `E_UNKNOWN_BLOCK`, exit 3. Set `">=1.3.0"`
for `[sources]`, `">=1.9.0"` for `[services]` and `[users]`, `">=1.10.0"` for `[system]`,
`[firewall]`, `[network]` and `[etc]`, and `">=1.11.0"` for `[kernel]` and `[sysctl]`.
