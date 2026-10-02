# Pin a package to a version

Keep a package at one version until you move it, and give a new machine the same versions as
this one. A pin is written into `host.toml` and locked in `lodi.lock` beside it, like
`flake.lock`. Both travel with your config.

You need a config made by `lodi import`, and the network. None of these commands changes the
machine. The next `lodi switch` does.

## Pin one package: `lodi pin`

1. List the versions you can pin:

   ```sh
   lodi pin bc
   ```

   ```text
   version      served      state
   1.07.1-3+b1  2026-10-01  installed, latest
   lodi pin bc --to 2026-10-01T00:00:00Z
   ```

   The last line is the command that pins that version. It writes nothing.

2. Pin it. On Debian and Arch, `--to` takes a day. On Ubuntu and Fedora it also takes a version:

   ```sh
   lodi pin bc --to 2026-09-01
   ```

   ```text
   pinned bc
   bc: 1.07.1-3+b1 from debian at 2026-09-01T00:00:00Z
   next: lodi switch
   ```

3. Switch, and commit both files:

   ```sh
   lodi switch
   git -C ~/.config/lodi commit -qam 'pin bc'
   ```

The pin is a line in `host.toml`. You can also write it by hand:

```toml
[packages.pin]
bc = "2026-09-01"                    # the version the archive had that day, held

[packages.debian.pin]
git = "2026-09-01T00:00:00Z"         # a per-distribution pin wins over [packages.pin]
```

lodi installs a pinned package and holds it. A version moved by hand is a `W_DRIFT` line, and
the switch stops until you give `--overwrite-drift`.

## Remove a pin: `lodi unpin`

```sh
lodi unpin bc
```

```text
unpinned bc
next: lodi switch
```

The next switch releases the hold and changes no version. A package with no pin prints
`bc has no pin; nothing written`.

## Pin the whole machine to one day

`[host] snapshot` installs every missing package from the distribution's archive as it was at
that time. `lodi import` writes the last full day. Move it to the last published day:

```sh
lodi pin --all
```

Or to a day you name, with `lodi pin --all --to 2026-09-01`. To remove it and every pin:

```sh
lodi unpin --all
```

A package already installed keeps its version. Which pin wins: `[packages.<distro>.pin]`, then
`[packages.pin]`, then `[host] snapshot`, then the live archive.

## Move to newer versions: `lodi update`

A plain `lodi switch` never moves a version. To move the snapshot to the last published day,
look the pins up again and switch:

```sh
lodi update
lodi switch
```

Or do both in one command, with `lodi switch --update`. Pins you set keep their value.

## On each distribution

| Distribution | A version pin | A day pin |
|---|---|---|
| Ubuntu | Yes | Yes |
| Debian | No, the archive gives no SHA-256 per package. Use a day | Yes |
| Arch | No, use a day | Yes |
| Fedora | Yes, one exact build | No |

The archives are snapshot.debian.org, snapshot.ubuntu.com and archive.archlinux.org. lodi
checks every index it downloads against its SHA-256 digest. A package from a
[third-party repository](add-a-repository.md) takes a version pin, but not a day pin.

**Old Arch packages.** An old Arch package may be signed by a key Arch has since retired. lodi
then stops with [`E_PIN_UNTRUSTED`](../ERRORS.md) and installs nothing. To check it against the
Arch keyring of the pin's own day, add this. lodi uses that keyring for one switch only:

```toml
[packages.arch]
archived_keyring = true
```

**Fedora builds.** A Fedora pin keeps one exact build, such as `0:1.10.4-1.fc44.x86_64`:

```sh
lodi pin pv --to 0:1.10.4-1.fc44.x86_64
```

lodi fetches that build from your mirror, or else from Fedora's signed copy on
`kojipkgs.fedoraproject.org`. It checks the file's SHA-256 against the lock, and dnf5 checks
Fedora's signature. When neither place has the build, the switch stops with
[`E_PIN_UNAVAILABLE`](../ERRORS.md) and changes nothing. `lodi pin --all` stops on Fedora with
[`E_UNSUPPORTED`](../ERRORS.md), because Fedora keeps no dated copy of its repositories.

## The same versions on a new machine

1. On this machine, pin the whole host and push your config:

   ```sh
   lodi pin --all
   git -C ~/.config/lodi commit -qam 'this machine'
   ```

2. On the new machine of the same distribution, clone the config and switch:

   ```sh
   git clone https://example.org/you/lodi.git ~/.config/lodi
   lodi switch --dry-run
   lodi switch
   ```

Every pinned package arrives at its pinned version. Every other declared package arrives at
the version of `[host] snapshot`. The switch also sets the `[system] hostname` that `host.toml`
declares, so change that line first. Importing again never moves a pin.

## Check it worked

Run `lodi pin bc` again. The `state` column marks the version as `installed` and pinned.

## When it fails

Nothing is written or installed when one of these stops the command.

- [`E_UNKNOWN_PACKAGE`](../ERRORS.md): The package is not in `host.toml`. Add it first.
- [`E_SNAPSHOT_TOO_OLD`](../ERRORS.md): The day is before the archive begins. Pick a later one.
- [`E_SNAPSHOT_FUTURE`](../ERRORS.md): The day is in the future. Pick an earlier one.
  serves.
- [`E_NO_MATCH`](../ERRORS.md): Ubuntu never published that version. Pick one `lodi pin` lists.
- [`E_NO_CHECKSUM`](../ERRORS.md): The version has no digest. Pick another.
- [`E_PIN_UNSATISFIABLE`](../ERRORS.md): The archive cannot install that version. Pin another one.
- [`E_CLOSURE_DRIFT`](../ERRORS.md): A pinned package is not at its version after the switch. Read
  the package manager's log.
- [`E_HASH_MISMATCH`](../ERRORS.md): A download does not match its digest.
- [`E_LOCK_STALE`](../ERRORS.md): `lodi.lock` does not match `host.toml`. Run `lodi update` and
  commit.
- [`E_LOCK_VERSION`](../ERRORS.md): The lock was written by a newer lodi. Update lodi.

Every flag is in [the command reference](../CLI.md#lodi-pin).
