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
      in
      {
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

        apps.fmt = {
          type = "app";
          program = "${pkgs.writeShellApplication {
            name = "fmt";
            runtimeInputs = with pkgs; [
              rustToolchain
              nixpkgs-fmt
              taplo
              prettier
            ];
            text = ''
              cargo fmt --all
              nixpkgs-fmt .
              taplo fmt
              prettier --write "**/*.md"
            '';
          }}/bin/fmt";
        };
      }
    );
}
