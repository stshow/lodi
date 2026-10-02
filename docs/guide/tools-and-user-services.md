# Put tools on your PATH and run your own services

Get the same command-line tools, at the same versions, on every machine you use. Keep your own
background jobs running as systemd user services. Both live in `home.toml`, and neither needs
root.

What you need:

- a config with a `home.toml`, as [Manage the files in your home](manage-your-home.md) shows.
- for services, a machine that runs systemd.

## Put tools on your PATH

### 1. Declare the tools

```toml
[tools]
ripgrep = "latest"
jq = "1.8"
```

A value is a version, a version prefix such as `"1.8"`, or `"latest"`. To find a tool's name,
list the catalogue tools whose name or description has a word in it:

```sh
lodi search grep
```

### 2. Switch your home

```sh
lodi switch --home
```

lodi downloads the tools into your own store and links them into one folder. Then it prints the
line that puts that folder on your `PATH`:

```text
add this line to your shell rc file (lodi never edits it):
  . "$HOME/.local/share/lodi/home-scope/profile.sh"
```

### 3. Add the line once

lodi never edits `~/.profile`, `~/.bashrc` or `~/.zshrc`. Add the line yourself, then open a new
shell:

```sh
echo '. "$HOME/.local/share/lodi/home-scope/profile.sh"' >> ~/.bashrc
```

With fish as your shell, lodi prints `W_FISH_HOOKS` instead, because fish does not read that
file. Declare `[programs.fish]`, and its `lodi.fish` puts the tools on `PATH` by itself.

### 4. Check it worked

```sh
command -v jq
```

It prints a path that ends in `.local/share/lodi/home-scope/profile/bin/jq`. A preview now
starts with this line:

```sh
lodi switch --home --dry-run
```

```text
nothing to switch (5798a24)
```

### How the versions stay fixed

The first switch locks each tool's exact version in your config's `lodi.lock`. Every later switch
uses those versions, so commit `lodi.lock` with `home.toml`. `lodi update` looks the versions up
again and writes the new ones into `lodi.lock`. A plain `lodi switch` never moves one.

When two tools ship a program of the same name, the first by name wins, and the other prints
`W_BIN_CONFLICT`. An unknown name stops with [`E_NO_RECIPE`](../ERRORS.md), which suggests the
nearest names. For a tool the catalogue does not have, write a recipe of your own in
`recipes/NAME.toml` at your config's root.
[The recipe guide](../../catalogue/README.md#a-recipe-of-your-own) shows how.

## Run your own services

### 1. Declare the service

```toml
[services."backup.service"]
enable = true
unit = """
[Unit]
Description=Back up my notes

[Service]
ExecStart=%h/.local/bin/backup-notes

[Install]
WantedBy=default.target
"""
```

Name each unit in full, such as `backup.service` or `backup.timer`.

- **`unit`** writes the unit file into `~/.config/systemd/user`. Leave it out for a unit a package
  already installed.
- **`enable = false`** disables and stops the unit.
- **`linger = true`** keeps your services running while you are logged out. lodi turns it off
  again only if lodi turned it on.

### 2. Preview, then switch

```sh
lodi switch --home --dry-run
```

The preview lists the unit file, then one line per service with the command lodi runs, such as
`+ service backup.service (systemctl --user enable --now -- backup.service)`. Then switch:

```sh
lodi switch --home
```

lodi enables and starts the unit with `systemctl --user`, as you and never as root.

### 3. Check it worked

```sh
systemctl --user is-enabled backup.service
```

```text
enabled
```

### Take a service out

Delete its table from `home.toml` and switch. The service goes back to the way it was before
lodi managed it. A unit file lodi wrote is disabled, stopped and removed. lodi never reads or
changes a user service you did not declare.

## When it fails

| What you see | What to do |
|---|---|
| [`E_NO_RECIPE`](../ERRORS.md) | use one of the tool names the hint suggests, or write a recipe of your own |
| [`E_UNKNOWN_UNIT`](../ERRORS.md) | correct the unit's name, or write it with `unit` |
| [`E_NO_RUNTIME`](../ERRORS.md) | your systemd user manager is not running. Log in as yourself, or set `linger = true`. |

Every key of `home.toml` is in [the file formats](../SCHEMAS.md).
