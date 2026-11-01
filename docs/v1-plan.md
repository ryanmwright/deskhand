# Deskhand V1: implementation plan

> **How this plan is executed.** This document is the source of truth for V1. Each milestone
> (M0–M14) is sized for **one Claude Code session**. At the start of a session, read AGENTS.md and
> this file, pick the first unchecked milestone, and tick its boxes as you go. At the end, add a line
> to the Session log (§9). If something turns out different from the plan (especially in M5,
> the spikes), edit this plan in the same commit.

## Context

kiot (davidedmundson/kiot) exposes a KDE desktop to Home Assistant over MQTT. It's C++/Qt,
KDE-only, pre-alpha, and its config is edited by hand. Deskhand gives the same automation surface
(idle, lock, power, media, notifications and so on) with a platform-neutral Rust core. Backends are
selected at compile time: KDE first, then GNOME, macOS and Windows. It should be easy to set up:
a wizard finds the broker, entities appear in HA through discovery, and nothing needs installing on
the HA side.

**Decisions made with the user:**
- **Transport:** MQTT with HA discovery, kept behind a transport boundary. A native zeroconf HA
  integration is post-V1.
- **Media:** stock HA entities by default. A flag also enables the HACS `mqtt_media_player` format.
- **Setup:** `deskhand setup` wizard, `deskhand doctor`, and a ksni tray icon (status and Pause).
- **Packaging (V1):** Nix flake (devShell, package, home-manager module). CI also attaches a
  glibc binary so the user can deploy to their own KDE host.
- **License:** MIT OR Apache-2.0, written from scratch with no kiot (LGPL) code.
- **Repo:** git with GitHub Actions.
- **Features:** read-only and automation-oriented. No "launch app" or "open URL" features.
  User-configured scripts are the only way HA can run commands.
- **Dev target:** this host runs XFCE (VM, nested KVM, 46 GB free). KDE work happens in a
  reproducible **Nix Plasma 6 Wayland VM** (`nix run .#plasma-vm`). The user may also deploy CI
  builds to a real KDE host.

## 1. Architecture

### Workspace (supersedes the layout in AGENTS.md; update AGENTS.md in M0)
```
crates/
  deskhand-core/     entity model, Ctx + typed handles, Hub actor, Integration/Transport traits,
                     supervisor, config framework, identity. No rumqttc, no zbus.
  deskhand-mqtt/     Transport impl: rumqttc, topics, HA device discovery, birth/LWT, stale cleanup,
                     compat media_player
  deskhand-common/   platform-neutral: scripts, command sensors, metrics (sysinfo), derived sensors
                     (session_state, in_call), domain kits (MediaKit/AudioKit/PowerKit) so entity
                     ids are identical on every OS
  deskhand-linux/    freedesktop, reused by the future GNOME backend: logind, ScreenSaver,
                     Notifications, MPRIS, UPower, power-profiles, BlueZ, systemd1, NetworkManager,
                     portal Settings, libpulse audio, camera, ext-idle-notify, GameMode
  deskhand-kde/      KWin script, virtual desktops, NightLight, Plasma DnD, ScreenBrightness,
                     kglobalaccel, DPMS, logout, colour scheme
  deskhand-testkit/  dev-only: FakeTransport, TestBus (private dbus-daemon), mock D-Bus services,
                     mosquitto container
  deskhand/          binary: clap CLI, wiring, features, setup wizard, doctor, tray
```
- **Features** on `deskhand`: `default = ["kde","tray","setup","audio-pulse"]`,
  `kde = ["linux", …]`. Later: `gnome`, `macos`, `windows`.
- **Runtime selection.**
  - Each backend returns candidate integrations grouped by capability, in priority order.
    Example for `idle`: wayland ext-idle-notify, then ScreenSaver polling, then logind IdleHint.
  - The supervisor probes the candidates and starts the first available one per capability.
  - KDE candidates are offered only when `XDG_CURRENT_DESKTOP` contains `KDE`.
  - Nothing assumes a capability exists.

### Core API (one trait plus typed handles, chosen over per-capability traits)
```rust
#[async_trait] pub trait Integration: Send + 'static {
    fn id(&self) -> &'static str;          // "audio.pulse"
    fn capability(&self) -> &'static str;  // "audio" (provider group)
    fn description(&self) -> &'static str; // shown by doctor
    async fn probe(&self) -> Probe;        // Available{detail} | Unavailable{reason}
    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError>; // &self so it can be restarted
}
```
**Entity model:**
- `EntitySpec { id, name, kind: EntityKind, icon, category, enabled_by_default, availability: Integration|Always, retain }`.
- `EntityKind` = Sensor{device_class,state_class,unit,precision,options} | BinarySensor | Switch |
  Button | Number{min,max,step,unit,mode} | Select{options} | Text | Event{event_types} | Notify |
  Lock | Image{Url|Bytes} | Update | MediaPlayer (compat only).
