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
      ../src
      ../tests
      ../benches
      ../proptest
    ];
  };
in
rustPlatform.buildRustPackage {
  pname = "fast-nix-gc";
  version = "0.1.0";
  inherit src;
  cargoLock.lockFile = ../Cargo.lock;
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [
    nix
    sqlite
  ];
  cargoBuildFlags = [
    "-p"
    "fast-nix-gc"
  ];
  cargoTestFlags = [
    "-p"
    "fast-nix-gc"
  ];
}
