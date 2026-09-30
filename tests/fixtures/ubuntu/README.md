# T-4 Ubuntu base fixtures (synthetic)

Every file here is **synthetic**: it has the shape of the real Ubuntu metadata (the formats were
observed on 2026-09-20 against `snapshot.ubuntu.com` and
`partner-images.canonical.com/oci/noble/`) but its versions, sizes and hashes are invented. No
file was copied from a live index, and none of the hashes identifies a real artifact. Hashes are
`sha256("synthetic:" + <pool file name>)`, as in `tests/fixtures/spike/lock/`.

| File | Shape of | Used as |
|---|---|---|
| `noble.Packages`, `noble-updates.Packages`, `noble-security.Packages` | Ubuntu `Packages` index stanzas | `dists/<suite>/main/binary-amd64/Packages.xz` on the snapshot service |
| `noble-universe.Packages` | the same, for the second component | `dists/noble/universe/binary-amd64/Packages.xz` |
| `ubuntu-noble-oci-amd64-root.manifest` | the package list Canonical publishes beside the rootfs tarball (`name<TAB>version`) | the base rootfs package list |

`tests/ubuntu_base.rs` derives the rest at test time, so the derived metadata is consistent by
construction: the xz-compressed indexes, the three `Release` files that list their SHA-256 and
size, the dated-directory HTML listing of the image server and the `SHA256SUMS` that pins the
rootfs tarball and this package list. Tests then tamper with one piece at a time.

What the synthetic Ubuntu data exercises, beside the plain closure: two components in one
repository (`main` and `universe`, where Debian's definition has one component in two archives),
three suites in **one** archive (`noble`, `noble-updates`, `noble-security` — Ubuntu carries
security where Debian has a separate host), a `Release` whose `Codename` is the bare `noble`
while its `Suite` is `noble-updates`, a base package upgraded because a dependency needs a newer
version (`libc6` from `noble-updates`), the highest version across suites (`libssl3t64` from
`noble-security`), alternatives (`libc6-dev | libc-dev`), a virtual name with one provider
(`awk`) and several (`c-compiler`, once `universe` is in the index), a `:any` qualifier
(`perl:any`), a base package in no index (`tzdata`), an unsatisfiable version
(`ubuntu-needs-future-ssl`), a foreign-architecture dependency (`ubuntu-needs-i386`) and a
conflict (`ubuntu-conflicts-make`).

Live discovery against `snapshot.ubuntu.com` and the image server is the evidence of
`sh scripts/m03-local.sh --case distros`, not these fixtures'.