- Device classes are `#[non_exhaustive]` enums with a `Custom(String)` variant.

**`Ctx` API:**
- Typed handles: `ctx.sensor(spec) -> Sensor` (`.set()`, `.set_attributes()`),
  `ctx.switch(spec) -> (Switch, Commands<bool>)`, `button -> Commands<()>`, `number`, `select`
  (`set_options` triggers re-discovery), `text`, `event` (`.fire(type, attrs)`), `notify`, `lock`,
  `image`.
- Other methods: `json_command(id)`, `observe(id)` (a watch receiver, used by derived sensors),
  `set_available`, `ready()`, `config::<T>()`, `cancelled()`.
- Dropping the last handle unregisters the entity, after a **30 s grace period** so restarts don't
  churn HA.

**Hub actor:**
- Single task fed by an mpsc channel.
- Owns the registry, last states, availability and the pause flag.
- Dedupes unchanged states and throttles per entity.
- **Startup barrier:** waits for every integration to call `ready()`, or 5 s.
- Debounces discovery changes by 1 s.

**Supervisor:** runs probes, restarts failed integrations with backoff (1 s → 60 s), re-probes
every 5 min and immediately on resume, and records status for `doctor`.

**Transport boundary:** `Transport::run(link)`.
- Hub → transport: Snapshot, Discovery, State, Attributes, Event, Availability, Pause,
  Sleep/Resume, Shutdown.
- Transport → hub: Connected, Disconnected, Command, HaOnline.

**Identity:**
- `node_id` = the first 12 hex characters of `sha256("deskhand\0"+machine_id+"\0"+user)`, which is
  stable across hostname changes and unique per user. It can be overridden.
- The device name defaults to `PrettyHostname`, then the hostname.
- The MAC goes into device `connections`, so HA Wake-on-LAN can use it (`publish_mac` toggle).

### MQTT and HA discovery
**Device-based discovery** at `homeassistant/device/deskhand_<node>/config` (HA 2024.11+): one
retained, atomic document. The retained copy also serves as the record of what was published, so
there is no state file.

**Topics:**
```
deskhand/<node>/status                 global availability; LWT "offline"; retained
deskhand/<node>/status/<capability>    per-integration availability; retained
deskhand/<node>/<entity>/state|attributes   retained (except events)
deskhand/<node>/<entity>/set           commands; never retained (also JSON endpoints such as rich notifications)
homeassistant/media_player/deskhand_<node>/config   only when compat is enabled
```

**Connection lifecycle:**
1. **Connect.** Use rumqttc 3.1.1 with rustls+ring, keepalive 30 s and LWT. **Set max packet
   size to 4 MiB in both directions**; the 10 KiB default silently breaks discovery and album art.
2. **On ConnAck, clean up stale entities:**
   1. Read our retained device config (1.5 s timeout).
   2. Send `{"p":…}` removal stubs for entities of disabled integrations and for dynamic entities
      gone past the grace period.
   3. For integrations that are enabled but not available right now, keep their components and
      mark them offline.
   4. Publish the full config.
   5. Subscribe to commands and `homeassistant/status`.
   6. Publish all states.
3. **Ignore retained messages on command topics.** A stale "poweroff" must never fire on connect.
4. **HA restart:** on `homeassistant/status=online`, wait a random 0–5 s, then resend discovery
   and all states.
5. **Reconnect** with jittered backoff (1–30 s), then resend a full snapshot. Drop queued stale
   states.
6. **Pause** (from the tray) publishes offline and ignores commands. **SIGTERM** publishes
   `session_state=off` and offline.
7. **Suspend:** a logind delay inhibitor runs this on PrepareForSleep(true): publish
   `session_state=sleeping`, publish offline, disconnect cleanly, release the inhibitor. On wake,
   rebuild the connection immediately and resync.

### Config (`~/.config/deskhand/config.toml`, mode 0600)
```toml
[device]   name = "…"; publish_mac = true            # node_id override optional
[mqtt]     host = "homeassistant.local"; port = 1883; username = "deskhand"
           password_file = "…"   # | password | password_command = [..]; tls, ca_file optional
           discovery_prefix = "homeassistant"; topic_prefix = "deskhand"
[general]  tray = true; idle_threshold = "5m"; log = "info"
[integrations.<capability>]  enabled = "auto" | true | false   # + per-integration options:
  lock.allow_remote_unlock = false     # false → Lock button + locked sensor instead of HA lock entity
  power.buttons = ["lock","suspend","hibernate","reboot","poweroff"]   # "logout" opt-in
  active_window.publish_title = true;  media.compat_mqtt_media_player = false; media.ignore_players
  audio.mic_ignore_apps; camera.ignore_processes; metrics.interval/disks/…; systemd.units = [..]
  dark_mode.light/dark; bluetooth.devices = "paired"|[MACs]; kde_shortcuts.invoke = [{component,action,name}]
[[shortcut]] id, name, default_keys        # custom global shortcut → HA event entity
[[script]]   id, name, command = [argv], timeout, shell = false   # → HA button
[[sensor]]   id, name, command, interval, unit, device_class, state_class, kind, json_attributes
```
Unknown keys produce warnings (via `serde_ignored`), not errors.

