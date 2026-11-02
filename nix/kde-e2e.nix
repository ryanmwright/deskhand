# End-to-end test in a Plasma 6 Wayland VM (`checks.kde-e2e`, run nightly in
# CI): the packaged Deskhand runs as a user service in the autologin session
# against a local Mosquitto, and the test checks discovery, lock state and a
# notification sent over MQTT. Needs KVM; build with
#   nix build .#checks.x86_64-linux.kde-e2e -L
{ pkgs, deskhand }:
let
  node = "e2e";
  config = (pkgs.formats.toml { }).generate "deskhand.toml" {
    device = {
      name = "E2E Desk";
      node_id = node;
    };
    mqtt.host = "127.0.0.1";
    general.log = "info,deskhand_mqtt=debug";
  };
in
pkgs.testers.runNixOSTest {
  name = "deskhand-kde-e2e";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation = {
        memorySize = 4096;
        cores = 4;
        qemu.options = [
          "-vga none"
          "-device virtio-vga"
        ];
      };

      services.desktopManager.plasma6.enable = true;
      services.displayManager = {
        sddm = {
          enable = true;
          wayland.enable = true;
        };
        defaultSession = "plasma";
        autoLogin = {
          enable = true;
          user = "alice";
        };
      };
      users.users.alice = {
        isNormalUser = true;
        uid = 1000;
        password = "alice";
      };

      services.mosquitto = {
        enable = true;
        listeners = [
          {
            address = "127.0.0.1";
            port = 1883;
            omitPasswordAuth = true;
            settings.allow_anonymous = true;
            acl = [ "topic readwrite #" ];
          }
        ];
      };

      # What the Home Manager module and `deskhand setup` install.
      systemd.user.services.deskhand = {
        description = "Deskhand";
        partOf = [ "graphical-session.target" ];
        after = [ "graphical-session.target" ];
        wantedBy = [ "graphical-session.target" ];
        serviceConfig = {
          Type = "notify";
          ExecStart = "${deskhand}/bin/deskhand --config ${config} run";
          Restart = "on-failure";
          RestartSec = 5;
        };
      };

      environment.systemPackages = [
        deskhand
        pkgs.mosquitto
        pkgs.jq
        pkgs.dbus
      ];
    };

  testScript = ''
    def sub(topic, count=1):
        return f"mosquitto_sub -h 127.0.0.1 -t '{topic}' -C {count} -W 5"

    def state(entity):
        return machine.succeed(sub("deskhand/${node}/" + entity + "/state")).strip()

    as_alice = "su - alice -c 'DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus {}'"

    start_all()
    machine.wait_for_unit("mosquitto.service")
    machine.wait_until_succeeds("pgrep -u alice plasmashell", timeout=300)
    machine.wait_for_unit("deskhand.service", "alice", timeout=120)

    with subtest("device online with discovery"):
        machine.wait_until_succeeds(
            sub("deskhand/${node}/status") + " | grep -qx online", timeout=60
        )
        machine.wait_until_succeeds(
            sub("homeassistant/device/deskhand_${node}/config")
            + " | jq -e '(.cmps // .components) | has(\"session_state\") and has(\"screen_locked\") and has(\"active_window\")'",
            timeout=60,
        )
        machine.succeed(as_alice.format("deskhand --config ${config} doctor") + " | tee /dev/stderr | grep -q 'online'")

    with subtest("lock state"):
        machine.wait_until_succeeds("test \"$(" + sub("deskhand/${node}/screen_locked/state") + ")\" = OFF", timeout=30)
        machine.succeed("loginctl lock-sessions")
        machine.wait_until_succeeds("test \"$(" + sub("deskhand/${node}/screen_locked/state") + ")\" = ON", timeout=30)
        machine.wait_until_succeeds("test \"$(" + sub("deskhand/${node}/session_state/state") + ")\" = locked", timeout=30)
        # The locker may still be starting and miss one Unlock: repeat it.
        machine.wait_until_succeeds(
            "loginctl unlock-sessions; test \"$(" + sub("deskhand/${node}/screen_locked/state") + ")\" = OFF",
            timeout=60,
        )

    with subtest("notification from MQTT"):
        machine.succeed(
            as_alice.format(
                "dbus-monitor --session \"interface=org.freedesktop.Notifications,member=Notify\""
            )
            + " > /tmp/notify.log 2>&1 &"
        )
        machine.sleep(2)
        machine.succeed("mosquitto_pub -h 127.0.0.1 -t deskhand/${node}/notify/set -m 'Hello from the e2e test'")
        machine.wait_until_succeeds("grep -q 'Hello from the e2e test' /tmp/notify.log", timeout=30)

    machine.screenshot("plasma")
  '';
}
