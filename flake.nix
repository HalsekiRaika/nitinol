{
  description = "TAKT + AI coding agents + Rust development environment for NixOS-WSL";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    codex-cli-nix = {
      url = "github:sadjow/codex-cli-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    claude-code-nix = {
      url = "github:sadjow/claude-code-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, codex-cli-nix, claude-code-nix, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ 
          (import rust-overlay)
        ];
        pkgs = import nixpkgs {
          inherit system overlays;
          config.allowUnfree = true;
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "clippy"
            "rustfmt"
            "rust-src"
            "rust-analyzer"
          ];
        };

      in {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain
            pkgs.cargo-edit
            pkgs.cargo-watch
            codex-cli-nix.packages.${system}.default
            claude-code-nix.packages.${system}.default
            pkgs.opencode
            pkgs.nodejs_22
            pkgs.gh
            pkgs.pkg-config
            pkgs.openssl
            pkgs.ripgrep
            pkgs.python3
          ];

          PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";

          shellHook = ''
            export NPM_CONFIG_PREFIX="$HOME/.npm-global"
            export PATH="$NPM_CONFIG_PREFIX/bin:$PATH"
            mkdir -p "$NPM_CONFIG_PREFIX"

            if ! command -v cargo-dylint &> /dev/null; then
              echo "📦 Installing cargo-dylint..."
              cargo install cargo-dylint dylint-link
            fi

            if ! command -v takt &> /dev/null; then
              echo "📦 Installing TAKT..."
              npm install -g takt
            fi

            echo "takt     : $(takt --version 2>/dev/null || echo 'installing...')"
            echo "claude   : $(claude --version 2>/dev/null || echo 'installing...')"
            echo "codex    : $(codex --version 2>/dev/null || echo 'not found')"
            echo "rustc    : $(rustc --version)"
            echo "cargo    : $(cargo --version)"
            echo "opencode : $(opencode --version 2>/dev/null || echo 'not found')"
            echo "gh       : $(gh --version | head -1)"
          '';
        };
      }
    );
}
