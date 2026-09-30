# Dependency licences of the shipped Lodi binary

Lodi is **GPL-3.0-or-later** (`LICENSE`). The released binary is a static musl
executable, so every crate in its dependency closure is *distributed inside it*: each one must
be under a licence that a GPL-3.0-or-later work may incorporate. This file is that audit, and
`python3 -B scripts/check-licenses.py` — a step of `scripts/gate.sh` — is the deterministic,
offline check that it is still true.

## 1. How the list was produced

```sh
cargo metadata --locked --offline --format-version 1
```

resolved from the committed `Cargo.lock`, walking the dependency graph from this workspace's own
package and following every edge whose kinds are not **dev only**. A dev-dependency is never
linked into the released binary, so it is out of the closure; **platform-specific edges are
kept**, which makes the list a superset of any single target's closure — the `windows-*` crates
below are in the graph but are not compiled into the `x86_64-unknown-linux-musl` or
`aarch64-unknown-linux-musl` assets. Auditing the superset is the conservative choice: a crate
that only builds elsewhere is still examined instead of being silently skipped.

The SPDX expression of each row is the crate's **own declared** `license` field as cargo reports
it. `scripts/check-licenses.py` fails when a crate's expression, its resolved version, or the
membership of the closure stops matching this table, and when a crate appears with an expression
this audit has not analysed.

**Result: 74 crates, every one compatible with GPL-3.0-or-later. No incompatible dependency was
found, so nothing had to be replaced.**

## 2. Why "compatible" is the right question here

GPL-3.0-or-later compatibility is one-way. A permissive licence (MIT, BSD, ISC, Zlib, 0BSD,
Apache-2.0, the Unicode licence) puts no condition on the combined work that GPLv3 forbids, so
its code may be incorporated into a GPLv3 work; the combined work — the `lodi` binary — is then
distributed under GPL-3.0-or-later, while each incorporated crate keeps its own notices. The
reverse does not hold, and is not needed: Lodi incorporates these crates, not the other way
round. Apache-2.0 in particular is compatible **with GPL version 3** (its patent and
indemnification terms are the reason it is not compatible with GPLv2), which is why this project
is `GPL-3.0-or-later` rather than `GPL-2.0-or-later`.

None of the 74 crates is under a copyleft licence, so no crate imposes a licence on Lodi. MPL-2.0
is named in this project's standing list of plainly compatible expressions but **no crate in the
closure uses it** today.

**The binary also embeds first-party data, which is not a dependency.** `build.rs` compiles the
recipe catalogue (`catalogue/`) into the binary; it is this project's own work under this
repository's own `LICENSE`, GPL-3.0-or-later like the tool around it (there is no separate
catalogue licence). It is not a crate, so it has no row in §4, and it
is the project's own licence, so it raises no compatibility question at all.

## 3. The analyses

### 3.1 Plainly permissive: MIT, Apache-2.0, ISC, BSD-3-Clause, Zlib

`MIT`, `Apache-2.0`, `ISC`, `BSD-3-Clause` and `Zlib`, alone or as an `OR` choice, need no
argument beyond §2: each is a short permissive licence whose only conditions are preservation of
the copyright notice and the licence text (and, for Apache-2.0, a NOTICE file and a statement of
modification). GPLv3 §7(b) explicitly allows such notice-preservation terms as additional
permissions-compatible requirements. Where a crate offers a choice (`MIT OR Apache-2.0` and its
orderings), the MIT branch is taken and recorded here; the choice is the distributor's.

### 3.2 `MIT/Apache-2.0` — the pre-SPDX spelling

`filetime` and `version_check` still declare `MIT/Apache-2.0`, the slash form cargo accepted
before SPDX expressions. It means the same dual offer as `MIT OR Apache-2.0`; both crates ship
`LICENSE-MIT` and `LICENSE-APACHE`. It is listed as its own expression rather than normalized,
so that a crate silently changing its declaration is still a change the checker sees.

### 3.3 `ring` — `Apache-2.0 AND ISC`

`ring` is the cryptographic backend under `rustls`. `AND`, not `OR`: different files of the crate
are under Apache-2.0 (the parts derived from BoringSSL) and under ISC (Brian Smith's own code and
the parts inherited from the original OpenSSL-derived ISC code). Both are permissive, both are
GPLv3-compatible by §2, and a conjunction of two compatible permissive licences is itself
compatible — the obligations are cumulative notice obligations, not conflicting restrictions. The
notices travel with the binary in the packaged copyright file (`packaging/copyright`).

### 3.4 `0BSD` and `Unlicense` — condition-free dedications

