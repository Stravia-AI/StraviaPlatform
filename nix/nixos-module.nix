{ self }:
{
  config,
  lib,
  pkgs,
  utils,
  ...
}:
let
  inherit (lib) mkOption types;
  cfg = config.services.stravia;
  absolutePath = types.addCheck types.str (
    value:
    lib.hasPrefix "/" value
    && value != "/"
    && !(lib.hasInfix "\n" value)
    && !(lib.hasInfix "\r" value)
    && !(lib.hasInfix "/../" "${value}/")
    && !(lib.hasInfix "/./" "${value}/")
    && !(lib.hasInfix "//" value)
  );
  runtimePath = types.addCheck absolutePath (
    value: value != "/nix/store" && !(lib.hasPrefix "/nix/store/" value)
  );
  reserved = [
    "STRAVIA_HOST"
    "STRAVIA_PORT"
    "STRAVIA_DATA_DIR"
    "STRAVIA_ADMIN_ORIGINS"
    "STRAVIA_TRUSTED_PROXIES"
    "STRAVIA_LOG_LEVEL"
    "STRAVIA_CONFIG_POLL_INTERVAL"
  ];
  conflicts = builtins.filter (name: builtins.hasAttr name cfg.environment) (
    reserved ++ [ "HOME" ]
  );
  escapeSpecifiers = lib.replaceStrings [ "%" ] [ "%%" ];
  quoteUnitValue = value: escapeSpecifiers (builtins.toJSON value);
  dataDirectoryRule = "d ${quoteUnitValue cfg.dataDir} 0700 ${quoteUnitValue cfg.user} ${quoteUnitValue cfg.group} - -";
  repeated =
    flag: values:
    lib.concatMap (value: [
      flag
      value
    ]) values;
  argv = [
    "${cfg.package}/bin/stravia-server"
    "--host"
    cfg.host
    "--port"
    (toString cfg.port)
    "--data-dir"
    cfg.dataDir
    "--log-level"
    cfg.logLevel
    "--config-poll-interval"
    (toString cfg.configPollInterval)
  ]
  ++ lib.optionals (cfg.configFile != null) [
    "--config"
    cfg.configFile
  ]
  ++ repeated "--admin-origin" cfg.adminOrigins
  ++ repeated "--trusted-proxy" cfg.trustedProxies
  ++ repeated "--proxy-cors-origin" cfg.proxyCorsOrigins;
  checkRuntime = pkgs.writeShellScript "stravia-runtime-boundaries" ''
    set -eu
    # 环境文件晚于 Environment 生效；冲突必须失败，不能悄悄改变具名选项。
    if [ "$HOME" != ${lib.escapeShellArg cfg.dataDir} ]; then
      echo 'stravia: EnvironmentFile must not override HOME' >&2
      exit 1
    fi
    ${lib.optionalString (cfg.configFile != null) ''
      # 初始化会在父目录创建临时文件并原子替换，不只是读取文件。
      test -d ${lib.escapeShellArg (builtins.dirOf cfg.configFile)}
      test -w ${lib.escapeShellArg (builtins.dirOf cfg.configFile)}
      if [ -e ${lib.escapeShellArg cfg.configFile} ]; then
        test -w ${lib.escapeShellArg cfg.configFile}
      fi
    ''}
  '';
