{
  config,
  lib,
  ...
}:
let
  cfg = config.services.fast-nix-gc;
  ocfg = config.services.fast-nix-optimise;
in
{
  imports = [ ./service-options.nix ];

  options.services.fast-nix-gc = {
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
  };

  options.services.fast-nix-optimise = {
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
          ExecStart = lib.escapeShellArgs cfg.argv;
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
          ExecStart = lib.escapeShellArgs ocfg.argv;
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
