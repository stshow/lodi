# Switch only your home or only the host

A plain `lodi switch` changes the host first, then your home. Run one part on its own when you
only changed one file, or when someone else looks after the machine.

What you need:

- a config made by `lodi import`, or by `lodi import --home` for the home alone.

## Switch only your home

`--home` needs no root and never asks for a password. On a shared machine, each person runs it
for their own home. lodi has no switch that changes every home at once.

### 1. Preview your home

```sh
lodi switch --home --dry-run
```

```text
create    .inputrc                  0644
1 file: 1 create
home: 1 file
config: 5798a24
```

The preview has only your home's lines. The host is not read as root and not changed.

### 2. Switch it

```sh
lodi switch --home
```

### 3. Check it worked

```sh
lodi switch --home --dry-run
```

```text
nothing to switch (5798a24)
```

## Switch only the host

Your home stays as it is. lodi asks for root through `sudo`, `doas` or `run0` only when the host
has changes. A preview never asks for a password.

**A switch removes packages.** With `packages = "exact"` in `host.toml`, a package whose line you
deleted is removed. Read the preview first.

### 1. Preview the host

```sh
lodi switch --host --dry-run
```

```text
+ package tree
host: +1 -0 packages
config: 5798a24
```

### 2. Switch it

To read the preview and answer a question before anything changes, add `--ask`:

```sh
lodi switch --host --ask
```

Without `--ask`, the switch shows the same preview and goes on. Answering no stops with
[`E_DECLINED`](../ERRORS.md), and nothing is changed.

### 3. Check it worked

```sh
lodi switch --host --dry-run
```

```text
nothing to switch (5798a24)
```

## When a part is missing

`--home` and `--host` cannot go together. A part your config does not declare stops with
[`E_NO_MANIFEST`](../ERRORS.md), and nothing is changed:

| What the error says | What to do |
|---|---|
| `declares no home for <login>, and --home runs only the home part` | run `lodi import --home`, or drop `--home` |
| `declares no host for this machine, and --host runs only the host part` | run `lodi import`, or drop `--host` |

The host part also stops until `lodi import` has asked whether lodi may manage this host. Every
flag of `lodi switch` is in [the command reference](../CLI.md#lodi-switch).
