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
    pg14 = {
      url = "github:neondatabase/postgres/REL_14_STABLE_neon";
      flake = false;
    };
    pg15 = {
      url = "github:neondatabase/postgres/REL_15_STABLE_neon";
      flake = false;
    };
    pg16 = {
      url = "github:neondatabase/postgres/REL_16_STABLE_neon";
      flake = false;
    };
  };
  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
    pg14,
    pg15,
    pg16,
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
          readline
          zlib
          icu
          perl
          openssl
          libseccomp
          curl
          protobuf
          pkg-config
          rustPlatform.bindgenHook
        ];
        nativeBuildInputs = with pkgs; [
          coreutils
          bash
          openssl
          pkg-config
          flex
          bison
          icu
          libseccomp
          curl
          perl
          protobuf
          rustPlatform.bindgenHook
          rustToolchain
        ];
      in
        with pkgs; {
          packages = {
            default = rustPlatform.buildRustPackage {
              inherit buildInputs nativeBuildInputs;
              name = "neondb";

              src = self;

              cargoLock = {
                lockFile = ./Cargo.lock;
                outputHashes = {
                  "heapless-0.8.0" = "sha256-phCls7RQZV0uYhDEp0GIphTBw0cXcurpqvzQCAionhs=";
                  "postgres-0.19.4" = "sha256-avMTCF5BRgSun1etqj+edVGMifyv3FxF0pS0WQ+CF5E=";
                  "parquet-49.0.0" = "sha256-E5KuNB9O+yOvnvrB4WjDNGSbsWLdFvcSNz9xBfUL3ac=";
                  "svg_fmt-0.4.2" = "sha256-+UyAowTBKc+3NJWJOH74cxV4l04UOH67KGac/NrJL1M=";
                  "tokio-epoll-uring-0.1.0" = "sha256-R33KZtNMRy34C0Ea9jgd1Jp9ckU+gHW8ufwC9sqOgiM=";
                };
              };

              postPatch = ''
                mkdir -p vendor/postgres-v14
                mkdir -p vendor/postgres-v15
                mkdir -p vendor/postgres-v16

                cp -r ${pg14}/* ./vendor/postgres-v14/
                cp -r ${pg15}/* ./vendor/postgres-v15/
                cp -r ${pg16}/* ./vendor/postgres-v16/

                substituteInPlace ./Makefile \
                  --replace-fail "&& git rev-parse HEAD" ""

                substituteInPlace ./scripts/ninstall.sh \
                  --replace-fail /bin/bash ${pkgs.bash}/bin/bash

                substituteInPlace ./vendor/postgres-v14/configure \
                  --replace-fail /bin/pwd ${pkgs.coreutils}/bin/pwd

                substituteInPlace ./vendor/postgres-v15/configure \
                  --replace-fail /bin/pwd ${pkgs.coreutils}/bin/pwd

                substituteInPlace ./vendor/postgres-v16/configure \
                  --replace-fail /bin/pwd ${pkgs.coreutils}/bin/pwd

                substituteInPlace ./vendor/postgres-v14/configure.ac \
                  --replace-fail /bin/pwd ${pkgs.coreutils}/bin/pwd

                substituteInPlace ./vendor/postgres-v15/configure.ac \
                  --replace-fail /bin/pwd ${pkgs.coreutils}/bin/pwd

                substituteInPlace ./vendor/postgres-v16/configure.ac \
                  --replace-fail /bin/pwd ${pkgs.coreutils}/bin/pwd
              '';

              updateAutotoolsGnuConfigScriptsPhase = ''
                echo "Stubbed"
              '';

              BUILD_TYPE = "release";

              CARGO_BUILD_FLAGS = "--features=testing";

              buildPhase = ''
                make -j`nproc` -s postgres-v16 neon-pg-ext-v16
                export PQ_LIB_DIR="$(pwd)/pg_install/v16/lib";
                export LD_LIBRARY_PATH="$PQ_LIB_DIR:$LD_LIBRARY_PATH"
                make -j`nproc` -s neon
              '';

              installPhase = ''
                mkdir -p $out/bin $out/pg_install
                cp -r pg_install/* $out/pg_install/
                cp target/release/neon_local $out/bin
                cp target/release/safekeeper $out/bin
                cp target/release/pageserver $out/bin
                cp target/release/storage_broker $out/bin
                cp target/release/storage_controller $out/bin
                cp target/release/proxy $out/bin
                cp target/release/pagectl $out/bin
                cp target/release/compute_ctl $out/bin
                cp target/release/storcon_cli $out/bin
                cp target/release/pagebench $out/bin
                cp target/release/trace $out/bin
                cp target/release/vm-monitor $out/bin
                cp target/release/wal_craft $out/bin
              '';

              doCheck = false;
              dontFixup = true;
            };
          };
          devShell = mkShell {
            inherit buildInputs;
            nativeBuildInputs =
              nativeBuildInputs
              ++ [
                rustToolchain
                rust-analyzer
              ];
            RUST_BACKTRACE = 1;
            RUST_SRC_PATH = "${rust.packages.stable.rustPlatform.rustLibSrc}";
          };
        }
    );
}
