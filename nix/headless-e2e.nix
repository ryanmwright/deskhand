# End-to-end test of headless mode (`checks.headless-e2e`): a VM with no
# desktop and a lingering user. The slim package runs as a user service
# wanted by default.target, so it must come up at boot without anyone
# logging in, and publish metrics but no session or desktop entities.
# Needs KVM; build with
#   nix build .#checks.x86_64-linux.headless-e2e -L
{ pkgs, deskhand }:
let
  node = "headless";
  config = (pkgs.formats.toml { }).generate "deskhand.toml" {
    device = {
      name = "Headless VM";
      node_id = node;
    };
    general.headless = true;
    mqtt.host = "127.0.0.1";
  };
in
pkgs.testers.runNixOSTest {
  name = "deskhand-headless-e2e";

  nodes.machine =
    { pkgs, ... }:
    {
      users.users.alice = {
        isNormalUser = true;
        uid = 1000;
        linger = true;
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

      # What `services.deskhand.headless` and `deskhand setup --headless`
      # install.
      systemd.user.services.deskhand = {
        description = "Deskhand";
        wantedBy = [ "default.target" ];
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
      ];
    };

  testScript = ''
    def sub(topic):
        return f"mosquitto_sub -h 127.0.0.1 -t '{topic}' -C 1 -W 5"

    start_all()
    machine.wait_for_unit("mosquitto.service")
    machine.wait_for_unit("user@1000.service")

    with subtest("online at boot without a login"):
        machine.wait_until_succeeds(
            sub("deskhand/${node}/status") + " | grep -qx online", timeout=120
        )
        # Nobody logged in: alice's manager runs only because she lingers
        # (its own session has class "manager").
        machine.succeed(
            "loginctl list-sessions --json=short"
            + " | jq -e '[.[] | select(.user == \"alice\" and .class != \"manager\")] == []'"
        )

    with subtest("info and metrics, no session or desktop entities"):
        machine.wait_until_succeeds(
            sub("homeassistant/device/deskhand_${node}/config")
            + " | jq -e '(.cmps // .components) | has(\"deskhand_version\") and has(\"memory_used\")'",
            timeout=60,
        )
        for absent in ["session_state", "screen_locked", "user_active", "media_player"]:
            machine.succeed(
                sub("homeassistant/device/deskhand_${node}/config")
                + f" | jq -e '(.cmps // .components) | has(\"{absent}\") | not'"
            )

    with subtest("doctor reports headless"):
        # Exit status aside (it fails on any unavailable `enabled = true`),
        # the text must show the headless state.
        _, out = machine.execute("su - alice -c 'deskhand --config ${config} doctor'")
        print(out)
        assert "Headless (lingering on" in out, out
        assert "disabled (headless)" in out, out
  '';
}
