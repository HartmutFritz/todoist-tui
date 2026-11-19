{
  description = "A fast, offline-first Todoist terminal client";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      overlays.default = final: _prev: {
        todoist-tui = final.callPackage ./nix/package.nix { };
      };

      packages = forAllSystems (system:
        let pkgs = import nixpkgs { inherit system; };
        in rec {
          todoist-tui = pkgs.callPackage ./nix/package.nix { };
          default = todoist-tui;
        });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/todoist-tui";
          meta.description = "A fast, offline-first Todoist terminal client";
        };
      });

      checks = forAllSystems (system:
        let pkgs = import nixpkgs { inherit system; };
        in import ./nix/checks.nix {
          inherit pkgs;
          package = self.packages.${system}.default;
        });
    };
}
