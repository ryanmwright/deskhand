# Deskhand

Deskhand is a daemon that exposes a desktop machine to MQTT and Home Assistant: sensors, controls, notifications, media, power and idle state. It is inspired by [kiot](https://github.com/davidedmundson/kiot), which is KDE-only. Deskhand starts with KDE but is designed so other desktops and operating systems can be added as plugins.

## Name

- **Project name:** Deskhand (a helper for your desk, not tied to any DE or OS).
- **Crate/binary name:** `deskhand`. It was free on crates.io, PyPI and npm as of 2026-10-31.
- **Plugin crates:** `deskhand-<backend>`, for example `deskhand-kde`, `deskhand-gnome`, `deskhand-macos`, `deskhand-windows`.

## Goals

- Publish desktop state to MQTT and accept commands from it.
- Integrate with Home Assistant through MQTT discovery.
- Keep the core platform-neutral. Platform code lives only in backend crates.
- Start with KDE. Add GNOME, macOS and Windows later without changing the core.

## Tech stack

- **Language:** Rust (stable), edition 2024, `rust-version = 1.95` (sysinfo 0.40 requires it). The toolchain comes from nixpkgs through the flake devShell.
- **Async runtime:** `tokio`.
- **MQTT:** `rumqttc` (MQTT 3.1.1, rustls with the `ring` provider). Home Assistant discovery is done by publishing device-based discovery JSON.
- **Config:** `serde` with TOML. Use `directories` for XDG and platform config paths.
- **Logging:** `tracing` and `tracing-subscriber`.
- **Errors:** `thiserror` in library crates, `anyhow` in the binary.
- **Linux desktop integration:** `zbus` (D-Bus), **pinned to 5.x** until `ksni` supports zbus 6. KDE and GNOME backends share most of this through `deskhand-linux`.
- **Audio:** `libpulse-binding`, behind the `audio-pulse` feature. It is the only runtime C dependency.
- **macOS:** `objc2` bindings (future backend).
- **Windows:** the `windows` crate (future backend).
- Dependency versions live only in `[workspace.dependencies]` in the root `Cargo.toml`. Crates use `foo.workspace = true`.

## Architecture

Cargo workspace (details in [`docs/v1-plan.md`](docs/v1-plan.md) §1):

```
crates/
  deskhand-core/      # entity model, Ctx + typed handles, hub, Integration/Transport traits,
                      # supervisor, config, identity. No MQTT, no D-Bus, no platform code.
  deskhand-mqtt/      # Transport impl: rumqttc, topics, HA discovery, birth/LWT, stale cleanup
  deskhand-common/    # platform-neutral integrations: scripts, command sensors, metrics,
                      # derived sensors, domain kits (media/audio/power)
  deskhand-linux/     # shared freedesktop integrations (logind, MPRIS, UPower, BlueZ, ...)
  deskhand-kde/       # KDE Plasma backend (KWin, PowerDevil, kglobalaccel, ...)
  deskhand-testkit/   # dev-only: FakeTransport, private D-Bus buses, mocks, broker container
  deskhand/           # binary: CLI, wiring, cargo features, setup wizard, doctor, tray
  # future: deskhand-gnome, deskhand-macos, deskhand-windows
```

- `deskhand-core` defines one `Integration` trait. Integrations register entities through `Ctx` and get typed handles back (`Sensor`, `Switch` plus a command stream, and so on). Per-capability traits are deliberately not used.
- Each backend returns candidate integrations grouped by capability, in priority order. The supervisor probes them and starts the first available one per capability.
- **Plugins are compile-time**, selected with cargo features on the `deskhand` binary (`kde`, `linux`, `tray`, `setup`, `audio-pulse`). Do not use dynamic loading; Rust handles it poorly and compile-time selection is simpler and safer.
- A backend only registers the capabilities its platform supports. The core must not assume any capability exists.
- Core API in brief: `Integration { id, capability, description, probe, run(&self, ctx) }`. `Ctx` hands out handles (`ctx.sensor(spec)`, `ctx.switch(spec) -> (Switch, Commands<bool>)`, ...). Call `ctx.ready()` once the initial states are set. The hub owns all state; the transport gets `HubEvent`s and requests a full `Snapshot` on every (re)connect.
- Tests that need a transport use `deskhand_core::testing::FakeTransport` (feature `test-util`, re-exported by `deskhand-testkit`) and `#[tokio::test(start_paused = true)]`.
- `deskhand_core::APP_ID` is the single source for the D-Bus name, desktop file and portal app id.

## Development

Everything runs inside the flake devShell: `nix develop` (or prefix commands with `nix develop -c`).

| Command | What it does |
|---|---|
| `just ci` | fmt check, clippy `-D warnings`, tests, cargo-deny (what CI runs) |
| `just it` | tests including the mosquitto container integration tests (`DESKHAND_IT=1`) |
| `just dev-up` | starts Home Assistant (:8123, `dev`/`devpassword`) and Mosquitto (:1883), then onboards HA and adds the MQTT integration |
| `just dev-down` / `just dev-reset` | stops the stack, or stops it and wipes the HA config |
| `just mqtt-watch` | prints all broker traffic |
| `nix build` | builds the `deskhand` package |

- Dev broker users: `deskhand`/`deskhand-dev` (ACL: `deskhand/#`, `homeassistant/#`) and `homeassistant`/`homeassistant-dev`.
- `dev/ha/bootstrap.sh` stores an HA refresh token in `dev/ha/config/.deskhand-dev-refresh-token` for scripted checks against the HA REST API.
- On this machine `docker` is podman. The devShell points testcontainers at the podman socket.
- This dev host runs XFCE, not KDE. KDE features are tested in the Plasma VM (`just vm`, added in M4) or on the user's real KDE host using CI artifacts.

## Conventions

- Keep platform-specific code out of `deskhand-core`.
- Prefer small, single-purpose entities over large ones.
- Never execute commands received over MQTT. Only run allowlisted, user-configured actions (scripts, systemd units, KDE shortcuts).
- Run `just ci` (at least `cargo fmt` and `cargo clippy`) before committing.
- `cargo deny` ignores are allowed only with a `reason` explaining why the advisory does not apply.

## Status

The V1 plan is approved: see [`docs/v1-plan.md`](docs/v1-plan.md).

- Each milestone (M0–M14) in that plan is sized for one session. Start each session by reading the plan and continuing with the first unchecked milestone.
- Tick the plan's checkboxes as work completes, and add a row to its Session log at the end of each session.

Next step: **M2 (MQTT transport)**.