## 2. V1 entity catalogue

- **Session (beyond kiot):**
  - `session_state` enum: active / idle / locked / sleeping / off.
  - `user_active` and `idle_seconds`.
  - `screen_locked`, plus a lock entity or a lock button (depending on `allow_remote_unlock`).
  - `keep_awake` switch (logind and ScreenSaver inhibitors).
  - `sleep_inhibited` (attribute lists who is inhibiting).
  - `lid_closed`, `docked`.
  - `power_event` event: sleep / resume.
- **Power:**
  - Buttons, published only if logind `Can*` allows them.
  - `on_ac_power`.
  - Battery: %, state, time remaining.
  - **Peripheral batteries** (mouse, keyboard, headset), as dynamic entities.
  - `power_profile` select.
- **Media:**
  - `media_state` sensor (attributes: title, artist, album, player, duration, position).
  - Play-pause, next, previous and stop buttons.
  - `media_volume` number.
  - `media_player` select (auto or a specific player).
  - `media_art` image.
  - Optional compat `media_player` entity (with seek).
- **Audio:**
  - Volume, mute, output and input device selects.
  - Mic volume, `mic_mute`, `mic_in_use` (attribute lists apps).
  - `headphones_connected`.
  - **`in_call`** (derived: mic in use by a real app for at least 10 s, or camera in use).
- **Camera:** `camera_in_use` (attributes: apps, devices). It is detected by scanning
  `/proc/*/fd`, with inotify only as the trigger, so it doesn't flicker on device probes.
- **KDE desktop:**
  - `active_window` (app_id, title, pid), `fullscreen`, `display_count`.
  - `virtual_desktop` select.
  - `dnd` switch.
  - Night light: running sensor, temperature, inhibit switch, plus a manual switch on Plasma 6.9+.
  - `accent_colour`, `dark_mode` switch.
  - Per-display `brightness_<x>` numbers (including DDC).
  - `screens_off` / `screens_on` buttons.
  - `shortcut_<id>` events, and buttons that invoke allowlisted KDE shortcuts.
- **Notifications:**
  - `notify` entity.
  - A rich JSON topic (`title`, `message`, `icon`, `urgency`, `timeout`, `tag` → replaces,
    `actions`, inline `reply`).
  - A `notification` event entity (`action` / `reply` / `closed`). This lets HA send "Doorbell —
    [Unlock] [Ignore]" and react to the click.
- **Connectivity:**
  - `bluetooth_power` switch, per-device connect switches (with battery attribute).
  - `wifi_ssid` (signal attribute), `network_connectivity`.
- **System:**
  - systemd user-unit switches (allowlist only).
  - Metrics: CPU, load, RAM, swap, disk, CPU temperature, network rx/tx, boot time, GPU on AMD
    sysfs or nvidia-smi.
  - `gamemode_active`.
  - A diagnostic "connected" sensor.
- **Scripts and custom sensors:** kiot parity.

## 3. Crates (checked 2026-10-31)

- **Runtime:** tokio 1.54, tokio-util (CancellationToken), async-trait.
- **MQTT:** rumqttc 0.25 (`use-rustls-no-provider`) + rustls 0.23/ring.
- **D-Bus:** **zbus 5.19** (tokio feature).
  - Stay on 5.x, not 6.x: ksni 0.3.7 and ashpd still require zbus 5.
  - Migrating to 6 is a later one-session task.
- **Config and paths:** serde, serde_json, toml 1.1, serde_ignored, humantime-serde,
  directories 6.
- **Logging and errors:** tracing / tracing-subscriber, thiserror 2 (libraries), anyhow (binary).
- **CLI and setup:** clap 4.6, inquire 0.9, mdns-sd 0.21, ksni 0.3.7.
- **Platform:** sysinfo 0.40 (also GPUs), rustix 1.1 (process-group kill), wayland-client 0.31 + wayland-protocols 0.32 (staging), inotify 0.11,
  libpulse-binding 2.30, sha2, sd-notify.
- **Tests:** insta, testcontainers 0.28 (GenericImage), tempfile, assert_cmd.
- **Avoided:** bluer and mpris (they pull in libdbus), pipewire (bindgen), notify-rust, bollard,
  aws-lc.
- **C dependencies:** libpulse is the only runtime one, behind the `audio-pulse` feature. dbus-daemon
  is needed for tests only.
