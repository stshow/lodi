# `tests/fixtures/search/` — the fixtures of `lodi search` and `lodi info` (M-0.4 T-6)

Three files, all synthetic and all offline. Nothing here was downloaded and nothing here is a
real artifact: the hashes of the rootfs, the package list and the `.deb` files are filler, and
the only digest that has to be right is the one of the index, which the test computes.

| File | What it is |
|---|---|
| `lodi.toml` | a `[container]` project on the Debian bookworm base, pinned to the snapshot `lodi.lock` records, so `lodi info` reports `lock: fresh` until a test edits it |
| `lodi.lock` | a lock of that project, valid for `lodi::lock::validate`, with one repository and one index. `__PACKAGES_SHA256__` and `__PACKAGES_SIZE__` are placeholders: the test compresses `packages` with xz, hashes the result, substitutes both, and writes the compressed bytes into the scratch store's `cache/dl/<sha256>`, which is exactly where `lodi lock` records them and where `lodi search` looks (design call D13) |
| `packages` | four stanzas of a Debian `Packages` index, in the real `deb822` shape, with folded `Description` fields so the scan is held to reading only the first line |

`lodi search` never fetches, so no server stands behind these. Emptying the scratch `cache/dl`
is how the tests exercise the cold-cache note.
