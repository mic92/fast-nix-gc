{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { nixpkgs, ... }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" "x86_64-darwin" ];
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "fast-gc";
            version = "0.1.0";
            src = ./.;
            cargoHash = "sha256-CmS7qn+tl/363RhgchZcm94qahCVPOT3fluhp+AyTNI=";
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ pkgs.nix ];
          };
        });

      devShells = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
              pkgs.pkg-config
              pkgs.nix
            ];
          };
        });
    };
}
