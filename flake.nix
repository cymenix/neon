{
  inputs = {
    nixpkgs = {
      url = "github:NixOS/nixpkgs/nixos-unstable";
    };
    flake-utils = {
      url = "github:numtide/flake-utils";
    };
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs = {
        nixpkgs = {
          follows = "nixpkgs";
        };
        flake-utils = {
          follows = "flake-utils";
        };
      };
    };
  };
  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        overlays = [(import rust-overlay)];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rustToolchain = (pkgs.pkgsBuildHost.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override {
          extensions = ["rust-src" "clippy" "llvm-tools"];
        };
        buildInputs = with pkgs; [
          coreutils
          bash
          openssl
          pkg-config
          flex
          bison
          icu
          readline
          zlib
          libseccomp
          curl
          protobuf
          rustPlatform.bindgenHook
        ];
        nativeBuildInputs = with pkgs; [
          rustToolchain
          rust-analyzer
        ];
      in {
        devShell = pkgs.mkShell {
          inherit buildInputs nativeBuildInputs;
          RUST_BACKTRACE = 1;
          RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
        };
      }
    );
}