- **Toolchain:** edition 2024, rust-version 1.95 (required by sysinfo 0.40), from nixpkgs (no overlay).

## 4. Risky integration points (validated in M5)

| Area | Primary | Fallback |
|---|---|---|
| Idle (Wayland) | `ext_idle_notifier_v1` (v2 `get_input_idle_notification`, so playing video doesn't count as activity), 1 s timeout + `idle_since` clock, on a dedicated thread | ScreenSaver `GetSessionIdleTime` polling (X11), then logind IdleHint |
| Custom global shortcuts | kglobalaccel D-Bus `doRegister` + `setShortcutKeys` + `globalShortcutPressed`. No consent dialog; shows in System Settings | XDG GlobalShortcuts portal (needs `host.portal.Registry.Register` and a .desktop file). Will be the GNOME path |
| Active window | Embedded KWin script → `loadScript`/`run` → `callDBus` back to us. **Check that the sender is the owner of org.kde.KWin**. Reload when KWin restarts | Unavailable on X11 in V1 |
| DPMS | Off: invoke PowerDevil's "Turn Off Screen" shortcut. On: `kscreen-doctor --dpms on`. Two buttons (state can't be read) | `kscreen-doctor --dpms off` |
| Plasma DnD | Read `Inhibited`; write with Plasma's `Inhibit`/`UnInhibit` (the cookie is released if we die) | Clear `plasmanotifyrc [DoNotDisturb]` |
| Night light | `inhibit`/`uninhibit`. `activateManual` only if introspection shows it | Inhibit only |
| Dark mode / accent | Read: portal `org.freedesktop.appearance`. Write: `plasma-apply-colorscheme` | `plasma-apply-lookandfeel` |
| Mic in use | libpulse source-outputs, excluding monitors, corked streams and ignore-listed apps | Native PipeWire (post-V1) |
| Camera | inotify trigger + `/proc/*/fd` scan with ignore list | Open/close counting |
| Lock | logind `LockedHint` on the resolved session; Lock/Unlock (unlock gated) | ScreenSaver `GetActive`/`ActiveChanged` |

## 5. Testing strategy
- **Hub unit tests:** use `FakeTransport` and paused tokio time. Cover dedupe, barrier, debounce,
  grace period, pause and availability.
- **Snapshot tests (insta):** discovery JSON for every EntityKind, the compat media_player
  payload, and stale-cleanup diffs.
- **MQTT integration tests (`DESKHAND_IT=1`):** run against a mosquitto container
  (testcontainers; podman socket documented in the devShell). Cover LWT, retained discovery,
  stale removal, birth republish, retained-command rejection, broker restart and 1 MiB payloads.
- **D-Bus tests:** the testkit `TestBus` starts private session and system dbus-daemons, and
  `#[zbus::interface]` mocks stand in for logind, ScreenSaver, Notifications, MPRIS, UPower,
  PowerProfiles, BlueZ, systemd1, NightLight, VirtualDesktopManager, ScreenBrightness,
  KGlobalAccel and KWin Scripting.
- **CLI tests (assert_cmd):** `doctor --json`, `print-discovery`, config errors.
- **Manual E2E:** `compose.yaml` runs mosquitto (passwd + ACL) and a pinned HA image. A bootstrap
  script completes onboarding and adds the MQTT integration through the REST API.
- **Plasma VM:** `nix run .#plasma-vm`. `checks.kde-e2e` (runNixOSTest) asserts discovery, lock
  state and notifications; it runs nightly in CI.
- **CI (GitHub Actions):** fmt, clippy `-D warnings`, test, mqtt-it, cargo-deny, `nix build`,
  nightly VM. **Release artifacts from M3:** a glibc binary built on ubuntu-22.04, uploaded per
  push and per tag, for deploying to the user's KDE host.

## 6. Milestones (one session each)

Dependency order:
- M0 → M1 → M2 → M3 is the core path.
- M4 → M5 come before M6–M11. M6–M9 can be reordered among themselves; M10 and M11 need M5.
- M12 needs M3; M13 needs M12; M14 is last.

### M0: Bootstrap
- [x] `git init`; `.gitignore`, `LICENSE-MIT`, `LICENSE-APACHE`, `rustfmt.toml`, `deny.toml`
- [x] `flake.nix` devShell: rustc, cargo, clippy, rustfmt, rust-analyzer, pkg-config,
      libpulseaudio, dbus, mosquitto, cargo-deny, cargo-insta, just. Stub `packages.default`
- [x] Workspace with all 7 crates as compiling stubs; `[workspace.dependencies]` pinned as in §3
- [x] `.github/workflows/ci.yml` (fmt, clippy, test, deny, nix build)
- [x] `compose.yaml`, `dev/mosquitto/*`, `dev/ha/bootstrap.sh`, `justfile` (`dev-up`, `it`, `vm`)
- [x] Update AGENTS.md: crate layout, zbus-5 pin, dev commands, link to this plan.
      Define the app-id / D-Bus name constant once
