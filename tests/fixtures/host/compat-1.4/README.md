# 1.4 compatibility goldens (fh-1 H1, LD-399)

One transcript per walk of `tests/host_flake_home.rs` (`compat_1_4_*`), with the placeholders of
`tests/fixtures/host/compat-1.2/README.md`, from the walk of `tests/support/compat.rs`:

- `in-place.txt`: a `[files]` manifest in place (`etc/lodi` of the root) with no `home/`,
  planned, applied and
  planned again, with the `host.lock` and the journal (every argv);
- `directory.txt`: the same manifest in a host directory named by `SOURCE`, with no `home/`;
- `import.txt`: an import in place, its output and every file it left under `etc/lodi`, with
  each path below the scratch root written relative to it.

Each root's `etc/passwd` names only root: with no `home/`, a 1.5 build reads no passwd file
(H1(a)). This build must reproduce them byte for byte.

Recording (never a validation command), by the procedure of `compat-1.2/README.md`:

1. `git worktree add --detach <scratch>/compat-1.4 <the recorded commit>`, outside the repository;
2. build it: `cargo build --locked --manifest-path <scratch>/compat-1.4/Cargo.toml --bin lodi`,
   with `CARGO_TARGET_DIR` in the scratch directory;
3. `LODI_RECORD_EXPECTED=1 LODI_COMPAT_BINARY=<that build's lodi> cargo test --locked --test
   host_flake_home compat_1_4`, with `COMPAT_RECORDED` in `tests/host_flake_home.rs` set to the
   version that binary reports;
4. remove the worktree and its target.

They were recorded from `c97370e`, the commit the `v1.4.0` release tag names (version 1.4.0 in
`Cargo.toml`); earlier drafts had recorded them from `origin/main` `64d4a7b` before `v1.4.0`
existed.

## `locks/`: 1.4's per-folder locks (fv-1, LD-416)

`locks/pins.lock` and `locks/home.lock` are what 1.4.1 itself wrote, and `locks/host.toml` and
`locks/home.toml` the manifests they lock. `tests/root_lock.rs` moves them into a repository's
root `lodi.lock` and requires every entry unchanged. Recorded by the procedure above from
`64dc857`, the commit the `v1.4.1` release tag names (version 1.4.1 in `Cargo.toml`), with
`LODI_RECORD_EXPECTED=1 LODI_COMPAT_BINARY=<that build's lodi> cargo test --locked --test root_lock
record_1_4_folder_locks`: 1.4.1's `host pin --all --to 2025-03-01T00:00:00Z` and `host pin tzdata
--to 2025-03-01` on the fake machine and recorded archives of `tests/support/pinverbs.rs` wrote
`pins.lock`, and its `home apply` of one inline tool served on loopback wrote `home.lock`.
