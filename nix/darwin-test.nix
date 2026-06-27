# A minimal nix-darwin system that enables both services through
# darwinModules.default. Used two ways (see flake.nix):
#   - checks.<darwin>.darwin-module builds .system (eval + build coverage)
#   - darwinConfigurations.ci is activated on a macOS runner
#     (.github/workflows/darwin-module.yaml)
#
# When activate is set we keep nix-darwin from managing nix.conf, since CI
# installs Nix with an external installer; that in turn disables the
# scheduled timers (the module asserts automatic -> nix.enable).
{
  darwin,
  darwinModule,
  system,
  activate ? false,
}:
darwin.lib.darwinSystem {
  inherit system;
  modules = [
    darwinModule
    {
      system.stateVersion = 5;
      system.primaryUser = "runner";
      nix.enable = !activate;

      services.fast-nix-gc = {
        enable = true;
        automatic = !activate;
        startCalendarInterval = [
          {
            Hour = 3;
            Minute = 15;
          }
        ];
        deleteOlderThan = "30d";
        ensureFree = "20%";
        gcRootsDirs = [ "/mnt/extra-roots" ];
      };

      services.fast-nix-optimise = {
        enable = true;
        automatic = !activate;
      };
    }
  ];
}
