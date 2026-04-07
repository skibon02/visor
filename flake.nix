{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-parts.url = "github:hercules-ci/flake-parts";
    nixgl.url = "github:guibou/nixGL";
  };

  outputs = inputs@{ self, nixpkgs, flake-parts, rust-overlay, nixgl, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [ "x86_64-linux" ];

      perSystem = { pkgs, system, ... }:
        let
          # Apply overlays to the pkgs instance provided by flake-parts
          pkgs = import nixpkgs {
            inherit system;
            overlays = [
              (import rust-overlay)
              nixgl.overlay
            ];
          };

          # Select the rust toolchain
          rustVersion = pkgs.rust-bin.stable.latest.default.override {
            extensions = [ "rust-src" ];
          };

          nixGL = pkgs.nixgl.auto.nixGLDefault;

          libraries = with pkgs; [
            dbus
              openssl
              udev
              libx11
              libxcursor
              libxi
              libxkbcommon
            ];

          ngl = pkgs.writeShellScriptBin "ngl" ''
            exec ${nixGL}/bin/nixGL "$@"
          '';

        in {
          devShells.default = pkgs.mkShell {
            nativeBuildInputs = [ pkgs.pkg-config ];

            buildInputs = [
              rustVersion
              nixGL
              ngl
            ] ++ libraries;

            env.RUST_SRC_PATH = "${rustVersion}/lib/rustlib/src/rust/library";

            shellHook = ''
              ln -sfn ${rustVersion}/bin .toolchain
              alias ngl="nixGL"

              export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath libraries}:$LD_LIBRARY_PATH

              echo "Run GUI apps with: ngl cargo run"
            '';
          };
        };
    };
}