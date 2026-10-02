# Set the hostname, time zone, firewall and network

Declare this machine's hostname, time zone, locale, keyboard, firewall and network in one file.

You need a config made by `lodi import`, by default in `~/.config/lodi`. The import already
wrote your hostname under `[system]`.

## Steps

1. Add what you want to set to `~/.config/lodi/host.toml`:

   ```toml
   [system]
   hostname = "sd1-basics"
   timezone = "Europe/Berlin"
   locale = "en_GB.UTF-8"
   keymap = "de"

   [firewall]
   allow = ["22/tcp"]

   [network]
   confirm_within = 30

   [network.interfaces.enp0s2]
   dhcp = true
   addresses = ["10.0.2.50/24"]
   ```

2. Preview. Each change is shown as the tool would make it. On a fresh Debian 12:

   ```sh
   lodi switch --dry-run
   ```

   ```text
   ~ hostname lodi-debian -> sd1-basics (hostnamectl set-hostname -- sd1-basics)
   ~ timezone Etc/UTC -> Europe/Berlin (timedatectl set-timezone -- Europe/Berlin)
   ~ locale LANG=C.UTF-8 -> LANG=en_GB.UTF-8 (localectl set-locale -- LANG=en_GB.UTF-8)
   ~ keymap us -> de (XKBLAYOUT in /etc/default/keyboard)
   + firewall table inet lodi (nft -f /etc/lodi/firewall.nft)
   + network enp0s2 (netplan /etc/netplan/90-lodi.yaml; ping 10.0.2.2 within 30 s, else put back)
   ```

3. Switch:

   ```sh
   lodi switch
   ```

What to know:

- **A setting you leave out is never touched.** lodi sets the hostname only when `[system]`
  names one.
- **Delete a line, and the next switch puts back what the machine had** before lodi first set
  it.
- `localectl list-locales` lists the locales you can declare. On Debian, install `locales-all`
  for more of them.
- lodi picks this machine's host by its hostname. After you change the hostname in a config
  with `config.toml`, rename its `[hosts.NAME]` entry to match.

## The firewall

`[firewall]` is one nftables table, `inet lodi`. lodi never touches another table. Incoming
traffic is dropped, except what `allow` names, answers to your own connections, the loopback
interface and ICMP. To let everything in, write `input = "accept"` under `[firewall]`.

lodi needs `nft`. On Debian and Ubuntu, add `nftables` to `[packages]` and switch first.
`lodi-firewall.service` loads the table at boot. Delete `[firewall]`, and the next switch
removes the table, its file and the unit.

## The network

lodi uses the network stack the machine already runs: netplan, NetworkManager or
systemd-networkd. It writes files of its own, named `lodi`, and never edits yours. Each
interface takes `dhcp`, `addresses`, `gateway` and `dns`.

**A network change that cuts the machine off is put back by itself.** After the change, lodi
pings one address: `[network] check`, else your gateway. When nothing answers within
`confirm_within` seconds, 90 by default, lodi puts the earlier files back and stops with
[`E_NETWORK_ROLLED_BACK`](../ERRORS.md). If your SSH session dies with it, a timer puts them
back about 30 seconds later:

```toml
[network]
check = "192.168.1.1"
confirm_within = 60
```

## Check it worked

```sh
hostnamectl
timedatectl
sudo nft list table inet lodi
```

Switch again, and the preview says `nothing to switch`.

## When it fails

Nothing changes when one of these stops the preview.

- [`E_UNKNOWN_SETTING`](../ERRORS.md): The machine has no such time zone or locale. Pick one it
  lists.
- [`E_FIREWALL_CONFLICT`](../ERRORS.md): `ufw` or `firewalld` is active. Turn it off, or drop
  `[firewall]`.
- [`E_NETWORK_STACK`](../ERRORS.md): lodi found no network stack it drives, or two at once.
- [`E_NETWORK_ROLLED_BACK`](../ERRORS.md): The machine could not be reached after the change. Check
  the addresses.
