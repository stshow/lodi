# Manage the files in your home

Keep your dotfiles the way you declared them. You write each file into `home.toml`, preview the
change, then switch. None of this needs root.

What you need:

- lodi, and a config with a `home.toml`. `lodi import` writes one, and
  [Use lodi only for your home](home-only.md) shows `lodi import --home`.
- Your `home.toml` is `~/.config/lodi/home.toml` unless your config is somewhere else.

## 1. Declare a file

Add an entry to `home.toml`. Its name is the file's path from your home folder:

```toml
[home.file.".inputrc"]
text = "set editing-mode vi\n"
mode = "0644"
on_remove = "restore"
```

`text` is the whole content of the file. `mode` and `on_remove` are optional. The table below
lists every key.

## 2. Preview the change

```sh
lodi switch --home --dry-run
```

```text
create    .inputrc                  0644
1 file: 1 create
home: 1 file
config: 5798a24
```

There is one line per file, then a count. The last line is the commit of your config. A preview
writes nothing.

## 3. Switch your home

If a file is already at that path and lodi did not write it, lodi keeps one copy of it in its
backup folder first. It prints `W_REPLACED_UNMANAGED` with the name of that copy.

```sh
lodi switch --home
```

Run it again, and nothing changes. Not even a file's time stamp moves.

## 4. Check it worked

```sh
cat ~/.inputrc
```

```text
set editing-mode vi
```

```sh
lodi switch --home --dry-run
```

```text
nothing to switch (5798a24)
```

## The keys of a file entry

| Key | What it does | Default |
|---|---|---|
| `text` | the file's content, written inline | |
| `source` | a file beside `home.toml` to copy. A folder becomes one entry per file in it. | |
| `mode` | the permissions, as an octal string such as `"0600"` | `"0644"` |
| `executable` | `true` sets the mode to `0755` | `false` |
| `state` | `present`, `absent`, or `seed` to write the file once and leave it to you after that | `present` |
| `backup` | keep one copy of what was at the path before lodi took it over | `true` |
| `on_remove` | what happens when the entry leaves `home.toml`: `restore`, `delete` or `keep` | `restore`, or `keep` for a seed |

Give each entry one of `text` or `source`, not both. `[home.xdg_config."PATH"]` takes the same
keys, with a path from `~/.config`, or from `$XDG_CONFIG_HOME` when it is set:

```toml
[home.xdg_config."git/ignore"]
source = "dotfiles/gitignore"
```

A path must stay inside your home. lodi lists every mistake in the file in one run, each with its
line and column, and writes nothing:

```text
lodi: error E_PATH_ESCAPE: `../x` is not a path inside the root: it has a `..` component
  --> ~/.config/lodi/home.toml:4:12
   = hint: paths in the home scope are relative to the root and may not leave it
```

## When you edited a file by hand

A file lodi wrote and you changed afterwards stops the whole switch of your home. Nothing is
written, not even the other files:

```sh
lodi switch --home
```

```text
lodi: error E_DRIFT: 1 managed file has changed since lodi wrote it; nothing was applied
   = .inputrc
   = hint: lodi switch --home --overwrite-drift takes them back
```

In the preview, `lodi switch --home --dry-run`, that file's line starts with `drift`.

You have two ways out:

- Copy your edit into `home.toml`, then switch again.
- Take the file back to what `home.toml` declares.

**`--overwrite-drift` replaces your edit.** lodi keeps the edited content in its backup folder
and prints `W_DRIFT` with the name of that copy. Your edit is gone from the file itself.

```sh
lodi switch --home --overwrite-drift
```

## Remove a file from your home

Delete its entry from `home.toml`, then preview. With `on_remove = "restore"`, the default, the
file you had before lodi wrote it comes back:

```sh
lodi switch --home --dry-run
```

```text
restore   .inputrc                  0644    (entry removed: the original is restored)
```

With `delete` the file is removed, and with `keep` it stays as it is. Then run
`lodi switch --home`.

## Move an old `[files]` table

A `[files]` table stops with [`E_UNKNOWN_BLOCK`](../ERRORS.md), and nothing is written. Move each
entry to `[home.file]`, with `text` in place of `content`:

```toml
[home.file.".inputrc"]
text = "set editing-mode vi\n"
```

## When it fails

| What you see | What to do |
|---|---|
| [`E_DRIFT`](../ERRORS.md) | a file was edited by hand. See [When you edited a file by hand](#when-you-edited-a-file-by-hand). |
| [`E_PATH_ESCAPE`](../ERRORS.md) | use a path inside your home, with no symbolic link in it |
| [`E_ATTR_CONFLICT`](../ERRORS.md) | give `text` or `source`, and `executable` or `mode`, not both |
| [`E_DUP_RESOURCE`](../ERRORS.md) | the same path is declared twice. Remove one entry. |
| [`E_TYPE`](../ERRORS.md) | fix the key or value at the line and column shown |
| [`E_UNSUPPORTED`](../ERRORS.md) | remove the table named, such as `[vars]` or `[modules]` |
| [`E_VERSION`](../ERRORS.md) | install the newer lodi that `min_lodi_version` asks for |
| [`E_CONFIG`](../ERRORS.md) | a `source` is missing, or `XDG_CONFIG_HOME` is outside your home |
| [`E_STORE_PERM`](../ERRORS.md) | make the folder named writable for you |

Every key of `home.toml` is in [the file formats](../SCHEMAS.md). The other home pages are
[Let lodi write a program's configuration](configure-a-program.md) and
[Put tools on your PATH and run your own services](tools-and-user-services.md).
