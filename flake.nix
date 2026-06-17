{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  inputs.treefmt-nix = {
    url = "github:numtide/treefmt-nix";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      treefmt-nix,
      ...
    }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.callPackage ./nix/package.nix { };
        }
      );

      formatter = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        (import ./nix/treefmt.nix { inherit pkgs treefmt-nix; }).config.build.wrapper
      );

      nixosModules.default = ./nix/module.nix;

      checks = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          proptest = pkgs.callPackage ./nix/proptest.nix { };
          treefmt = (import ./nix/treefmt.nix { inherit pkgs treefmt-nix; }).config.build.check ./.;
        }
        // nixpkgs.lib.mapAttrs' (n: nixpkgs.lib.nameValuePair "package-${n}") self.packages.${system}
        // nixpkgs.lib.mapAttrs' (n: nixpkgs.lib.nameValuePair "devshell-${n}") self.devShells.${system}
        // nixpkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          nixos-test = import ./nix/nixos-test.nix { inherit pkgs; };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.callPackage ./nix/shell.nix { };
        }
      );
    };
}
