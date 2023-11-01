self: {
  pkgs,
  config,
  lib,
  ...
}:
with lib; {
  options = {
    services.fioul = {
      enable = lib.mkEnableOption "fioul-server, a tool to cache french gas station informations";

      package = mkOption {
        type = types.package;
        default = self.packages."${pkgs.system}".fioul-server;
      };

      port = mkOption {
        type = types.port;
        default = 3000;
        description = "Port on which fioul will listen";
      };
    };
  };

  config = let
    cfg = config.services.fioul;
  in
    mkIf cfg.enable {
      systemd.services.fioul-server = {
        description = "fioul-server";
        after = ["network.target"];
        wantedBy = ["multi-user.target"];

        serviceConfig = {
          Type = "simple";
          DynamicUser = true;
          ExecStart = "${cfg.package}/bin/fioul-server";
          # Security
          NoNewPrivileges = true;
          # Sandboxing
          ProtectSystem = "strict";
          ProtectHome = true;
          PrivateTmp = true;
          PrivateDevices = true;
          PrivateUsers = true;
          ProtectHostname = true;
          ProtectClock = true;
          ProtectKernelTunables = true;
          ProtectKernelModules = true;
          ProtectKernelLogs = true;
          ProtectControlGroups = true;
          RestrictAddressFamilies = ["AF_UNIX AF_INET AF_INET6"];
          LockPersonality = true;
          MemoryDenyWriteExecute = true;
          RestrictRealtime = true;
          RestrictSUIDSGID = true;
          PrivateMounts = true;
        };

        environment = {
          FIOUL_PORT = toString cfg.port;
        };
      };
    };
}
