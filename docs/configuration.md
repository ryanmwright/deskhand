# Configuration

Deskhand reads one TOML file, by default `~/.config/deskhand/config.toml`
(`--config PATH` or `DESKHAND_CONFIG` to use another). `deskhand setup` writes a
working one; everything below is optional except `[mqtt] host`.

- Unknown keys are reported as warnings (in the log and by `deskhand doctor`),
  not errors, so a typo never stops the daemon.
- Durations are written like `"30s"`, `"5m"`, `"1h"`.
- Keep the file private (`chmod 600`); `deskhand setup` does this.

After editing, restart the service (`systemctl --user restart deskhand`) and run
`deskhand doctor` to see what each integration does with the new settings.

## A complete example

```toml
[device]
name = "Office PC"

[mqtt]
host = "homeassistant.local"
username = "deskhand"
password_file = "~/.config/deskhand/password"

[general]
idle_threshold = "5m"

[integrations.power]
buttons = ["suspend", "reboot", "poweroff", "logout"]

[integrations.lock]
allow_remote_unlock = false

[integrations.systemd]
units = ["syncthing.service"]

[integrations.media]
ignore_players = ["kdeconnect"]

[integrations.metrics]
enabled = false

[[script]]
id = "backup"
name = "Run backup"
command = ["/home/me/bin/backup.sh"]
timeout = "30m"

[[sensor]]
id = "updates_pending"
name = "Pending updates"
command = "checkupdates | wc -l"
shell = true
interval = "1h"

[[shortcut]]
id = "lights"
name = "Toggle desk lights"
default_keys = "Meta+Shift+L"
```

## `[device]`

| Key | Default | |
|---|---|---|
| `name` | pretty host name, else host name | Device name in Home Assistant. Entity ids are derived from it (`sensor.office_pc_session_state`). |
| `node_id` | derived | The id in MQTT topics and Home Assistant's device identifier. Derived from the machine id and the user, so it survives host name changes and differs per user. Only set it to keep the id after reinstalling. |
| `publish_mac` | `true` | Publish the main network interface's MAC address, so Home Assistant's Wake-on-LAN can wake the machine. |

## `[mqtt]`

| Key | Default | |
|---|---|---|
| `host` | `"homeassistant.local"` | Broker host name or address. |
| `port` | `1883`, `8883` with TLS | |
| `username` | none | With Home Assistant's Mosquitto add-on, a Home Assistant user. |
| `password_file` | none | File whose first line is the password (`~/` works). Recommended. |
| `password_command` | none | Command whose output is the password, as an argv list: `["pass", "show", "mqtt"]`. |
| `password` | none | The password itself. Prefer one of the two above. |
| `tls` | `false` | Connect with TLS (rustls, system certificate store). |
| `ca_file` | system roots | PEM CA bundle for a private CA. |
| `client_cert`, `client_key` | none | PEM client certificate and key for mutual TLS. |
| `discovery_prefix` | `"homeassistant"` | Must match Home Assistant's MQTT discovery prefix. |
| `topic_prefix` | `"deskhand"` | Topics are `<topic_prefix>/<node_id>/...`. |
| `keepalive` | `"30s"` | |
| `client_id` | `deskhand-<node_id>` | |

Use only one of `password`, `password_file` and `password_command`.

## `[general]`

| Key | Default | |
|---|---|---|
| `tray` | `true` | Show the tray icon (status, Pause, open config, Quit) when the desktop has a StatusNotifierItem tray. |
| `idle_threshold` | `"5m"` | How long without input before `user_active` turns off. |
| `log` | `"info"` | Log filter, e.g. `"info,deskhand_mqtt=debug"`. `RUST_LOG` overrides it. Logs go to the journal: `journalctl --user -u deskhand -f`. |

## `[integrations.<capability>]`

Every capability (see [entities.md](entities.md)) can have a table. All of
them accept `enabled`:

| `enabled` | |
|---|---|
| `"auto"` (default) | Start it if this desktop supports it. |
| `true` | Start it, and report an error (`deskhand doctor`) if this desktop does not support it. |
| `false` | Never start it. Its entities are removed from Home Assistant. |

```toml
[integrations.active_window]
enabled = false      # don't publish the focused window at all
```

Power buttons and two capabilities are off unless you opt in: the power
buttons (list them in `power.buttons`), `logout` (add `"logout"` there too)
and `outputs` (`enabled = true`).

The other options, by capability:

