{
  pkgs,
  treefmt-nix,
}:
treefmt-nix.lib.evalModule pkgs {
  projectRootFile = "flake.lock";
  programs.nixfmt = {
    enable = true;
    package = pkgs.nixfmt-rs;
  };
  programs.rustfmt = {
    enable = true;
    edition = "2024";
  };
  programs.deadnix.enable = true;
}
