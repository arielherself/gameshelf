{
  description = "gameshelf: a TUI game launcher for Windows games via umu-launcher and Proton";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # As recommended by umu-launcher's README ("From our flake").
    umu = {
      url = "github:Open-Wine-Components/umu-launcher?dir=packaging/nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      umu,
    }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: {
        gameshelf = pkgs.callPackage ./nix/package.nix {
          umu-launcher = umu.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.gameshelf;
      });

      overlays.default = final: _prev: {
        gameshelf = final.callPackage ./nix/package.nix {
          umu-launcher = umu.packages.${final.stdenv.hostPlatform.system}.default;
        };
      };

      checks = forAllSystems (pkgs: {
        inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) gameshelf;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.gameshelf ];
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            umu.packages.${pkgs.stdenv.hostPlatform.system}.default
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-rfc-style);
    };
}
