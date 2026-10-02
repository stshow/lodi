# Keep your config somewhere else

Keep your config in a folder of your choice, add a second machine to it, or switch straight from
a git URL. By default your config is `~/.config/lodi`, and you never type a path.

## Use another folder

1. Type the path once, on any command:

   ```sh
   lodi import ~/dotfiles
   ```

2. From then on, lodi remembers it. Every command finds it without a path:

   ```sh
   lodi switch --dry-run
   ```

A path or URL is remembered once the command worked, in
`~/.local/state/lodi/config-path`. Type another to replace it.

To use a folder for one command, or in a script, set `LODI_REPO`:

```sh
LODI_REPO=~/dotfiles lodi switch --dry-run
```

A plain `sudo` drops `LODI_REPO`. You never need `sudo` for lodi, because lodi asks for root
itself.

### Which config a command uses

Each command takes the first of these:

1. the path or URL you type,
2. `LODI_REPO`, when it is set,
3. the remembered path,
4. `~/.config/lodi`.

lodi never looks in the current folder. A folder holding a project's `lodi.toml` is never a
config. A path inside a config, such as `~/dotfiles/files`, stops and names the config's root.

## Add a second machine

1. On the second machine, clone your config and import:

   ```sh
   git clone https://example.org/you/lodi.git ~/.config/lodi
   lodi import
   ```

   The import writes this machine to `<hostname>/host.toml`, beside the first. It writes
   `config.toml`, which says which `host.toml` is which machine:

   ```toml
   [hosts.desktop]
   host = "host.toml"
   homes = { you = "home.toml" }

   [hosts.laptop]
   host = "laptop/host.toml"
   homes = { you = "home.toml" }
   ```

   Both machines share one `home.toml`.

2. Commit and push:

   ```sh
   git -C ~/.config/lodi add -A
   git -C ~/.config/lodi commit -qm 'add laptop'
   ```

`lodi switch` picks the host named as the machine's hostname. `lodi pin`, `unpin` and `update`
take `--host NAME` for the other one.

## Switch from a git URL

`lodi switch` also takes a public git URL:

```sh
lodi switch --dry-run github:you/lodi
```

The preview's first line names the source, the ref and the commit. `github:`, `gitlab:` and
`codeberg:` expand to that site's `https://` address.

- **Whoever can push to the config controls root on this machine.** Switch only from one whose
  writers you would give root.
- **The commit is locked.** Later switches use the same commit and fetch nothing. `--ref NAME`
  picks a branch or tag, `--rev COMMIT` one commit, and `--refresh` the newest one.
- **lodi never writes into a fetched config.** Pin and import in a clone, push, then switch
  with `--refresh`.
- **Only `https://` works.** A private config works from a local clone.

## Files lodi keeps on the machine

| Path | What it is |
|---|---|
| `/etc/lodi/may-manage` | Written by `lodi import` when you allow lodi to manage the machine |
| `/etc/lodi/host.lock` | What the last switch did. Never a pin |
| `/etc/lodi/originals/` | Copies of files lodi wrote over |
| `/var/lib/lodi/host/journal/` | One journal per switch, so a stopped switch can go on |
| `~/.local/state/lodi/config-path` | The remembered config |
| `~/.local/state/lodi/logs/` | One log per run, the 20 newest |
| `~/.local/state/lodi/git/` | The commits of configs fetched from a URL |

`lodi.lock`, in your config, holds your pins and your home's tools. [The file
formats](../SCHEMAS.md#the-config-index) describe `config.toml` and the lock.

## Check it worked

Run any command with no path. The preview's last line names the commit of the config it used:

```sh
lodi switch --dry-run
```

## When it fails

- [`E_NO_MANIFEST`](../ERRORS.md): No config was found, or the remembered folder is gone. Type its
  path once.
- [`E_CONFIG`](../ERRORS.md): The folder is not a config to import into, or `config.toml` has an
  unknown key.
- [`E_PATH_ESCAPE`](../ERRORS.md): A folder on the way is a link, or others can write it. Fix the
  owner or mode it names.
- [`E_INSECURE_URL`](../ERRORS.md): Use an `https://` address.
- [`E_FETCH`](../ERRORS.md): The URL could not be fetched. Nothing changed.
- [`E_HOST_ROOT_REQUIRED`](../ERRORS.md): `LODI_HOST_REQUIRE_ROOT=1` is set. Unset it, or name a
  scratch tree with `--root DIR`.
- [`E_STORE_IO`](../ERRORS.md): A file could not be written. What the run created is removed.
