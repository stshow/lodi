# Security policy

## Threat model

lodi manages installed packages and files on your machine, some of it as root, and it downloads
software from third parties. Here is what it does, in plain terms.

### What runs as root

Only the host scope needs root, for exactly three commands: `sudo lodi host arm`,
`sudo lodi host import` and `sudo lodi host apply` (or the combined `sudo lodi import` and
`sudo lodi apply`, which do the same for the host and then each home). lodi never invokes
`sudo` itself — every privileged command is one you type yourself. `lodi host plan` and
`lodi host versions` are read-only and need no root.

`sudo lodi host arm` writes one empty marker file, `/etc/lodi/host-allowed`; every other host
command refuses before it reads anything until that marker exists. Delete the file to take the
permission back.

`sudo lodi host apply`/`sudo lodi apply` performs a real system change — on Arch it runs
`pacman -Syu`, upgrading the whole machine — and there is no rollback: a failed apply stops and
names what ran.

The home scope (your own files and user-wide tools) and the project scope (one directory's
pinned tools) never need root, and refuse to run under `sudo`.

### What it downloads

- **Project tools** (`lodi lock`, `lodi develop`, `lodi shell`): upstream builds of tools such
  as Python and Node.js, from the catalogue's built-in recipes, or the URL an inline manifest
  entry gives.
- **Host packages and sources**: distribution packages through apt, pacman or dnf, and, when a
  manifest declares one, a third-party apt repository and its signing keyring, over HTTPS.
- **Container environments**: the base rootfs and the package files a project's `[container]`
  needs, from the distribution's own archive.
- **Its own release**: `SHA256SUMS` and the binary or package you install, from this
  repository's GitHub Releases page.

Commands that only read or plan (`lodi host plan`, `lodi plan`, `lodi lock --check`,
`lodi info`) make no network request and write nothing.

### How it pins and verifies

- Every tool or package lodi resolves is pinned to an exact version at lock or import time, in
  a lock file (`lodi.lock`, `host.lock`, `pins.lock`); a later run reuses those exact bytes
  instead of re-resolving.
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
  `SigLevel = Never`. lodi fetches them over HTTPS and records their SHA-256 in the lock at the
  first `lodi lock`, and the package files are pinned from them. Whoever can serve you a
  different index over HTTPS at that first lock decides what gets pinned. The host scope's own
  package installs are different: apt, pacman and dnf check the distribution's signatures as
  usual.
- **Applying a host from a git URL.** `sudo lodi host apply <git URL>` (and `lodi host plan` on
  one) fetches the repository over HTTPS and runs what it declares as root. There is no commit
  or tag signature check. The first resolve trusts whoever controls that repository and its
  hosting; `host.lock` then pins the commit and the hash of its tree, and a later apply of the
  same URL and ref reuses exactly that tree until you pass `--refresh` or `--rev`. Apply only
  repositories you control or trust as much as root on the machine.
- **`LODI_FETCH_REWRITE`.** This environment variable redirects download URL prefixes to a
  mirror (`<prefix>=<replacement>`, several separated by `;`). A replacement must be HTTPS, or
  plain HTTP to a loopback address. The lock still records the original URLs and every recorded
  hash is still checked, but an unlocked first resolve trusts whatever the mirror serves. A
  plain `sudo` resets the environment and drops it; `sudo -E`, or a sudoers rule that keeps it,
  passes it to the root commands. See [the CLI reference](docs/CLI.md#environment-variables).

Checking the distributions' index signatures and git commit signatures is not implemented.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting on this repository (Security tab → Report a
vulnerability). Do not open a public issue for a security problem.