- **Accept:** `nix develop -c cargo test` passes; `just dev-up` gives HA on :8123 with MQTT
  configured; CI is green.
- **Status:** done. CI passed on the first push to github.com/ryanmwright/deskhand. `APP_ID` is `io.github.ryanmwright.Deskhand`. If the repo moves to
  an org, change it in `deskhand-core` before V1 ships; it is cheap to change until then.

### M1: Core model and Hub
- [x] EntitySpec/EntityKind/State/CommandValue; typed handles (Drop unregisters); Ctx
- [x] Integration/Transport/Probe traits; Hub actor; supervisor (priority groups, backoff, re-probe)
- [x] Config framework (`enabled = auto|true|false`, raw `[integrations.*]` tables, serde_ignored);
      identity; testkit FakeTransport
- **Accept:** about 25 paused-time unit tests pass; clippy is clean.
- **Status:** done, with 50 tests (hub, supervisor, config, identity, entity). Deviations from the
  design above:
  - `Integration::run(&self, ctx)` instead of `self: Box<Self>`, so a failed integration can be
    restarted. Integrations must be `Send + Sync` and keep per-run state inside `run`.
  - A full command stream (capacity 8) drops the *newest* command with a warning. tokio mpsc
    cannot evict the oldest.
  - The startup barrier also has an absolute bound (`max_startup`, 15 s from hub start) in case
    probing itself hangs. The 5 s `startup_timeout` starts when the supervisor reports startup
    complete.
  - Lock state uses `ctx::LockState`. `Ctx::raw` is an untyped escape hatch for update and
    media_player.

### M2: MQTT transport
- [x] rumqttc setup (TLS, 4 MiB packets, LWT, password_file/command); topic layout
- [x] Device-discovery renderer with insta snapshots; command routing (retained rejected)
- [x] Birth republish with jitter; stale cleanup; reconnect and resync; `deskhand run` with a demo
      info integration
- [x] testcontainers integration tests
- **Accept:** the device appears in the compose HA; `kill -9` makes it unavailable within 45 s;
  a renamed entity is cleaned up after restart; integration tests pass.
- **Status:** done, checked against the dev HA:
  - Entities appear in HA.
  - `kill -9` made them unavailable in under 1 s (the LWT fires on socket close).
  - SIGTERM published offline immediately.
  - Disabling an integration removed its entities, and re-enabling brought them back.
  - 15 unit and snapshot tests, plus 8 broker integration tests (`just it`), stable over repeated runs.
- **Notes:**
  - JSON command endpoints use `<id>/set` like entities, not a separate `notifications/send`.
  - The poll task owns rumqttc's event loop and exits when the transport goes away, so a
    crashed transport cannot keep a zombie session alive.
  - The transport publishes only with `try_*` calls (queue of 2048), so a dead connection
    never blocks the hub.
  - `Snapshot` drives the first discovery publish. On first connect the transport reads its
    own retained device document (1.5 s timeout) to find stale entities.
  - `deskhand_linux::platform_identity()` reads files only. hostname1, the Plasma version and
    other D-Bus details come in M6.

### M3: CLI, doctor, common integrations, CI artifacts
- [x] `run | doctor [--json] | print-discovery | uninstall`; tracing (journald-friendly); sd-notify
- [x] Scripts (argv, timeout, last-run attributes); command sensors; sysinfo metrics + GPU; uptime;
      derived-sensor scaffolding
- [x] CI job builds a release binary on ubuntu-22.04 and uploads it as an artifact.
      `docs/dev/deploy-to-host.md` (copy the binary, run `deskhand doctor`)
- **Accept:** a script button runs from HA; metrics update; `uninstall` leaves no retained topics.
- **Status:** done, checked against the dev HA:
  - Pressing the script button through HA's REST API ran the command, and the result sensor
    showed `success` with exit code 0.
  - Metrics and command sensors update on their intervals.
  - `uninstall` refused while the daemon was online. After stopping it, `uninstall` removed all
    retained messages, HA deleted the device, and `mosquitto_sub --retained-only` found nothing.
  - 104 tests: 6 CLI, 21 common, 50 core, 1 linux, 17 MQTT unit, and 9 broker tests that run
    with `just it` (including a new uninstall test).
