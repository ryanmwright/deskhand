<p align="center">
  <img src="data/io.github.ryanmwright.Deskhand.svg" width="96" alt="">
</p>

# Deskhand

Deskhand puts your desktop computer into [Home Assistant](https://www.home-assistant.io/):
whether you are at it, locked, asleep or in a call, what is playing, and buttons
and switches to control it. It runs as a small background service on the
computer and talks to Home Assistant over MQTT, so nothing needs to be
installed in Home Assistant.

It starts with **KDE Plasma 6 on Linux**. The generic Linux parts (power, media,
audio, notifications, Bluetooth, metrics, ...) also work on other desktops, and
the core is built so GNOME, macOS and Windows backends can be added.

## What you get

A device in Home Assistant with, depending on your desktop and hardware:

- **Presence:** `session_state` (active, idle, locked, sleeping, off), idle
  time, screen locked, lock button, keep awake.
- **Calls:** microphone in use, camera in use, and `in_call` for a meeting
  light.
- **Media:** what is playing, play/pause/next, volume, cover art.
- **Audio:** volume, mute, output and input device.
- **Notifications** from Home Assistant, with buttons and replies that come back
  as events ("Doorbell — [Unlock] [Ignore]").
- **Power:** battery, power profile, and, if you allow them, suspend, restart
  and shut down buttons.
- **Peripherals:** keyboard, mouse and headset batteries, each its own device,
  including Logitech keyboards and mice on a Bolt or Unifying receiver; and,
  if you allow it, the keyboard backlight.
- **KDE Plasma:** active window, virtual desktop, Do not disturb, night light,
  dark mode, per-monitor brightness, screens on/off, and global keyboard
  shortcuts that trigger Home Assistant automations.
- **System:** CPU, memory, disks, network, temperatures; switches for chosen
  systemd user services; buttons for your own scripts; sensors from commands.

The full list is in [docs/entities.md](docs/entities.md).

Deskhand never runs commands it receives over MQTT: Home Assistant can only
trigger the scripts, services and shortcuts you list in the config. See
[SECURITY.md](SECURITY.md).

## Quick start

You need Home Assistant with an MQTT broker, and a Linux desktop with a systemd
user session.

### 1. Set up MQTT in Home Assistant

1. **Settings → Add-ons → Add-on store → Mosquitto broker**: install and start.
2. **Settings → Devices & services**: add the **MQTT** integration it offers.
3. **Settings → People → Users → Add user**: create a user for Deskhand, e.g.
   `deskhand`, with a password.

(Details and other brokers: [docs/home-assistant.md](docs/home-assistant.md#the-broker).)

### 2. Install Deskhand

**From a release** (x86_64 or aarch64, glibc 2.35 or newer): download
`deskhand-<version>-<arch>-linux-gnu.tar.gz` from the
[releases](https://github.com/ryanmwright/deskhand/releases), then:

```sh
tar xzf deskhand-*-linux-gnu.tar.gz
cd deskhand-*-linux-gnu
install -Dm755 deskhand ~/.local/bin/deskhand
cp -r share/. ~/.local/share/
```

You need libpulse (`libpulse0` on Debian and Ubuntu, `libpulse` on Fedora and
Arch); every desktop with sound has it. If your shell doesn't find `deskhand`
afterwards, `~/.local/bin` is not on your `PATH`: run `~/.local/bin/deskhand`
instead (or log out and in again, which adds it on most distributions).

**With Nix** (flake):

```sh
nix profile install github:ryanmwright/deskhand
```

or use the Home Manager module instead of steps 2 and 3:

```nix
# flake inputs: deskhand.url = "github:ryanmwright/deskhand";
imports = [ inputs.deskhand.homeManagerModules.default ];
services.deskhand = {
  enable = true;
  passwordFile = "/run/agenix/deskhand-mqtt";  # outside the Nix store
  settings.mqtt = { host = "homeassistant.local"; username = "deskhand"; };
  # headless = true;  # a server or VM: starts at boot without a login
};
# With headless, also in the NixOS configuration:
# users.users.<you>.linger = true;
```

**From source:** Rust 1.99 or newer and libpulse headers (`libpulse-dev`,
`pulseaudio-libs-devel`), then
`cargo install --locked --git https://github.com/ryanmwright/deskhand deskhand`.

### 3. Run the setup

```sh
deskhand setup
```

It looks for the broker on your network, asks for the username and password
and which power buttons (suspend, restart, shut down; none by default) Home
Assistant may press, tests the connection, writes `~/.config/deskhand/config.toml` (the password goes
to a separate private file) and installs and starts a systemd user service that
runs whenever you are logged in to your desktop.

On a server or VM without a desktop, run `deskhand setup --headless`: the
service then starts at boot without a login, and only what makes sense there
is published ([headless machines](docs/configuration.md#headless-machines-servers-and-vms)).

### 4. Find it in Home Assistant

**Settings → Devices & services → MQTT** now lists your computer. Try
locking the screen and watch "Session state" change. Then build automations:
[docs/home-assistant.md](docs/home-assistant.md) has examples (lights off when
you lock, a meeting light, pausing music, actionable notifications, shortcut
triggers), and [`blueprints/`](blueprints) has ready-made ones.

If something doesn't show up:

```sh
deskhand doctor                    # config, broker, what works on this desktop and why
journalctl --user -u deskhand -f   # the service's log
```

## Documentation

- [docs/configuration.md](docs/configuration.md): every config option,
  scripts, command sensors, global shortcuts.
- [docs/entities.md](docs/entities.md): every entity.
- [docs/home-assistant.md](docs/home-assistant.md): broker setup, automations,
  actionable notifications, troubleshooting.
- [SECURITY.md](SECURITY.md): what Home Assistant can and cannot do to the
  computer, broker ACLs, privacy options.
- [CONTRIBUTING.md](CONTRIBUTING.md): development setup, adding integrations
  and desktop backends.
- [CHANGELOG.md](CHANGELOG.md)

## Other commands

| | |
|---|---|
| `deskhand run` | Run the daemon in the foreground (what the service does). |
| `deskhand doctor [--json]` | Check everything without starting the daemon. |
| `deskhand print-discovery` | Show what would be published to Home Assistant. |
| `deskhand uninstall` | Remove this computer from the broker and Home Assistant. |

The tray icon (when your desktop has a tray) shows the connection, and its
**Pause** stops publishing until you resume.

## Acknowledgements

Inspired by [kiot](https://github.com/davidedmundson/kiot), which does this for
KDE. Deskhand is written from scratch.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT) at your option.
