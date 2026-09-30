# JSON build-index fixture

`index.json` is a **recorded slice of the real release index** the recipe discovers versions
from, taken once with `sh scripts/m04-local.sh --record --case toolchains` (M-0.4 T-4) and
sanitized by the recorder: the eight newest entries, of each one `version`, `stable` and its
`files` list, and of each file only the fields the strategy reads — `filename`, `os`, `arch`,
`version`, `sha256`, `size` and `kind` — and only the files this architecture's operating
system publishes. Nothing else is written, so no URL beyond the ones the recipe renders itself,
no timestamp and nothing about the machine that recorded it is in this file.

It is the real payload in every respect the strategy can see, including the pre-release entries
whose version string is not a version at all (`…rc3`) and the builds for other architectures:
that is the noise the file-name template has to walk past, and it is real rather than crafted.
The `sha256` each entry carries is the real digest upstream publishes — that is what
`E_HASH_MISMATCH` is tested against in `tests/text_index.rs`, where the bytes served under the
same URL are a synthetic archive and therefore do **not** hash to it.

Re-record with `sh scripts/m04-local.sh --record --case toolchains`. The file changes when
upstream publishes a release; that is expected, and the tests read what the file says rather
than a version written into them.
