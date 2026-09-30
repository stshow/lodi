# `adoptium` fixtures

These are **recorded slices of the real Adoptium API**, taken once with
`sh scripts/m04-local.sh --record` (M-0.4 T-5) and sanitized by the recorder, which keeps only
the fields the strategy reads and drops every other one rather than redacting it. No
identifier, timestamp, download count, release note, vendor field or source link is written.

- `available-releases.json` — `/v3/info/available_releases`: `available_lts_releases` (which is
  the only field the strategy reads), beside `available_releases`,
  `most_recent_feature_release`, `most_recent_feature_version`, `most_recent_lts` and
  `tip_version`. Those extra fields are what makes the file evidence rather than an assertion:
  the newest line the API knows is an **early-access** one well above the newest LTS, so a
  `latest` that resolved to the newest line instead of the newest LTS would be visible here.

- `feature-releases-21.json` — one page of `/v3/assets/feature_releases/21/ga?os=linux&
  architecture=x64&image_type=jdk&jvm_impl=hotspot`, the newest six releases, each with its
  `version_data` (`major`, `minor`, `security`, `patch` where the release has one, `build`,
  `semver`) and its binary's `package` (`name`, `link`, `checksum`, `size`). The two newest
  entries are `21.0.12+101.0.LTS` and `21.0.12+8.0.LTS` — the first is OpenJDK `21.0.12.1+1`,
  a patched build of the second — which is the real pair that proves the ordering key is built
  from those fields and not from the semver string, whose build metadata version precedence
  ignores.

- `feature-releases-<N>.json` for the newest LTS line at recording time — the page `latest`
  resolves to, through `available-releases.json`.

- `no-checksum.json` is the one file here that was **not** recorded as it stands: it is
  `feature-releases-21.json` with the newest binary's `package.checksum` removed. Adoptium
  publishes a checksum for every binary, so the rule that an asset with no digest is
  `E_NO_CHECKSUM` and **not** a download needs a page that does not. Nothing else was changed,
  and no test downloads anything from this file.

- `both-url-and-adoptium.toml` is a crafted **recipe**, not API output: a recipe that declares
  `strategy = "adoptium"` and an `[asset] url` at the same time, which is `E_RECIPE_INVALID`.
  The shipped `catalogue/tools/jdk.toml` cannot prove that refusal, because a shipped recipe
  that was refused would fail the catalogue lint long before a test ran.

Re-record with `sh scripts/m04-local.sh --record`. A recorded file changes when Adoptium
publishes a release; that is expected, and the tests read what the files say rather than a
version written into them.
