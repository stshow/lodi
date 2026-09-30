# A layout-0 store

`store-0.3.0/` is a whole **store tree** that `lodi` 0.3.0 actually built — the shape every
released Lodi has ever written, and therefore layout 0 (`docs/SCHEMAS.md`, "The store layout").
`tests/layout.rs` restores it into a scratch `LODI_HOME` and requires this build to migrate it to
layout 1 on the first write, to leave every byte of it alone otherwise, and to still enter the
environment it holds.

Do not edit anything here. A released version's store cannot change retroactively; if something
looks wrong it is telling you something true. It is recorded again from a new tag, not adjusted.

## How it was recorded

1. A `git worktree` of the published tag `v0.3.0` was checked out into a scratch directory
   **outside** the repository and built with the repository's toolchain wrapper
   (`cargo build --locked`). No tag was created or moved.
2. A project was written with a manifest declaring one env pair and one task:

   ```toml
   [project]
   version = "1"
   name = "layout"

   [env]
   LAYOUT = "1"

   [tasks.hello]
   run = "echo hello"
   ```

3. That binary was run with `env -i` and a scratch `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and
   `LODI_HOME`: `lodi lock`, then `lodi develop -- /bin/sh -c 'echo entered; echo LAYOUT=$LAYOUT'`,
   then `lodi run hello`. The manifest resolves to no packages, so nothing was fetched and no
   network was reachable.
4. The recording ran inside an unprivileged mount namespace (`unshare -rm`) with the scratch
   directory bound over `/tmp`, so the only paths any file could record are `/tmp/rec/…` rather
   than a private scratch path — the same concession `tests/fixtures/schemas/README.md` describes,
   for the same reason. As it turned out no file in this tree records a path at all.
5. The worktree was removed afterwards.

## What is here

| Path | What it is |
| --- | --- |
| `CENSUS.tsv` | `<type> <mode> <path>` for every directory and file of the recorded store, sorted by path |
| `tree/…` | the recorded **regular files**, at their paths inside the store |
| `project/lodi.toml`, `project/lodi.lock` | the manifest and the lock the store was realized from |

The census is the fixture's authority, because git stores neither an empty directory nor a
directory's mode. `tests/layout.rs` restores the tree from it: it creates every `d` row, copies
every `f` row out of `tree/`, and then applies the recorded modes from the deepest path upwards,
so the read-only store entry (`0555`) and its read-only plan (`0444`) end up exactly as 0.3.0 left
them. It also checks the census and `tree/` agree in both directions, so neither a file added here
without a row nor a row without a file can pass unnoticed.

The store holds one realized environment, `store/env-<h32>` — its plan `env.json`, its sidecar
`store/.meta/<entry>.json`, its per-entry lock and the shared `store/.lock` — and the empty
`cache/dl`, `cache/shell`, `gcroots/sessions`, `logs` and `store/tmp` directories 0.3.0 creates.
There is deliberately **no** `.layout.json`: its absence is what makes this store layout 0, and it
is the one fact the migration is allowed to change.

## What is not here

The entry's `tree/bin` is empty because the manifest declares no packages, so this fixture proves
the migration over a store with one environment and no artifacts. A store with `store/art-…`
entries, a warm `img-…` record or a downloaded cache differs from this one only in what the 0 → 1
step already promises not to touch: that step writes the marker and nothing else, and
`src/layout.rs` pins it as additive with a unit test. Recording a store with artifacts would need
the network, and one with an image would need a container runtime; neither would test anything the
census here does not.
