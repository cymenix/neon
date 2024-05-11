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
            # build with nix build '.?submodules=1'
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
                # Gets rid of git errors, however postgres binaries will not have
                # the neon postgres fork commit hash appended to the postgres version number string
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

              BUILD_TYPE = "release";

              CARGO_BUILD_FLAGS = "--features=testing";

              buildPhase = ''
                export PQ_LIB_DIR="$(pwd)/pg_install/v16/lib";
                make -j`nproc` -s
              '';

              installPhase = ''
                mkdir -p $out/bin $out/pg_install
                cp -r pg_install/* $out/pg_install/
                ln -s $out/pg_install/v16/lib $out/lib
                ln -s $out/pg_install/v16/bin/* $out/bin/
                cp --target-directory=$out/bin target/release/{neon_local,safekeeper,pageserver,storage_broker,storage_controller,proxy,pagectl,compute_ctl}
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
            shellHook = ''
              export PQ_LIB_DIR="$(pwd)/pg_install/v16/lib";
              export LD_LIBRARY_PATH="$PQ_LIB_DIR:$LD_LIBRARY_PATH"
            '';
          };
        }
    );
}
