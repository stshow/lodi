# Turn services on and off, and add users

Declare which systemd units run on this machine, and which accounts and groups it has.

You need a config made by `lodi import`, by default in `~/.config/lodi`.

## Turn a service on or off

1. Add each unit you care about to `~/.config/lodi/host.toml`, `enabled` or `disabled`. Name it
   in full, ending in `.service`, `.socket`, `.timer` or `.path`:

   ```toml
   [services]
   "fstrim.timer" = "enabled"
   "logrotate.timer" = "disabled"
   ```

2. Preview. Each change is shown as `systemctl` would run it:

   ```sh
   lodi switch --dry-run
   ```

   ```text
   + service fstrim.timer (systemctl enable --now -- fstrim.timer)
   - service logrotate.timer (systemctl disable --now -- logrotate.timer)
   ```

3. Switch:

   ```sh
   lodi switch
   ```

What the states mean:

- `enabled` is enabled and running now. `disabled` is disabled and stopped. A unit you started
  or stopped by hand is put back the way `host.toml` says.
- A unit whose package the same switch installs is enabled once the package is there.
- **Delete a unit's line, and the next switch runs `systemctl preset` for it.** The unit is
  enabled or disabled as your distribution ships it.
- **A unit you never declared is never touched.**

## Add a user and a group

1. Declare the account, and its group:

   ```toml
   [users.alice]
   uid = 1001
   groups = ["sudo"]
   shell = "/bin/bash"
   ssh_keys = ["ssh-ed25519 AAAAC3Nza... alice-laptop"]

   [groups.devs]
   gid = 3000
   members = ["alice"]
   ```

2. Preview:

   ```sh
   lodi switch --dry-run
   ```

   ```text
   + group devs (gid 3000)
   + user alice (uid 1001, locked password)
   ~ group devs (members alice)
   + ssh keys alice (1 key(s))
   ```

3. Switch:

   ```sh
   lodi switch
   ```

The account gets a **locked password**, and the key goes into `~/.ssh/authorized_keys` in its
home. Log in with the key, or set a password on the machine with `sudo passwd alice`.
`host.toml` has no key for a password, so none can reach your config.

- An existing account keeps its password. lodi sets its shell, adds it to the groups you list,
  and adds a key it does not have yet. It removes no key.
- A group gets exactly the members you list, and every account whose `groups` names it.
- **Deleting an account or a group from `host.toml` leaves it on the machine**, with its home.
  The preview says `= user alice (no longer managed: left as it is, with its home)`. Remove an
  account yourself with `sudo userdel alice`.

| Key | What it does |
|---|---|
| `[users.NAME] uid` | The account's UID. Optional: the distribution picks one |
| `[users.NAME] groups` | Groups the account joins. Each exists or is declared |
| `[users.NAME] shell` | The login shell, an absolute path. Optional |
| `[users.NAME] home` | The home, `/home/NAME` when left out |
| `[users.NAME] ssh_keys` | Public keys, one line each, for `authorized_keys` |
| `[groups.NAME] gid` | The group's GID. Optional |
| `[groups.NAME] members` | Exactly the group's members |

## Check it worked

```sh
systemctl is-enabled fstrim.timer
id alice
```

Switch again, and the preview says `nothing to switch`.

## When it fails

- [`E_UNKNOWN_UNIT`](../ERRORS.md): The machine has no such unit, or it is static or masked. Nothing
  changed.
- [`E_IDENTITY_CONFLICT`](../ERRORS.md): The account's UID or home, or the group's GID, differs from
  the machine's. lodi never changes one.
- [`E_SSH_KEY`](../ERRORS.md): `ssh_keys` holds a private key, or something that is not one public
  key.
- [`E_IDENT`](../ERRORS.md): Use lowercase letters, digits, `_` and `-` in the name.
- [`E_TYPE`](../ERRORS.md): A service state is `"enabled"` or `"disabled"`.
