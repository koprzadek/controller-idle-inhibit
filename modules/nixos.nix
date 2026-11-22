# NixOS module: services.controller-idle-inhibit.*
#
# Installs the package system-wide and defines a systemd user unit
# equivalent to the shipped controller-idle-inhibit.service: bound to
# graphical-session.target, restarted on failure, logging captured by
# the journal.
#
# river does not start graphical-session.target by itself — still add the
# two lines from the README ("Hooking into your river session") to your
# river startup.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.controller-idle-inhibit;

  cliArgs =
    lib.optionals (cfg.grace != null) [
      "--grace"
      (toString cfg.grace)
    ]
    ++ lib.optionals cfg.buttonsOnly [ "--buttons-only" ]
    ++ lib.optionals cfg.verbose [ "--verbose" ]
    ++ cfg.extraArgs;
in
{
  options.services.controller-idle-inhibit = {
    enable =
      lib.mkEnableOption ''
        controller-idle-inhibit, a daemon that suppresses Wayland idle while
        a gamepad is in use
      '';

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ../nix/package.nix { };
      defaultText = lib.literalExpression "pkgs.controller-idle-inhibit";
      description = "The controller-idle-inhibit package to use.";
    };

    grace = lib.mkOption {
      type = with lib.types; nullOr ints.unsigned;
      default = 120;
      description = ''
        Seconds to keep inhibiting idle after the last gamepad input
        (passed as --grace). Set to `null` to use the program default.
      '';
    };

    buttonsOnly = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Only count button presses and ignore stick/trigger movement
        (passed as --buttons-only). Useful for pads that stream
        gyro/accelerometer noise while idle.
      '';
    };

    verbose = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Enable verbose logging (passed as --verbose).";
    };

    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "--poll-ms 50" ];
      description = "Additional command-line arguments for the daemon.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    systemd.user.services.controller-idle-inhibit = {
      description = "Inhibit Wayland idle while a gamepad is in use";
      wantedBy = [ "graphical-session.target" ];
      partOf = [ "graphical-session.target" ];
      after = [ "graphical-session.target" ];

      serviceConfig = {
        Type = "simple";
        ExecStart = lib.escapeShellArgs (
          [ "${cfg.package}/bin/controller-idle-inhibit" ] ++ cliArgs
        );
        Restart = "on-failure";
        RestartSec = 3;
      };
    };
  };
}