- **Notes and deviations:**
  - Capabilities: `scripts` (`[[script]]`), `command_sensors` (`[[sensor]]`), `metrics`. Each
    script is a button `script_<id>` plus a diagnostic enum sensor `script_<id>_result`
    (running/success/failed/timeout; attributes `last_started`, `duration_s`, `exit_code`). A
    press while the script runs is ignored. Script output is logged at debug level only, never
    published.
  - Commands run with stdin closed, in their own process group (killed as a group on timeout or
    shutdown, through `rustix`, since the workspace denies `unsafe`), with output capped at
    64 KiB. `command` is an argv array, or a string with `shell = true`.
  - Command sensors: trimmed stdout is the state; `kind = "binary_sensor"` parses on/off;
    `json_attributes = true` takes `state` from a JSON object and the other keys as attributes.
    Failure makes the state unknown and logs once per failure streak.
  - **GPU uses sysinfo 0.40's own `Gpus`** (AMD/Intel sysfs, NVIDIA through NVML, Vulkan
    fallback) instead of hand-written sysfs and `nvidia-smi` parsing. `Gpus` is not `Send`, so
    metrics are sampled on a dedicated thread that stops when the integration's run ends.
  - "Uptime" is a `boot_time` timestamp sensor, so HA shows a relative time and nothing ticks.
  - Metrics register only the entities the first sample could read (no temperature entity on a
    VM, no GPU entity without a GPU). Network rates skip loopback, container, bridge and VPN
    interfaces unless `network_interfaces` lists some.
  - `HubHandle::wait_released()` drives systemd `READY=1` (after the startup barrier, even if the
    broker is down). `STOPPING=1` on shutdown. No watchdog yet.
  - Logs go to stderr. Under journald (`JOURNAL_STREAM`) each line has a `<N>` syslog priority
    prefix, so `journalctl -p warning` works.
  - `doctor` exits 1 when the config is invalid, the broker is unreachable, or an integration
    with `enabled = true` has no provider. It reads our retained status and discovery document
    to say whether a daemon is online and how many entities are published.
  - `uninstall` uses a separate admin client id with no last will. It refuses while our status
    is `online` unless `--force`; `--dry-run` lists the topics; it asks for confirmation unless
    `--yes`. It clears the discovery document first.
  - Release artifacts: `deskhand-<ref>-x86_64-linux-gnu.tar.gz` plus `.sha256` per push; `v*`
    tags also attach them to a draft GitHub release.
  - Fixed `dev/ha/bootstrap.sh` re-runs: HA returns 404 for `/api/onboarding` once onboarding is
    finished, so the script now waits on `/api/` and treats 404 as done.

### M4: Plasma VM and D-Bus testkit
- [x] **Prerequisite: disk.** The Plasma closure, Rust targets and HA image need about 40 GB.
      The user grew the disk on 2026-11-01 (256 GB). The VM closure took about 17 GB; 78 GB is
      still free, so `nix-collect-garbage -d` was not needed (and was not run).
- [x] `nixosConfigurations.plasma-vm` + `apps.plasma-vm`: Plasma 6 Wayland, SDDM autologin,
      4 GB RAM, 4 vCPU, virtio-gpu, host broker at 10.0.2.2, workspace shared over 9p, runs a
      deskhand user service from the shared build. Nested KVM is available here
- [x] TestBus (private session and system buses); first mocks (logind, ScreenSaver)
- **Accept:** the VM boots into a Plasma Wayland desktop and `org.kde.KWin` is on its bus; a mock
  bus test passes in CI.
- **Status:** done locally. The VM boots to the Plasma desktop in about 30 s (checked with a QEMU
  screenshot). Over ssh: `XDG_SESSION_TYPE=wayland`, `XDG_CURRENT_DESKTOP=KDE`, and
  `org.kde.KWin`, `org.kde.plasmashell` and `org.freedesktop.ScreenSaver` are on the session bus.
  The `deskhand-dev` service (host build, `Type=notify`) reached the host broker and its entities
  appeared in HA. The 4 mock bus tests pass locally; CI installs `dbus` and the tests fail there,
  instead of skipping, if `dbus-daemon` is missing.
- **Notes:**
  - VM definition: `nix/plasma-vm.nix`. Disk image and generated ssh key live in `dev/vm/`
    (ignored by git). The launcher refuses to run outside the repository, exports
    `DESKHAND_WORKSPACE` for the 9p share and unsets `LD_LIBRARY_PATH`. The developer's shell
    exported a newer alsa-lib there, which broke QEMU with a glibc symbol error.
  - The host's `/nix/store` is shared into the VM, so `cargo build` on the host is enough;
    `just vm-deploy` rebuilds and restarts the service. The VM user `dev` has uid 1000 to match
    the host user on the 9p share.
  - ssh on 127.0.0.1:2222 with a key the launcher generates (`dev/vm/ssh.sh`, `just vm-ssh`).
    Password login is off. `just vm-headless` + `just vm-screenshot` (QEMU writes PNG itself)
    let an agent check the desktop without a window.
  - virtio-vga without host GL (software rendering), so the VM also runs headless.
  - CI evaluates the VM derivation and checks formatting of `nix/*.nix`; building and booting
    it in CI is the M13 nightly job.
  - Mocks: `MockLogind` (Manager with GetSession/`auto`, Can*, power calls, Inhibit returning a
    pipe fd whose closing releases the lock, PrepareForSleep, IdleHint/LidClosed/Docked/
    *Inhibited properties; Session at `_31` and `auto` with LockedHint, Lock/Unlock signals) and
    `MockScreenSaver` (both KDE paths, idle time, Inhibit/UnInhibit, ActiveChanged). Handles
    change state with `PropertiesChanged` and record calls.

