# Channel-manifest fixture

`channel-rust-stable.toml` is a **recorded slice of the real stable channel manifest** the
recipe discovers the version from, taken once with the maintainers' recording probe
(`--record --case toolchains`, M-0.4 T-4) and sanitized by the recorder: every line of the real
document is kept except the per-target sections of targets other than
`x86_64-unknown-linux-gnu` and one decoy, `aarch64-apple-darwin`. The header, every package's
own `version` line and every component are the real file unchanged — that is the noise the
`line` template has to walk past, and it is real rather than crafted, including the several
distinct version strings the document carries for its own components.

The decoy target is there so the slice proves that the recipe reads the target it asked for out
of a document that describes many. Nothing in this file identifies a machine, an account or a
time of day; the dated URLs it carries are upstream's own published paths.

No digest in this file is used by a test: this recipe verifies each artifact against the
`.sha256` sidecar upstream publishes beside it, which `tests/text_index.rs` serves from the
loopback fixture server for the synthetic archives it builds.

Re-record with the maintainers' recording probe (`--record --case toolchains`). The file changes
when upstream promotes a new stable release; that is expected, and the tests read what the file says
rather than a version written into them.
