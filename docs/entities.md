# Entities

Everything Deskhand can publish, grouped by **capability**. Each capability is
provided by the first integration that works on this desktop (`deskhand doctor`
shows which one runs), and is left out when none does. Configure capabilities in
[configuration.md](configuration.md).

Home Assistant names entities `<domain>.<device>_<name>`: with the device
"Office PC", `session_state` (named "Session state") becomes
`sensor.office_pc_session_state`. The ids below are Deskhand's own, used in MQTT
topics (`deskhand/<node>/<id>/state`) and in `deskhand print-discovery`.

When the daemon stops, sleeps or is paused from the tray, every entity becomes
unavailable except `session_state` and `deskhand_connected`, which keep showing
`off`/`sleeping` and disconnected.

## Session

| Id | Type | |
|---|---|---|
| `session_state` | sensor | `active`, `idle`, `locked`, `sleeping` or `off`. The one to use in most automations. |
| `user_active` | binary sensor | Input within `idle_threshold` (default 5 min). Playing a video does not count as activity. |
| `idle_seconds` | sensor (diagnostic) | Seconds since the last input. |
| `screen_locked` | binary sensor | |
| `lock_screen` | button | Locks the screen. |
| `screen_lock` | lock | Only with `allow_remote_unlock = true`, instead of the button: locks **and unlocks**. |
| `keep_awake` | switch | Keeps the machine from sleeping and the screen from blanking. |
| `sleep_inhibited` | binary sensor (diagnostic) | Something blocks sleep; attribute `inhibitors` (who and why). |
| `lid_closed`, `docked` | binary sensors | Laptops only. |
| `power_event` | event | `sleep` just before suspending. (`resume` is usually missed while reconnecting; use `session_state`.) |

## Power

| Id | Type | |
|---|---|---|
| `suspend`, `hibernate`, `hybrid_sleep`, `suspend_then_hibernate`, `reboot`, `power_off` | buttons | Only those listed in `power.buttons` (none by default) that the system allows. |
| `logout` | button | KDE, opt-in (`"logout"` in `power.buttons`): ends the session without asking. |
| `on_ac_power` | binary sensor | |
| `battery_level`, `battery_state`, `battery_time_remaining` | sensors | Laptops (UPower). Time in minutes. |
| `power_profile` | select | power-profiles-daemon: `power-saver`, `balanced`, `performance`. |
| `gamemode_active` | binary sensor | Feral GameMode is active. |

## Peripheral batteries

