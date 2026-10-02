# Add a third-party apt or dnf repository

Install a package from a vendor's own apt or dnf repository, such as Docker's, with its signing
key checked before anything is written.

You need a config made by `lodi import`, the vendor's key file, and its SHA-256 digest. Check
the key with the vendor through a channel you trust. On Arch, a `[sources]` table stops with an
error, and lodi never edits `pacman.conf`.

`lodi import` already writes each third-party repository it found as a commented `[sources]`
block. Remove the `# ` to take it on purpose.

## Add an apt repository on Debian or Ubuntu

1. Put the key next to `host.toml`, as `~/.config/lodi/files/docker.asc`, and print its digest:

   ```sh
   sha256sum ~/.config/lodi/files/docker.asc
   ```

2. Declare the repository, and the package you want from it:

   ```toml
   [sources.docker]
   uris = ["https://download.docker.com/linux/debian"]
   suites = ["bookworm"]
   components = ["stable"]
   signed_by = "files/docker.asc"
   signed_by_sha256 = "<the digest sha256sum printed>"

   [packages]
   common = ["docker-ce"]
   ```

3. Preview. The repository comes first, then the index refresh:

   ```sh
   lodi switch --dry-run
   ```

   ```text
   + source docker (keyring, sources)
   ~ index (apt-get update -o APT::Update::Error-Mode=any)
   ```

4. Switch:

   ```sh
   lodi switch
   ```

   lodi writes `/etc/apt/keyrings/lodi-docker.asc` and
   `/etc/apt/sources.list.d/lodi-docker.sources`, refreshes the index, then installs.

| Key | Needed | What it takes |
|---|---|---|
| `types` | no, `["deb"]` | `deb` or `deb-src` |
| `uris` | yes | `https://` addresses only |
| `suites` | yes | Suite names. One that ends in `/` is a flat repository |
| `components` | yes, unless every suite is flat | Component names |
| `architectures` | no | dpkg names such as `amd64` |
| `signed_by` | this or `signed_by_url` | The key's path, from the config's folder |
| `signed_by_url` | this or `signed_by` | The key's `https://` address |
| `signed_by_sha256` | yes | The key's SHA-256 digest |

## Add a dnf repository on Fedora

The same table adds a dnf repository. The key must be ASCII-armored:

```toml
[sources.ghcli]
uris = ["https://cli.github.com/packages/rpm"]
signed_by = "files/ghcli.asc"
signed_by_sha256 = "cec6e9ed82d3949ca5f4428cc968b41ef5e7416cb3653cdfc2a421977663bbfd"

[packages.fedora]
add = ["gh"]
```

```sh
lodi switch --dry-run
```

```text
+ source ghcli (key, repo)
~ index (dnf5 --refresh --repo=lodi-ghcli makecache)
+ package gh 0:2.97.0-2.fc44.x86_64
```

The preview reads the index from before the refresh, so it may name Fedora's own build. The
switch installs the one from the repository. lodi writes `/etc/pki/rpm-gpg/lodi-ghcli.asc` and
`/etc/yum.repos.d/lodi-ghcli.repo`, with `gpgcheck=1`. For dnf, the keys are `uris`, the
`signed_by` keys above, and `repo_gpgcheck = true` to check the repository's own signature too.

## Check it worked

```sh
apt-cache policy docker-ce
```

On Fedora, `dnf5 repolist` lists `lodi-ghcli`. Take the table out, and the next switch removes
both files and nothing else.

## What stops it

lodi checks all of this before it writes anything:

- **The key's digest.** A key the vendor replaced stops with [`E_HASH_MISMATCH`](../ERRORS.md).
  Check the new key, then write its digest.
- **Public keys only.** A file with a private key, a binary key on Fedora, or one that is not a
  keyring stops with [`E_TYPE`](../ERRORS.md).
- **`https://` only.** Another scheme stops with [`E_UNSUPPORTED`](../ERRORS.md). A
  `signed_by_url` that leads to `http://` stops with [`E_INSECURE_URL`](../ERRORS.md).
- **Checks stay on.** `gpgcheck = false` or `repo_gpgcheck = false` stops with
  [`E_GPGCHECK_OFF`](../ERRORS.md).
- **One repository, one entry.** If the machine already lists it, lodi stops with
  [`E_DUP_RESOURCE`](../ERRORS.md) and names that file. Disable that entry first.
- **A file edited by hand.** Either file lodi wrote, changed since, stops the switch with
  [`E_DRIFT`](../ERRORS.md) until you give `--overwrite-drift`.

If apt or dnf cannot reach the repository, the switch puts the files back and installs nothing.
