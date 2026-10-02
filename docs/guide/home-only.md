# Use lodi only for your home

Manage your own dotfiles, programs and tools with lodi. This works on a machine whose host you
do not manage, such as a work laptop or a shared server. You need no root, and lodi never asks
for it.

What you need:

- lodi on your `PATH`, and `git` if you want your config in git.
- No `sudo` rights.

## 1. Import your home

```sh
lodi import --home
```

```text
next: review ~/.config/lodi, commit it, then run lodi switch --home --dry-run
```

It writes `~/.config/lodi/home.toml` and nothing else. It does not ask whether lodi may manage
this host, and it reads nothing as root. A folder it creates gets `git init`, and nothing is
committed.

The new `home.toml` copies nothing from your home. It opens none of your files, so no secret
ends up in it. Every table in it is commented out, with a short note on what each one does.

A `home.toml` that is already there is kept as it is.

## 2. Declare what you want

Uncomment a table, or add your own. These pages show each kind:

- [Manage the files in your home](manage-your-home.md)
- [Let lodi write a program's configuration](configure-a-program.md)
- [Put tools on your PATH and run your own services](tools-and-user-services.md)

## 3. Preview, then switch your home

```sh
lodi switch --home --dry-run
```

```sh
lodi switch --home
```

With no `host.toml` in your config, a plain `lodi switch` has only your home to change. Typing
`--home` makes sure it never touches the host.

## 4. Commit your config

```sh
git -C ~/.config/lodi add -A
git -C ~/.config/lodi commit -m "My home"
```

On another machine, clone the config to `~/.config/lodi` and run `lodi switch --home` there.

## 5. Check it worked

```sh
lodi switch --home --dry-run
```

```text
nothing to switch (5798a24)
```

## A config with several hosts

When your config has a `config.toml`, `lodi import --home` looks up this host and your login in
it. A host with no entry there stops with [`E_CONFIG`](../ERRORS.md), and nothing is written.
The hint says what to do:

```text
   = hint: import the host with lodi import, or add this host's entry to
     ~/.config/lodi/config.toml by hand
```

[The file formats](../SCHEMAS.md#the-config-index) show how `config.toml` lists each host and
its homes.
