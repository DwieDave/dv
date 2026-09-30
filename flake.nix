{
  description = "dv: fast Ratatui TUI for large JSON/YAML files";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, rust-overlay, ... }:
    let
      system = "aarch64-darwin";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
      rustToolchain = pkgs.rust-bin.stable.latest.default.override {
        extensions = [
          "rust-src"
          "rust-analyzer"
          "clippy"
          "rustfmt"
          "llvm-tools-preview"
        ];
      };
    in
    {
      formatter.${system} = pkgs.nixfmt;

      devShells.${system} = {
        default = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchain

            # test & quality
            cargo-nextest # fast test runner
            cargo-insta # snapshot tests (ratatui TestBackend buffers)
            cargo-llvm-cov # coverage
            cargo-deny # licenses / advisories / bans
            cargo-audit # RustSec advisories
            cargo-machete # unused deps
            bacon # background check/test loop

            # performance
            hyperfine # CLI benchmarks (preprocess < 1s target)
            samply # sampling profiler (macOS-native)
            cargo-bloat # binary size breakdown

            # test data
            jq
            yq-go

            just
            nixfmt
          ];

          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          RUST_BACKTRACE = "1";
        };

        # Nightly + cargo-fuzz for the fuzz targets (T6.3): `nix develop .#fuzz`.
        fuzz = pkgs.mkShell {
          packages = [
            (pkgs.rust-bin.nightly.latest.minimal.override { extensions = [ "rust-src" ]; })
            pkgs.cargo-fuzz
            pkgs.just
          ];
        };
      };
    };
}
