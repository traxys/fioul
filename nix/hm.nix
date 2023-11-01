self: {
  pkgs,
  config,
  lib,
  ...
}:
with lib; let
  cfg = config.programs.fioul;

  tomlFormat = pkgs.formats.toml {};

  configDir = config.xdg.configHome;
in {
  options.programs.fioul = {
    enable = mkEnableOption "fioul, a CLI to query french gas station prices";

    settings = mkOption {
      inherit (tomlFormat) type;
      default = {};
      defaultText = literalExpression "{ }";
      example = literalExpression ''
        {
          default = {
            server = "http://localhost:3000";
          };

          display = {
            dates = false;
          };
        }
      '';
      description = ''
        Configuration written to
        {file}`$XDG_CONFIG_HOME/fioul/config.toml` on Linux
        See <https://github.com/traxys/fioul/blob/master/config.toml>
        for more informatio.
      '';
    };
  };

  config = mkIf cfg.enable {
    home.packages = [self.packages."${pkgs.system}".fioul];

    home.file."${configDir}/fioul/config.toml" = mkIf (cfg.settings != {}) {
      source = tomlFormat.generate "fioul-config" cfg.settings;
    };
  };
}