### M5: KDE risk spikes
- [ ] `examples/` probes run inside the VM: ext-idle-notify v2 (and its behaviour while video
      plays), KWin script callback plus sender check, kglobalaccel register/press/persist, Plasma
      DnD Inhibit semantics, DPMS off/on, libpulse mic filters, camera `/proc` scan, logind
      `session/auto` from a user service
- [ ] Record the results in `docs/dev/spikes.md`; **amend §4 and M6–M11 of this plan if anything
      is blocked**
- **Accept:** every item is marked works, fallback or blocked.

### M6: Linux session and power
- [ ] LinuxEnv (session and system connections, desktop detection, hostname1); lock state and
      control (unlock gate); power buttons via `Can*`
- [ ] PrepareForSleep delay inhibitor wired through Hub Sleep/Resume to the transport; keep-awake
- [ ] Idle providers (wayland, screensaver, logind); `session_state`; lid/docked; sleep_inhibited;
      mock tests
- **Accept (VM):** locking flips `screen_locked`/`session_state`; suspend shows sleeping and the
  device is back online within 10 s of resume; 5 min idle flips `user_active`.

### M7: Linux hardware and services
- [ ] UPower (battery, AC, dynamic peripherals); power-profile select; NetworkManager SSID and
      connectivity
- [ ] systemd unit switches (allowlist, Subscribe); BlueZ adapter and dynamic devices
      (ObjectManager); GameMode; mocks
- **Accept:** mock tests pass; on the host, a BT device appears and its switch works, and a
  systemd unit stays in sync.

### M8: Notifications and MPRIS media
- [ ] notify entity; rich JSON topic; action and reply events; tag → replaces_id
- [ ] `MediaSource` trait + MediaKit; MPRIS multiplexer (NameOwnerChanged, PropertiesChanged,
      Seeked); art image; compat media_player renderer and snapshot; mock MPRIS tests
- **Accept:** clicking a notification action fires an HA automation; HA controls play/pause and
  volume; the compat media_player works, including seek.

### M9: Audio, camera, in-call
- [ ] libpulse mainloop thread with subscriptions; AudioKit (volume, mute, device selects, mic,
      mic_in_use, headphones)
- [ ] Camera inotify + `/proc` scan; `in_call` derived sensor; `audio-pulse` feature gating
- **Accept:** volume stays in sync both ways; switching output works; a browser call sets
  mic_in_use, camera_in_use and in_call, with app names.

### M10: KDE desktop part 1
- [ ] KWin script integration (active window, fullscreen, display count, reload on KWin restart)
- [ ] Virtual desktop select; night light; DnD; accent colour and dark mode (portal read in
      linux, write in kde); mocks
- **Accept (VM):** entities reflect UI changes within 1 s and HA commands change the desktop.

### M11: KDE desktop part 2
- [ ] kglobalaccel custom shortcuts → events; invoke-allowlist buttons. Use a **fixed component
      name `"deskhand"`, not `APP_ID`**, so renaming the app id never orphans the user's key bindings
- [ ] ScreenBrightness dynamic numbers; DPMS buttons; Plasma logout; opt-in output enable/disable
- **Accept:** a shortcut fires an HA automation and appears in System Settings; brightness works.

### M12: Setup wizard and tray
- [ ] `deskhand setup`: broker discovery in this order: mDNS `_mqtt._tcp`, then
      `_home-assistant._tcp` host on port 1883, then `homeassistant.local`, then localhost.
      Credentials and TLS; connection test with readable CONNACK errors; atomic 0600 write
- [ ] Install, enable and start the systemd user unit (`PartOf`/`After`/`WantedBy=graphical-session.target`,
      `Type=notify`, `Restart=on-failure`); if the binary is in /nix/store, point to the
      home-manager module instead
- [ ] ksni tray (status, Pause, open config, quit; only when a StatusNotifierWatcher exists);
      polish doctor output
- **Accept:** setup on a fresh VM user ends with a running service and the device in HA; Pause
  makes the device unavailable.

### M13: Nix packaging and VM e2e test
- [ ] `packages.default` (buildRustPackage, libpulse, .desktop file, unit template)
- [ ] `homeManagerModules.default` (`services.deskhand.{enable,package,settings,passwordFile,tray}`)
- [ ] `checks.kde-e2e` (runNixOSTest) plus a nightly CI job
- **Accept:** `nix build` and `nix flake check` pass; the HM module gives a working service with
  the password outside the store.

