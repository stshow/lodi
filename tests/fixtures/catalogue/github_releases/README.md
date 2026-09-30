# `github_releases` fixtures

`<owner>--<name>.json` is a **recorded slice of the real GitHub releases API**, taken once with
`sh scripts/m04-local.sh --record` (M-0.4 T-3) and sanitized by the recorder: the six newest
releases of the repository, and of each one only the fields the strategy reads — `tag_name`,
`draft`, `prerelease`, and per asset its `name`, `size`, `digest` and `browser_download_url`.
Nothing else is written, so no account, login, avatar, e-mail, release body or timestamp is in
these files. They are the real payload in every respect the strategy can see, including the
older releases for which the API publishes **no** `digest` — that is what `E_NO_CHECKSUM` is
tested against, and it is real rather than crafted.

`crafted-odd-tags.json` is the one file here that was **not** recorded: three releases of
`sharkdp/fd` whose tags are `nightly`, `v1.2.3-rc1` and `v1.2.2`. No real repository of the
shipped recipes publishes such a tag, and the rule that a tag the template does not match is
ignored rather than mis-parsed needs one present to be proven. Its digests are not real and no
test downloads its assets.

Re-record with `sh scripts/m04-local.sh --record`, or one repository's slice alone with
`sh scripts/m04-local.sh --record --only NAME` — which is how `jqlang--jq.json` was added
without refreshing the six recorded before it, and the sixteen of the recipe batch (rb-1) the
same way, one `--only NAME` each. A recorded file changes when upstream
publishes a release; that is expected, and the tests read what the file says rather than a
version written into them.
