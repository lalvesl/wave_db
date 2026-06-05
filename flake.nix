{
  description = "WaveDB — user-partitioned, tenant-centric embedded database";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self
    , nixpkgs
    , flake-utils
    , rust-overlay
    , ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        # Reads channel, components, and targets from rust-toolchain.toml.
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        # Custom rust platform using the project toolchain (includes wasm32 target).
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        # wasm-bindgen-cli built at the exact version used by the crate in Cargo.lock.
        wasmBindgenCli = pkgs.rustPlatform.buildRustPackage rec {
          pname = "wasm-bindgen-cli";
          version = "0.2.121";

          src = pkgs.fetchCrate {
            inherit pname version;
            hash = "sha256-ZOMgFNOcGkO66Jz/Z83eoIu+DIzo3Z/vq6Z5g6BDY/w=";
          };

          cargoHash = "sha256-DPdCDPTAPBrbqLUqnCwQu1dePs9lGg85JCJOCIr9qjU=";

          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [
            pkgs.openssl
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.darwin.apple_sdk.frameworks.Security
          ];
        };
      in
      {
        packages.wasm = rustPlatform.buildRustPackage {
          pname = "wavedb-wasm";
          version = "0.1.0";
          src = ./.;

          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ wasmBindgenCli ];

          buildPhase = ''
            runHook preBuild
            cargo build --target wasm32-unknown-unknown --release -p wavedb-wasm
            runHook postBuild
          '';

          installPhase = ''
            runHook preInstall
            mkdir -p $out
            wasm-bindgen \
              --out-dir $out \
              --target bundler \
              target/wasm32-unknown-unknown/release/wavedb_wasm.wasm
            runHook postInstall
          '';

          doCheck = false;
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            pkg-config
            rustToolchain

            # Code quality
            cargo-mutants
            cargo-deny
            taplo
            nixpkgs-fmt
            prettier

            # Testing
            cargo-nextest

            # WASM
            wasm-pack
            wasm-bindgen-cli
          ];

          buildInputs =
            with pkgs;
            [
              openssl
            ]
            ++ lib.optionals stdenv.isDarwin [
              darwin.apple_sdk.frameworks.SystemConfiguration
              darwin.apple_sdk.frameworks.CoreFoundation
              darwin.apple_sdk.frameworks.Security
            ];

          shellHook = ''
            export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig"
          '';
        };

        apps.wavedb_monitor = {
          type = "app";
          program = "${
            pkgs.writeShellApplication {
              name = "wavedb-monitor";
              runtimeInputs = [ rustToolchain ];
              text = ''
                cargo run --release --bin wavedb-monitor "$@"
              '';
            }
          }/bin/wavedb-monitor";
        };

        # ── real_example: multi-process orchestrated load test ──────────────────
        #
        # Builds all five binaries first, then runs the orchestrator which
        # spawns subprocesses pointing at the sibling binaries in the same
        # release output directory.
        apps.real_example = {
          type = "app";
          program = "${
            pkgs.writeShellApplication {
              name = "real_example";
              runtimeInputs = [ rustToolchain ];
              text = ''
                set -euo pipefail

                echo "── Building all real_example binaries ──────────────────────────"
                cargo build --release \
                  --bin real_example \
                  --bin re_slow_node \
                  --bin re_quick_node \
                  --bin re_client \
                  --bin re_monitor

                echo "── Launching orchestrator ──────────────────────────────────────"
                # The orchestrator discovers its sibling binaries via
                # std::env::current_exe() — all five binaries live in the same
                # target/release/ directory after the cargo build above.
                exec ./target/release/real_example "$@"
              '';
            }
          }/bin/real_example";
        };

        apps.fmt = {
          type = "app";
          program = "${
            pkgs.writeShellApplication {
              name = "fmt";
              runtimeInputs = with pkgs; [
                rustToolchain
                nixpkgs-fmt
                taplo
                prettier
                jq
              ];
              text = ''
                cargo fmt --all
                nixpkgs-fmt .
                taplo fmt
                prettier --write "**/*.md"
                while IFS= read -r -d "" f; do
                  tmp="$(mktemp)"
                  jq . "$f" > "$tmp" && mv "$tmp" "$f"
                done < <(find . -name "*.jsonl" -not -path "./.git/*" -not -path "./target/*" -print0)
              '';
            }
          }/bin/fmt";
        };
      }
    );
}
