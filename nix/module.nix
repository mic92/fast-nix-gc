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

    dates = lib.mkOption {
      type = with lib.types; either singleLineStr (listOf str);
      apply = lib.toList;
      default = [ "03:15" ];
      example = "weekly";
      description = ''
        When to run garbage collection. Calendar event in the format
        specified by {manpage}`systemd.time(7)`.
      '';
    };

    randomizedDelaySec = lib.mkOption {
      type = lib.types.singleLineStr;
      default = "0";
      example = "45min";
      description = "Randomized delay before each run.";
    };

    persistent = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Run on next boot if a scheduled run was missed.";
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
      description = "Free space until this much is available, then stop.";
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
  };

  options.services.fast-nix-optimise = {
    enable = lib.mkEnableOption "fast-nix-optimise, a faster nix-store --optimise";

    package = lib.mkOption {
      type = lib.types.package;
      default = cfg.package;
      defaultText = lib.literalExpression "config.services.fast-nix-gc.package";
      description = "Package providing the fast-nix-optimise binary.";
    };

    automatic = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Run store deduplication automatically on a schedule.";
    };

    dates = lib.mkOption {
      type = with lib.types; either singleLineStr (listOf str);
      apply = lib.toList;
      default = [ "04:15" ];
      example = "weekly";
      description = ''
        When to run. Calendar event in the format specified by
        {manpage}`systemd.time(7)`.
      '';
    };

    randomizedDelaySec = lib.mkOption {
      type = lib.types.singleLineStr;
      default = "0";
      example = "45min";
      description = "Randomized delay before each run.";
    };

    persistent = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Run on next boot if a scheduled run was missed.";
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
  };

  config = lib.mkMerge [
    (lib.mkIf cfg.enable {
      assertions = [
        {
          assertion = cfg.automatic -> config.nix.enable;
          message = "services.fast-nix-gc.automatic requires nix.enable";
        }
      ];

      warnings = lib.optional (cfg.automatic && config.nix.gc.automatic) ''
        Both services.fast-nix-gc.automatic and nix.gc.automatic are enabled.
        Disable nix.gc.automatic to avoid running two garbage collectors.
      '';

      systemd.services.fast-nix-gc = {
        description = "Fast Nix Garbage Collector";
        # `nix config show` for keep-derivations/keep-outputs.
        path = [ config.nix.package ];
        serviceConfig = {
          Type = "oneshot";
          ExecStart = lib.escapeShellArgs (
            [ "${cfg.package}/bin/fast-nix-gc" ]
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
            ++ cfg.extraArgs
          );
        };
        startAt = lib.optionals cfg.automatic cfg.dates;
        restartIfChanged = false;
      };

      systemd.timers.fast-nix-gc = lib.mkIf cfg.automatic {
        timerConfig = {
          RandomizedDelaySec = cfg.randomizedDelaySec;
          Persistent = cfg.persistent;
        };
      };
    })

    (lib.mkIf ocfg.enable {
      assertions = [
        {
          assertion = ocfg.automatic -> config.nix.enable;
          message = "services.fast-nix-optimise.automatic requires nix.enable";
        }
      ];

      warnings = lib.optional (ocfg.automatic && config.nix.optimise.automatic) ''
        Both services.fast-nix-optimise.automatic and nix.optimise.automatic are
        enabled. Disable nix.optimise.automatic to avoid running both.
      '';

      systemd.services.fast-nix-optimise = {
        description = "Fast Nix Store Optimiser";
        # `nix config show` for keep-derivations/keep-outputs.
        path = [ config.nix.package ];
        serviceConfig = {
          Type = "oneshot";
          ExecStart = lib.escapeShellArgs (
            [ "${ocfg.package}/bin/fast-nix-optimise" ]
            ++ lib.optionals (ocfg.minSize != null) [
              "--min-size"
              (toString ocfg.minSize)
            ]
            ++ lib.optionals (ocfg.jobs != null) [
              "--jobs"
              (toString ocfg.jobs)
            ]
            ++ ocfg.extraArgs
          );
        };
        startAt = lib.optionals ocfg.automatic ocfg.dates;
        restartIfChanged = false;
        # Avoid racing the GC.
        after = lib.optional cfg.enable "fast-nix-gc.service";
      };

      systemd.timers.fast-nix-optimise = lib.mkIf ocfg.automatic {
        timerConfig = {
          RandomizedDelaySec = ocfg.randomizedDelaySec;
          Persistent = ocfg.persistent;
        };
      };
    })
  ];
}
