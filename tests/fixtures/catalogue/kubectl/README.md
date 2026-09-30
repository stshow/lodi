# `kubectl` fixtures

`stable.txt` is the **recorded real discovery document** the `kubectl` recipe reads, and
`kubectl.sha256` the recorded real `.sha256` sidecar of the asset that
document names. Each is kept verbatim: one line, a version or a bare digest, with nothing that
identifies a machine, an account or a time of day. Only the current version is discoverable from
this document.

The tests serve synthetic artifacts, so the sidecar they serve beside one is that artifact's own
digest; the recorded sidecar is what `lodi lock` must pin when it is served as recorded.

Re-record with the maintainers' recording probe (`--record --only kubectl`). The files change when
upstream publishes a new release; that is expected, and the tests read what the files say rather
than a version written into them.
