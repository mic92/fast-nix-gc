{
  lib,
  rustPlatform,
  pkg-config,
  nix,
  sqlite,
}:
let
  fs = lib.fileset;
  src = fs.toSource {
    root = ../.;
    fileset = fs.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      ../proptest
    ];
  };
in
rustPlatform.buildRustPackage {
  pname = "fast-nix-gc";
  version = "0.1.0";
  inherit src;
  cargoLock = {
    lockFile = ../Cargo.lock;
    outputHashes = {
      "harmonia-file-nar-3.1.0" = "sha256-YklzRujFo5lvFsdLoedE6OL6OvSwNk/nfwlGxulyTS4=";
    };
  };
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [
    nix
    sqlite
  ];
  cargoBuildFlags = [
    "-p"
    "fast-nix-gc"
    "-p"
    "fast-nix-optimise"
  ];
  cargoTestFlags = [
    "-p"
    "fast-nix-gc"
    "-p"
    "fast-nix-common"
    "-p"
    "fast-nix-optimise"
  ];
}