in
{
  options.services.stravia = {
    enable = lib.mkEnableOption "Stravia Server";
    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.stravia-server;
      description = "Stravia Server package, including the production WebUI and base Wasm vendor.";
    };
    user = mkOption {
      type = types.nonEmptyStr;
      default = "stravia";
      description = "Non-root service user; custom users must already be declared.";
    };
    group = mkOption {
      type = types.nonEmptyStr;
      default = "stravia";
      description = "Service group; custom groups must already be declared.";
    };
    dataDir = mkOption {
      type = runtimePath;
      default = "/var/lib/stravia";
      description = "Writable persistent data root (0700); changing it does not migrate existing data.";
    };
    host = mkOption {
      type = types.nonEmptyStr;
      default = "127.0.0.1";
      description = "Listening address, independent of firewall configuration.";
    };
    port = mkOption {
      type = types.addCheck types.port (port: port != 0);
      default = 23471;
      description = "Fixed HTTP listening port.";
    };
    openFirewall = mkOption {
      type = types.bool;
      default = false;
      description = "Allow the HTTP TCP port without changing host or authentication.";
    };
    adminOrigins = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Additional management origin restriction; empty means unrestricted origins.";
    };
    trustedProxies = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Trusted immediate proxy addresses or CIDRs; empty trusts none.";
    };
    proxyCorsOrigins = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Proxy CORS origins; empty preserves the server's local-origin defaults.";
    };
    logLevel = mkOption {
      type = types.enum [
        "error"
        "warn"
        "info"
        "debug"
        "trace"
      ];
      default = "info";
      description = "Server logging level.";
    };
    configPollInterval = mkOption {
      type = types.ints.unsigned;
      default = 3;
      description = "Configuration polling interval in seconds; zero disables polling.";
    };
    configFile = mkOption {
      type = types.nullOr runtimePath;
      default = null;
      description = "Application-owned writable runtime TOML path; its parent must support atomic replacement. Null uses dataDir/server.toml.";
    };
    environment = mkOption {
      type = types.attrsOf types.str;
      default = { };
      description = "Non-secret additional environment. Named deployment fields and HOME cannot be overridden. Values are public in the Nix store.";
    };
    environmentFile = mkOption {
      type = types.nullOr runtimePath;
      default = null;
      description = "Required-if-specified runtime systemd environment file, not a shell script or secure credential interface.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion =
          cfg.user != "root"
          && cfg.group != "root"
          && (config.users.users.${cfg.user}.uid or null) != 0
          && (config.users.groups.${cfg.group}.gid or null) != 0;
        message = "services.stravia must use a non-root user and group.";
      }
      {
        assertion = builtins.hasAttr cfg.user config.users.users;
        message = "services.stravia.user must name a declared NixOS user.";
      }
      {
        assertion = builtins.hasAttr cfg.group config.users.groups;
        message = "services.stravia.group must name a declared NixOS group.";
      }
      {
        assertion = conflicts == [ ];
        message = "services.stravia.environment conflicts with owned fields: ${lib.concatStringsSep ", " conflicts}";
      }
    ];
    users.users = lib.mkIf (cfg.user == "stravia") {
      stravia = {
        isSystemUser = true;
        group = cfg.group;
      };
    };
    users.groups = lib.mkIf (cfg.group == "stravia") { stravia = { }; };
    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [ cfg.port ];
    # 目录由系统 tmpfiles 管理，避免特权 ExecStartPre 继承应用环境或跟随不可信链接。
    systemd.tmpfiles.rules = lib.optional (cfg.dataDir != "/var/lib/stravia") dataDirectoryRule;
    systemd.services.stravia = {
      description = "Stravia Server";
      wantedBy = [ "multi-user.target" ];
      after = [
        "network.target"
      ]
      ++ lib.optional (cfg.dataDir != "/var/lib/stravia") "systemd-tmpfiles-setup.service";
      environment = lib.mapAttrs (_: escapeSpecifiers) (
        cfg.environment
        // {
          HOME = cfg.dataDir;
        }
      );
      unitConfig = {
        StartLimitIntervalSec = 60;
        StartLimitBurst = 3;
        RequiresMountsFor = lib.concatMapStringsSep " " quoteUnitValue (
          [ cfg.dataDir ] ++ lib.optional (cfg.configFile != null) (builtins.dirOf cfg.configFile)
        );
      };
      serviceConfig = {
        Type = "exec";
        User = cfg.user;
        Group = cfg.group;
        ExecStart = utils.escapeSystemdExecArgs argv;
        ExecStartPre = [ (utils.escapeSystemdExecArgs [ checkRuntime ]) ];
        # 此指令整行是单一路径，不像 Exec* 那样去除参数引号。
        EnvironmentFile = lib.optional (cfg.environmentFile != null) (escapeSpecifiers cfg.environmentFile);
        UnsetEnvironment = reserved;
        RuntimeDirectory = "stravia";
        RuntimeDirectoryMode = "0700";
        WorkingDirectory = "/run/stravia";
        UMask = "0077";
        Restart = "on-failure";
        RestartSec = 5;
        KillSignal = "SIGTERM";
        TimeoutStopSec = 120;
      }
      // lib.optionalAttrs (cfg.dataDir == "/var/lib/stravia") {
        StateDirectory = "stravia";
        StateDirectoryMode = "0700";
      };
    };
  };
}
