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
| `just run` | runs the daemon against the dev stack (`dev/config.toml`) |
| `cargo run -p deskhand -- --config dev/config.toml doctor` | checks config, broker and integrations (also `print-discovery`, `uninstall`) |
| `dev/ha/states.sh [regex]` | prints matching HA entity states (default: `deskhand`) |
| `dev/ha/api.sh METHOD PATH [JSON]` | calls the dev HA REST API (services, automations, history) |
| `just mqtt-watch` | prints all broker traffic |
| `nix build` | builds the `deskhand` package (crane; `nix/package.nix`) |
| `just e2e` | the `kde-e2e` NixOS test: Plasma VM with the packaged daemon (needs KVM; nightly in CI) |
| `just flake-check` | every flake check: package, crane fmt/clippy/test/deny, HM module, e2e |

- Dev broker users: `deskhand`/`deskhand-dev` (ACL: `deskhand/#`, `homeassistant/#`) and `homeassistant`/`homeassistant-dev`.
- `dev/ha/bootstrap.sh` stores an HA refresh token in `dev/ha/config/.deskhand-dev-refresh-token` for scripted checks against the HA REST API.
- Never `builtins.getFlake` a plain path of this repo: it copies `target/` (tens of GB) into the store. Use `git+file://` or `.#` references.
- Use `pkill -x deskhand`, not `pkill -f`: the `-f` pattern also matches the shell running the command.
- On this machine `docker` is podman. The devShell points testcontainers at the podman socket.
- This dev host runs XFCE, not KDE. KDE features are tested in the Plasma 6 Wayland VM or on the user's real KDE host using CI artifacts (`docs/dev/deploy-to-host.md`).
- Plasma VM (`nix/plasma-vm.nix`): `just vm` (window) or `just vm-headless` + `just vm-screenshot` (PNG at `/tmp/plasma-vm.png`). The repo is mounted at `/mnt/deskhand`; the `deskhand-dev` user service runs the host's `target/debug/deskhand` with `dev/vm/config.toml` against the host broker (10.0.2.2). `just vm-deploy` rebuilds and restarts it (dropping the guest's page cache: the VM can otherwise keep stale 9p pages of a rebuilt binary and segfault); `just vm-ssh '<cmd>'` runs commands as `dev` (uid 1000). `just vm-reset` deletes the VM disk.
- Spike probes (`cargo build --examples`, results in `docs/dev/spikes.md`) run in the VM as a user service: `just vm-ssh 'systemd-run --user --wait --pipe /mnt/deskhand/target/debug/examples/<probe>'`. Plain ssh has no Wayland env, and logind's `auto` session is the ssh one. Send keys with the QEMU monitor (`sendkey`) and click with `dev/vm/click.sh X Y [right]` (QMP tablet events, needs `just vm-headless`); HMP `mouse_move` does nothing. The VM can suspend but never resumes (QEMU); recover with `system_reset` in the monitor.
- D-Bus tests: `deskhand_testkit::TestBus` starts private session and system buses; mocks live in `deskhand_testkit::mocks`. Start tests with `require_dbus!()`.

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

All V1 milestones (M0–M14) are done. v0.1.1 is the first release (v0.1.0 was tagged but never published); the release workflow creates a draft to publish. Power buttons are opt-in (none by default). The two host checks below were assumed good for the tag; the user confirms them later. Next step: **M15 (headless mode and herdr)**, planned in `docs/v1-plan.md`; start by reading `herdr api schema --json`. Still open: the binary-cache CI decision (plan, M13). Pending on the KDE host: resume from suspend (M6) and a browser call with camera and a muted mic (M9). Bluetooth (M7) and monitor brightness (M11) are confirmed there.