`adler2` offers `0BSD OR MIT OR Apache-2.0`; `byteorder` and `memchr` offer `Unlicense OR MIT`.
0BSD is the BSD licence with the notice requirement removed, and the Unlicense is a public-domain
dedication with a permissive fallback. Neither imposes any condition that GPLv3 could conflict
with; the FSF lists the Unlicense as a GPL-compatible public-domain dedication. In both cases an
MIT branch is also offered, so even a reader who distrusts public-domain dedications has a plainly
compatible choice.

### 3.5 `unicode-ident` — `(MIT OR Apache-2.0) AND Unicode-3.0`

The crate's code is dual MIT/Apache-2.0; the Unicode character tables it embeds are under the
**Unicode License v3**. That licence is a permissive, BSD-style grant over the Unicode data files
whose only conditions are that the copyright notice and the licence text accompany the data. It
imposes no restriction on the combined work, so the conjunction is compatible by §2, and the
notice travels with the binary in the packaged copyright file.

### 3.6 `wasi` — `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT`

The LLVM exception is an **additional permission** on top of Apache-2.0 (it waives the
attribution requirement for compiled output); GPLv3 §7 allows additional permissions and lets a
downstream distributor remove them. The plain `Apache-2.0` and `MIT` branches are offered anyway,
so the compatible choice exists with or without the exception. `wasi` is a `wasm32` target
dependency and is not compiled into either shipped asset.

### 3.7 `webpki-roots` — `CDLA-Permissive-2.0`

This is the one expression in the closure that is not a software licence, and the one that needed
a real reading. `webpki-roots` ships the **Mozilla CA root certificate data** that `ureq`'s
bundled root store uses (lodi fetches over HTTPS only, with bundled roots). The Community Data
License Agreement – Permissive 2.0 covers *Data*, and its full text is committed beside this
file as `docs/licenses/CDLA-PERMISSIVE-2.0.txt`. Reading it clause by clause:

- **§1.1** grants any recipient the right to use, modify and share the Data.
- **§2.1** is the *only* condition on sharing: "a Data Recipient may share Data, with or without
  modifications, so long as the Data Recipient makes available the text of this agreement with
  the shared Data." That is a notice requirement, the kind GPLv3 §7(b) expressly permits.
- **§3.1** states that the agreement imposes **no** restriction or obligation on *Results* —
  anything computed from the Data. There is no copyleft, no share-alike and no field-of-use term.
- **§4** is a warranty disclaimer, which GPLv3 §7(a) expressly permits.

So CDLA-Permissive-2.0 is compatible: it adds nothing to the combined work beyond a notice and a
disclaimer, and it constrains nothing about Lodi's own code. **It does create one shipping
obligation**, and this project meets it: the agreement's text is reproduced in full in the
committed `packaging/copyright`, which `scripts/release.sh` installs into the `.deb`
(`/usr/share/doc/lodi/copyright`) and into the Arch package
(`/usr/share/licenses/lodi/copyright`), and `scripts/test-release.py` checks that both packages
carry that file byte for byte. The source tarball carries `packaging/copyright` and this file.

## 4. The closure

74 crates, from `Cargo.lock`. The last column names the section above that justifies the
expression.

