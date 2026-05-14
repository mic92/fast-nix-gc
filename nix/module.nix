{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.fast-nix-gc;
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

    minFree = lib.mkOption {
      type = lib.types.nullOr lib.types.singleLineStr;
      default = null;
      example = "50G";
      description = "Free space until this much is available, then stop.";
    };

    keepRecent = lib.mkOption {
      type = lib.types.nullOr lib.types.singleLineStr;
      default = null;
      example = "7d";
      description = "Keep store paths registered within this time window.";
    };

    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Extra arguments to pass to fast-nix-gc.";
    };
  };

  config = lib.mkIf cfg.enable {
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
      serviceConfig = {
        Type = "oneshot";
        ExecStart = lib.escapeShellArgs (
          [ "${cfg.package}/bin/fast-nix-gc" ]
          ++ lib.optionals (cfg.deleteOlderThan != null) [
            "--delete-older-than"
            cfg.deleteOlderThan
          ]
          ++ lib.optionals (cfg.minFree != null) [
            "--min-free"
            cfg.minFree
          ]
          ++ lib.optionals (cfg.keepRecent != null) [
            "--keep-recent"
            cfg.keepRecent
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
  };
}
