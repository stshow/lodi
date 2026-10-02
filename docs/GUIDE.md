# The lodi guide

With lodi you import this machine into a config of plain TOML files, then make this machine, or
a new one, match it with `lodi switch`. Projects get their own tools the same way.

Start with [your first switch](guide/first-run.md): import, edit and switch, once. The
[README](../README.md) is the short version. Every command and flag is in
[the command reference](CLI.md), every error code in [the error reference](ERRORS.md) and every
file format in [the schema reference](SCHEMAS.md).

## Quick start

Install lodi from a release, or build it from source.

### Install from a release

Download the file for your distribution and `SHA256SUMS` from the project's Releases page,
`https://github.com/stshow/lodi/releases`. Put them in one folder, then check and install:

```sh
sha256sum -c --ignore-missing SHA256SUMS
sudo apt install ./lodi_2.0.0_amd64.deb                  # Debian, Ubuntu
sudo pacman -U lodi-2.0.0-1-x86_64.pkg.tar.zst           # Arch
sudo dnf install ./lodi-2.0.0-1.x86_64.rpm               # Fedora
install -Dm755 lodi-2.0.0-x86_64-linux-musl ~/.local/bin/lodi   # any distribution, no root
```

Run one of the four install lines. The three packages put the same static binary at
`/usr/bin/lodi`. The last line puts it in your own `~/.local/bin`, which works on other
distributions too. Installing lodi changes no other package and no setting.

The aarch64 files, `lodi_2.0.0_arm64.deb`, `lodi-2.0.0-1-aarch64.pkg.tar.zst`,
`lodi-2.0.0-1.aarch64.rpm` and `lodi-2.0.0-aarch64-linux-musl`, are built but never run by
the project. They may not work.

### Build from source

You need x86_64 Linux, `python3`, and either `rustup` or Nix with flakes. Both read the Rust
version from the repository. With Nix, first open its shell as
[Build lodi from source](BUILDING.md) shows. Then run this in a clone of the repository:

```sh
cargo build --locked --release --target x86_64-unknown-linux-musl   # static build
install -Dm755 target/x86_64-unknown-linux-musl/release/lodi ~/.local/bin/lodi
lodi --version                                                      # lodi 2.0.0
lodi --help
```

The binary is static. It needs neither Rust nor Nix to run.

## Everyday tasks

The host and your home:

- [Install a package on this machine](guide/install-a-package.md)
- [Preview a change before you switch](guide/preview-a-change.md)
- [Switch only your home or only the host](guide/switch-one-part.md)
- [Pin a package to a version](guide/pin-a-package.md)
- [Go back to an earlier config](guide/go-back.md)
- [Keep your config somewhere else](guide/where-your-config-lives.md)
- [Use lodi only for your home](guide/home-only.md)
- [Manage the files in your home](guide/manage-your-home.md)
- [Let lodi write a program's configuration](guide/configure-a-program.md)
- [Put tools on your PATH and run your own services](guide/tools-and-user-services.md)
- [Write a file under /etc](guide/write-a-file-under-etc.md)
- [Turn services on and off, and add users](guide/services-and-users.md)
- [Set the hostname, time zone, firewall and network](guide/set-system-settings.md)
- [Change kernel parameters or the bootloader](guide/kernel-and-boot.md)
- [Add a third-party apt or dnf repository](guide/add-a-repository.md)

Projects:

- [Run a project with its own tools](scopes/project.md)
- [Write a recipe for a tool](../catalogue/README.md)

## Supported and tested platforms

| Platform | Architecture | What the project runs |
|---|---|---|
| Ubuntu 24.04, Debian 12, Arch, Fedora 44 | x86_64 | each release is installed on a fresh virtual machine and the quick start is run, before it ships |
| Ubuntu 26.04 | x86_64 | the same, but a failure there does not stop the release |
| other Linux | x86_64 | nothing. The static binary may work, but no one has tried it |
| every distribution | aarch64 | nothing. The aarch64 files are built and never run |

Each release's notes say what it was tried on. A `[container]` project also needs rootless
Podman, which lodi never installs.

## License

lodi is free software under the GNU General Public License, version 3 or any later version.
The full text is in [LICENSE](../LICENSE). It comes with no warranty.

The license covers lodi itself. It does not cover your `lodi.toml`, your lock files, the
environments lodi builds or what you run in them. The licenses of the libraries in the binary
are listed in [the dependency list](licenses/DEPENDENCIES.md).
