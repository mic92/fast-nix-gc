{
  lib,
  rustPlatform,
  pkg-config,
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
  pname = "fast-nix-gc-proptest";
  version = "0.1.0";
  inherit src;
  cargoLock = {
    lockFile = ../Cargo.lock;
    outputHashes = {
      "harmonia-nar-3.1.0" = "sha256-PIwK3nLlkNLf5pC+PWzwE34bTf4t/7uTrw0GjPDeU7M=";
    };
  };
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ sqlite ];
  cargoTestFlags = [
    "-p"
    "fast-nix-gc-proptest"
  ];
  # No binary to install, only tests.
  doInstallCheck = false;
  installPhase = "touch $out";
}
