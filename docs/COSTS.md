# What lodi costs

This page tells you how much disk, network and time lodi takes on a typical machine. Each number
is one measurement on one machine, with the command that produced it and the date.

## The binary

```sh
cargo build --release --locked
stat -c %s target/release/lodi
```

```text
11159400
```

The `lodi` program is about 11 MB on disk. That is lodi 1.5.0 built from its source on
2026-09-26. The 1.5.0 release files were not out yet on that date. So this is not the size of a
downloaded release file.

## What a project downloads

Take a project whose `lodi.toml` asks for `jq = "1.8"` and `ripgrep = "15"`. Lock it, allow it,
and enter it for the first time:

```sh
lodi lock
```

```text
wrote lodi.lock (resolved jq, ripgrep): jq 1.8.2, ripgrep 15.2.0
```

```sh
lodi trust
lodi develop -- rg --version
```

The first `lodi develop` downloads 4.5 MB. It prints what it unpacked, and the line ends with
`(4533630 bytes downloaded)`. The two tools then take 8.1 MB in lodi's store, as `du -sb`
measured it. A later `lodi develop` downloads nothing. This was lodi 1.5.0 on 2026-09-26.

## How long it takes

With an empty store, the first `lodi develop -- true` in that project took 0.67 to 0.76
seconds over three runs. The next run took under 0.01 seconds, because nothing was left to
download. This was lodi 1.5.0, built as above, on 2026-09-26.

The machine was a shared x86_64 Linux workstation with 30 cores. Your times
depend on your network and your disk, so read them as a rough guide.
