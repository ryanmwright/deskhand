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
  dark_mode.light/dark; bluetooth.devices = "paired"|[MACs]; shortcuts.invoke = [{component,action,name,id}]
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
  - `screens_on` switch (KWin DPMS protocol; `screens_off` button as the fallback).
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

## 4. Risky integration points (validated in M5, see `docs/dev/spikes.md`)

| Area | Primary | Fallback |
|---|---|---|
| Idle (Wayland) | `ext_idle_notifier_v1` (v2 `get_input_idle_notification`, so playing video doesn't count as activity), 1 s timeout + `idle_since` clock, on a dedicated thread | ScreenSaver `GetSessionIdleTime` polling (X11), then logind IdleHint |
| Custom global shortcuts | kglobalaccel D-Bus `doRegister` + `setShortcutKeys` (IsDefault, then SetPresent) + `globalShortcutPressed`. No consent dialog; shows in System Settings. **Pad every key sequence to 4 ints**: a shorter array aborts KWin | XDG GlobalShortcuts portal (needs `host.portal.Registry.Register` and a .desktop file). Will be the GNOME path |
| Active window | Embedded KWin script → `loadScript` + `/Scripting/Script<id>.run` → `callDBus` back to us with one JSON string. **Check that the sender is the owner of org.kde.KWin**. Reload when KWin restarts | Unavailable on X11 in V1 |
| DPMS | `org_kde_kwin_dpms_manager` Wayland protocol: reports each output's mode (also when input wakes it) and sets it, so one `screens_on` switch (checked in M11) | Invoke PowerDevil's "Turn Off Screen" shortcut (`screens_off` button) |
| Plasma DnD | Read `Inhibited` (covers user DnD and app inhibitions). Write `plasmanotifyrc [DoNotDisturb] Until` with a KConfig change notification (on = far future, off = delete; also ends a user-set DnD) | `Inhibit`/`UnInhibit` (released if we die; cannot end a user DnD) |
| Night light | `inhibit`/`uninhibit`. `activateManual` only if introspection shows it | Inhibit only |
| Dark mode / accent | Read: portal `org.freedesktop.appearance`. Write: `plasma-apply-colorscheme` | `plasma-apply-lookandfeel` |
| Mic in use | libpulse source-outputs, excluding monitors, corked streams and ignore-listed apps | Native PipeWire (post-V1) |
| Camera | inotify trigger + `/proc/*/fd` scan with ignore list | Open/close counting |
| Lock | logind `LockedHint` on the session from `User.Display` (fallback `GetSession("auto")`; never `GetSessionByPID` from a service); Lock/Unlock (unlock gated) | ScreenSaver `GetActive`/`ActiveChanged` |

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
- [x] `examples/` probes run inside the VM: ext-idle-notify v2 (and its behaviour while video
      plays), KWin script callback plus sender check, kglobalaccel register/press/persist, Plasma
      DnD Inhibit semantics, DPMS off/on, libpulse mic filters, camera `/proc` scan, logind
      `session/auto` from a user service
- [x] Record the results in `docs/dev/spikes.md`; **amend §4 and M6–M11 of this plan if anything
      is blocked**
- **Accept:** every item is marked works, fallback or blocked.
- **Status:** done. All eight work on Plasma 6.6.6; nothing is blocked. Changes made to §4 and
  M6/M9/M10/M11:
  - logind: resolve the session through `User.Display`.
  - DnD: write `plasmanotifyrc` with a KConfig notification instead of `Inhibit`.
  - kglobalaccel: pad key sequences to four ints. A short array crashed KWin in the spike.
  - KWin script: pass one JSON string.
  - Camera: check apps that use the camera through PipeWire on the real host.

### M6: Linux session and power
- [x] LinuxEnv (session and system connections, desktop detection, hostname1); lock state and
      control (unlock gate); power buttons via `Can*`. Session = `User.Display`, fallback
      `GetSession("auto")` (spikes §8)
- [x] PrepareForSleep delay inhibitor wired through Hub Sleep/Resume to the transport; keep-awake
- [x] Idle providers (wayland, screensaver, logind); `session_state`; lid/docked; sleep_inhibited;
      mock tests
- **Accept (VM):** locking flips `screen_locked`/`session_state`; suspend shows sleeping and the
  device is back online within 10 s of resume; 5 min idle flips `user_active`.
- **Status:** done, with one part verified outside the VM:
  - Locking (loginctl and HA's Lock screen button) flipped `screen_locked` and `session_state`
    in HA within 3 s.
  - 5 min without input: `user_active` was still on at 290 s and off at 330 s (`idle_time` 330).
    `session_state` showed `locked`, because Plasma auto-locks at 5 min and locked wins over idle.
  - HA's Suspend button put the VM to sleep, and HA showed `sleeping` with the device offline.
    **The QEMU guest never resumes**, from S3 or s2idle (even with an RTC alarm). So resume was
    verified with a new broker test (sleep publishes offline and the retained `sleeping`, stays
    offline, and is back online about 2 ms after resume), plus the mock logind test of the
    inhibitor sequence. Check a real resume on the KDE host.
  - Keep awake took a logind `sleep:idle` block inhibitor and a ScreenSaver inhibition, which
    `sleep_inhibited` reported.
- **Fixed on the way:** the M2 transport never sent the offline status or DISCONNECT on sleep.
  The poll task checked `active` before it sent the queued requests. `active` now only gates
  reconnecting. Found by the new broker test.
- **Notes and deviations:**
  - New `Lifecycle::PrepareSleep` (`Ctx::notify_prepare_sleep`). The power integration fires
    `power_event: sleep`, sends PrepareSleep (so `session_state=sleeping` is published while
    still connected), waits 300 ms, sends Sleep (offline, disconnect), waits 1.2 s, then releases
    the delay inhibitor. A `power_event: resume` fired right after waking is usually dropped,
    because the transport is still reconnecting. Use `session_state` or `connected` in
    automations.
  - Capabilities: `lock` (`lock.logind`, `lock.screensaver`), `power` (`power.logind`),
    `keep_awake`, `idle` (`idle.wayland`, `idle.screensaver`, `idle.logind`), `session_state`
    (in deskhand-common). Entities: `screen_locked`, `lock_screen` button or `screen_lock` lock
    (`allow_remote_unlock`), `suspend`/`hibernate`/`hybrid_sleep`/`suspend_then_hibernate`/
    `reboot`/`power_off` (only if `Can*` is `yes`; `challenge` hides them), `power_event`,
    `sleep_inhibited` (attribute `inhibitors`), `lid_closed`/`docked` (only with a lid switch),
    `keep_awake`, `user_active`, `idle_seconds` (0 while active, refreshed every 30 s while
    idle), `session_state`.
  - `power.buttons` default is `["suspend","hibernate","reboot","poweroff"]`. "lock" comes from
    the lock capability; "logout" from the KDE backend (M11).
  - hostname1 was not needed: identity still reads files, including `/etc/machine-info`.
  - The Wayland idle thread polls its socket with a 500 ms timeout, so it stops promptly on
    cancel.
  - VM: suspend uses s2idle (`SuspendState=freeze`), which at least fails without corrupting
    state. Recover a frozen guest with `system_reset` in the QEMU monitor. Polkit refuses
    suspend from ssh (not the active session); use the HA button or `sudo`.
  - Fixed a race in the M3 uninstall broker test (it read retained topics before the attributes
    had been published). It failed in CI on the M5 push.

### M7: Linux hardware and services
- [x] UPower (battery, AC, dynamic peripherals); power-profile select; NetworkManager SSID and
      connectivity
- [x] systemd unit switches (allowlist, Subscribe); BlueZ adapter and dynamic devices
      (ObjectManager); GameMode; mocks
- **Accept:** mock tests pass; on the host, a BT device appears and its switch works, and a
  systemd unit stays in sync.
- **Status:** done (Bluetooth checked on the KDE host on 2026-11-01):
  - Mock tests pass for every integration (9 new, including dynamic add/remove for UPower
    peripherals and BlueZ devices).
  - **systemd** was checked in the VM against the real user manager. HA's switch started and
    stopped `deskhand-test.service`, and starting or stopping it with `systemctl` was reflected
    in HA within 3 s.
  - **Power profile:** HA's select set power-profiles-daemon in the VM. The opposite direction
    is covered by the mock test; the VM has no `performance` profile to switch to.
  - **Bluetooth:** this dev host and the VM have no adapter; the user confirmed on the KDE host
    that it works (after the rfkill fix in "Host fixes").
- **Notes and deviations:**
  - Capabilities and entities:
    - `battery` (`battery_level`, `battery_state`, `battery_time_remaining` in min,
      `on_ac_power`, all only when UPower's display device is a present battery; plus dynamic
      `battery_<model>` sensors for peripherals with `kind`/`state` attributes).
    - `power_profile` select (`org.freedesktop.UPower.PowerProfiles`, falling back to
      `net.hadess.PowerProfiles`).
    - `network` (`network_connectivity` enum; `wifi_ssid` with `signal` and `interface`
      attributes, only with a Wi-Fi device).
    - `bluetooth` (`bluetooth_power` switch; dynamic `bt_<mac>` switches with `address`,
      `paired`, `battery` attributes; `devices = "paired"` or a list of addresses).
    - `systemd` (`systemd_<unit>` switches; the `.service` suffix is dropped from the id, other
      unit types keep it).
    - `gamemode` (`gamemode_active`, attribute `clients`; available when GameMode is
      installed, since it is D-Bus activated).
  - UPower, NetworkManager, BlueZ and systemd treat *any* signal from the service as "re-read
    everything" (debounced 0.25–1 s) instead of tracking individual properties. This covers
    objects appearing and disappearing. The match rule uses the service's **unique** name (zbus
    compares senders by unique name; a well-known name never matched), and a
    `NameOwnerChanged` ends the stream, so the integration restarts when the service does.
  - Dynamic entities are dropped when their object disappears and unregister after the hub's
    grace period. Bluetooth device switches abort their command-forwarding task on removal,
    so the command stream doesn't keep the entity alive.
  - Bluetooth connect/disconnect calls run in their own task, because connecting can take
    seconds.
  - `deskhand-linux` tests share a `TestRig` that remembers the latest published states.
    Waiting on single events missed values that arrived together in the startup snapshot.

### M8: Notifications and MPRIS media
- [x] notify entity; rich JSON topic; action and reply events; tag → replaces_id
- [x] `MediaSource` trait + MediaKit; MPRIS multiplexer (NameOwnerChanged, PropertiesChanged,
      Seeked); art image; compat media_player renderer and snapshot; mock MPRIS tests
- **Accept:** clicking a notification action fires an HA automation; HA controls play/pause and
  volume; the compat media_player works, including seek.
- **Status:** done, checked in the VM against the dev HA:
  - HA sent a rich notification (`mqtt.publish` to `notification/set`). Plasma showed it with
    Reply, Unlock and Ignore. Clicking Unlock fired `notification_event` (`action: unlock`,
    `tag: door`) and a test automation conditioned on it ran. Typing into the inline reply sent
    `reply: "yes"`. Each was followed by `closed`.
  - mpv (with the `mpris` script) playing an MP3 with embedded cover art: `media_state`
    showed title, artist, album and duration, and `media_art` and the compat player both
    served the exact cover bytes. HA's play/pause button and volume number controlled mpv, and
    the volume stayed in sync both ways. Through the compat player (HACS `mqtt_media_player`
    2026.10.0 installed in the dev HA), play, volume_set and seek worked: after seeking to
    120 s, mpv was at 122 s two seconds later. When mpv quit, the state went to `idle`.
  - 13 new tests: MediaKit (5, against a fake source), MPRIS and notifications against new
    `MockMpris`/`MockNotifications` (3), compat payloads, topics and discovery (4, including a
    snapshot), and a broker test for the compat player's fields and commands.
- **Notes and deviations:**
  - Capabilities and entities:
    - `media` (`media.mpris`): `media_state` enum sensor (playing/paused/stopped/idle;
      attributes player, title, artist, album, duration, position, position_updated_at,
      art_url), buttons `media_play_pause`/`media_next`/`media_previous`/`media_stop`,
      `media_volume` (0–100 % slider), `media_player` select (`auto` or a player's name, with
      ` 2` for duplicates), `media_art` image, and `media` (the compat player) only with
      `compat_mqtt_media_player = true`. Option `ignore_players` matches the player name
      (MPRIS `Identity`) or app id (`DesktopEntry`, else the bus name suffix), ignoring case.
    - `notifications` (`notifications.freedesktop`): `notify` entity, JSON endpoint
      `notification` (`notification/set`, separate from the event entity because the hub
      cannot share an id between an entity and a JSON endpoint), and `notification_event`
      (`action`/`reply`/`closed` with `tag` and `title`). The JSON also takes
      `{"tag": ..., "dismiss": true}`. Unknown JSON keys are ignored. The body is escaped when
      the server renders markup. Events are only reported for our own notifications (at most
      256 tracked).
  - `auto` follows the player that most recently started playing, then the one that played
    last, then the first. A chosen player that quits resets the select to `auto`.
  - Core: `MediaCmd`, `CommandValue::Media`, `State::Media(MediaPlayerState)` and
    `Ctx::media_player` replace the `raw` escape hatch for media players. `MediaKit` lives in
    `deskhand-common`, so `deskhand-linux` now depends on it.
  - **Compat media player:** the component reads `state_*_topic`/`command_*_topic` keys
    (renamed upstream; older docs show the unprefixed names). It allows a single availability
    topic, so it follows Deskhand's global status (LWT), not the `media` capability. Each
    field has its own retained topic (`<id>/title`, ...) and only changed fields are
    republished; the position is resent on every state change, because HA extrapolates it
    from the time it arrives. Album art is base64. Commands: `<id>/set`
    (Play/Pause/PlayPause/Next/Previous), `<id>/seek`, `<id>/set_volume`; no play_media.
    The document carries `deskhand_capability`, so stale cleanup treats it like device
    components. HA needs `mqtt_media_player:` in `configuration.yaml`, because the
    component only subscribes to discovery from `async_setup`.
  - **Cover art:** `file://` and base64 `data:` URLs only (up to 2 MiB). Remote `http(s)` art
    (Spotify, browsers) is not fetched, to avoid an HTTP client dependency. Its URL is in
    `art_url`, which is only set for remote art. Without art the image is cleared (empty
    payload), and HA then serves no picture. The image declares `image/jpeg` even for PNG
    art; browsers sniff it.
  - MPRIS: any signal on `/org/mpris/MediaPlayer2` from a player's owner marks it dirty;
    dirty players are re-read with `GetAll` after 100 ms without signals. That also refreshes
    `Position`, which MPRIS never signals. Seek uses `SetPosition` with the track id, falling
    back to a relative `Seek`.
  - Notifications register their entities and call `ready()` before `GetCapabilities`: at
    login plasmashell may still be starting, and the call blocked the startup barrier in the
    first VM run.
  - The `TestRig` from `deskhand-linux` moved to `deskhand_core::testing` (with `wait_event`,
    `state`, `attributes`, `settle`, `take_events`), so `deskhand-common` tests use it too.
  - Dev tooling: `dev/vm/click.sh X Y` clicks in a headless VM through QMP absolute tablet
    events (`just vm-headless` now opens `/tmp/plasma-vm.qmp`). HMP `mouse_move` only sends
    relative motion to the PS/2 mouse, which KWin ignores here. `dev/ha/api.sh` calls the dev
    HA REST API. The VM's mpv has the `mpris` script. Test media lives in the ignored
    `dev/vm/media/`.

### M9: Audio, camera, in-call
- [x] libpulse mainloop thread with subscriptions; AudioKit (volume, mute, device selects, mic,
      mic_in_use, headphones)
- [x] Camera inotify + `/proc` scan (250 ms settle); `in_call` derived sensor; `audio-pulse`
      feature gating. On the host, check camera use through PipeWire (spikes §7) and corked mic
      streams (§6)
- **Accept:** volume stays in sync both ways; switching output works; a browser call sets
  mic_in_use, camera_in_use and in_call, with app names.
- **Status:** done in the VM (PipeWire 1.6 pulse server, virtual null sinks and a virtual
  source); **the browser call is still to be checked on the KDE host**, together with camera
  use through PipeWire and a muted (corked) mic stream:
  - Volume and mute changed with `pactl` showed up in HA, and HA's volume, mute, mic volume and
    output select changed the sound server (the default sink moved to the chosen device).
  - A device added while running and made default showed up in `audio_output`.
  - `parecord` from the virtual mic set `mic_in_use` (`apps: ["parecord"]`); a process
    holding `/dev/video0` set `camera_in_use` (`apps: ["sleep"]`); `in_call` turned on at once
    from the camera and its `microphone` attribute after 10 s. Recording a monitor source did
    not count. Everything went off after the processes stopped.
  - 7 new tests: AudioKit (2, fake source), `in_call` timing (paused time), camera (2, fake
    `/dev` and `/proc` with real inotify), headphone detection, and a hub test for the fix below.
- **Fixed on the way (core hub):** a select's state that used a brand-new option was published
  before the debounced discovery update, so HA rejected it ("Invalid option") and the hub's
  dedupe never sent it again (`audio_output` stayed unknown in HA). Entities whose spec changes
  now get their state resent one debounce interval after the discovery update. This also
  covered the M7 power-profile select and the M8 media player select.
- **Notes and deviations:**
  - Capabilities and entities:
    - `audio` (`audio.pulse`, feature `audio-pulse`): `audio_volume` (HA name "Volume") and
      `audio_mute` for the default output, `audio_output`/`audio_input` selects (device
      descriptions, ` 2` for duplicates; monitor sources are not inputs), `mic_volume`,
      `mic_mute`, `mic_in_use` (attribute `apps`), `headphones_connected` (active port name
      contains headphone/headset, or `device.form_factor` is headphone/headset).
      `mic_ignore_apps` extends a built-in list of volume meters (pavucontrol, plasma-pa,
      kmix, GNOME's volume control), matched on application name, id or binary.
    - `camera` (`camera.proc`): `camera_in_use` with `apps` (process names) and `devices`;
      `ignore_processes` option. `/dev` is watched for hotplugged cameras.
    - `in_call` (deskhand-common): microphone in use for 10 s (released after 5 s) or camera
      in use; attributes `microphone` and `camera`.
  - The pulse thread uses the standard mainloop with 50 ms iterations (commands are checked
    between them), re-reads server info, sinks, sources and source outputs after events, and
    never autospawns a server. Volume changes keep the channel balance (`ChannelVolumes::scale`).
    Volume above 100 % is shown as is; HA's slider stops at 100.
  - `AudioKit` follows the same pattern as `MediaKit`: an `AudioSource` trait with
    `next_change` and `command`, so other OS backends get the same entity ids.

### M10: KDE desktop part 1
- [x] KWin script integration (active window, fullscreen, display count, reload on KWin restart);
      also connect `captionChanged`/`fullScreenChanged` of the active window (spikes §2)
- [x] Virtual desktop select; night light; DnD (read `Inhibited`, write `plasmanotifyrc` with
      KConfig notify; spikes §4); accent colour and dark mode (portal read in linux, write in
      kde); mocks
- **Accept (VM):** entities reflect UI changes within 1 s and HA commands change the desktop.
- **Status:** done, checked in the VM (Plasma 6.6.6) against the dev HA, each within 1 s
  unless noted:
  - Active window: Konsole and mpv were reported with app id and title. mpv `--fs` set
    `fullscreen`, and closing it returned to Konsole with fullscreen off. A Konsole title change
    (`cd /tmp`) arrived after about 2 s, because Konsole updates its title on a timer.
  - Virtual desktops: switching from HA changed KWin's current desktop, and switching in
    KWin updated HA. A desktop added in KWin appears in the options after the 1 s discovery
    debounce.
  - DnD: HA on wrote `Until=2100,…` and plasmashell's `Inhibited` followed; off deleted it.
    Plasma's own "toggle do not disturb" shortcut was reflected both ways.
  - Night light: HA's suspend switch set and cleared KWin's `inhibited`.
  - Dark mode: HA switched to BreezeDark (`plasma-apply-colorscheme` takes about 2–3 s), and
    a scheme change on the desktop updated HA. `plasma-apply-colorscheme --accent-color`
    updated `accent_colour` through the portal.
  - 12 new tests: KDE crate 10 (active window with a forged-call check and the title option,
    desktops, night light, DnD write and `Inhibit` fallback, scheme names, tool lookup) and
    portal appearance 2, against a new `MockKWin` (Scripting, VirtualDesktopManager,
    NightLight), `MockPortal`, and DnD support in `MockNotifications`.
- **Fixed on the way:**
  - NixOS `systemd.user.services` units get a minimal `PATH` (coreutils, grep, systemd), so
    `kwriteconfig6` and `plasma-apply-colorscheme` were not found and DnD silently fell back
    to `Inhibit`. Tools are now looked up in `PATH`, then `/run/current-system/sw/bin`, the
    per-user and Nix profiles, and `/usr/bin`. (Home Manager units inherit the user
    manager's full `PATH`, but Deskhand should not depend on that.)
  - KWin's `VirtualDesktopManager.desktops` is `a(uss)` on the wire although its
    introspection says `a(iss)`; both are accepted, and the mock sends `a(uss)`.
- **Notes and deviations:**
  - Capabilities and entities:
    - `active_window` (`active_window.kwin`): `active_window` (app id: desktop file name, else
      window class; `none` without a window; attributes `title` unless `publish_title =
      false`, `app_id`, `resource_class`, `pid`, `output`; coalesced to one update per
      500 ms), `fullscreen`, `display_count`. The lock screen is reported as `kwin_wayland`
      (KWin draws it).
    - `virtual_desktop` (`virtual_desktop.kwin`): select by name (` 2` for duplicates),
      attributes `number` and `count`.
    - `night_light` (`night_light.kwin`): `night_light` (running; attributes `enabled`,
      `mode`, `target_temperature`), `night_light_temperature` (K), `night_light_inhibit`
      switch (HA name "Suspend night light"). Plasma 6.6 has no `activateManual`, so there is
      no "force on" switch. Turning the switch off only ends Deskhand's own inhibition.
    - `dnd` (`dnd.plasma`): switch "Do not disturb". On writes a far-future date (2100)
      rather than now + 1 year as the tray does.
    - `appearance` (`appearance.kde` on Plasma, else `appearance.portal` in deskhand-linux,
      read-only with `dark_mode` as a binary sensor): `dark_mode`, `accent_colour` (`#rrggbb`,
      attribute `rgb`). Options `light_scheme`/`dark_scheme` (default BreezeLight/BreezeDark).
      The plan's `dark_mode.light/dark` options became `[integrations.appearance]`.
  - Every KDE candidate probes unavailable outside Plasma (`XDG_CURRENT_DESKTOP` without
    `KDE`) or without KWin, and they are registered before the Linux ones, so `appearance.kde`
    wins over `appearance.portal`.
  - The KWin script calls back on Deskhand's unique bus name (no well-known name needed), at
    `/io/github/ryanmwright/Deskhand/KWin`; the callback rejects any sender that is not the
    current owner of `org.kde.KWin`. A KWin restart ends the run, and the restart reloads the
    script. A leftover script of the same plugin name is unloaded first, and the script is
    unloaded on stop.
  - `deskhand_linux::util` is public (signal helpers, now with a path filter) for backend
    crates.
  - VM: Plasma auto-locks after 5 min idle, which makes the active window `kwin_wayland`.
    `sudo loginctl unlock-sessions` unlocks (polkit refuses it from ssh without sudo).

### M11: KDE desktop part 2
- [x] kglobalaccel custom shortcuts → events; invoke-allowlist buttons. Use a **fixed component
      name `"deskhand"`, not `APP_ID`**, so renaming the app id never orphans the user's key bindings.
      **Encode each key sequence as exactly four ints** (unit-tested); a short array aborts KWin
      (spikes §3)
- [x] ScreenBrightness dynamic numbers; DPMS buttons (or a switch if `org_kde_kwin_dpms` reports
      the state); Plasma logout; opt-in output enable/disable
- **Accept:** a shortcut fires an HA automation and appears in System Settings; brightness works.
- **Status:** done, checked in the VM (Plasma 6.6.6) against the dev HA:
  - A real Meta+Shift+F12 press (QEMU `sendkey`) fired `event.…_toggle_lights`, and an HA
    automation on it ran. System Settings → Shortcuts lists "Deskhand" under System Services
    with "Toggle lights", default Meta+Shift+F12. The spike's leftover `probe` action was removed
    on start.
  - Brightness: HA 40 % set PowerDevil's `display0` to 4000/10000, and a change made over D-Bus
    (7500) showed as 75 % in HA. QEMU's monitor shows up as a display ("QEMU Monitor"). The user
    confirmed brightness on the KDE host's real monitor.
  - DPMS: the spike probe (`examples/dpms_probe.rs`) showed that the protocol reports and sets
    the mode, so it is a switch. HA's switch turned the screen off and on, a key press woke it and
    HA followed, and screens PowerDevil blanked after idle showed as off.
  - HA's Overview button (invoke allowlist) opened KWin's Overview; HA's logout button ended the
    session. Disabling the VM's only output from HA was refused.
  - 19 new tests: key encoding (5, including the byte layout of `a(ai)`), shortcuts (3),
    brightness (3), DPMS (2), logout (1), outputs (3, with a fake `kscreen-doctor`), `[[shortcut]]`
    config (2). New mocks: `MockKGlobalAccel` (rejects key sequences that are not four ints),
    `MockScreenBrightness`, `MockShutdown`.
- **Caught by the mocks:** a `[&str; 4]` action id serialises as the struct `(ssss)`, not `as`;
  it must be a `Vec`/slice.
- **Notes and deviations:**
  - Capabilities and entities:
    - `shortcuts` (`shortcuts.kglobalaccel`): `shortcut_<id>` events (`pressed`) for
      `[[shortcut]]` (`id`, `name`, `default_keys` as a string or list, chords separated by
      `", "`; parsed in `deskhand-common`, keys checked at config load), and
      `kde_shortcut_<id>` buttons for `[integrations.shortcuts] invoke = [{component, action,
      name?, id?}]` (id defaults to the slugified action). The plan's `kde_shortcuts.invoke`
      became `shortcuts.invoke` (one capability). Actions of the `deskhand` component that are not
      in the config are unregistered **before** registering, because a stale action can hold the
      keys a new one wants.
    - `brightness` (`brightness.powerdevil`, `org.kde.ScreenBrightness`, Plasma 6.2+):
      `brightness_<label>` (0–100 % slider; id from the display label, ` 2` for duplicates,
      since PowerDevil's `display<N>` names depend on detection order), attributes `display`,
      `internal`. Sets with `SetBrightness(value, SuppressIndicator)`.
    - `dpms` (`dpms.kwin` on its own Wayland thread; fallback `dpms.powerdevil`): `screens_on`
      switch (HA name "Screens"; on while any output is on; attribute `outputs` with each
      output's mode), or a `screens_off` button.
    - `logout` (`logout.plasma`): `logout` button through `org.kde.Shutdown.logout` (no prompt),
      only when `"logout"` is in `[integrations.power] buttons`.
    - `outputs` (`outputs.kscreen`, opt-in with `[integrations.outputs] enabled = true`):
      `output_<name>` switches through `kscreen-doctor output.<id>.enable|disable`, read with
      `kscreen-doctor -j` every 10 s and after commands. The last enabled output is never
      disabled.
  - The binary passes `KdeSettings` (shortcuts, invoke list, logout, outputs opt-in) to
    `deskhand_kde::candidates`, because a probe cannot read the config.
  - kglobalaccel keeps a binding the user (or an earlier run) saved, even an empty one, over the
    default keys. If the default keys are taken when a shortcut is first registered, it stays
    unbound until the user binds it (warned in the log).
  - `just vm-ssh` now quotes its arguments; before, `just vm-ssh 'a; b'` ran `b` on the host.

### Follow-ups from the KDE host (V1, do after M11)
- [x] **Per-app volume for players that ignore MPRIS volume.** Chromium-based browsers (Brave,
      Chrome, Edge, Vivaldi; bus name `org.mpris.MediaPlayer2.chromium.instance<N>`) report
      `Volume` 1.0 and ignore `Set` (Chromium's `DbusProperties::OnSetProperty` is
      `NOTIMPLEMENTED`); since the host fixes `media_volume` is unknown for them. Real fix:
      when the active player has no usable MPRIS volume, read and set the volume of its
      PulseAudio/PipeWire stream (sink-input), matched by application name, binary or
      process. Connects the `media` and `audio` integrations (a shared lookup in
      `deskhand-linux`, or an `AudioSource` command for stream volume). Pulled forward from the
      post-V1 "per-app volume" item.
- **Status:** done, checked in the VM with Chromium 156 (from the binary cache, run from the
  shared store) playing an mp3. Its MPRIS name belongs to the browser process (4302), and its
  sink-input to a child process (4470). HA's `media_volume` showed the stream's 100 %, setting
  40 % in HA set the sink-input to 40 %, and `pactl set-sink-input-volume` 70 % showed as 70 % in
  HA.
  - `deskhand_linux::streams::Streams` (in `LinuxEnv`) is the shared lookup. The pulse thread
    also subscribes to sink-inputs and publishes them (`app`, `binary`, `pid`, volume, mute), and
    attaches a setter that sends stream-volume commands to the thread (channel balance kept).
    Detached when the audio integration stops.
  - `media` uses the streams for any player without a usable MPRIS volume: those of its process
    or of a child (up to 8 levels, via `/proc/<pid>/status`), else by name (`application.name` =
    MPRIS `Identity`, or binary = app id) for sandboxed apps whose pids differ. The volume shown
    is the loudest matching stream; setting it sets them all. Without the audio integration the
    volume stays unknown, as before.
  - 4 new tests: stream matching, the registry, ancestors, and the Chromium MPRIS test now
    driving the volume through a stream.

### M12: Setup wizard and tray
- [x] `deskhand setup`: broker discovery in this order: mDNS `_mqtt._tcp`, then
      `_home-assistant._tcp` host on port 1883, then `homeassistant.local`, then localhost.
      Credentials and TLS; connection test with readable CONNACK errors; atomic 0600 write
- [x] Install, enable and start the systemd user unit (`PartOf`/`After`/`WantedBy=graphical-session.target`,
      `Type=notify`, `Restart=on-failure`); if the binary is in /nix/store, point to the
      home-manager module instead
- [x] ksni tray (status, Pause, open config, quit; only when a StatusNotifierWatcher exists);
      polish doctor output
- **Accept:** setup on a fresh VM user ends with a running service and the device in HA; Pause
  makes the device unavailable.
- **Status:** done, checked in the VM as `dev` with no Deskhand config or unit (the
  `deskhand-dev` service stopped, since it has the same node id):
  - The interactive wizard (driven with `expect` over `ssh -tt`): manual host 10.0.2.2, a wrong
    password first ("not authorised", then "Change the settings" with the answers pre-filled),
    then the right one, "✓ Connected to the broker", config written, service installed and
    started. The config was 0600 in a 0700 directory, the password in its own 0600 file.
    `deskhand doctor` showed the service active and the daemon online, and HA had live states.
  - Tray: the icon showed in Plasma's tray; its menu showed "plasma-vm: Connected", Pause, Open
    config file, Quit. Pause published `offline` (entities unavailable in HA, except the
    always-available `session_state`) and switched the icon; unpausing brought it back. Quit
    stopped the daemon with status 0, and systemd left it stopped.
  - 6 new tests: setup (config round trip with permissions and backup, TLS/anonymous rendering,
    candidate order, unit file and Nix-store detection), a CLI test of `setup --yes` (failed
    connection test, `--skip-test`, `--force`, doctor reads the result), the tray icon mapping,
    plus hub status assertions.
- **Notes and deviations:**
  - `setup --yes` runs without prompts from flags (`--host`, `--port`, `--username`,
    `--password-stdin`, `--tls`, `--ca-file`, `--name`, `--no-service`, `--skip-test`,
    `--force`). An existing config is replaced only after confirmation or with `--force`, and
    kept as `config.toml.bak`. The config holds `password_file` next to it, never the password.
    The device name is written only if it differs from the default.
  - mDNS browses for 3 s. A found host name is used if it accepts TCP here, else its address.
    Reachable candidates are listed first.
  - The unit gets `--config` only when setup ran with an explicit `--config`. A unit that is a
    symlink into the Nix store (Home Manager) is never replaced.
  - Core: `HubHandle::status()` (a watch of `HubStatus { connected, paused }`) feeds the tray.
  - The tray shows when `[general] tray = true` and ksni can register with a
    StatusNotifierWatcher; otherwise it logs and the daemon runs without it. Icons are theme
    names (`user-online`, `user-away` when paused, `user-offline`); our own icon comes with the
    `.desktop` file in M13.
  - Doctor: shows the `deskhand.service` state (with a hint when not installed or failed),
    suggests `deskhand setup` when the config is missing, and no longer repeats the underlying
    error (`ConfigError` keeps it in the message instead of `source()`). Mosquitto answers a
    wrong password with "not authorised", so that message now mentions the password.
  - `dev/vm/click.sh X Y right` right-clicks (tray menus).

### M13: Nix packaging and VM e2e test
- [x] `packages.default` (buildRustPackage, libpulse, .desktop file, unit template)
- [x] `homeManagerModules.default` (`services.deskhand.{enable,package,settings,passwordFile,tray}`).
      **Base module done early (after M9):** everything except `tray` (M12); see the note below
- [x] `checks.kde-e2e` (runNixOSTest) plus a nightly CI job
- **Accept:** `nix build` and `nix flake check` pass; the HM module gives a working service with
  the password outside the store.
- **Status:** done. `nix build` and every check pass locally (each built: `deskhand`,
  `cargo-fmt`, `cargo-clippy`, `cargo-test`, `cargo-deny`, `hm-module`, `kde-e2e`), and
  `nix flake check --no-build --all-systems` evaluates for x86_64 and aarch64. The HM module's
  service and password handling were checked in the VM in M9's follow-up (see below).
  - `kde-e2e` boots Plasma 6 (SDDM autologin), runs the packaged daemon as a user service
    against a local Mosquitto and passes in about 2.5 min of test script: discovery has
    `session_state`, `screen_locked` and `active_window`; `deskhand doctor` sees the daemon
    online; `loginctl lock-sessions` publishes `screen_locked=ON` and `session_state=locked`, and
    unlocking publishes `OFF`; a message on `notify/set` reaches `org.freedesktop.Notifications`
    (seen with `dbus-monitor`). It saves a screenshot.
- **Decisions (with the user):** crane instead of `buildRustPackage`; desktop file, icon, user
  unit, completions and man page in the package; a `features` override instead of a separate
  minimal package; tests as crane flake checks.
- **Notes and deviations:**
  - `nix/package.nix` (crane, `callPackage`): dependencies are one cached derivation
    (`buildDepsOnly`), so a code change only rebuilds the workspace crates.
    `deskhand.override { features = [ "kde" "tray" ]; }` builds a variant; without `audio-pulse`
    it has no libpulse (checked: no libpulse in `ldd`, no `setup` command).
  - The package installs `share/applications/io.github.ryanmwright.Deskhand.desktop`
    (`NoDisplay`, since it is a daemon), `share/icons/hicolor/scalable/apps/<APP_ID>.svg` (new
    icon: a monitor with a house on HA blue), `share/systemd/user/deskhand.service` (for NixOS
    `systemd.packages`), bash/zsh/fish completions and `deskhand.1`. The completions and man page
    come from hidden `deskhand completions <shell>` and `deskhand man` subcommands
    (`clap_complete`, `clap_mangen`).
  - The tray uses the app icon with a status overlay (`media-playback-paused`,
    `network-disconnect`) when the icon is installed, else the theme status icons as in M12.
  - Checks: `cargo-test` runs the unit and D-Bus tests in the sandbox (`dbus-daemon` from
    `nativeCheckInputs`; `CI=1` makes a missing daemon fail instead of skip). `cargo-deny` checks
    bans, licenses and sources only (advisories need the network; the CI job checks them).
  - HM module: `tray` (default true) sets `general.tray`; an explicit `settings.general.tray`
    wins. The check asserts both values.
  - CI: `.github/workflows/nightly.yml` builds `checks.x86_64-linux.kde-e2e` at 03:17 UTC and on
    demand (KVM opened for the build user, disk space freed first) and uploads the screenshot.
    The push CI still only evaluates the checks. `just e2e` and `just flake-check` run them
    locally.
  - In the e2e test Plasma's locker sometimes misses the first `unlock-sessions`; the test
    repeats it.
  - Disk: `builtins.getFlake (toString ./.)` copies the whole directory, including the 28 GB
    `target/`, into the store and filled the disk; use `git+file://` (tracked files only) or
    `.#` references. Deleting `target/debug/incremental` (13 GB) freed space.
  - The binary-cache CI decision below is still open.

- **Base Home Manager module (pulled forward after M9, so the user can run Deskhand on the KDE
  host):** `nix/hm-module.nix`, exported as `homeManagerModules.default` and
  `homeModules.default`. Options: `enable`, `package` (defaults to this flake's package),
  `settings` (freeform TOML, so later config keys need no module change) and `passwordFile`
  (sets `mqtt.password_file`). Assertions reject `settings.mqtt.password` (it would land in the
  store) and require `mqtt.host`. The config is generated into the store and also linked to
  `~/.config/deskhand/config.toml`, so `deskhand doctor` finds it. The user unit is
  `Type=notify`, `PartOf`/`After`/`WantedBy=graphical-session.target`, `Restart=on-failure`.
  - `checks.<system>.hm-module` (`nix/hm-module-check.nix`) evaluates the module against a stub
    of the Home Manager options it sets (no home-manager input) and checks the rendered config,
    the unit and the password assertion.
  - Checked with real Home Manager (release-26.05) in a throwaway flake for the VM's `dev`
    user. HM activation cannot run in the VM (the guest's Nix does not know host-built store
    paths), so the generated unit, wants link and config were linked in by hand. The service
    reached `active (running)` through the notify handshake, read the password from
    `passwordFile`, and HA showed the device under the module's name. `deskhand doctor` with
    no `--config` used the linked config.
  - Still in M13: `tray`, the `.desktop` file and icon, and the `kde-e2e` NixOS test.
  - **Binary cache CI (decide in M13):** the user's ncps cache is internal only, so GitHub-hosted
    CI cannot push to it. Preferred direction: mirror the GitHub repo into the user's Forgejo
    (pull mirror) and build there with an instance-level Forgejo runner on a NixOS host whose
    `post-build-hook` pushes to ncps (`allow-put-verb`, builds signed with the user's key).
    Current Forgejo source turns a pull-mirror sync into a `push` event for Actions
    (`SyncPushCommits`); it runs on the mirror interval, needs Actions enabled on the mirror,
    and never sees `pull_request`. Verify on the user's Forgejo version first. Alternatives: a
    self-hosted GitHub runner (per-repo for personal accounts; restrict to `push` on `main`,
    since the repo is public), or buildbot-nix. Also consider crane so updates only rebuild
    workspace crates.
  - Disk: a second release build of the package in a flake with a different nixpkgs pin
    filled the disk (17 GB free before). Make test flakes follow `deskhand/nixpkgs` so they
    reuse the package.

### M14: Docs and release
- [x] README (features, quick start), `docs/configuration.md`, `docs/entities.md`,
      `docs/home-assistant.md` (automations: lights off on lock or sleep, meeting light from
      `in_call`, pause media when locked, notify when a job finishes, shortcut triggers)
- [x] `blueprints/` (actionable notification, idle → lights, meeting light); `SECURITY.md`
      (broker ACL per node, unlock risk, title privacy, command allowlist); `CONTRIBUTING.md`
      (adding an integration or backend); release workflow
- **Accept:** a new user can go from README to a working setup using only the docs.
- **Status:** done. Checked by following the README in the VM as `dev` (no Deskhand config,
  dev service stopped): the tarball from the new packaging script, the documented `install` and
  `cp -r share/. ~/.local/share/` steps, `deskhand setup`, then the device in HA with live
  states, and locking the screen changed "Session state". The tray showed the installed app
  icon. Found on the way: `~/.local/bin` is not on `PATH` everywhere (NixOS), now said in the
  README.
  - Every automation in `docs/home-assistant.md` was loaded into the dev HA with the VM's
    entity ids and all nine came up enabled (schema-valid). All three blueprints load. The
    actionable-notification blueprint was run for real: HA showed "Doorbell" with Unlock/Ignore
    in Plasma, and clicking Unlock ran the first button's actions.
  - Every relative link and anchor in the new docs resolves; actionlint and shellcheck pass.
- **Notes and deviations:**
  - Docs: `README.md`, `docs/configuration.md` (every option, from the option structs),
    `docs/entities.md` (from the VM's `print-discovery` plus the code for hardware the VM lacks),
    `docs/home-assistant.md` (broker, entity ids, automations, actionable notifications, shortcut
    triggers, media player card, uninstall, troubleshooting), `SECURITY.md` (vulnerability
    reports through GitHub's private advisories, allowlists, unlock, broker ACL per node,
    credentials, privacy), `CONTRIBUTING.md` (dev setup, architecture, adding an integration
    with a skeleton, adding a backend), `CHANGELOG.md`.
  - Blueprints in `blueprints/automation/deskhand/`: `meeting_light`, `idle_lights` (lights
    follow `session_state`), `actionable_notification`. Plasma sends "closed" right after an
    action, so the actionable blueprint handles the event entity's events in order in a loop;
    a template trigger that looked at the latest event saw only "closed" and missed the click.
  - Release: `.github/scripts/package.sh` builds the tarball (binary, desktop file, icon,
    completions, man page, README, CHANGELOG, licenses) for CI's per-push artifact and for
    `.github/workflows/release.yml`. On a `v*` tag that workflow checks the tag against the
    workspace version and CHANGELOG, builds x86_64 and aarch64 (`ubuntu-22.04-arm`) tarballs,
    and creates a **draft** release with the version's CHANGELOG section. CI no longer attaches
    to releases itself.
  - `deskhand setup` with a Nix binary now uses a profile link that resolves to it
    (`~/.nix-profile/bin/deskhand`, the per-user or system profile), so `nix profile install`
    users get a service that survives upgrades; only a bare store path gets the Home Manager
    hint.
  - Dev: the VM can keep stale 9p pages of a rebuilt `target/debug/deskhand` while the old one
    runs, and new processes then segfault (`ld.so` assertion); `just vm-deploy` now stops the
    service and drops the guest's caches before starting it.

### M15 (post-V1): Headless mode and herdr
Context (2026-11-02, with the user): the user runs coding agents in
[herdr](https://github.com/herdrdev/herdr) on headless dev VMs and attaches with
`herdr --remote <vm>` from more than one client machine. The herdr server, and so the agents'
state, lives on each VM. So Deskhand runs **on each VM** (one HA device per VM, the same no
matter which client is attached); a desktop-side herdr integration only matters for agents
run locally.

- [ ] **Headless mode.** A user unit wanted by `default.target` (not
      `graphical-session.target`), plus `loginctl enable-linger` so it runs without a login:
      `deskhand setup --headless` (asks to enable lingering; polkit may need sudo) and the HM
      module (`services.deskhand.headless`).
- [ ] **Headless defaults.** `doctor` on the (non-KDE) dev host, outside a graphical session,
      showed that probing alone is not enough: besides metrics, `lock` and `idle` (logind, would
      follow ssh sessions), `power`, `keep_awake`, `session_state`, `in_call`, `media` (MPRIS only
      needs a session bus, which lingering provides), `camera` (only needs `/proc` and `/dev`),
      and, where a notification daemon or sound server runs, `notifications` and `audio` all
      start. With `[general] headless = true` (set by `setup --headless` and the HM option), these
      session/desktop capabilities and the KDE ones default to off instead of `auto`;
      `enabled = true` still turns one on (e.g. `power` for VM reboot buttons). Expected result
      on a VM: deskhand info, metrics, herdr, and configured systemd units, scripts and sensors.
      Consider a build with `--no-default-features --features linux,herdr` (no libpulse, no
      tray) for VMs.
- [ ] **`herdr` integration** (platform-neutral, `deskhand-common` or its own crate, feature
      `herdr`). herdr's socket API (docs: herdr.dev/docs/socket-api/):
      - Socket `~/.config/herdr/herdr.sock` (or `sessions/<name>/herdr.sock`; resolution
        `--session`, `HERDR_SOCKET_PATH`, `HERDR_SESSION`, default). Newline-delimited JSON:
        `{"id":"req_1","method":"ping","params":{}}` → `{"id":"req_1","result":{...}}` or
        `{"id":...,"error":{"code":...,"message":...}}`.
      - Push: `events.subscribe` with `subscriptions: [{"type": "pane.agent_status_changed", ...}]`
        streams events after an ack. No replay; a slow subscriber gets `events_lost` and is
        disconnected. Events are invalidation signals: re-read with `session.snapshot` (or
        `agent.list` / `pane.list`). Other events: `pane.agent_detected`, `pane.created`,
        `pane.closed`, `pane.exited`, `pane.updated`, `pane.output_matched`.
      - Agent status per pane: `idle`, `working`, `blocked` (needs the user), `done` (idle and
        not yet seen), `unknown`.
      - CLI: `herdr status`, `herdr api snapshot`, `herdr api schema --json` (read this first:
        exact event payloads, what a subscription without `pane_id` means, workspace/agent
        names).
      - Entities (draft): `herdr_working`, `herdr_blocked` count sensors (attribute: the agents,
        with workspace and pane); a `herdr_agent` event entity firing `started`, `blocked`, `done`
        (and maybe `exited`) with pane, agent and workspace attributes, for notification
        automations; maybe a status sensor per pane as dynamic entities (like Bluetooth devices).
        Probe: the socket answers `ping`. Reconnect when the herdr server restarts.
      - Same pattern as MPRIS and BlueZ: subscribe, re-read the snapshot on any event, diff, fire
        events for transitions.
- [ ] Docs (entities, configuration, home-assistant: "notify my phone when an agent is
      blocked"), a blueprint if it is worth it.
- **Accept:** on a headless machine, Deskhand starts without a login, the device shows herdr
  agents, and an HA automation notifies when an agent becomes `blocked` and when it is `done`.
- herdr 0.9.1 is installed on the dev host (`devenv`), with a live server socket, so the
  integration can be tested against the real thing. Don't disturb the user's running sessions:
  use a separate `--session` for tests.

### M16–M19 (post-V1): Logitech HID++ peripheral batteries
Context (2026-11-04, with the user): an MX Keys S on a Logi Bolt receiver does not show up in
Deskhand, Plasma or `upower -d`. The kernel exposes no battery for it, probably because the Bolt
receiver is bound to `hid-generic` (M16 confirms). Logitech's HID++ protocol reaches the device
from user space over `/dev/hidrawN`. The design is a generic peripheral battery model that HID++
feeds as a *supplemental* source next to UPower. It is not specific to the MX Keys S: the MX Keys
Mini, MX Master 3S and MX Anywhere work through the same HID++ features. M16–M19 do not depend on
M15.

**Requirements (user):**
- HID++ is a supplemental source.
- Bolt (and Unifying) receiver discovery and paired-device enumeration.
- Battery features: Unified Battery 0x1004, falling back to 0x1000 and 0x1001 (estimated %).
- States: charging, discharging, full and unknown.
- Connection status and the time of the last successful reading.
- Identity: model ID, WPID, serial and firmware, plus the receiver and slot.
- No duplicate entities; keep the `battery_<model>…` naming scheme.
- Graceful handling of hotplug, sleeping devices and missing receivers.
- No unnecessary polling.
- UPower and HID++ availability independent of each other, and no competing `battery` integration.
- Nothing Logitech-specific in core.
- Backlight is the only control, and it is opt-in.

**Decisions:**
- **Own HA device per peripheral and per receiver** (`via_device`: peripheral → receiver → desktop).
  - The user accepts the one-time move of today's UPower `battery_<model>` sensors onto these
    devices, while the user base is tiny.
  - Object ids keep the `battery_<model>…` scheme.
- **New integration** `peripheral_battery.merged` (capability `peripheral_battery`).
  - It owns all peripheral battery entities and merges *sources*: UPower peripherals and HID++.
    Each is optional, with its own backoff loop.
  - `battery.upower` keeps only the laptop battery and AC.
  - Why: feeding HID++ through the UPower integration would make HID++ need UPower, and a second
    `battery` integration would make the supervisor pick only one of the two.
- **Protocol:** [`openlogi-hidpp`](https://crates.io/crates/openlogi-hidpp) (lib name `hidpp`).
  - It is OpenLogi's fork of `hidpp`: 0BSD, runtime-agnostic, with HID I/O through its
    `RawHidChannel` trait. It covers Bolt and Unifying receivers and every feature listed here,
    including the future ones.
  - Solaar is GPL Python, so it is used as a behaviour reference only.
  - Don't use `openlogi-hid` (OpenLogi's app layer over a forked `async-hid`).
  - Pin it exactly (69 releases in its first 5 months) and keep all use behind one adapter
    module. Vendoring is the fallback.
  - It needs **Rust 1.99**. Take the toolchain from a second, nixos-unstable nixpkgs input until
    nixos-26.11, and add `0BSD` to the licence allowlist in `deny.toml`.

**Entities per peripheral (on its own device):**
- `battery_<model>`: %. Attributes `kind`, `state`, `source`, `level`, `estimated`, `slot`,
  `encrypted`.
- `battery_<model>_state`: enum in UPower's vocabulary.
- `battery_<model>_low`: binary sensor, battery class. The device's flag or level, else ≤ 15 %.
- `battery_<model>_connected`: binary sensor, connectivity class.
- `battery_<model>_updated`: diagnostic timestamp of the last reading.
- Model, model ID (WPID), serial and firmware go into HA's device fields.
- A receiver device has a diagnostic `paired_devices` sensor (slot → name, WPID, online).
- A sleeping device keeps its last values; only `connected` turns off.

**Merging:**
- Sources report readings keyed like today's `peripheral_from` (model, else serial, else path).
  If the model slug differs between sources, the key falls back to the serial.
- HID++ wins while it has a link or a fresh reading. Otherwise UPower wins, which covers Unifying
  devices the kernel exposes as `hidpp_battery_*`.
- An entity is removed only when no source knows the device. Removal uses the hub's 30 s grace
  period.

#### M16: HID++ spike on the KDE host
- [x] Rust 1.99 toolchain (second nixpkgs input), `openlogi-hidpp` pinned, `0BSD` in `deny.toml`.
- [x] `crates/deskhand-linux/examples/hidpp_probe.rs`:
      - Lists hidraw nodes with vendor, product, bound driver and permissions.
      - Dumps the receiver (unique ID, firmware, pairings, device arrival).
      - Per device: the feature table, identity (0x0003/0x0005), battery (0x1004/0x1000/0x1001)
        and backlight (0x1982).
      - Then logs every report for 120 s. Meanwhile the user lets the keyboard sleep, wakes it,
        plugs in USB-C, changes the backlight, and unplugs and replugs the receiver.
- [x] The user runs it from a CI artifact (`docs/dev/deploy-to-host.md`). This needs read and
      write access to the hidraw node, so a temporary udev rule or `sudo`. Results go in
      `docs/dev/spikes.md`, and the raw captures are saved as test fixtures.
- **Settles:**
  - Which driver is bound to the Bolt receiver.
  - What a sleeping device looks like (link-down notification vs. error vs. timeout).
  - Whether 0x1004 sends an event for every percentage change, which decides `battery_refresh`.
  - Whether `openlogi-hidpp` handles the MX Keys S correctly.
- **Status:** done (2026-11-04). Two runs on the KDE host; results in `docs/dev/spikes.md` §9,
  Bolt traffic in `crates/deskhand-linux/tests/fixtures/hidpp/` (decoded by
  `tests/hidpp_fixtures.rs`).
  - The Bolt receiver is bound to `hid-generic`. `openlogi-hidpp` handles it and the MX Keys S
    correctly: pairings, codename, serial, firmware, 0x1004 and 0x1982, with events.
  - **No polling needed:** charging changes and backlight key presses are events. After about
    10 minutes untouched the keyboard drops its link (0x41 offline). On the next key press it
    reconnects (0x41 online, then 0x1d4b) and sends a 0x1004 report by itself.
    `battery_refresh` defaults to `off`.
  - Decisions for M18:
    - on link-up, use the unasked 0x1004 report and only read if none arrives within a few
      seconds;
    - match replies by software ID (Solaar was talking on the same node);
    - leave the receiver's notification flags set on exit (Solaar relies on them too);
    - skip `logitech-hidpp-device` nodes (the kernel owns them and UPower reports them);
    - log only HID++ reports.
  - The receiver replug was only seen indirectly (writes to the old node fail). The M19 host
    check covers it.
  - The toolchain comes from the `nixpkgs-rust` flake input (nixos-unstable, Rust 1.99) for the
    devShell and crane. Drop that input once nixos-26.11 is out. `0BSD` was already allowed.
  - `deskhand_linux::hidraw` (feature `hidpp`) was pulled forward from M18:
    - sysfs enumeration (bus, vendor:product, name, uniq, bound driver);
    - a `RawHidChannel` over the node with `poll(2)` and plain read/write, so no unsafe code and
      no C library.
    - `hidpp` runs its read loop on its own thread and joins it on drop, so a read waits at most
      100 ms and then yields. A test drives `HidppChannel` over a socketpair to check this.
  - The CI `release-binary` job also uploads `hidpp-probe-x86_64-linux-gnu` (the probe plus the
    udev rule). The rule, `data/udev/70-deskhand-logitech.rules`, is the one M18 packages.
  - Found in the crate: 0x1000 (battery status) has no event source in `openlogi-hidpp`, unlike
    0x1004, 0x1982 and 0x1d4b. M18 has to decode its broadcast itself, and the capture is
    needed for that.

#### M17: Sub-devices and the generic peripheral model
- [x] Core sub-devices.
      - `ctx.device(DeviceSpec) -> DeviceHandle`, with
        `DeviceSpec { key, name, manufacturer, model, model_id, serial_number, sw_version, hw_version, via }`.
      - `EntitySpec::on_device(&handle)`; entities default to the main device.
      - Updates re-render discovery through the 1 s debounce.
      - A device without entities or handles is removed after the 30 s grace period.
      - Generic: nothing Logitech-specific.
- [x] MQTT.
      - One retained document per device at `homeassistant/device/deskhand_<node>_<key>/config`,
        with `via_device` and the main device's availability topics.
      - Stale cleanup reads the retained `deskhand_<node>_*` documents and clears the ones that
        are gone.
      - `uninstall` and `print-discovery` cover sub-devices.
      - insta snapshots and a broker test for stale-device removal.
- [x] `deskhand-common/src/peripherals.rs`.
      - `PeripheralSource` (`id`, `probe`, `run(sink)`) and `PeripheralSink`. The sink takes
        readings and removals and lets a source register extra entities for a peripheral it owns.
      - Merging, devices and entities, and the `PeripheralBattery` integration.
      - Probe: available if any source is.
      - Per-source backoff (1 s → 60 s), re-probing on resume and every 5 min.
      - `ready()` after the first report from each source, or after 5 s.
- [x] Config: `[integrations.peripheral_battery] upower = "auto"; hidpp = "auto"; controls = []`.
- [x] Split `UPowerPeripherals` out of `upower.rs`, filling `State`, `BatteryLevel`/`WarningLevel`,
      `IsPresent` and `UpdateTime`. Split the existing mock test, keeping the
      `battery_mx_master_3` assertion.
- [x] Merge tests on paused time:
      - One entity when both sources report a device, with HID++ preferred.
      - When HID++ stops, UPower takes over.
      - Each source works alone.
      - The laptop `battery` capability is unaffected.
- [x] Check in the dev HA whether moving `battery_<model>` to a sub-device keeps its entity ID and
      history. CHANGELOG: per-peripheral devices and the new `status/peripheral_battery` topic.
- **Status:** done (2026-11-04). Deviations from the items above:
  - **No device handle.** A sub-device is a `SubDevice` value on `EntitySpec::device`
    (`EntitySpec::on_device(Arc<SubDevice>)`). A device exists while it has entities, so the
    entity grace period covers it. Changed device info (firmware, a receiver link) is published
    by registering the entities again with the new `SubDevice`. New handles come first, so the
    generation check turns this into an update, not a removal.
  - **Topic:** sub-device documents use HA's optional node level,
    `homeassistant/device/deskhand_<node>/<key>/config`, so a single subscription
    (`homeassistant/device/deskhand_<node>/+/config`) reads exactly ours. Its retained copies are
    read before the main document, which ends the retained read. Unique IDs are unchanged
    (`deskhand_<node>_<entity>`). The main document is published first, so a moved entity is
    removed (stub) before it appears in its new document. A sub-device document with nothing
    left is deleted (empty retained payload), which removes the device from HA.
  - **Config:** sources go in a table, `[integrations.peripheral_battery.sources]`
    (`upower = false`). `controls` and the sink's extra entities come with their first user
    (backlight, M19).
  - **Merging:** each source's key is `slug(model)`, else the serial, else the device path (the
    0.1.x rule), so ids are unchanged; a matching serial joins two keys. The winner is the first
    source in priority order whose reading says connected, else the first. The others fill
    missing identity fields.
  - **Tests:** the merge tests run on real time, because the per-source backoff sleeps; a run
    takes about 3 s.
  - **HA check** (dev HA 2026.10.4, simulated with mosquitto_pub): an entity moved from the main
    document into a sub-device document keeps its entity ID (HA restores the ID of a re-added
    unique ID) and its history. It now sits on the new device, with a brief unavailable gap.
  - This host has no UPower running. `doctor` shows `peripheral_battery` unavailable with the
    source's reason, and `print-discovery` is unchanged without sub-devices. Real peripherals are
    checked on the KDE host in M19.

#### M18: HID++ source
- [ ] `crates/deskhand-hidpp` (platform-neutral) wrapping `hidpp` in one adapter module.
      - `HidBackend` trait (`enumerate`, `open -> RawHidChannel`, `hotplug`), so macOS IOHID and
        Windows HID can be added later.
      - `Leased` software IDs, so Solaar, OpenLogi or the kernel driver can share the node.
- [ ] Receiver task.
      - Set notification flags (`hid-generic` sets none).
      - Collect pairings (slot, WPID, kind, unit ID/serial, codename) and the receiver's unique ID
        and firmware.
      - Trigger device arrival, so link states arrive without contacting sleeping devices.
- [ ] Device on link-up.
      - Feature discovery once, then identity, then one battery read: 0x1004, else 0x1000, else
        0x1001.
      - After that, battery comes only from the device's own events.
      - Link down: `connected` turns off and the values stay.
      - A link-up or 0x1D4B reconnect triggers one re-read.
      - A sleeping device is never queried.
      - `battery_refresh` (default `off`, set by M16) re-reads only connected devices.
- [ ] Receiver gone (`ENODEV`, read error): its devices report disconnected and the source waits
      for hotplug. Unpairing removes the device.
- [ ] `deskhand-linux/src/hidraw.rs`.
      - Enumerates `/sys/class/hidraw/*/device` (vendor 046d) and picks the interface from the
        report descriptor (vendor page 0xFF00, report IDs 0x10/0x11).
      - I/O with `AsyncFd`; hotplug with inotify on `/dev`.
      - Probe reason for `EACCES` points to the udev rule.
- [ ] Wiring: feature `hidpp` (in `default` and `linux`); `doctor` shows receivers, driver,
      permissions, slots and batteries.
- [ ] `data/udev/70-deskhand-logitech.rules` (`TAG+="uaccess"` for Logitech receivers and
      HID++ Bluetooth devices; added in M16).
      - Installed to `$out/lib/udev/rules.d`; docs for NixOS `services.udev.packages` and for
        installing it by hand from the tarball.
      - The rule is a one-time root step per machine, not per install. Solaar's rules (packaged
        by Fedora, Arch and Debian/Ubuntu) do the same, so `doctor` accepts either and prints the
        install command only when a Logitech node can't be opened.
      - `deskhand setup` offers to install it (with `sudo`, after showing the command) when it
        finds a Logitech receiver it can't open.
      - `uaccess` only covers the user of the active local session. Headless mode (M15) would
        need a group-based variant (`GROUP=…, MODE="0660"`); document it there.
      - Security note: same access as Solaar.
- [ ] Tests with a `MockReceiver: RawHidChannel` and replays of the M16 captures:
      - Startup with a sleeping device.
      - Wake gives exactly one read.
      - Events arrive without requests (polling check).
      - Receiver loss and return inside the grace period.
      - Unpairing.
      - Devices with only 0x1000 or only 0x1001.
      - Sysfs fixture tree for enumeration.

#### M19: Backlight, docs, host check
- [ ] Backlight (0x1982), opt-in with `controls = ["backlight"]`.
      - `battery_<model>_backlight` switch and `_backlight_level` number.
      - Config-category numbers for the hands-out, hands-in and powered timeouts.
      - Device-side changes come back through events.
- [ ] Docs: entities, configuration (udev), and an HA example "keyboard battery low".
- [ ] A "Future HID++ controls" section:
      - Host switching (0x1814/0x1815).
      - Key and button diversion (0x1B04) to an `event` entity.
      - Fn inversion (0x40A3), disabled keys (0x4521) and OS mode (0x4531).
      - Mouse DPI (0x2201), SmartShift (0x2111), hi-res wheel (0x2121) and thumbwheel (0x2150).
      - Bluetooth-direct HID++, and BlueZ devices as sub-devices.
      - Never pairing or unpairing over MQTT.
- **Accept:** on the KDE host, with the CI artifact and udev rule:
  - `doctor` lists the Bolt receiver and the MX Keys S.
  - HA shows "MX Keys S", via the receiver, via the desktop, with model, model ID, serial,
    firmware, battery, state, low, connected and updated.
  - Sleeping turns `connected` off and keeps the value; a key press refreshes `updated`.
  - USB-C gives `charging`.
  - Replugging the receiver leaves no duplicates. This can be simulated without touching the
    hardware: set the receiver's USB `authorized` attribute to 0 for 5 s, then back to 1. Its
    hidraw nodes disappear and come back, as with a real replug.
  - With UPower stopped, the keyboard still updates.
  - The backlight works both ways.

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
| 2026-11-01 | M5 | Eight probes run in the VM on Plasma 6.6.6; all work (`docs/dev/spikes.md`). Plan changes: logind session via `User.Display`; DnD written through `plasmanotifyrc` + KConfig notify; kglobalaccel key sequences must be four ints (a short array aborted KWin); KWin script sends one JSON string. VM gained a v4l2loopback camera and mpv |
| 2026-11-01 | M6 | LinuxEnv (`User.Display` session); lock (logind/ScreenSaver, unlock gate), power (Can*-gated buttons, delay inhibitor + PrepareSleep/Sleep/Resume, power_event, lid/dock, sleep_inhibited), keep_awake, idle (Wayland v2 thread, ScreenSaver poll, logind), `session_state`. 12 new mock/unit tests and a sleep/resume broker test. Fixed: transport never sent offline/DISCONNECT on sleep; flaky uninstall broker test (CI red on the M5 push). VM checks passed except resume: the QEMU guest never wakes, so resume is covered by the broker test and still needs a check on the real host |
| 2026-11-01 | M7 | UPower battery/AC/dynamic peripheral batteries, power-profile select, NetworkManager connectivity and SSID, BlueZ adapter and dynamic device switches, systemd user-unit switches (allowlist), GameMode; mocks for all six services and 9 tests. systemd and power profile checked in the VM; Bluetooth needs a check on the KDE host (no adapter here). Signal matching uses the service's unique name, with a restart when the owner changes |
| 2026-11-01 | M8 | Notifications (notify entity, rich JSON with tag/actions/inline reply/dismiss, `notification_event`) and media (`MediaSource` + `MediaKit` in common, MPRIS multiplexer, art from file/data URLs, HACS `mqtt_media_player` compat renderer with per-field diffing). Checked in the VM: a notification action fired an HA automation, the inline reply arrived, and HA controlled mpv's play/pause, volume and seek, including through the compat player. 13 new tests, `MockMpris`/`MockNotifications`, `TestRig` moved to core. Fixed: notifications held up the startup barrier while plasmashell started. New `dev/vm/click.sh` (QMP) and `dev/ha/api.sh` |
| 2026-11-01 | M9 | AudioKit (common) + libpulse thread (volume, mute, device selects, mic, `mic_in_use` with apps, headphones), camera (inotify trigger, 250 ms settle, `/proc` scan, hotplug), `in_call` derived sensor. Checked in the VM with virtual devices: volume sync both ways, output switching, mic/camera/in_call with app names. Fixed a hub bug: a select state using a new option was rejected by HA and never resent. 7 new tests. Browser call, PipeWire camera use and corked mic still to check on the KDE host |
| 2026-11-01 | M13 (part) | Base Home Manager module pulled forward: `homeManagerModules.default` (`enable`, `package`, `settings`, `passwordFile`), a stub-based `checks.hm-module`, checked with real Home Manager in the VM. Also fixed a clippy lint that only the newer stable toolchain in CI reports (M8 push was red) |
| 2026-11-01 | M10 | KDE crate: active window via a KWin script (sender-checked callback, caption/fullscreen tracking, reload on KWin restart), virtual desktop select, night light with inhibit switch, Plasma DnD (plasmanotifyrc + KConfig notify, `Inhibit` fallback), colour scheme writer; portal `appearance` (dark mode, accent colour) in linux. All checked in the VM. Fixed: tools not found under NixOS user units' minimal PATH; KWin's desktops property has a different wire type than introspected. 12 new tests, `MockKWin`, `MockPortal` |
| 2026-11-01 | Host fixes | From the user's KDE host: (1) D-Bus re-reads waited for a quiet gap that never came on busy services (NetworkManager scanning, BlueZ, some players), so `wifi_ssid` and media entities froze; settling is now capped. (2) Bluetooth power-on failed with "Failed" after Plasma turned Bluetooth off: Plasma soft-blocks it with rfkill; Deskhand now lifts the block through `/dev/rfkill` first. (3) Chromium-based players' fixed MPRIS volume is treated as absent. (4) VLC owns two MPRIS names and showed up twice; `X` and `X.instance<N>` of one process are merged. Per-app volume added as a V1 follow-up |
| 2026-11-01 | M11 | KDE crate: kglobalaccel custom shortcuts (`[[shortcut]]` → `shortcut_<id>` events, four-int key encoding, stale actions removed first) and allowlisted invoke buttons, PowerDevil per-display brightness, DPMS `screens_on` switch over KWin's Wayland protocol (new spike probe) with a PowerDevil fallback button, opt-in logout and output switches. All checked in the VM, including a real key press driving an HA automation and the entry in System Settings. 19 new tests and three Plasma mocks; the kglobalaccel mock caught a `(ssss)`-instead-of-`as` action id. `just vm-ssh` quoting fixed |
| 2026-11-01 | Follow-up | Per-app volume: shared `Streams` registry in `LinuxEnv` fed by the pulse thread (sink-inputs, stream-volume setter); MPRIS players without a usable volume (Chromium-based) use their streams, matched by process tree, then name. Checked in the VM with real Chromium. Host feedback: Bluetooth and monitor brightness confirmed |
| 2026-11-01 | M12 | `deskhand setup` (mDNS/fallback broker discovery, credentials and TLS, connection test with retry, atomic 0600 config plus password file, systemd user unit or a Home Manager hint; `--yes` for scripts), ksni tray (status, Pause, open config, Quit) over a new `HubHandle::status()` watch, doctor shows the service state. Checked in the VM: interactive setup to a running service and the device in HA; Pause/unpause/Quit from the tray. 6 new tests |
| 2026-11-01 | M13 | Crane package (cached deps, `features` override) with desktop file, icon, user unit, completions and man page; flake checks for fmt, clippy, tests (D-Bus tests in the sandbox) and deny; HM `tray` option; `kde-e2e` NixOS test (Plasma, Mosquitto, packaged daemon: discovery, lock state, notification) passing, nightly workflow. Tray uses the app icon when installed. Disk filled once (getFlake on a path copies `target/`) |
| 2026-11-02 | M14 | README, configuration, entities and Home Assistant docs, SECURITY, CONTRIBUTING, CHANGELOG; three blueprints; packaging script shared by CI and a new tag-driven release workflow (draft release, x86_64 and aarch64). Checked by following the README in the VM to a working device, loading every documented automation and blueprint into the dev HA, and running the actionable-notification blueprint (fixed: click followed by close). Setup now uses Nix profile links. Fixed VM 9p stale-binary segfaults in `vm-deploy`. **V1 milestones complete** |
| 2026-11-02 | Release | v0.1.0 tagged. Resume from suspend (M6) and the browser call with camera and muted mic (M9) were assumed good for the tag, to be confirmed on the KDE host |
| 2026-11-02 | 0.1.1 | With the user: no power buttons by default (`power.buttons = []`; suspend, hibernate, restart, shut down and logout are opt-in, like remote unlock); `deskhand setup` asks which (`--power-buttons` for `--yes`). The v0.1.0 draft is not published; 0.1.1 is the first release |
| 2026-11-04 | Planning | M16–M19 planned with the user: Logitech HID++ peripheral batteries (MX Keys S on Bolt is invisible to the kernel and UPower). Generic `peripheral_battery` integration merging UPower and HID++ sources, per-peripheral HA devices (accepted one-time move of `battery_<model>`), `openlogi-hidpp` (0BSD) for the protocol (needs Rust 1.99), backlight the only opt-in control |
| 2026-11-04 | M16 (part) | Toolchain moved to Rust 1.99 (`nixpkgs-rust` input for the devShell and crane); `openlogi-hidpp =0.9.0` pinned. `deskhand_linux::hidraw` (sysfs enumeration, `RawHidChannel` with poll and plain read/write, no unsafe) and `hidpp_probe` (receiver, pairings, features, identity, battery, backlight, 120 s event watch, hex capture). Tested over a socketpair against `HidppChannel`. CI uploads the probe with a udev rule. Waiting for the run on the KDE host (`docs/dev/spikes.md` §9) |
| 2026-11-04 | M16 | Two probe runs on the KDE host (Fedora 44). Bolt on `hid-generic` confirmed; identity, 0x1004 and 0x1982 work through `openlogi-hidpp` with events; sleep is a 0x41 link-down after ~10 min, and wake brings 0x41 + 0x1d4b + an unasked battery report, so no polling (`battery_refresh` off). Solaar's udev rules give access; Solaar shares the node (software-ID matching). Captures saved as fixtures with decoding tests. Probe now skips kernel-owned nodes, logs only HID++ and logs node changes |
| 2026-11-04 | M17 | Sub-devices: `SubDevice` on `EntitySpec`, one retained MQTT document per device under `homeassistant/device/deskhand_<node>/<key>/config` (adopted, kept, stubbed and deleted like the main one; uninstall and print-discovery cover them). `peripherals.rs` in common: sources, sink, merge by key or serial, five entities per peripheral on its own device, receivers as devices, per-source supervision and config. UPower peripherals are now a source (same `battery_<model>` ids) with state, low, connected and updated. Dev HA keeps entity ids and history when an entity moves to a sub-device. New snapshot, 4 planner tests, broker test, 6 merge tests, UPower mock test |

## Verification (whole V1)
1. `nix develop -c just ci` runs fmt, clippy, unit tests, snapshots and mosquitto integration tests.
2. `just dev-up` starts HA and the broker; `nix run .#plasma-vm` starts deskhand in the VM; the
   "Deskhand" device in HA shows the §2 entities.
3. Drive each milestone's acceptance checks in the VM (lock, idle, suspend, notification action,
   media, shortcut).
4. `nix flake check` runs the kde-e2e VM test.
5. Deploy the CI binary to the user's real KDE host and check hardware features: Bluetooth,
   camera, DDC brightness, suspend.
