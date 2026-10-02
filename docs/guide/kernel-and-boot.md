# Change kernel parameters or the bootloader

Set kernel parameters, modules and sysctl values, or move from GRUB to systemd-boot. If the new
boot entry fails, the one you booted before stays in the boot menu.

You need a config made by `lodi import`, by default in `~/.config/lodi`, and a machine that
starts through UEFI for a bootloader change.

**A parameter or loader change takes effect at once.** There is no trial boot and nothing to
confirm. The next boot uses it. The entry that was the default before stays in the boot menu as
`lodi: known good`. You pick it there by hand.

## Set kernel parameters, modules and sysctl

1. Add them to `~/.config/lodi/host.toml`:

   ```toml
   [kernel]
   parameters = ["mitigations=off"]
   modules = ["dummy"]
   blacklist = ["pcspkr"]

   [sysctl]
   "net.ipv4.ip_forward" = "1"
   ```

2. Preview:

   ```sh
   lodi switch --dry-run
   ```

   ```text
   + parameters mitigations=off (the default from the next boot; the entry before it stays as …)
   + modules dummy (/etc/modules-load.d/lodi.conf; modprobe -- dummy)
   + blacklist pcspkr (/etc/modprobe.d/lodi-blacklist.conf)
   ~ sysctl net.ipv4.ip_forward 0 -> 1 (sysctl -w net.ipv4.ip_forward=1)
   ```

   The first line is shortened here. It ends `stays as lodi-known-good`.

3. Switch, then reboot for the parameters:

   ```sh
   lodi switch
   ```

- Parameters go into one line of lodi's own in `/etc/default/grub`, then `grub-mkconfig` runs.
  On Fedora lodi runs `grubby`, which also sets them for kernels you install later.
- A module in `modules` is loaded now and at every boot. One in `blacklist` stops loading from
  the next boot on. lodi never unloads a running module.
- A sysctl value is set now and at every boot.
- **Delete a line, and the next switch puts back your distribution's own setting.** A sysctl
  value goes back at once. A parameter or a module goes back at the next boot.

## Change the bootloader

1. Declare the loader, and its timeout and default entry if you want them:

   ```toml
   [boot]
   loader = "systemd-boot"
   timeout = 4
   default = "debian-*"
   ```

2. Preview:

   ```sh
   lodi switch --dry-run
   ```

   ```text
   ~ loader grub -> systemd-boot (…; the default from the next boot; grub stays as the fallback …)
   + timeout 4 (/boot/efi/loader/loader.conf)
   ```

3. Switch, then reboot:

   ```sh
   lodi switch
   ```

The new loader goes first in the firmware's boot order. The earlier one stays installed, right
after it. If the new loader cannot start, the firmware starts the old one. A switch that fails
puts the EFI system partition and the boot order back as they were.

- `timeout` is in seconds, 0 to 600.
- `default` is written as the loader takes it. For GRUB that is `0` or `saved`. For
  systemd-boot it is an entry name, or a pattern such as `debian-*` or `@saved`.
- Write `loader = "grub"` and switch to go back to GRUB the same way.
- Delete `[boot]`, and the next switch puts your distribution's loader first again.

## Go back to the entry that booted before

1. Reboot, and pick `lodi: known good` in the boot menu. Under GRUB it is a copy of the entry
   that was the default. Under systemd-boot it is `lodi-known-good.conf`, sorted after your
   distribution's entries.
2. Once the machine is up, take the change out of `host.toml` and switch, so the next boot is
   the old one by default.

Only an entry that has booted becomes the fallback. Make two changes before one reboot, and the
fallback stays the entry you last booted. The preview then says
`lodi-known-good stays as it is: the default has not booted`.

To start the earlier loader once after a loader change, pick it in the firmware's boot menu.
Or run `sudo efibootmgr --bootnext NUMBER`, with its number from `efibootmgr`, and reboot.

## Check it worked

After the reboot:

```sh
cat /proc/cmdline
bootctl status
```

The first shows your parameters. The second names the loader that started.

## When it fails

Nothing changes when one of these stops the preview.

- [`E_BOOT_LOADER`](../ERRORS.md): lodi found no `/etc/default/grub`, no EFI system partition, or
  not the loader in use.
- [`E_BOOT_NOT_UEFI`](../ERRORS.md): The machine did not start through UEFI. Drop `[boot]`.
- [`E_UNKNOWN_SETTING`](../ERRORS.md): The machine has no such sysctl name.
