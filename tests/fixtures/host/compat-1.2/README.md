# 1.2 compatibility goldens (LD-365, M-Sources §7)

One transcript per walk of `tests/host_sources.rs` (`compat_1_2_*`): every host manifest committed
under `tests/fixtures/host/import/*/*/expected.toml`, and one `[files]` manifest, each planned,
applied and planned again against the fake machine of `tests/support/fakepm.py`, with the
`host.lock` and the journal the apply left. They were recorded from the 1.2.0 binary, and this
build must reproduce them byte for byte: a manifest without `[sources]` plans and applies exactly
as it did.

What cannot hold still is written as a placeholder, the same way for both binaries: the scratch
root `<root>`, the fake machine's directory `<fake>`, every UTC instant `<time>`, a journal id
`<journal>`, a backup's `<stamp>`, the invoking user's ids (`<uid>`, `<gid>`, `<id>`), and the
running binary's own version, written back as `1.2.0`.

Recording (never a validation command):

1. `git worktree add --detach <scratch>/compat-1.2.0 <the 1.2.0 release commit>`, outside the
   repository;
2. build it with the repository's toolchain:
   `sh scripts/toolchain.sh cargo build --locked --manifest-path <scratch>/compat-1.2.0/Cargo.toml --bin lodi`
   with `CARGO_TARGET_DIR` in the scratch directory;
3. `LODI_RECORD_EXPECTED=1 LODI_COMPAT_BINARY=<that build's lodi> cargo test --locked --test host_sources compat_1_2`;
4. remove the worktree and its target.

The goldens in this directory were recorded that way from commit `8b4a565` (release: version 1.2.0).
They were recorded again the same way, from the same commit, when LD-367 changed the import's
comments in every `expected.toml` and added five scenarios: only each walk's `manifestDigest`
moved, because the manifest's bytes did.
They were recorded again the same way, from the same commit, when M-Pin made `[host] snapshot`
live (LD-395): each import manifest is walked in its unpinned form, its `snapshot` line taken out
(`tests/support/fakehost.rs` `without_snapshot`), because the walk holds a build to 1.2 for a
host with nothing pinned; and the import's comments changed again (P11). Only each walk's
`manifestDigest` moved. LD-414 added two import fixtures (`debian-12/own-suite`,
`ubuntu-24.04/proposed`); their unpinned manifests were also recorded from the 1.2.0 binary at
`8b4a565`. R8 added `debian-12/mixed-backports` and `ubuntu-24.04/one-line-proposed`, recorded from
that same released binary. The walk now covers 23 manifests; the original 21 transcripts remain
unchanged.
