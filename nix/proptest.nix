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
      ../crates
      ../proptest
      ../difftest
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
      "harmonia-file-nar-3.1.0" = "sha256-YklzRujFo5lvFsdLoedE6OL6OvSwNk/nfwlGxulyTS4=";
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
