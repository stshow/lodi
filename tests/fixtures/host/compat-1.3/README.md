# 1.3 compatibility goldens (M-Pin P3, LD-395)

One transcript per walk of `tests/host_pin.rs` (`compat_1_3_*`): every host manifest committed
under `tests/fixtures/host/import/*/*/expected.toml` in its unpinned form — its `[host] snapshot`
line taken out, and no `[packages.pin]` and no `pins.lock` — and one `[files]` manifest, each
planned, applied and planned again against the fake machine of `tests/support/fakepm.py`, with
the `host.lock` and the journal the apply left (the journal records every argv). This build must
reproduce them byte for byte: a host with nothing pinned plans, simulates and applies exactly as
1.3 does. The walk is `tests/support/compat.rs`, the walk of the 1.2 goldens
(`tests/fixtures/host/compat-1.2/README.md`), with the same placeholders.

Recording (never a validation command), by the procedure of `compat-1.2/README.md`:

1. `git worktree add --detach <scratch>/compat-1.3.0 <the 1.3.0 release commit>`, outside the
   repository;
2. build it with the repository's toolchain:
   `sh scripts/toolchain.sh cargo build --locked --manifest-path <scratch>/compat-1.3.0/Cargo.toml --bin lodi`
   with `CARGO_TARGET_DIR` in the scratch directory;
3. `LODI_RECORD_EXPECTED=1 LODI_COMPAT_BINARY=<that build's lodi> cargo test --locked --test host_pin compat_1_3`;
4. remove the worktree and its target.

`v1.3.0` did not exist when M-Pin's core package built, so the goldens in this directory were
first recorded that way from commit `4fe59ac` of `origin/main` (1.3's M-Sources on `main`, version
1.2.0 in `Cargo.toml`). Before the package landed they were recorded again from the `v1.3.0`
release commit `ae1604a`, built in the scratch worktree's own `target/` (no `CARGO_TARGET_DIR`),
with `COMPAT_RECORDED` in `tests/host_pin.rs` set to `1.3.0`, the version that binary reports and
the transcripts carry. Only that version string moved; the walks were otherwise byte for byte the
same. LD-414 added `debian-12/own-suite` and `ubuntu-24.04/proposed`; R8 added
`debian-12/mixed-backports` and `ubuntu-24.04/one-line-proposed`, recorded from that same
released binary. The original 21 transcripts remain unchanged. LD-396 keeps three historical
Arch `arch-*.toml` inputs copied from the published tag's import fixtures. Arch import now
changes its snapshot and comment; the unpinned walk uses the historical inputs. The 1.3
goldens themselves are unchanged.
