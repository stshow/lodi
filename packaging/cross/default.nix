# The aarch64 cross-build environment for the release build (LD-34; AGENTS.md §9.3a).
#
#   nix-shell packaging/cross/default.nix --run 'COMMAND'
#
# It adds nothing to the host and changes no pinned file. Every input is taken from the
# repository's own pins:
#
#   * nixpkgs and rust-overlay come from the root flake.lock (the same revisions flake.nix
#     uses for the development shell), fetched by their locked narHash;
#   * the Rust release and the host target come from rust-toolchain.toml, with
#     aarch64-unknown-linux-musl added here only — rust-toolchain.toml is inside the M-Spike
#     evidence's sources digest and is never edited for a cross build.
#
# `ring` and the other dependencies that compile C and assembly need a cross C compiler.
# `zig cc -target aarch64-linux-musl` is that compiler (it carries its own musl headers, so no
# host package is installed); `rust-lld` with `-C link-self-contained=yes` links the static
# aarch64 ELF. The wrapper drops cargo's `cc` crate's own `--target=<rust triple>` argument,
# which zig does not understand.
#
# Caches: zig writes to $LODI_ZIG_CACHE when the caller sets it, otherwise to a directory in
# the build tree. Nothing is written to the user's home beyond cargo's and Nix's own caches.
let
  lock = builtins.fromJSON (builtins.readFile ../../flake.lock);
  pinned =
    name:
    let
      l = lock.nodes.${name}.locked;
    in
    builtins.fetchTarball {
      url = "https://github.com/${l.owner}/${l.repo}/archive/${l.rev}.tar.gz";
      sha256 = l.narHash;
    };

  pkgs = import (pinned "nixpkgs") {
    system = "x86_64-linux";
    overlays = [ (import (pinned "rust-overlay")) ];
  };

  toolchain = (builtins.fromTOML (builtins.readFile ../../rust-toolchain.toml)).toolchain;
  rust = pkgs.rust-bin.stable.${toolchain.channel}.minimal.override {
    targets = toolchain.targets ++ [ "aarch64-unknown-linux-musl" ];
  };

  zigCache = ''
    export ZIG_GLOBAL_CACHE_DIR="''${LODI_ZIG_CACHE:-$PWD/target/zig-cache}"
    export ZIG_LOCAL_CACHE_DIR="$ZIG_GLOBAL_CACHE_DIR"
    mkdir -p "$ZIG_GLOBAL_CACHE_DIR"
  '';

  zcc = pkgs.writeShellScriptBin "lodi-aarch64-cc" ''
    ${zigCache}
    args=()
    for a in "$@"; do
      case "$a" in
        --target=*) ;;
        *) args+=("$a") ;;
      esac
    done
    exec ${pkgs.zig}/bin/zig cc -target aarch64-linux-musl "''${args[@]}"
  '';
  zar = pkgs.writeShellScriptBin "lodi-aarch64-ar" ''
    ${zigCache}
    exec ${pkgs.zig}/bin/zig ar "$@"
  '';
in
pkgs.mkShell {
  packages = [
    rust
    zcc
    zar
    pkgs.zig
    pkgs.python3
    pkgs.zstd
    pkgs.gzip
    pkgs.gnutar
    pkgs.coreutils
    pkgs.git
  ];

  CC_aarch64_unknown_linux_musl = "lodi-aarch64-cc";
  AR_aarch64_unknown_linux_musl = "lodi-aarch64-ar";
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER = "rust-lld";
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS = "-C link-self-contained=yes -C linker-flavor=ld.lld";

  shellHook = zigCache;
}
