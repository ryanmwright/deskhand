# Changelog

All notable changes to Deskhand. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). The release workflow uses the
section of the tagged version as the release notes.

## [Unreleased]

### Changed

- **Peripheral batteries are devices of their own.** A keyboard, mouse or
  headset battery now shows up in Home Assistant as its own device (with its
  model, serial number and firmware), linked to the computer, instead of a
  sensor on the computer's device. The entity ids (`battery_<model>`) stay the
  same, and Home Assistant keeps the entity id and history when the entity
  moves to its new device (checked against Home Assistant 2026.10). The
  entities now belong to the new `peripheral_battery` capability (availability
  topic `status/peripheral_battery`); `battery` keeps the laptop battery and AC
  power. `enabled = false` under `[integrations.battery]` no longer hides
  peripheral batteries; use `[integrations.peripheral_battery]`.

### Added

- **Logitech receivers (HID++).** Keyboards and mice on a Logi Bolt or
  Unifying receiver report battery, charging state, low battery and
  connection, even where the kernel and UPower don't see them (Bolt). Each is
  its own device, linked to its receiver's device (unique ID, firmware,
  `paired` sensor). Readings come from the devices' own events and each wake
  from sleep; sleeping devices are never contacted, and nothing is polled
  unless `battery_refresh` is set. Needs a one-time udev rule (or Solaar's):
  `deskhand udev-rule`, offered by `deskhand setup`, shipped in the tarball
  and the Nix package (`lib/udev/rules.d`). `deskhand doctor` lists receivers,
  paired devices and batteries. Cargo feature `hidpp` (default).
- **Keyboard backlight** (Logitech, opt-in with `controls = ["backlight"]`
  under `[integrations.peripheral_battery]`): `battery_<model>_backlight`
  switch, `_backlight_level`, `_backlight_mode` (automatic or manual) and the
  three fade-out timeouts. Changes made with the keyboard's keys show up right
  away; nothing is sent to a sleeping keyboard.
- A mouse or keyboard on a receiver the kernel drives (Unifying, Lightspeed)
  is one device, even though UPower and HID++ name it differently: the
  receiver's unit ID (the kernel's serial) joins them, and the UPower-era
  entity id stays.
- Per peripheral: `battery_<model>_state`, `_low`, `_connected` and
  `_updated` (time of the last reading). A sleeping peripheral keeps its last
  battery value; only `connected` turns off.
- Sub-devices in MQTT discovery: one retained document per device at
  `homeassistant/device/deskhand_<node>/<key>/config`, removed again when the
  device goes. `deskhand uninstall` clears them too.

## [0.1.1] - 2026-11-01

The first release: KDE Plasma 6 on Linux, with the generic Linux integrations
working on other desktops too.

### Changed (from the unpublished 0.1.0)

- **No power buttons by default.** Home Assistant can only suspend, hibernate,
  restart, shut down or log out the computer when you list those buttons in
  `[integrations.power] buttons`. `deskhand setup` asks which ones
  (`--power-buttons` without prompts). Locking the screen stays available.

### Added

- MQTT transport with Home Assistant device discovery: one device per computer
  and user, availability (including while asleep), stale entity cleanup,
  reconnects, TLS and password files.
- Session: `session_state` (active, idle, locked, sleeping, off), idle time
  (Wayland, ScreenSaver or logind), lock state and Lock screen button (remote
  unlock only when enabled), keep awake, sleep inhibitors, lid and dock, sleep
  and resume events.
- Power buttons (suspend, hibernate, reboot, shut down, logout on KDE), AC and
  battery, peripheral batteries, power profiles, GameMode.
- Media players over MPRIS: state, controls, volume (also for Chromium-based
  browsers, through their audio stream), player choice and cover art; optional
  entity for the HACS `mqtt_media_player`.
- Audio: volume, mute, output and input selection, microphone in use (with
  apps), headphones; camera in use; derived `in_call`.
- Desktop notifications, including actions, inline replies, replacing and
  dismissing by tag, with click events back to Home Assistant.
- Connectivity: Bluetooth power and device switches, Wi-Fi network,
  connectivity.
- System metrics (CPU, memory, swap, disks, network, temperatures, GPU),
  systemd user unit switches, `[[script]]` buttons and `[[sensor]]` command
  sensors.
- KDE Plasma: active window and fullscreen, virtual desktops, Do not disturb,
  night light, dark mode and accent colour, per-display brightness (including
  DDC/CI monitors), screens on/off, custom global shortcuts as events, KDE
  shortcuts as buttons, opt-in output switches.
- `deskhand setup` (broker discovery, connection test, systemd user service),
  `deskhand doctor`, `print-discovery`, `uninstall`, and a tray icon with
  Pause.
- Nix flake: package, Home Manager module, NixOS VM end-to-end test.

## [0.1.0] - 2026-11-01

Tagged but not published; everything in it is in 0.1.1.

[0.1.1]: https://github.com/ryanmwright/deskhand/releases/tag/v0.1.1
