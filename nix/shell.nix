{
  callPackage,
  mkShell,
  clippy,
  rustfmt,
}:
mkShell {
  inputsFrom = [ (callPackage ./package.nix { }) ];
  packages = [
    clippy
    rustfmt
  ];
}