| Capability | Option | Default | |
|---|---|---|---|
| `power` | `buttons` | `[]` (none) | Power buttons Home Assistant may press, if the system allows them: `suspend`, `hibernate`, `hybrid_sleep`, `suspend_then_hibernate`, `reboot`, `poweroff`, and `logout` (KDE, ends the session without asking). `deskhand setup` asks which. |
| `lock` | `allow_remote_unlock` | `false` | `false`: a **Lock screen** button only. `true`: a lock entity that Home Assistant can also unlock with. See [SECURITY.md](../SECURITY.md). |
| `active_window` | `publish_title` | `true` | `false` keeps window titles (which can contain document names, chat partners, URLs) out of Home Assistant. |
| `media` | `ignore_players` | `[]` | Media players to leave out, by name or app id (`"firefox"`, `"kdeconnect"`), ignoring case. |
| `media` | `compat_mqtt_media_player` | `false` | Also publish a media player entity for the HACS [mqtt_media_player](https://github.com/bkbilly/mqtt_media_player) integration ([home-assistant.md](home-assistant.md#a-media-player-card)). |
| `audio` | `mic_ignore_apps` | `[]` | More apps whose recordings don't count as "microphone in use" (volume meters like pavucontrol are already ignored). |
| `camera` | `ignore_processes` | `[]` | Processes whose camera use doesn't count. |
| `bluetooth` | `devices` | `"paired"` | `"paired"`, or a list of addresses (`["AA:BB:CC:DD:EE:FF"]`), for the per-device connect switches. |
| `systemd` | `units` | `[]` | systemd **user** units Home Assistant may start and stop, by full name. Only these. |
| `metrics` | `interval` | `"30s"` | |
| `metrics` | `disks` | `["/"]` | Mount points to report usage for. |
| `metrics` | `network` | `true` | Download and upload rates. |
| `metrics` | `network_interfaces` | all physical | Interfaces to count. By default loopback, containers, bridges and VPN tunnels are skipped. |
| `metrics` | `temperature` | `true` | CPU temperature. |
| `metrics` | `gpu` | `true` | GPU load, memory and temperature, where available. |
| `appearance` | `light_scheme` | `"BreezeLight"` | KDE colour scheme for the dark mode switch's off position. |
| `appearance` | `dark_scheme` | `"BreezeDark"` | ... and its on position. |
| `shortcuts` | `invoke` | `[]` | KDE shortcuts Home Assistant may trigger, one button each. See [below](#kde-shortcuts-as-buttons). |

## `[[script]]`: buttons that run commands

The only way Home Assistant can make Deskhand run a program. Each entry becomes
a `script_<id>` button and a `script_<id>_result` sensor (`running`, `success`,
`failed`, `timeout`; attributes `exit_code`, `last_started`, `duration_s`).
Pressing the button carries no arguments.

| Key | Default | |
|---|---|---|
| `id` | required | Lowercase letters, digits and `_`. |
| `name` | the id | Button name. |
| `command` | required | Argv list: `["systemctl", "--user", "restart", "foo"]`. |
| `shell` | `false` | With `true`, `command` is one string run by `sh -c`. |
| `timeout` | `"60s"` | The process group is killed after this. |
| `icon` | none | `"mdi:backup-restore"` |

A press while the script runs is ignored.

## `[[sensor]]`: values from commands

| Key | Default | |
|---|---|---|
| `id` | required | Entity id, used as is. |
| `name` | the id | |
| `command`, `shell` | | As for scripts. |
| `interval` | `"60s"` | |
| `timeout` | `"10s"` | |
| `kind` | `"sensor"` | Or `"binary_sensor"` (output `on`/`off`, `true`/`false`, `1`/`0`, `yes`/`no`). |
| `unit` | none | `"°C"`, `"%"`, ... |
| `device_class` | none | Any Home Assistant device class for the kind (`temperature`, `connectivity`, ...). |
| `state_class` | none | `measurement`, `total` or `total_increasing` (for statistics). |
| `icon` | none | |
| `json_attributes` | `false` | The output is a JSON object: its `state` key is the value and the other keys become attributes. |

A failing or timed-out command makes the value `unknown`.

## `[[shortcut]]`: global shortcuts that trigger Home Assistant (KDE)

Each entry registers a global shortcut and a `shortcut_<id>` event entity that
fires `pressed`. It appears in System Settings → Keyboard → Shortcuts under
"Deskhand", where you can change the keys.

| Key | Default | |
|---|---|---|
| `id` | required | Lowercase letters, digits and `_`. |
| `name` | the id | Shown in System Settings and Home Assistant. |
| `default_keys` | unbound | `"Meta+Shift+L"`, a list of alternatives, or a chord sequence `"Ctrl+K, Ctrl+L"`. Modifiers: `Meta` (`Super`), `Ctrl`, `Alt`, `Shift`. Keys: letters, digits, `F1`–`F35`, `Space`, `Esc`, `Tab`, `Return`, `Del`, `Home`, `PgUp`, arrows, `Media Play`, `Volume Up`, punctuation. |

If the keys are already taken by another shortcut, the shortcut starts unbound
(the log says so); bind it in System Settings. A binding you change there is
kept.

## KDE shortcuts as buttons

`[integrations.shortcuts] invoke` lists existing KDE shortcuts that Home
Assistant may trigger, as if their keys were pressed:

```toml
[integrations.shortcuts]
invoke = [
  { component = "kwin", action = "Overview", name = "Overview" },
  { component = "org_kde_powerdevil", action = "Turn Off Screen", id = "screen_off" },
]
```

Each becomes a `kde_shortcut_<id>` button (`id` defaults to the action, made
lowercase). Find component and action names in `~/.config/kglobalshortcutsrc`:
the section is the component, the key before `=` is the action.

## Home Manager

With the flake's Home Manager module, the config is written from Nix:

```nix
services.deskhand = {
  enable = true;
  passwordFile = "/run/agenix/deskhand-mqtt";   # sets mqtt.password_file
  tray = true;                                   # sets general.tray
  settings = {
    device.name = "Office PC";
    mqtt = { host = "homeassistant.local"; username = "deskhand"; };
    integrations.systemd.units = [ "syncthing.service" ];
    shortcut = [ { id = "lights"; name = "Toggle desk lights"; default_keys = "Meta+Shift+L"; } ];
  };
};
```

`settings` takes any key on this page. Never put the password in `settings`: it
would end up in the world-readable Nix store (the module refuses it).
