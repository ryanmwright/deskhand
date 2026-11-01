# `nix flake check` for nix/hm-module.nix without a home-manager input: the
# options the module sets are stubbed, and the rendered config and unit are
# checked. The real module is exercised in the Plasma VM (docs/v1-plan.md).
{ pkgs, module }:
let
  inherit (pkgs) lib;
  stub = {
    options = {
      assertions = lib.mkOption {
        type = lib.types.listOf lib.types.attrs;
        default = [ ];
      };
      home.packages = lib.mkOption {
        type = lib.types.listOf lib.types.package;
        default = [ ];
      };
      xdg.configFile = lib.mkOption {
        type = lib.types.attrsOf lib.types.attrs;
        default = { };
      };
      systemd.user.services = lib.mkOption {
        type = lib.types.attrsOf lib.types.attrs;
        default = { };
      };
    };
  };
  eval =
    config:
    (lib.evalModules {
      modules = [
        module
        stub
        { _module.args.pkgs = pkgs; }
        config
      ];
    }).config;
  good = eval {
    services.deskhand = {
      enable = true;
      passwordFile = "/run/secrets/mqtt";
      settings = {
        device.name = "Test Desk";
        mqtt.host = "broker.lan";
        integrations.media.compat_mqtt_media_player = true;
      };
    };
  };
  failed = c: map (a: a.message) (lib.filter (a: !a.assertion) c.assertions);
  withPassword = eval {
    services.deskhand = {
      enable = true;
      settings.mqtt = {
        host = "broker.lan";
        password = "secret";
      };
    };
  };
  disabled = eval { services.deskhand.settings.mqtt.host = "x"; };
  service = good.systemd.user.services.deskhand;
in
assert failed good == [ ];
assert lib.length (failed withPassword) == 1;
assert disabled.systemd.user.services == { };
assert service.Service.Type == "notify";
assert lib.hasInfix " run" service.Service.ExecStart;
assert service.Install.WantedBy == [ "graphical-session.target" ];
pkgs.runCommand "deskhand-hm-module-check" { nativeBuildInputs = [ pkgs.gnugrep ]; } ''
  config=${good.xdg.configFile."deskhand/config.toml".source}
  grep -qx 'password_file = "/run/secrets/mqtt"' "$config"
  grep -qx 'host = "broker.lan"' "$config"
  grep -qx 'compat_mqtt_media_player = true' "$config"
  grep -q -- "--config $config run" <<< "${service.Service.ExecStart}"
  cp "$config" $out
''
