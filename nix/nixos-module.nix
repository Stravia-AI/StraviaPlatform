{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.stravia;
in
{
  options.services.stravia = {
    enable = lib.mkEnableOption "Stravia AI protocol gateway";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "inputs.stravia.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = "Stravia server package to run.";
    };

    host = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1";
      description = "Address on which the Stravia server listens.";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 23471;
      description = "TCP port on which the Stravia server listens.";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Whether to open the configured TCP port in the firewall.";
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/run/secrets/stravia.env";
      description = ''
        Optional systemd environment file for Stravia settings and secrets.
        Use it for values such as STRAVIA_ADMIN_TOKEN, STRAVIA_PUBLIC_ORIGIN,
        STRAVIA_STORAGE_BACKEND, and STRAVIA_POSTGRES_DSN.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    networking.firewall.allowedTCPPorts = lib.optionals cfg.openFirewall [ cfg.port ];

    systemd.services.stravia = {
      description = "Stravia AI protocol gateway";
      documentation = [ "https://github.com/Stravia-AI/StraviaPlatform" ];
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];

      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} --host ${cfg.host} --port ${toString cfg.port} --data-dir /var/lib/stravia";
        Restart = "on-failure";
        RestartSec = "5s";

        DynamicUser = true;
        StateDirectory = "stravia";
        StateDirectoryMode = "0700";
        WorkingDirectory = "/var/lib/stravia";
        UMask = "0077";

        CapabilityBoundingSet = "";
        LockPersonality = true;
        NoNewPrivileges = true;
        PrivateDevices = true;
        PrivateTmp = true;
        ProtectHome = true;
        ProtectSystem = "strict";
      }
      // lib.optionalAttrs (cfg.environmentFile != null) {
        EnvironmentFile = cfg.environmentFile;
      };
    };
  };
}