### M14: Docs and release
- [ ] README (features, quick start), `docs/configuration.md`, `docs/entities.md`,
      `docs/home-assistant.md` (automations: lights off on lock or sleep, meeting light from
      `in_call`, pause media when locked, notify when a job finishes, shortcut triggers)
- [ ] `blueprints/` (actionable notification, idle → lights, meeting light); `SECURITY.md`
      (broker ACL per node, unlock risk, title privacy, command allowlist); `CONTRIBUTING.md`
      (adding an integration or backend); release workflow
- **Accept:** a new user can go from README to a working setup using only the docs.

## 7. Security principles (apply throughout)
- MQTT never carries commands to execute. Scripts, systemd units and KDE shortcuts are allowlisted
  in config.
- Retained command messages are ignored.
- Remote unlock is off by default.
- Window titles can be withheld (`publish_title = false`).
- The KWin callback checks the sender.
- The config is 0600; passwords can come from a file or a command.

## 8. Post-V1 roadmap
- **Backends:** GNOME (Mutter IdleMonitor, GlobalShortcuts portal, a Shell extension for the
  active window); macOS (objc2); Windows (`windows` crate); wlroots/Hyprland.
- **Packaging:** AUR, deb, rpm, Flatpak (portal-only providers), nixpkgs upstream, NixOS module.
- **Transport:** native HA integration (zeroconf + websocket, no broker); MQTT v5.
- **Features:** update entity (GitHub releases), screen-share detection, opt-in screenshot image,
  opt-in notification mirroring to HA, Activities, keyboard layout, per-app volume,
  Docker/Podman, gamepad, TTS/playmedia, Music Assistant, X11 active window, GUI settings,
  localisation. Macros and app/game launcher only with a strong use case.
- **Maintenance:** zbus 6 migration; pure-Rust `pulseaudio` crate if it matures.

## 9. Session log
| Date | Milestone | Outcome / notes |
|---|---|---|
| 2026-10-31 | Planning | Plan approved; decisions recorded in Context |
| 2026-10-31 | M0 | Flake devShell, 7-crate workspace, CI workflow, cargo-deny, dev HA + Mosquitto stack (`just dev-up`, verified discovery and ACL). Findings: sysinfo 0.40 needs Rust 1.95, so `rust-version` was raised. rumqttc 0.25.1 pulls rustls-webpki 0.102 (advisories), but only for an error type; ignored in deny.toml with reasons. The HA MQTT config flow needs `protocol` and `other_settings`. CI passed on the first push |
| 2026-11-01 | M1 | Core model, hub, supervisor, config and identity in `deskhand-core`; `FakeTransport` behind a `test-util` feature, re-exported by the testkit. 50 tests on paused time, stable over 30 runs. Deviations recorded under M1 |
| 2026-11-01 | M2 | MQTT transport (device discovery, stale cleanup, birth, LWT, reconnect, pause, sleep), `deskhand run` with a version/start-time integration, Linux file-based identity, `just run` and `dev/ha/states.sh`. Bug found by an integration test: an aborted transport left its poll task holding the connection (no LWT); fixed |
| 2026-11-01 | M3 | `doctor [--json]`, `print-discovery [--all]`, `uninstall` (admin MQTT client), journald priorities, sd-notify READY/STOPPING via `HubHandle::wait_released`. Scripts, command sensors, sysinfo metrics (GPU via sysinfo, sampled on its own thread because `Gpus` is not `Send`), `boot_time`, derived-sensor helpers (`Inputs`, `Debounced`). CI `release-binary` job on ubuntu-22.04 and `docs/dev/deploy-to-host.md`. Acceptance checked against the dev HA. Fixed `bootstrap.sh` re-runs. User grew the disk (95 GB free) for M4 |
| 2026-11-01 | M4 | Plasma 6 Wayland VM (`nix/plasma-vm.nix`, `just vm`/`vm-headless`/`vm-ssh`/`vm-deploy`/`vm-screenshot`), workspace over 9p, host build as a `deskhand-dev` user service reporting to the host broker; checked by screenshot, over ssh (KWin on the bus) and in HA. `TestBus` plus logind and ScreenSaver mocks with 4 tests. Launcher unsets a host `LD_LIBRARY_PATH` that broke QEMU |

## Verification (whole V1)
1. `nix develop -c just ci` runs fmt, clippy, unit tests, snapshots and mosquitto integration tests.
2. `just dev-up` starts HA and the broker; `nix run .#plasma-vm` starts deskhand in the VM; the
   "Deskhand" device in HA shows the §2 entities.
3. Drive each milestone's acceptance checks in the VM (lock, idle, suspend, notification action,
   media, shortcut).
4. `nix flake check` runs the kde-e2e VM test.
5. Deploy the CI binary to the user's real KDE host and check hardware features: Bluetooth,
   camera, DDC brightness, suspend.
