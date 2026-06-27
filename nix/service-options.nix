# Platform-independent options shared by the NixOS and nix-darwin modules.
# Scheduling options (systemd dates vs launchd intervals) live in the
# platform modules that import this file.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.fast-nix-gc;
  ocfg = config.services.fast-nix-optimise;
in
{
  options.services.fast-nix-gc = {
    enable = lib.mkEnableOption "fast-nix-gc, a faster nix-collect-garbage";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./package.nix { }";
      description = "fast-nix-gc package to use.";
    };

    automatic = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Run garbage collection automatically on a schedule.";
    };

    deleteOlderThan = lib.mkOption {
      type = lib.types.nullOr lib.types.singleLineStr;
      default = null;
      example = "30d";
      description = "Delete profile generations older than this.";
    };

    ensureFree = lib.mkOption {
      type = lib.types.nullOr lib.types.singleLineStr;
      default = null;
      example = "50G";
      description = ''
        Free space until this much is available, then stop. Accepts an
        absolute size like "50G" or a percentage of the store's filesystem
        like "20%".
      '';
    };

    keepRecent = lib.mkOption {
      type = lib.types.nullOr lib.types.singleLineStr;
      default = null;
      example = "1d";
      description = ''
        Keep store paths registered within this time window. Avoids deleting
        build dependencies fetched during a recent build.
      '';
    };

    noVacuum = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Skip the SQLite VACUUM after garbage collection. Enable on busy
        builders, where concurrent nix-daemon readers prevent cleanup of
        the database-sized WAL that VACUUM produces; see the README.
      '';
    };

    chunkSize = lib.mkOption {
      type = lib.types.nullOr lib.types.ints.positive;
      default = null;
      description = ''
        Number of dead paths invalidated per database transaction. Lower
        values keep the WAL (and its disk use) smaller during deletion at
        the cost of more checkpoints; null uses the default (65536).
      '';
    };

    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Extra arguments to pass to fast-nix-gc.";
    };

    argv = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      internal = true;
      readOnly = true;
    };
  };

  options.services.fast-nix-optimise = {
    enable = lib.mkEnableOption "fast-nix-optimise, a faster nix-store --optimise";

    package = lib.mkOption {
      type = lib.types.package;
      default = config.services.fast-nix-gc.package;
      defaultText = lib.literalExpression "config.services.fast-nix-gc.package";
      description = "Package providing the fast-nix-optimise binary.";
    };

    automatic = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Run store deduplication automatically on a schedule.";
    };

    minSize = lib.mkOption {
      type = lib.types.nullOr lib.types.ints.unsigned;
      default = null;
      example = 4096;
      description = "Skip files smaller than this many bytes.";
    };

    jobs = lib.mkOption {
      type = lib.types.nullOr lib.types.ints.positive;
      default = null;
      description = "Concurrency. Defaults to the number of CPUs.";
    };

    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Extra arguments to pass to fast-nix-optimise.";
    };

    argv = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      internal = true;
      readOnly = true;
    };
  };

  config = {
    services.fast-nix-gc.argv = [
      "${cfg.package}/bin/fast-nix-gc"
    ]
    ++ lib.optionals (cfg.deleteOlderThan != null) [
      "--delete-older-than"
      cfg.deleteOlderThan
    ]
    ++ lib.optionals (cfg.ensureFree != null) [
      "--ensure-free"
      cfg.ensureFree
    ]
    ++ lib.optionals (cfg.keepRecent != null) [
      "--keep-recent"
      cfg.keepRecent
    ]
    ++ lib.optional cfg.noVacuum "--no-vacuum"
    ++ lib.optionals (cfg.chunkSize != null) [
      "--chunk-size"
      (toString cfg.chunkSize)
    ]
    ++ cfg.extraArgs;

    services.fast-nix-optimise.argv = [
      "${ocfg.package}/bin/fast-nix-optimise"
    ]
    ++ lib.optionals (ocfg.minSize != null) [
      "--min-size"
      (toString ocfg.minSize)
    ]
    ++ lib.optionals (ocfg.jobs != null) [
      "--jobs"
      (toString ocfg.jobs)
    ]
    ++ ocfg.extraArgs;
  };
}
