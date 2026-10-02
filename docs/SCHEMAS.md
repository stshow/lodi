# On-disk schemas

lodi keeps files you rarely read: a lock beside your manifest, a store of tools, and a record
of what it wrote. This page lists each file, shows an example, and says which versions read it.

**What lodi promises.** From 2.0 on, lodi reads every schema version a 2.0 or later lodi has
written.

A file in a format 2.0 changed, written by a 1.x lodi, stops the command. The error gives the
artifact's code and the file's path. It names the file's schema version, the versions this lodi
reads and the lodi that wrote the file. lodi never repairs, moves or deletes such a file.

lodi never guesses, never half-reads a file and never rewrites it to a shape you did not ask for.

## Every file lodi writes

| Artifact | Path | Root | Writes | Reads | Version field | Writer field | Example |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `project-lock` | `lodi.lock` | the project folder | 1 | 1 | `version` | `generatedBy` | [lock](#locks) |
| `shell-lock` | `cache/shell/<h32>.lock` | the store root | 1 | 1 | `version` | `generatedBy` | [lock](#locks) |
| `api-cache` | `cache/api/<h32>.json` | the store root | 1 | 1 | `version` | `lodiVersion` | [cache](#caches) |
| `versions-cache` | `cache/versions/<distro>/<name>.json` | the store root | 1 | 1 | `version` | `lodiVersion` | [cache](#caches) |
| `home-state` | `home-scope/state.json` | the data root | 5 | 5 | `version` | `lodiVersion` | [home](#home-records) |
| `home-backup-index` | `home-scope/backups/index.json` | the data root | 1 | 1 | `version` | `lodiVersion` | [home](#home-records) |
| `home-gc-root` | `gcroots/home` | the store root | 1 | 1 | `version` | `lodiVersion` | [store](#store-records) |
| `session-root` | `gcroots/sessions/<pid>` | the store root | 1 | 1 | `version` | `lodiVersion` | [store](#store-records) |
| `host-lock` | `etc/lodi/host.lock` | the host root | 6 | 6 | `version` | `generatedBy` | [host](#host-records) |
| `config-lock` | `<config>/lodi.lock` | the config root | 1 | 1 | `version` | `generatedBy` | [config](#the-config-lock) |
| `host-journal` | `var/lib/lodi/host/journal/<id>.json` | the host root | 1 | 1 | `version` | `lodiVersion` | [host](#host-records) |
| `store-sidecar` | `store/.meta/<entry>.json` | the store root | 1 | 1 | `version` | `lodiVersion` | [store](#store-records) |
| `image-record` | `store/.meta/img-<h32>.json` | the store root | 1 | 1 | `version` | `lodiVersion` | [store](#store-records) |
| `environment-plan` | `store/env-<h32>/env.json` | the store root | 1 | 1 | `schema` | `lodiVersion` | [store](#store-records) |
| `store-layout` | `.layout.json` | the store root | 2 | 2 | `layout` | `lodiVersion` | [layout](#the-store-layout) |
| `trust-store` | `trust.json` | the store root | 1 | 1 | `version` | `lodiVersion` | [trust](#the-trust-store) |

The store root and the data root are `$LODI_HOME`, by default `~/.local/share/lodi`. The config
root is your config's folder, and the XDG config folder is `${XDG_CONFIG_HOME:-~/.config}`. 2.0
changed the host lock and the home state, so their versions start above any 1.x version. Every
other format kept its number.

lodi does not version your `lodi.toml`,
backup copies, downloads, build logs or its shell scripts. The examples come from real runs of
lodi 2.0.0 in an empty home, unless they say otherwise.

## Locks

`lodi run` wrote this `project-lock` for a project with `ripgrep = "15"` under `[tools]` and a
task `hello` that runs `rg --version`:

```json
{
  "base": null, "format": "lodi-spike-lock/1", "generatedBy": "lodi 2.0.0",
  "manifestHash": "sha256:bdc1ec01ebf0c4550c347f70374fcf83662011d4ed1874dbd99baac8f9f7fce8",
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
        "sha256": "sha256:6f60afe5670807f70a063f583f14e6c80954f0a815bbf2f63665458dbc433da5" },
      "request": { "constraint": "15", "name": "ripgrep", "provider": "binary" },
      "spec": { "arch": "x86_64", "bin": ["rg"], "env": {}, "path": ["."] },
      "tag": "15.2.0", "version": "15.2.0"
    }
  },
  "profiles": { "default": { "packages": ["ripgrep"] } }, "version": 1
}
```

`lodi shell ripgrep` writes a shell lock of this shape, and the `[tools]` of your `home.toml`
are locked in the config's `lodi.lock`. Every lock ever written has the `format` string
`lodi-spike-lock/1`, and lodi knows a lock by it. A tool from `recipes/NAME.toml` at your
config's root records `"input": "user"`, the file's name and its SHA-256.

## Store records

`lodi run` and `lodi develop` download and unpack into the store. Each entry gets a small
record beside it, the `store-sidecar`:

```json
{
  "complete": true, "created": "2026-10-02T14:21:42Z",
  "identity": "sha256:33e15bcf1624b25cdd2a55813a47a2f95dbe126268203e76aa6a585d1e7b149c",
  "lodiVersion": "2.0.0", "name": "art-33e15bcf1624b25cdd2a55813a47a2f9-ripgrep-15.2.0",
  "references": [], "type": "art", "version": 1,
  "treeHash": "sha256:f12d25ff22887ec63c52a1fe15feb9d15a6440bc93f278c063ea51f1ca71bd86"
}
```

The `environment-plan`, `env.json`, describes one environment. Its hash is the name of the store
entry that holds it:

```json
{
  "arch": "x86_64", "env": [], "format": "lodi-spike-plan/1",
  "lodiMajor": 0, "lodiVersion": "2.0.0", "mode": "host",
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
  "lodiVersion": "2.0.0", "version": 1
}
```

A `session-root` does the same while `lodi develop`, `lodi run` or `lodi shell` runs, and goes
when it ends. This one is the project's above, with `<project>` for its folder:

```json
{
  "activation": {
    "envhash": "sha256:f3476f0e9faff95123024b088cf72aedd016b4e145d948f8b69010067ae81895",
    "id": "sha256:e7a1dcecd8fe7a133c51c371dc760463874ef1c02eacf3dfa11556e72b65a50e",
    "profile": "default", "projectRoot": "<project>", "runtime": "host"
  },
  "entries": ["env-f3476f0e9faff95123024b088cf72aed",
              "art-33e15bcf1624b25cdd2a55813a47a2f9-ripgrep-15.2.0"],
  "lodiVersion": "2.0.0", "pid": 3927963, "version": 1
}
```

An `image-record` is written after lodi builds a container image. It holds `version`, `name`,
`type`, `identity`, `tag`, `imageId`, `closureHash`, `packages`, `created`, `complete` and
`lodiVersion`. Build one with `lodi develop` in a `[container]` project.

## Home records

`lodi switch` records each home file it wrote in `home-state`, version 5:

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
  }, "lodiVersion": "2.0.0", "source": "~/.config/lodi", "version": 5
}
```

A home with services adds `services`, each user unit lodi manages and the state it goes back
to, and `"linger": true` if lodi turned lingering on.

`.inputrc` was already there, so lodi kept a copy. The `home-backup-index` lists it:

```json
{
  "backups": {
    "94029cdb18d5c02ff6fa90c605076e260835fc9a0be378a38847a3f5817192f5-283382f3da0d1465": {
      "kind": "original", "mode": "0644", "path": ".inputrc",
      "sha256": "283382f3da0d14654f172e5d1cebec0d37646729db778e5e01acd54e9401cf61"
    }
  }, "lodiVersion": "2.0.0", "version": 1
}
```

### Home state records the folder it was applied from

`source` is the home folder whose manifest was applied. `lodi switch` stops with `E_DECLINED`
when the record names another folder it was not told. A `"state": "seed"` file is written once
and never compared again, and an origin of `"kind": "program"` names the `"module"` that wrote it.

## Caches

The `api-cache` keeps a GitHub answer that came with an `ETag`. The next request sends
`If-None-Match`, and only a `304` answer is read from the file. The body is cut short here:

```json
{
  "body": "…", "lodiVersion": "2.0.0", "version": 1,
  "etag": "W/\"7c9d490b3ab72e0d7f7d95c7daccf93e7f0b1aa97ff3b3917ab82359f8fc5251\"",
  "request": ["accept: application/vnd.github+json"],
  "sha256": "ad0954bd8efac3dada0763f668c780574224faa4f2385d964e2745600c53bc57",
  "url": "https://api.github.com/repos/BurntSushi/ripgrep/releases?per_page=30"
}
```

- It never holds your `Authorization` header or `GITHUB_TOKEN`.
- lodi asks GitHub again for a file of another version, a wrong digest, or not privately yours.
- `cache/api/` is created at mode 0700 the first time GitHub answers with a tag.
- `lodi gc` leaves it alone.

The `versions-cache` keeps what the archive offered for one package name. `lodi pin tree`
writes it on a machine of that distribution. It holds `distro`, `name`, `fetched`,
`lodiVersion`, `version` and `versions`, a list of `version` and `arrived` pairs.

- A file less than one day old is used with no request. An older one, or one that does not
  match, is fetched again. With the archive out of reach too, it stops with `E_REPO_UNREACHABLE`.
- It holds no machine name, user or path.

## The trust store

`lodi develop` and `lodi run` record the task text you allowed in `~/.local/share/lodi/trust.json`.
`<project>` stands for your project's folder:

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

`lodi switch` records what it did on the host in the `host-lock`, version 6. Here lodi installed
`tree` and wrote two files. It shows one file, with placeholders for the machine:

```json
{
  "version": 6, "format": "lodi-host-lock/6", "generatedBy": "lodi 2.0.0",
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

The same switch wrote a `host-journal`, one JSON line per step, so an interrupted switch can
continue. Its first line lists every action and command. Then each action gets a `begin` and
an `end` line, such as `{"record":"end","action":"p1","at":"<time>"}`, and the last line is
`{"record":"finished","at":"<time>"}`.

### What the host lock holds

- `baseline` is the machine as lodi last saw it at an import or a switch.
  - `explicit` lists the packages you installed from the distribution's own repositories.
  - `holds` lists each held package with its version, none on Arch.
  - `files` lists each file's path and the digest of its bytes, never its content.
- `source` is the folder whose manifest was read. `lodi switch` stops with `E_DECLINED` when it
  is another folder.
- `git` is there for a host read from a git URL only.
  - `url` is the full `git+https://` URL, and `ref` the ref you asked for.
  - `rev` is the 40-digit commit applied, and `narHash` is `sha256:` of its tree.
- `services`, such as `{"fstrim.timer": "enabled"}`, for a host that declares `[services]`.
- `basics` is there for `[system]`, `[firewall]`, `[network]`, `[kernel]` and `[sysctl]`.
  - A `[system]` or `[sysctl]` basic keeps what it `was` before lodi set it.
  - A switch puts that back once the line is gone.
  - `firewall` is a digest, and `kernel.parameters` what the loader last got.

The pins a host declares are locked in the config's `lodi.lock` (`config-lock`), never in a
`pins.lock`. 2.0 reads no `pins.lock`, `home.lock` or 1.x `lodi.lock`.

## The config's own files

Your config is a folder you own, kept in git. lodi writes `host.toml`, `home.toml`, `lodi.lock`
and `files/host/` into it on `lodi import`. A config with more than one host also has a
`config.toml`.

### The config index

`config.toml` names each host of the config and the home each person gets on it. `lodi import`
writes it when it adds a second host to a flat config:

```toml
[hosts.desktop]
host = "host.toml"
homes = { you = "home.toml" }

[hosts.laptop]
host = "laptop/host.toml"
homes = { you = "home.toml" }
```

- `[hosts.NAME]` is one host. `NAME` is a machine's hostname, a plain name with no `/`.
- `host` is the path of its `host.toml`, from the config's root.
- `homes` maps a login to the path of that person's `home.toml`, from the config's root.
- A path may not start with `/` or hold `..`, or the command stops with `E_PATH_ESCAPE`.
- Any other key, or a file over 64 KiB, stops with `E_CONFIG`.

A machine uses the host named as its hostname, or the one `--host NAME` names. With no
`config.toml`, the config is flat: `host.toml` and `home.toml` at its root, whatever the
hostname. The file has no version field.

### The config lock

The config's `lodi.lock`, the `config-lock`, pins what every host and home of the config
resolved to, as `flake.lock` does beside `flake.nix`. `lodi import`, `lodi switch`,
`lodi update`, `lodi pin` and `lodi unpin` write it. Its `format` is `lodi-config-lock` and its
`version` is 1. Here the digests are cut short:

```json
{
  "format": "lodi-config-lock",
  "homes": {
    "home.toml": { "tools": { "ripgrep": { "version": "15.2.0", "…": "…" } } }
  },
  "hosts": {
    "host.toml": {
      "arch": "x86_64", "distro": "debian", "release": "12",
      "pins": {
        "bc": { "filename": "pool/main/b/bc/bc_1.07.1-3+b1_amd64.deb", "policy": "date",
          "repository": "debian", "requested": "2026-09-01", "sha256": "sha256:…",
          "snapshot": "2026-09-01T00:00:00Z", "version": "1.07.1-3+b1" }
      },
      "snapshot": { "indexes": { "debian/dists/bookworm/Release": "sha256:…" },
        "instant": "2026-09-25T00:00:00Z", "requested": "2026-09-25" }
    }
  },
  "version": 1
}
```

- `hosts` has one section per `host.toml`, and `homes` one per `home.toml`, each keyed by the
  file's path from the root. A home two hosts share has one section.
- A host section holds the machine's `distro`, `release` and `arch`, the `snapshot` its
  `[host] snapshot` resolved to, and each `[packages.pin]` entry under `pins`.
- `keys` holds the checked `sha256:` digest of each `signed_by_url` key, by source name.
- A Fedora pin adds `epoch`, `release`, `arch` and `source`, the signed copy it can be fetched
  from, and has no `snapshot`.
- A home section holds its `tools`, each in the shape of a project lock's package, and its
  `profiles`, when it has any.

A missing `lodi.lock` is an empty lock. A file lodi cannot parse stops with `E_CONFIG` and is
never replaced.

## The store layout

`$LODI_HOME/.layout.json` records the shape of the whole store:

```json
{ "layout": 2, "lodiVersion": "2.0.0", "written": "2026-10-02T14:21:37Z" }
```

Layout 2 is lodi 2.0's. A store without it is not a 2.0 store. That is a store with no
`.layout.json`, one from lodi 1.x (layout 1), or one whose marker records no number. lodi reads
nothing of such a store and never migrates it. It starts the store fresh.

- `lodi develop`, `lodi run`, `lodi shell`, `lodi switch` and `lodi gc` write the layout 2
  `.layout.json` into a store without it, and nothing else.
- If another lodi holds the store, lodi prints `lodi: waiting for the store lock` and waits.
- `lodi gc --dry-run` and `lodi search` never write it.
- A newer layout stops the command with `E_STORE_VERSION`, exit 6, and changes nothing.

## When a lodi meets a file it does not read

Run the newest lodi you have against a shared store, config or host root. An older lodi stops on
a field it does not know, instead of misreading the file.

- **A 1.x host lock.** Versions 1 to 5 stop with `E_LOCK_VERSION`, which names the file.
- **A 1.x home state.** Versions 1 to 4 stop with `E_STORE_IO`, which names the file.
- **What to do.** Move the file aside, then import or switch again.
- **A newer record.** It stops the same way and names the lodi that wrote it.

A store record with no `version` is version 1. A file with no writer field is read normally.
Reading a file never rewrites it.

## Require a lodi version in a manifest

```toml
[host]
version = "1"
min_lodi_version = ">=2.0.0"
```

`min_lodi_version` works in `[project]`, `[home]` and `[host]`. It takes a constraint such as
`"0.3"` or `">=0.3.0"`, and an empty string sets no minimum.

When the running lodi does not match, or the value is not a constraint, the command stops with
`E_VERSION`, exit 3. It names both versions, before anything is fetched or written.

An older lodi stops at a table it does not know with `E_UNKNOWN_BLOCK`, exit 3.
