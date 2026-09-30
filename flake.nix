{
  # The pinned development toolchain (LD-13). flake.lock fixes nixpkgs and
  # rust-overlay; rust-toolchain.toml fixes the Rust release, components and
  # the x86_64-unknown-linux-musl target. The built `lodi` binary is static and
  # does not need Nix at run time.
  description = "Lodi development toolchain";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, rust-overlay, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
      rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        # mkShell supplies the C compiler (the linker cargo invokes) and binutils.
        packages = [
          rust
          pkgs.python3
        ];
      };
    };
}
