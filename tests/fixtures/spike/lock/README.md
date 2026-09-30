# S-2 lock fixtures (synthetic)

Every file here is **synthetic**: it has the shape of the real upstream metadata (observed
2026-09-19 for the formats only) but its versions, sizes and hashes are invented. No file was
copied from a live index, and none of the hashes identifies a real artifact. Hashes are
`sha256("synthetic:" + <file name>)`.

| File | Shape of | Used as |
|---|---|---|
| `bookworm.Packages`, `bookworm-updates.Packages`, `bookworm-security.Packages` | Debian `Packages` index stanzas | `dists/<suite>/main/binary-amd64/Packages.xz` on the snapshot service |
| `rootfs.manifest` | debuerreotype `rootfs.manifest` (`name<TAB>version`) | the base rootfs package list |
| `nodejs-index.json` | `https://nodejs.org/dist/index.json` | the Node.js release index |
| `python-releases.json` | GitHub releases API for python-build-standalone | the release list with asset `digest` fields |

`tests/spike_lock.rs` derives the rest from these files at test time, so the derived metadata
is consistent by construction: the xz-compressed indexes and the `Release` files that list their
SHA-256 and size, the OCI `index.json` and image manifest of the base, the `SHA256SUMS` and
`SHASUMS256.txt` checksum files, and the branch reference of the base repository. Tests then
tamper with one piece at a time.

What the synthetic Debian data exercises: a transitive closure (`build-essential` pulls the
compiler chain), alternatives (`libc6-dev | libc-dev`), virtual names with one provider
(`awk` from the base's `mawk`) and several (`c-compiler`), `:any` qualifiers (`perl:any`), the
highest version across suites (`libssl-dev` from `bookworm-security`), a base package upgraded
because a dependency needs an exact newer version (`libc6`), a base package absent from every
index (`tzdata`), a versioned `Breaks` that does not apply (`pkgconf`), an unsatisfiable version
(`spike-needs-future-ssl`), a foreign-architecture dependency (`spike-needs-i386`) and a
conflict (`spike-conflicts-make`).

Live discovery against snapshot.debian.org, GitHub and nodejs.org is S-5's evidence, not these
fixtures'.
