# Build lodi from source

Build and test lodi on your own machine. You need x86_64 Linux, `git` and `python3`. You also
need either `rustup` or Nix with flakes.

## Get the pinned Rust toolchain

`rust-toolchain.toml` names the Rust release lodi builds with. With `rustup`, the first `cargo`
command in the source folder installs it for you.

With Nix, open a shell that has the same toolchain:

```sh
nix --extra-experimental-features 'nix-command flakes' develop
```

Nix downloads the toolchain into its own store. It installs nothing else on your machine.

## Build

```sh
cargo build --locked
```

The program is `target/debug/lodi`. For a static binary like the release files, build for musl:

```sh
cargo build --locked --release --target x86_64-unknown-linux-musl
```

That one is `target/x86_64-unknown-linux-musl/release/lodi`.

## Test

```sh
cargo test --locked
```

The tests run lodi against scratch folders, fake package managers and a local git server. They
never change your packages, your home folder or `/etc`. Some tests build container environments
and need rootless Podman, so `podman info` has to work without root.

## Check your change before you send it

```sh
sh scripts/check.sh
```

This runs `cargo fmt --check`, clippy with warnings as errors, the build and every test. It
prints `check: OK` when all of them pass. Install the git hooks once per clone, too:

```sh
sh scripts/install-hooks.sh
```
