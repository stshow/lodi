# Write a file under /etc

Put the exact text you want in a file under `/etc`, such as `/etc/motd`, and keep it that way.

You need a config made by `lodi import`, by default in `~/.config/lodi`.

## Steps

1. Add a table to `~/.config/lodi/host.toml`, keyed by the file's path below `/etc`:

   ```toml
   [etc."motd"]
   text = """
   Welcome to the build box.
   """
   ```

2. See what the switch would change:

   ```sh
   lodi switch --dry-run
   ```

   On a fresh Debian 12 the host part of the preview says:

   ```text
   ~ file /etc/motd (content)
   W_REPLACED_UNMANAGED: /etc/motd already exists; its original bytes will be kept
   ```

3. Switch:

   ```sh
   lodi switch
   ```

   lodi writes exactly that text to `/etc/motd`. It writes no other path under `/etc` that
   `host.toml` does not name. The first time it writes over a file, it keeps the old one
   under `/etc/lodi/originals/`.

## Check it worked

```sh
cat /etc/motd
```

```text
Welcome to the build box.
```

## Keys

| Key | Default | What it does |
|---|---|---|
| `text` | none | The file's text |
| `mode` | `"0644"` | The file's mode |
| `owner`, `group` | `"root"` | The file's owner and group |

A mode with the set-uid or set-gid bit, or one anyone can write, is written as you declare it.
The preview warns about it first with `W_FILE_MODE`.

## When you edit the file by hand

The next preview shows a `W_DRIFT` line, and the switch stops with nothing changed. Put your
edit into `text`, or give `--overwrite-drift` to write the declared text back. See
[Install a package on this machine](install-a-package.md#a-file-or-package-changed-by-hand).

## When it fails

- [`E_PATH_ESCAPE`](../ERRORS.md): The path leaves `/etc`, or passes a link. Fix the path.
- [`E_PROTECTED_PATH`](../ERRORS.md): The path holds secrets or package-manager keys, such as
  `shadow`, SSH host keys or `/etc/pki`. lodi never writes it.
- [`E_DUP_RESOURCE`](../ERRORS.md): Two tables name the same file. Keep one.
- [`E_UNKNOWN_ATTR`](../ERRORS.md): Fix the key the message names.
