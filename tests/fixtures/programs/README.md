# Program-module writer goldens (M-Home c-2)

One file per writer of `src/home/render`, each the exact bytes a test module of
`tests/home_programs.rs` renders from the manifest table that test names beside it: the ownership
header in the format's own comment syntax, the body, and the table's `extra`.

| Golden | Writer | Test module |
| --- | --- | --- |
| `demo.toml` | canonical TOML, with a TOML `extra` merged | `demo` |
| `gitish.gitconfig` | git-config INI, quoting and escaping | `gitish` |
| `kvish.conf` | space-separated key-value | `kvish` |
| `tmuxish.conf` | tmux `set -g` lines | `tmuxish` |
| `luaish.lua` | Lua assignments, `--` header | `luaish` |
| `opaque.txt` | an opaque `extra` appended byte-exact (I11) | `opaque` |

They are rendered by the code, never written by hand: `LODI_RECORD_EXPECTED=1 cargo test --locked
--test home_programs` rewrites them, and an unset variable compares, which is what a gate runs. A
changed golden is a changed file on every user's disk at the next apply, so it is reviewed as one.
`opaque.txt` holds a carriage return, a tab and a control character on purpose, and no final
newline. No golden carries a date, a version, a user, a host or an absolute path (I3, I12).

## The shipped modules (M-Home pa-1)

`git/`, `bash/`, `zsh/` and `fish/` hold one directory per module this build ships, read by
`tests/home_programs_shipped.rs`: `home.toml` declares the table, `home.reordered.toml` declares
the same table in another key order and spelling, `snippets/` holds the files its `snippets` key
names, and `expected/` holds the files it renders, by file name — `config` and `ignore` for git,
`init.bash`, `init.zsh` and `lodi.fish` for the shells. Both manifests must render `expected/`
byte for byte, under any root. `LODI_RECORD_EXPECTED=1 cargo test --locked --test
home_programs_shipped` rewrites them. `bash/snippets/nonl.sh` has no final newline on purpose.
The only address in them is `someone@example`, a placeholder in the reserved `example` top-level
domain; it has no dot after the `@`, so the sanitization grep of `AGENTS.md` §1.6 does not read
it as an address, as it would `example.com`.

## The editors, terminals and prompt (M-Home pb-1)

`nvim/`, `helix/`, `alacritty/`, `kitty/`, `tmux/` and `starship/` follow the same layout and
the same recipe, read by the same test: `expected/` holds `init.lua`, `config.toml`,
`alacritty.toml`, `kitty.conf`, `tmux.conf` and `starship.toml`. Each `home.reordered.toml`
declares its table in another key order and spelling (dotted keys, sub-tables and inline tables),
and the TOML modules' reordered `extra` declares its own keys in another order too, so the
canonical merge is what makes the bytes equal. `nvim`'s golden starts with the `--` header of
Lua; every other one with `#`. The starship `init` line in the shell files has no golden of its
own: `tests/home_programs_shipped.rs` renders the three shell modules beside `[programs.starship]`
and checks the line and its place in each file.
