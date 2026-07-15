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
  };

  outputs = { self, nixpkgs, flake-utils, codex-cli-nix, claude-code-nix }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
        };
      in {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            codex-cli-nix.packages.${system}.default
            claude-code-nix.packages.${system}.default
            pkgs.opencode
            pkgs.nodejs_22
            pkgs.gh
            pkgs.pkg-config
            pkgs.openssl
            pkgs.gcc
            pkgs.ripgrep
            pkgs.python3
          ];

          PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
            pkgs.openssl
            pkgs.zlib
            pkgs.gcc.cc.lib
          ];

          shellHook = ''
            if ! rustup show active-toolchain &> /dev/null; then
              echo "Installing Rust stable toolchain..."
              rustup default stable
              rustup component add clippy rustfmt rust-analyzer rust-src
            fi

            if ! command -v cargo-dylint &> /dev/null; then
              echo "Installing cargo-dylint..."
              cargo install cargo-dylint dylint-link
            fi

            export NPM_CONFIG_PREFIX="$HOME/.npm-global"
            export PATH="$NPM_CONFIG_PREFIX/bin:$PATH"
            export NPM_CONFIG_UPDATE_NOTIFIER=false
            mkdir -p "$NPM_CONFIG_PREFIX"

            if ! command -v takt &> /dev/null; then
              echo "Installing TAKT..."
              npm install -g takt
            fi

            echo "takt     : $(takt --version 2>/dev/null || echo 'installing...')"
            echo "claude   : $(claude --version 2>/dev/null || echo 'installing...')"
            echo "codex    : $(codex --version 2>/dev/null || echo 'not found')"
            echo "rustc    : $(rustc --version)"
            echo "cargo    : $(cargo --version)"
            echo "dylint   : $(cargo dylint --version 2>/dev/null || echo 'not installed')"
            echo "opencode : $(opencode --version 2>/dev/null || echo 'not found')"
            echo "gh       : $(gh --version | head -1)"
          '';
        };
      }
    );
}
