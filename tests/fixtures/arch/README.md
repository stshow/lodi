# Arch repository fixtures

`core-2024-09-01.slice.db.hex` is a hex encoding of a 555-byte gzip-tar slice of the Arch Linux
Archive database served as `repos/2024/09/01/core/os/x86_64/core.db`. It contains the two `desc`
members for `acl` and `archlinux-keyring` (10,240 bytes after gzip decompression). The outer tar
was rebuilt deterministically from those members so the repository does not commit the complete
database. The source database is Arch Linux repository metadata and is redistributed only as the
small format fixture needed to exercise this parser.

The other databases in `tests/arch_db.rs` are synthetic and built at test time with the shared
raw tar and gzip helpers. They exist to exercise individual fields, malformed inputs and bounds;
they are not presented as evidence of the upstream format.

`resolver-packages.tsv` is a synthetic, committed resolver fixture. Its eight tab-separated
columns name packages, versions, dependency relations, provides, replaces, conflicts, groups and
descriptions. `tests/arch_resolve.rs` turns those rows into the same parsed package values the
resolver consumes; no row is represented as upstream data.

`iso-listing.html`, `core.desc`, `extra.desc` and `bootstrap-rootfs.synthetic` are synthetic,
reviewable inputs for the T-4 pinning tests. The test wraps each desc stanza in the real gzip-tar
database format and derives every digest at runtime. The synthetic rootfs payload is deliberately
not an archive: T-4 verifies a previously recorded artifact hash but never imports it.

`lodi.lock` and the golden files under `build/` are synthetic, reviewable T-5 fixtures. The lock
is a hand-written canonical-JSON pin of `arch` / `rolling` / `x86_64` at a fixed snapshot: its
repository URLs have the shape the archive serves, and every artifact hash in it is the SHA-256 of
the ASCII string `lodi fixture: <label>`, so each one is re-derivable by hand and none is presented
as an upstream digest. Its closure is six packages, two of them requested. `tests/arch_image.rs`
renders the build files from that lock and compares them byte for byte with `build/Containerfile`,
`build/Containerfile.no-packages`, `build/pacman.conf` and `build/mirrorlist`; `build/read-back.txt`
is a read-back transcript in the upstream query's record format, written for these fixture packages.
The tests that drive a whole image build from this lock run against a recorded stand-in for the
container runtime, never a real image; the real build was measured by the milestone's probe.
