# Home Manager module: runs Deskhand as a systemd user service in the
# graphical session, or with `headless = true` at boot without a login
# (needs NixOS `users.users.<name>.linger = true`).
#
#   imports = [ inputs.deskhand.homeManagerModules.default ];
#   services.deskhand = {
#     enable = true;
#     passwordFile = "/run/agenix/deskhand-mqtt";
#     settings = {
#       device.name = "My Desktop";
#       mqtt = { host = "homeassistant.local"; username = "deskhand"; };
#     };
#   };
#
# `settings` is the config file (docs/v1-plan.md §1 "Config") as Nix; any key
# Deskhand understands works. The file lands in the world-readable Nix store,
# so the password must come from `passwordFile` (or
# `settings.mqtt.password_command`), never `settings.mqtt.password`.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.deskhand;
  toml = pkgs.formats.toml { };
  packages = self.packages.${pkgs.stdenv.hostPlatform.system};
  # `settings.general.*`, if set, wins over `tray` and `headless`.
  general = {
    inherit (cfg) tray;
  }
  // lib.optionalAttrs cfg.headless { headless = true; };
  settings = lib.recursiveUpdate (lib.recursiveUpdate { inherit general; } cfg.settings) (
    lib.optionalAttrs (cfg.passwordFile != null) {
      mqtt.password_file = cfg.passwordFile;
    }
  );
  configFile = toml.generate "deskhand.toml" settings;
in
{
  options.services.deskhand = {
    enable = lib.mkEnableOption "Deskhand, which exposes this desktop to Home Assistant over MQTT";

    package = lib.mkOption {
      type = lib.types.package;
      default = if cfg.headless then packages.deskhand-headless else packages.default;
      defaultText = lib.literalExpression ''
        if headless then inputs.deskhand.packages.''${system}.deskhand-headless
        else inputs.deskhand.packages.''${system}.default
      '';
      description = "The Deskhand package.";
    };

    headless = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        A machine without a desktop session (a server or VM). The service is
        wanted by `default.target` instead of the graphical session, session
        and desktop integrations default to off (`general.headless`), there
        is no tray, and the default package is the slim `deskhand-headless`
        build. To start at boot without a login, the user must linger: set
        `users.users.<name>.linger = true` in the NixOS configuration.
      '';
    };

    settings = lib.mkOption {
      inherit (toml) type;
      default = { };
      example = lib.literalExpression ''
        {
          device.name = "My Desktop";
          mqtt = {
            host = "homeassistant.local";
            username = "deskhand";
          };
          integrations.systemd.units = [ "syncthing.service" ];
        }
      '';
      description = ''
        Deskhand's `config.toml` as Nix. It is written to the Nix store and
        linked to `~/.config/deskhand/config.toml`, so `deskhand doctor` uses
        it too. Do not put the MQTT password here; use `passwordFile`.
      '';
    };

    tray = lib.mkOption {
      type = lib.types.bool;
      default = !cfg.headless;
      defaultText = lib.literalExpression "!headless";
      description = ''
        Show a tray icon (status, Pause, Quit) when the desktop has a
        StatusNotifierItem tray. Sets `general.tray`.
      '';
    };

    passwordFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/run/agenix/deskhand-mqtt";
      description = ''
        File whose first line is the MQTT password (`~/` is expanded). Sets
        `mqtt.password_file`. Keep it outside the Nix store.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = !(settings ? mqtt && settings.mqtt ? password);
        message = "services.deskhand.settings.mqtt.password would put the MQTT password in the Nix store; use services.deskhand.passwordFile instead.";
      }
      {
        assertion = settings ? mqtt && settings.mqtt ? host;
        message = "services.deskhand.settings.mqtt.host must be set to your MQTT broker.";
      }
    ];

    home.packages = [ cfg.package ];

    xdg.configFile."deskhand/config.toml".source = configFile;

    systemd.user.services.deskhand = {
      Unit = {
        Description = "Deskhand: this desktop in Home Assistant";
        Documentation = [ "https://github.com/ryanmwright/deskhand" ];
      }
      # Idle, lock, KDE and notification integrations need the session's
      # Wayland and D-Bus environment, which the desktop imports into the
      # user manager when the graphical session starts.
      // lib.optionalAttrs (!cfg.headless) {
        PartOf = [ "graphical-session.target" ];
        After = [ "graphical-session.target" ];
      };
      Service = {
        Type = "notify";
        ExecStart = "${lib.getExe cfg.package} --config ${configFile} run";
        Restart = "on-failure";
        RestartSec = 5;
      };
      Install.WantedBy = [ (if cfg.headless then "default.target" else "graphical-session.target") ];
    };
  };
}