| Crate | Version | SPDX expression | GPLv3-compatible because |
|---|---|---|---|
| `adler2` | `2.0.1` | `0BSD OR MIT OR Apache-2.0` | 0BSD is condition-free; MIT also offered — §3.4 |
| `base64` | `0.23.1` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `block-buffer` | `0.10.4` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `byteorder` | `1.5.0` | `Unlicense OR MIT` | public-domain dedication; MIT also offered — §3.4 |
| `bytes` | `1.12.1` | `MIT` | permissive, GPL-compatible — §3.1 |
| `cc` | `1.4.7` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `cfg-if` | `1.0.5` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `cpufeatures` | `0.2.17` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `crc` | `3.4.0` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `crc-catalog` | `2.5.0` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `crc32fast` | `1.5.2` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `crypto-common` | `0.1.7` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `digest` | `0.10.7` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `equivalent` | `1.0.2` | `Apache-2.0 OR MIT` | dual; MIT chosen — §3.1 |
| `filetime` | `0.2.29` | `MIT/Apache-2.0` | pre-SPDX spelling of the same dual offer — §3.2 |
| `find-msvc-tools` | `0.1.13` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `flate2` | `1.1.10` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `generic-array` | `0.14.7` | `MIT` | permissive, GPL-compatible — §3.1 |
| `getrandom` | `0.2.17` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `hashbrown` | `0.17.1` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `http` | `1.5.0` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `httparse` | `1.10.1` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `indexmap` | `2.14.2` | `Apache-2.0 OR MIT` | dual; MIT chosen — §3.1 |
| `itoa` | `1.0.18` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `libc` | `0.2.189` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `log` | `0.4.34` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `lzma-rs` | `0.3.0` | `MIT` | permissive, GPL-compatible — §3.1 |
| `memchr` | `2.8.3` | `Unlicense OR MIT` | public-domain dedication; MIT also offered — §3.4 |
| `miniz_oxide` | `0.9.1` | `MIT OR Zlib OR Apache-2.0` | any of three; MIT chosen — §3.1 |
| `once_cell` | `1.21.4` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `percent-encoding` | `2.3.2` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `proc-macro2` | `1.0.107` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `quote` | `1.0.47` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `ring` | `0.17.14` | `Apache-2.0 AND ISC` | both permissive; Apache-2.0 is one-way GPLv3-compatible — §3.3 |
| `rustls` | `0.23.45` | `Apache-2.0 OR ISC OR MIT` | any of three; MIT chosen — §3.1 |
| `rustls-pki-types` | `1.15.1` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `rustls-webpki` | `0.103.15` | `ISC` | permissive, GPL-compatible — §3.1 |
| `serde` | `1.0.229` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `serde_core` | `1.0.229` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `serde_derive` | `1.0.229` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `serde_json` | `1.0.151` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `sha1collisiondetection` | `0.3.4` | `MIT` | Sequoia's port of git's SHA-1DC; MIT notices, GPLv3-compatible — §3.1 |
| `sha2` | `0.10.9` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `shlex` | `2.0.1` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `simd-adler32` | `0.3.10` | `MIT` | permissive, GPL-compatible — §3.1 |
| `subtle` | `2.6.1` | `BSD-3-Clause` | permissive, GPL-compatible — §3.1 |
| `syn` | `3.0.6` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `tar` | `0.4.46` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `toml_datetime` | `0.6.11` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `toml_edit` | `0.22.27` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `toml_write` | `0.1.2` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 (toml_edit's `display` feature) |
| `typenum` | `1.20.1` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `unicode-ident` | `1.0.26` | `(MIT OR Apache-2.0) AND Unicode-3.0` | Unicode-3.0 is permissive — §3.5 |
| `untrusted` | `0.9.0` | `ISC` | permissive, GPL-compatible — §3.1 |
| `ureq` | `3.4.2` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `ureq-proto` | `0.6.4` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `utf8-zero` | `0.8.1` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `version_check` | `0.9.5` | `MIT/Apache-2.0` | pre-SPDX spelling of the same dual offer — §3.2 |
| `wasi` | `0.11.1+wasi-snapshot-preview1` | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | the exception only adds permission; MIT also offered — §3.6 |
| `webpki-roots` | `1.0.9` | `CDLA-Permissive-2.0` | data, not code; only condition is notice — §3.7 |
| `windows-sys` | `0.52.0` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `windows-targets` | `0.52.6` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `windows_aarch64_gnullvm` | `0.52.6` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `windows_aarch64_msvc` | `0.52.6` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `windows_i686_gnu` | `0.52.6` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `windows_i686_gnullvm` | `0.52.6` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `windows_i686_msvc` | `0.52.6` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `windows_x86_64_gnu` | `0.52.6` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `windows_x86_64_gnullvm` | `0.52.6` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `windows_x86_64_msvc` | `0.52.6` | `MIT OR Apache-2.0` | dual; MIT chosen — §3.1 |
| `winnow` | `0.7.15` | `MIT` | permissive, GPL-compatible — §3.1 |
| `zeroize` | `1.9.0` | `Apache-2.0 OR MIT` | dual; MIT chosen — §3.1 |
| `zlib-rs` | `0.6.8` | `Zlib` | permissive, GPL-compatible — §3.1 |
| `zmij` | `1.0.23` | `MIT` | permissive, GPL-compatible — §3.1 |

## 5. What the check does, and its negative control

`python3 -B scripts/check-licenses.py` (a step of `scripts/gate.sh`, offline) fails when:

- a crate in the closure has **no row** here;
- a row names a crate that is **no longer** in the closure;
- a row's version or SPDX expression **disagrees** with what cargo resolves;
- a crate's expression is **not one this audit has analysed** — the script's
  `ALLOWED_EXPRESSIONS` is the machine-readable conclusion of §3, and any other expression fails
  whatever it says, so a new dependency under an unlisted licence cannot be shipped by accident;
- `Cargo.toml` does not declare `license = "GPL-3.0-or-later"`, or `LICENSE` is not the GPL
  version 3 text.

`python3 -B scripts/check-licenses.py --self-test` is the negative control: it runs the same
comparison over fabricated closures and audits and fails unless each broken one is rejected for
its own reason — an unlisted expression (rejected *even when* a row for it was added), a new
crate with no row, a crate that changed its expression, a version bump the audit has not seen,
and a stale row for a crate that left the closure — while the sound pair passes.

Widening `ALLOWED_EXPRESSIONS` is a deliberate edit that comes with its analysis in §3. If a
dependency ever arrives under a licence that is **not** compatible, the rule is to record it and
stop, not to weaken the check.
