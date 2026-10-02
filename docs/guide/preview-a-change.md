# Preview a change before you switch

See exactly what `lodi switch` would do to this machine and your home, before it does anything.
You can also preview what importing again would write into your config.

You need a config made by `lodi import`, by default in `~/.config/lodi`.

## See what a switch would change

1. Run the switch with `--dry-run`:

   ```sh
   lodi switch --dry-run
   ```

   ```text
   + package tree
   host: +1 -0 packages
   create    .inputrc                  0644
   1 file: 1 create
   home: 1 file
   config: 5798a24
   ```

   The host part comes first, then your home part, then your config's commit. A `+` line
   adds, `~` changes, `-` removes and `=` leaves a thing as it is.

2. Read every `-` line. Under `packages = "exact"` a `-` package line is a package the switch
   removes.

A preview changes nothing on the machine and writes no file. It never asks for a password. It
runs every check a switch runs first, so a mistake in `host.toml` shows up here.

To preview one part only, add `--host` or `--home`:

```sh
lodi switch --dry-run --host
```

## Switch only after you say yes

`--ask` shows the same preview, then waits for your answer:

```sh
lodi switch --ask
```

```text
switch? [y/N]
```

Anything but `y` stops with [`E_DECLINED`](../ERRORS.md), and nothing changes. Without a
terminal to ask on, it stops the same way.

## Import again after you change the machine by hand

Installed or removed packages with `apt`, `pacman` or `dnf` since the last import? Import again.
lodi adds what changed on the machine to `host.toml`, and keeps your own edits.

1. See what it would write first:

   ```sh
   lodi import --dry-run
   ```

   When nothing changed, it says so and writes nothing:

   ```text
   [1/1] read the host
   [1/1] read the host: done, 0.5s
   would keep ~/.config/lodi/home.toml as it is
   would keep ~/.config/lodi/host.toml: nothing to merge
   nothing was written (--dry-run)
   ```

   Otherwise it shows the lines it would add to or remove from `host.toml`.

2. Import for real:

   ```sh
   lodi import
   ```

What the merge does:

- A package you installed by hand is added. One you removed by hand leaves every list it is in.
- A line you deleted stays deleted. Your comments, blank lines and list order stay too.
- A configuration file changed on the machine is named in a `W_RECONCILE` line. It is never
  copied in.
- When both sides changed the same thing, your side wins, and lodi prints a `W_RECONCILE` line.

## Check it worked

After the switch, preview again. There is nothing left to do:

```sh
lodi switch --dry-run
```

```text
nothing to switch (5798a24)
```

A `+uncommitted` after the commit, and a warning, mean your config has changes git does not
hold yet. Commit them, so `git` can take you back to this state:

```text
nothing to switch (5798a24+uncommitted)
lodi: warning: config has uncommitted changes; git can't take you back to this
```

Every flag is in [the command reference](../CLI.md#lodi-switch).
