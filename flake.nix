{
  description = "A basic flake with a shell";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  inputs.flake-utils.url = "github:numtide/flake-utils";
  inputs.naersk.url = "github:nix-community/naersk";
  inputs.rust-overlay.url = "github:oxalica/rust-overlay";

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    naersk,
    rust-overlay,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };
      rust = pkgs.rust-bin.stable.latest.default;
      naersk' = pkgs.callPackage naersk {
        cargo = rust;
        rustc = rust;
      };
    in {
      devShell = pkgs.mkShell {
        nativeBuildInputs = [rust];
        RUST_PATH = "${rust}";
        RUST_DOC_PATH = "${rust}/share/doc/rust/html/std/index.html";
      };

      packages = rec {
        fioul = naersk'.buildPackage {
          name = "fioul";
          inherit ((pkgs.lib.importTOML ./cli/Cargo.toml).package) version;
          src = ./.;

          cargoBuildOptions = opts: opts ++ ["--package fioul-cli"];
        };
        fioul-server = naersk'.buildPackage {
          name = "fioul-server";
          inherit ((pkgs.lib.importTOML ./server/Cargo.toml).package) version;
          src = ./.;

          cargoBuildOptions = opts: opts ++ ["--package fioul-server"];
        };
        default = fioul;
      };
    })
    // {
      nixosModules.default = import ./nix/nixos.nix self;
      homeManagerModules.default = import ./nix/hm.nix self;
    };
}
