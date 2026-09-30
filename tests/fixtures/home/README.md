# Home-scope fixtures (M-0.5 T-2, T-3; M-Import T-4; M-Home)

Committed input for `tests/home_manifest.rs`, `tests/home_plan.rs` and — through
the maintainers' home-scratch script, which seeds `several` into the scratch configuration root
unless `--bare` is given (`LD-176`) — the by-hand walk-through of
`lodi home plan|apply|status`. Nothing here is generated and nothing here is a mock: each
directory is copied into a throwaway configuration root and the
**built binary** is run against it, so what these files describe is what a user would meet.

- `several/` — a home manifest with several `[files]` entries, one of each origin (`content` and
  `source`), one with a non-default `mode`, and one `state = "absent"`. `dotfiles/inputrc` is the
  `source` it names, relative to the manifest's own directory. It is the acceptance case for the
  plan's lexicographic order and its mode column, and the manifest the home-scope validation
  commands apply by hand.
- `errors/home.toml` — one manifest holding **every** load error of this task at once, so the
  test can prove that all of them are reported in a single run, sorted by position. It names
  `./nope`, which deliberately does not exist.
- `import/full/home.toml` (M-Import T-4) — the `home.toml` the **1.0** `lodi home import` emitted
  over the scratch home of 1.0's `tests/home_import.rs`, whose bundle `files/` held the dotfiles
  of 1.0's allowlist. Since M-Home im-1 (LD-341) no build writes such a bundle; the file stays as
  the 1.0 bundle `tests/home_compat.rs` populates and plans (invariant I10), and it is never
  re-recorded. Its dotfiles are hand-authored and carry no real name, address, host name or path.
- `import/stubs/home.toml` (M-Home im-1, LD-341) — the `home.toml` that `lodi home import` must
  emit, byte for byte, over the scratch home `tests/home_import.rs` builds: an unreadable file of
  markers at every path a shipped module writes or is shadowed by, and a scratch `PATH` with
  `bash`, `fish`, `git`, `jq` and `ripgrep`. It holds the commented stubs, `[tools]`, the
  `[home.file]` example and the WHAT THIS FILE DOES NOT CARRY note, and no byte of any planted
  file. It is recorded from the built binary, never hand-edited, and re-recorded only by
  `LODI_RECORD_EXPECTED=1 cargo test --locked --test home_import` (after a module is added or a
  stub changes); an unset variable compares.

- `golden-1.0.0/` (M-Home c-1, invariant I10) — what the **1.0.0 binary** printed, exit status,
  standard output and standard error, for each step of the walks in `tests/home_compat.rs` over
  seven walks: `fresh.txt` (fresh plans), `apply.txt` (apply, unchanged re-entry and refused
  drift), `removed.txt` (removed entries restored or deleted), `errors.txt` (all fixture
  diagnostics), `identical.txt` (already-identical unmanaged targets), `import.txt` (the
  scratch-populated import bundle over empty and already-identical homes), and `specimen.txt`
  (reading the state written by 1.0.0). Every comparison is direct and byte for byte, including
  every hint; there are no hint exceptions. Recorded from a `git worktree` of `v1.0.0` by
  `LODI_RECORD_EXPECTED=1 LODI_COMPAT_BINARY=<its lodi> cargo test --locked --test home_compat`
  (the recipe of `tests/fixtures/schemas/README.md`); never re-recorded from a later build. The
  scratch root is written as `<root>`.

No file here carries an absolute path: the manifests are copied next to whatever root the test
allocated, and `source` is always relative to that copy.
