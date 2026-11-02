# Changelog

All notable changes to Deskhand. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). The release workflow uses the
section of the tagged version as the release notes.

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
