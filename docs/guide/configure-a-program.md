# Let lodi write a program's configuration

Set up git, your shell, your editor or your terminal from a few keys in `home.toml`. lodi
writes the program's own configuration file for you.

What you need:

- a config with a `home.toml`, as [Manage the files in your home](manage-your-home.md) shows.

## 1. Declare the program

Add a `[programs.NAME]` table to `home.toml`:

```toml
[programs.git]
user.name = "Example User"
init.defaultBranch = "main"
```

## 2. Preview and switch

```sh
lodi switch --home --dry-run
```

The preview names the file and the table it comes from, such as
`create .config/git/config 0644 (programs.git)`. Then write it:

```sh
lodi switch --home
```

## 3. Check it worked

```sh
head -2 ~/.config/git/config
```

```text
# Managed by lodi from home.toml [programs.git]. Edit home.toml, not this file:
# a hand edit here is drift, and `lodi switch` stops on it.
```

To change a setting, edit `home.toml` and switch again. A hand edit of the written file stops
the next switch, as
[When you edited a file by hand](manage-your-home.md#when-you-edited-a-file-by-hand) explains.

## The programs lodi can configure

Every table also takes `enable`, `backup`, `on_remove` and `extra`, which is text copied as it
is. For a program with no table, declare its file with `[home.file]`. Any other name stops with
[`E_UNKNOWN_BLOCK`](../ERRORS.md), which lists the ten names below.

| Table | The file it writes | Keys |
|---|---|---|
| `git` | `~/.config/git/config`, and `~/.config/git/ignore` when `ignores` is set | `user.name`, `user.email`, `init.defaultBranch`, `core.editor`, `core.pager`, `pull.rebase`, `push.autoSetupRemote`, `merge.conflictStyle`, `alias`, `ignores` |
| `bash` | `~/.local/share/lodi/home-scope/shell/init.bash` | `env`, `path`, `aliases`, `snippets`, `histsize`, `histfilesize`, `histcontrol`, `shopt` |
| `zsh` | `~/.local/share/lodi/home-scope/shell/init.zsh` | `env`, `path`, `aliases`, `snippets`, `histsize`, `savehist`, `setopt` |
| `fish` | `~/.config/fish/conf.d/lodi.fish` | `env`, `path`, `aliases`, `snippets`, `abbrs`, `greeting` |
| `nvim` | `~/.config/nvim/init.lua` | `leader`, `opt.number`, `opt.relativenumber`, `opt.expandtab`, `opt.ignorecase`, `opt.smartcase`, `opt.termguicolors`, `opt.shiftwidth`, `opt.tabstop`, `opt.mouse`, `opt.clipboard`, `colorscheme` |
| `helix` | `~/.config/helix/config.toml` | `theme`, `editor.line-number`, `editor.mouse`, `editor.cursorline`, `editor.auto-format`, `editor.true-color`, `editor.rulers`, `editor.cursor-shape.insert`, `.normal`, `.select` |
| `alacritty` | `~/.config/alacritty/alacritty.toml` | `font.size`, `font.normal.family`, `window.opacity`, `window.padding.x`, `window.padding.y`, `window.decorations`, `scrolling.history` |
| `kitty` | `~/.config/kitty/kitty.conf` | `font_family`, `font_size`, `background_opacity`, `scrollback_lines`, `enable_audio_bell`, `confirm_os_window_close`, `map` |
| `tmux` | `~/.config/tmux/tmux.conf` | `prefix`, `mouse`, `status`, `base-index`, `escape-time`, `history-limit`, `default-terminal`, `mode-keys` |
| `starship` | `~/.config/starship.toml` | `format`, `add_newline`, `command_timeout`, `scan_timeout`, `character.success_symbol`, `character.error_symbol`, `shells` |

## Set up your shell

```toml
[programs.bash]
env = { EDITOR = "nvim" }
path = ["~/.local/bin"]
aliases = { ll = "ls -l" }
snippets = ["snippets/prompt.sh"]
```

lodi never opens `~/.bashrc` or `~/.zshrc`. After a switch it prints the line to add there
yourself:

```text
add this line to your bash rc file (lodi never edits it):
  . "$HOME/.local/share/lodi/home-scope/shell/init.bash"
```

Fish reads `conf.d/lodi.fish` on its own, so it needs no line.

In the shell files a value is single-quoted, so a `$` stays as it is. A leading `~/` becomes
`"$HOME"/`. A `snippets` file sits beside `home.toml` and is copied in after the typed lines. A
`path` entry is added only when it is not on `PATH` yet, so sourcing a file twice does no harm.

`[programs.starship]` adds one line to each shell file, which starts starship only when it is on
`PATH`. `shells` picks the shells that get the line. Without it, every shell table you declare
gets it, and `shells = []` adds it to none.

## The values each key takes

A value goes on one line of the file. A newline in a value, or a value outside the ranges below,
stops with [`E_TYPE`](../ERRORS.md), and the hint gives the allowed values.

```toml
[programs.helix]
theme = "onedark"
editor.line-number = "relative"
```

| Key | Allowed values |
|---|---|
| `editor.line-number` | `absolute` or `relative` |
| `editor.cursor-shape.*` | `block`, `bar`, `underline` or `hidden` |
| `window.decorations` | `Full`, `None`, `Transparent` or `Buttonless` |
| `mode-keys` | `vi` or `emacs` |
| `shells` | `bash`, `zsh` or `fish` |
| `font.size`, `font_size` | a number above 0 |
| `window.opacity`, `background_opacity` | from 0 to 1 |
| `scrolling.history` | from 0 to 100000 |
| `editor.rulers` | columns from 1 to 65535 |
| any other number | 0 or more, and at most 2147483647 for nvim, tmux and alacritty padding |

For helix, alacritty and starship, `extra` is TOML added to the typed keys. A key set in both
places stops with [`E_ATTR_CONFLICT`](../ERRORS.md).

## When the program is missing or reads another file

A table whose program is not on `PATH` prints `W_PROGRAM_NOT_FOUND`. lodi still writes its
files and installs nothing. Helix's program is `hx`, so check for that name:

```sh
command -v hx
```

An older file the program reads first prints [`W_SHADOWED`](../ERRORS.md):

| Table | The older file |
|---|---|
| `git` | `~/.gitconfig` |
| `nvim` | `~/.config/nvim/init.vim` |
| `tmux` | `~/.tmux.conf` |

Move its settings into the table, then delete the older file.

Some programs read these files only from a given version. Alacritty reads `alacritty.toml` from
version 0.13 on. Tmux reads `~/.config/tmux/tmux.conf` from version 3.1 on. Starship 1.26 reads
`~/.config/starship.toml` even when `XDG_CONFIG_HOME` points elsewhere.
