{
  description = "Local development environment for nod";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fenixpkgs = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, fenixpkgs, ... }:
    let
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
    in
    {
      devShells = nixpkgs.lib.genAttrs systems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          fenix = fenixpkgs.packages.${system};
          toolchain = fenix.combine [
            (fenix.stable.withComponents [
              "cargo"
              "clippy"
              "rust-analyzer"
              "rust-src"
              "rustc"
            ])
            # rustfmt.toml uses nightly-only options, as does the formatting CI job.
            fenix.complete.rustfmt
          ];
        in
        {
          default = pkgs.mkShell {
            name = "nod-dev";

            packages = with pkgs; [
              toolchain
              rust-cbindgen
              cmake
              just
              ninja
              pkg-config
              python3
              uv
            ];

            buildInputs = with pkgs; [
              bzip2
              openssl
              xz
              zlib
              zstd
            ];

            RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";
          };
        });
    };
}
