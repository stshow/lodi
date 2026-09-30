# `tests/fixtures/tools/`

The `[tools]` fixtures of M-0.4 T-2 (`spec/01` §3.8). They are manifests only: every artifact
they name is built byte by byte by `tests/tools.rs` and served from a loopback server, so the
checks are offline and the hashes cannot drift. A `@NAME@` placeholder is substituted by the
test with a hash of the archive it built.
