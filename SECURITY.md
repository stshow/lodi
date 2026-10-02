# Security policy

## Threat model

lodi manages installed packages and files on your machine, some of it as root, and it downloads
software from third parties. Here is what it does, in plain terms.

### What runs as root

You never type `sudo`. lodi starts as you and asks for root itself, through the first of
`sudo`, `doas` or `run0` on `PATH`. It does so for two commands only, and only for the host
part:

- `lodi import` reads the host as root and, the first time, writes the marker. `lodi import
  --dry-run` also reads the host as root, and writes nothing. `lodi import --home` needs no root.
- `lodi switch` runs the host part as root, and only when the host part has changes. A preview,
  `lodi switch --dry-run`, never asks for a password.

The first `lodi import` asks once whether lodi may manage this host. A yes writes one empty
marker file, `/etc/lodi/may-manage`. Until it exists, `lodi switch` does not touch the host and
points you to `lodi import`. Delete the file to take the permission back.

The host part of `lodi switch` is a real system change. On Arch it runs `pacman -Syu`, which
upgrades the whole machine. A failed switch stops and names what ran.

Everything else runs as you. That is your home part of `lodi switch`, `lodi update`, `lodi pin`,
`lodi unpin` and every project command. No file of your config is written by root.

### What it downloads

- **Project tools** (`lodi develop`, `lodi run`, `lodi shell`, `lodi update`): upstream builds
  of tools such as Python and Node.js, from the catalogue's built-in recipes, your own recipes,
  or the URL an inline manifest entry gives.
- **Host packages and sources**: distribution packages through apt, pacman or dnf, and, when a
  manifest declares one, a third-party apt repository and its signing keyring, over HTTPS.
- **Container environments**: the base rootfs and the package files a project's `[container]`
  needs, from the distribution's own archive.
- **Its own release**: `SHA256SUMS` and the binary or package you install, from this
  repository's GitHub Releases page.

`lodi search` makes no network request and writes nothing. The previews `lodi import --dry-run`,
`lodi switch --dry-run` and `lodi gc --dry-run` write nothing. The first two can still look
versions up over the network.

### How it pins and verifies

- Every tool or package lodi resolves is pinned to an exact version at lock or import time, in
  a lock file (your config's `lodi.lock`, or a project's `lodi.lock`); a later run reuses those
  exact bytes instead of re-resolving.
- A declared download with no recorded checksum is refused (`E_NO_CHECKSUM`); a download whose
  bytes don't match the recorded checksum is refused (`E_HASH_MISMATCH`), and nothing is
  written.
- A declared apt repository's signing key is pinned by its own SHA-256 (`signed_by_sha256`);
  apt itself then verifies the repository's signatures against that key.
- lodi's own release files are not signed. `SHA256SUMS`, checked with `sha256sum -c`, catches
  corruption, not forgery.

### Known limitations: what is trusted without a signature

Not every download is checked against a signature. These are trusted on HTTPS plus
trust-on-first-lock: the first resolve trusts what the server sends, records its hash, and later
runs refuse anything that differs.

- **Container environments and project locks.** The Debian `Release` file and the Arch `core.db`
  and `extra.db` a `[container]` build reads are not checked against the distributions'
  OpenPGP signatures (`InRelease`, `.db.sig`); the Arch image runs pacman with
  `SigLevel = Never`. lodi fetches them over HTTPS and records their SHA-256 in the lock the
  first time the project is locked, and the package files are pinned from them. Whoever can
  serve you a different index over HTTPS at that first lock decides what gets pinned. The host
  scope's own package installs are different: apt, pacman and dnf check the distribution's
  signatures as usual.
- **Switching from a git URL.** `lodi switch <git URL>` (and `lodi switch --dry-run` on one)
  fetches the config over HTTPS, and its host part runs what it declares as root. There is no
  commit or tag signature check. The first fetch trusts whoever controls that repository and its
  hosting. lodi then records the commit and the hash of its tree, and a later switch of the same
  URL and ref reuses exactly that tree until you pass `--refresh` or `--rev`. Switch only to
  configs you control or trust as much as root on the machine.
- **`LODI_FETCH_REWRITE`.** This environment variable redirects download URL prefixes to a
  mirror (`<prefix>=<replacement>`, several separated by `;`). A replacement must be HTTPS, or
  plain HTTP to a loopback address. The lock still records the original URLs and every recorded
  hash is still checked, but an unlocked first resolve trusts whatever the mirror serves. When
  lodi asks for root itself, the host part of `lodi switch` keeps this variable. If you type
  `sudo` yourself, a plain `sudo` drops it, and `sudo -E` passes it on. See
  [the CLI reference](docs/CLI.md#environment-variables).

Checking the distributions' index signatures and git commit signatures is not implemented.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting on this repository (Security tab → Report a
vulnerability). Do not open a public issue for a security problem.