Keyboards, mice, headsets and other battery devices, from UPower (the kernel's
battery reports, Bluetooth) and from Logitech Bolt and Unifying receivers over
HID++ ([setup](configuration.md#logitech-receivers)). **Each peripheral is its
own Home Assistant device**, linked to the computer (or to its receiver), with
its model, model ID, serial number and firmware in the device info. `<key>` is
the device's serial as a slug, so it stays when the name changes: for a
Logitech device the unit id that both the kernel and its receiver report
(`16-fc-6e-8e` → `16_fc_6e_8e`), for a Bluetooth device its address. A device
without a serial is keyed by its model name (`mx_master_3`). Home Assistant's
entity ids still come from the device name (`sensor.mx_master_3_battery`). A
device that two sources see is one device; HID++ is used while it has a link
to the device, UPower otherwise.

| Id | Type | |
|---|---|---|
| `battery_<key>` | sensor (%) | Attributes `kind`, `state`, `source` (and `level`, `estimated`, `slot`, `encrypted` where known). |
| `battery_<key>_state` | sensor | `charging`, `discharging`, `full`, `empty`, `pending_charge`, `pending_discharge`, `error` or `unknown`. |
| `battery_<key>_low` | binary sensor | The device's own warning, else 15 % or less. |
| `battery_<key>_connected` | binary sensor | Off while the peripheral sleeps or is out of range; the battery keeps its last value. |
| `battery_<key>_updated` | timestamp (diagnostic) | When the battery was last read. |
| `<receiver>_paired` | sensor (diagnostic) | On a receiver's device (`bolt_<unique id>`, `unifying_<unique id>`): how many devices are paired; attribute `slots` (slot, name, model ID, online). |

A peripheral's device and entities go away when it is no longer reported (for
example unpaired or unplugged), not while it sleeps. A Logitech keyboard keeps
its last values while asleep (after about 10 minutes untouched) and while its
receiver is unplugged; `updated` shows when it last reported.

### Keyboard backlight

Opt-in with `controls = ["backlight"]` under
[`[integrations.peripheral_battery]`](configuration.md#integrationscapability),
for Logitech keyboards with a backlight (HID++ feature 0x1982, such as the MX
Keys and MX Keys S). On the keyboard's device:

| Id | Type | |
|---|---|---|
| `battery_<key>_backlight` | switch | The backlight as a whole. On can still be dark: level 0, or a bright room in automatic mode. |
| `battery_<key>_backlight_level` | number | `0` up to the keyboard's levels − 1 (`7` on the MX Keys S). Setting a level switches to manual mode, where it holds. |
| `battery_<key>_backlight_mode` | select (config) | `automatic` (follows the light sensor) or `manual`. Only on keyboards with a light sensor. |
| `battery_<key>_backlight_timeout_hands_out` | number, s (config) | How long it stays on after the last key press. |
| `battery_<key>_backlight_timeout_hands_in` | number, s (config) | The same while your hands are still near the keyboard. |
| `battery_<key>_backlight_timeout_powered` | number, s (config) | The same while it charges. |

Timeouts go in 5-second steps, up to 2 hours. Changes made with the
keyboard's own backlight keys show up right away. A sleeping keyboard can't be
reached: changes made while it sleeps are dropped (the entities keep showing
the keyboard's real state), so wake it with a key press first. The keyboard
stores the settings itself, so they survive Deskhand and a restart of the
keyboard.

## Media

| Id | Type | |
|---|---|---|
| `media_state` | sensor | `playing`, `paused`, `stopped` or `idle`. Attributes `player`, `title`, `artist`, `album`, `duration`, `position`, `art_url`. |
| `media_play_pause`, `media_next`, `media_previous`, `media_stop` | buttons | |
| `media_volume` | number (%) | The player's volume. For players that ignore volume over MPRIS (Chromium, Chrome, Brave, Edge, Vivaldi) it is their audio stream's volume. |
| `media_player` | select | `auto` follows the player that started playing most recently; or pick one. |
| `media_art` | image | Cover art (local files and embedded art only). |
| `media` | media player | Only with `compat_mqtt_media_player = true` and the HACS component. |

## Audio and calls

| Id | Type | |
|---|---|---|
| `audio_volume`, `audio_mute` | number (%), switch | Default output. |
| `audio_output`, `audio_input` | selects | Default output and input device. |
| `mic_volume`, `mic_mute` | number (%), switch | Default input. |
| `mic_in_use` | binary sensor | An app is recording; attribute `apps`. Monitors and volume meters don't count. |
| `headphones_connected` | binary sensor | |
| `camera_in_use` | binary sensor | Attributes `apps`, `devices`. |
| `in_call` | binary sensor | Microphone in use for 10 s, or camera in use. Attributes `microphone`, `camera`. |

Audio needs PulseAudio or PipeWire's pulse server.

## Desktop (KDE Plasma)

| Id | Type | |
|---|---|---|
| `active_window` | sensor | App id of the focused window (`org.kde.konsole`, `firefox`). Attributes `title` (unless `publish_title = false`), `app_id`, `resource_class`, `pid`, `output`. |
| `fullscreen` | binary sensor | The focused window is fullscreen. |
| `display_count` | sensor | Connected displays. |
| `virtual_desktop` | select | Current virtual desktop by name; attributes `number`, `count`. |
| `dnd` | switch | Do not disturb. |
| `night_light` | binary sensor | Night light is shifting colours; attributes `enabled`, `mode`, `target_temperature`. |
| `night_light_temperature` | sensor (K) | |
| `night_light_inhibit` | switch | "Suspend night light", like the tray's toggle. |
| `dark_mode` | switch | Switches between the light and dark colour schemes. A read-only binary sensor on other desktops (from the settings portal). |
| `accent_colour` | sensor | `#rrggbb`. |
| `brightness_<display>` | number (%) | One per display: laptop panels and DDC/CI monitors (Plasma 6.2+). Named after the display, e.g. `brightness_dell_u2723qe`. |
| `screens_on` | switch | Displays on or off (Wayland). Any input turns them back on. On X11: a `screens_off` button instead. |
| `output_<name>` | switch | Opt-in: enable or disable an output (`output_hdmi_a_1`). The last enabled one is never disabled. |
| `shortcut_<id>` | event | `pressed`, for each `[[shortcut]]`. |
| `kde_shortcut_<id>` | button | Each shortcut in `shortcuts.invoke`. |

## Notifications

| Id | Type | |
|---|---|---|
| `notify` | notify | Each message becomes a desktop notification. |
| `notification` | (command topic) | Rich notifications as JSON on `deskhand/<node>/notification/set`: title, icon, urgency, timeout, actions, inline reply, replace and dismiss by `tag`. See [home-assistant.md](home-assistant.md#actionable-notifications). |
| `notification_event` | event | `action` (attribute `action`), `reply` (attribute `reply`), `closed` (attribute `reason`), each with the notification's `tag` and `title`. |

## Connectivity

| Id | Type | |
|---|---|---|
| `bluetooth_power` | switch | |
| `bt_<address>` | switch | Connects a paired device; attributes `address`, `paired`, `battery`. |
| `wifi_ssid` | sensor | Attribute `signal`. |
| `network_connectivity` | sensor | NetworkManager's `full`, `limited`, `portal`, `none`. |

## System

| Id | Type | |
|---|---|---|
| `cpu_percent`, `load_1m`, `load_5m`, `load_15m` | sensors | |
| `memory_percent`, `memory_used`, `swap_percent` | sensors | |
| `disk_<mount>` | sensor (%) | `disk_root` for `/`, one per `metrics.disks` entry. |
| `network_rx`, `network_tx` | sensors (kB/s) | |
| `cpu_temperature` | sensor (°C) | |
| `gpu_percent`, `gpu_memory_percent`, `gpu_temperature` | sensors | Where the GPU driver reports them. |
| `boot_time` | sensor (diagnostic) | |
| `systemd_<unit>` | switch | Each unit in `systemd.units`; starts and stops it. |
| `script_<id>`, `script_<id>_result` | button, sensor | Each `[[script]]`. |
| `<id>` | sensor or binary sensor | Each `[[sensor]]`. |
| `deskhand_version`, `deskhand_started`, `deskhand_connected` | diagnostic | |
