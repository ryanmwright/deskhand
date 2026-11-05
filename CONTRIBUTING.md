# Contributing

Thanks for helping. Bug reports with `deskhand doctor --json` output (remove
anything private) are very welcome, as are new integrations and desktop
backends.

## Development setup

Everything runs in the Nix flake's dev shell (Rust, libpulse, dbus-daemon,
Mosquitto, cargo-deny, just):

```sh
nix develop
just ci          # fmt check, clippy -D warnings, tests, cargo-deny: what CI runs
```

| | |
|---|---|
| `just it` | also the broker tests against a Mosquitto container (Docker or Podman) |
| `just dev-up` | Home Assistant on :8123 (`dev`/`devpassword`) and Mosquitto on :1883, with MQTT set up |
| `just run` | the daemon against that stack (`dev/config.toml`) |
| `just vm-headless`, `just vm-deploy` | a Plasma 6 Wayland VM running your build (KDE work on a non-KDE machine) |
| `just e2e` | the end-to-end NixOS test (Plasma, broker, packaged daemon) |
| `just flake-check` | every Nix check |

Without Nix you need Rust 1.99+, libpulse headers and `dbus-daemon`; D-Bus
tests skip themselves when `dbus-daemon` is missing (outside CI).

[AGENTS.md](AGENTS.md) has more detail on the dev environment, and
[docs/v1-plan.md](docs/v1-plan.md) records the design and every decision made
so far.

## How it fits together

```
crates/
  deskhand-core/     entity model, Ctx and typed handles, hub, supervisor, config. No platform code.
  deskhand-mqtt/     the transport: topics, Home Assistant discovery, availability
  deskhand-common/   platform-neutral integrations and the media/audio kits
  deskhand-linux/    freedesktop integrations (logind, MPRIS, UPower, BlueZ, ...)
  deskhand-kde/      KDE Plasma (KWin, PowerDevil, kglobalaccel, ...)
  deskhand-testkit/  test helpers: private D-Bus buses, mock services
  deskhand/          the binary: CLI, wiring, setup, doctor, tray
```

- An **integration** provides one **capability** (`media`, `idle`, `lock`,
  ...). A backend offers several candidate integrations per capability in
  priority order; the supervisor starts the first one whose `probe` succeeds,
  restarts it with backoff when it fails, and probes again later.
- Integrations register entities through `Ctx` and get typed handles. The hub
  owns all state, deduplicates it and hands it to the transport. Integrations
  never see MQTT.
- Backends are chosen at compile time with cargo features on the binary
  (`kde`, `linux`, `tray`, `setup`, `audio-pulse`).

## Adding an integration

1. Pick the crate: platform-neutral code in `deskhand-common`, freedesktop
   D-Bus in `deskhand-linux`, desktop-specific in its backend crate.
2. Implement `Integration`:

   ```rust
   use deskhand_core::{Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};

   pub const WIDGET_ON: EntityId = EntityId::from_static("widget_on");

   #[derive(Debug)]
   pub struct Widget { /* connections from LinuxEnv, options passed in */ }

   #[async_trait::async_trait]
   impl Integration for Widget {
       fn id(&self) -> &'static str { "widget.foo" }          // <capability>.<provider>
       fn capability(&self) -> &'static str { "widget" }
       fn description(&self) -> &'static str { "Widget state through Foo" }

       async fn probe(&self) -> Probe {
           // Cheap check that this can work here; the reason is shown by `doctor`.
           Probe::unavailable("Foo is not running")
       }

       async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
           let opts: WidgetOptions = ctx.config()?;          // [integrations.widget]
           let (switch, mut commands) =
               ctx.switch(EntitySpec::switch(WIDGET_ON, "Widget").icon("mdi:widgets"));
           switch.set(read_state().await?);
           ctx.ready();                                     // initial states are set
           let cancel = ctx.cancelled();
           loop {
               tokio::select! {
                   () = cancel.cancelled() => return Ok(()),
                   Some(on) = commands.recv() => apply(on).await?,
                   // ... wait for changes and call switch.set(...)
               }
           }
       }
   }
   ```

   - Keep per-run state inside `run`: a failed run is restarted on the same
     value.
   - Return an error when the service goes away; the supervisor restarts the
     integration.
   - Call `ctx.ready()` once, after the first states are set.
   - Entity ids are `snake_case`, short and stable; they become MQTT topics and
     Home Assistant entity ids. Prefer several small entities to one big one.
3. Add it to the backend's `candidates()` list (`deskhand_linux::candidates`,
   `deskhand_kde::candidates`), before weaker alternatives for the same
   capability.
4. Test it:
   - `deskhand_core::testing::rig(integration, "widget")` runs it against a
     fake transport; `wait_state`, `wait_attr`, `wait_event` and `t.command`
     drive it.
   - For D-Bus services, write a `#[zbus::interface]` mock in
     `deskhand-testkit/src/mocks` and start it on a `TestBus` (begin the test
     with `require_dbus!()`).
   - Pure logic gets plain unit tests; time-based logic uses
     `#[tokio::test(start_paused = true)]`.
5. Document new entities in [docs/entities.md](docs/entities.md) and options in
   [docs/configuration.md](docs/configuration.md).

### Rules

- Never run anything that arrives over MQTT. Actions must be allowlisted in
  the config (see [SECURITY.md](SECURITY.md)).
- No platform code in `deskhand-core`.
- Dependency versions live in the root `Cargo.toml`
  (`[workspace.dependencies]`); crates use `foo.workspace = true`. Avoid C
  dependencies (libpulse is the only runtime one) and anything that links
  libdbus.
- `cargo deny` exceptions need a `reason`.

## Adding a desktop or OS backend

1. Create `crates/deskhand-<backend>` (e.g. `deskhand-gnome`) with a
   `candidates(...) -> Vec<Box<dyn Integration>>` function. Reuse
   `deskhand-linux` where the freedesktop interfaces apply.
2. Every candidate's `probe` must fail outside its desktop (KDE checks
   `XDG_CURRENT_DESKTOP`), so a build with several backends picks the right one.
3. Keep entity ids identical across backends: implement `MediaSource` or
   `AudioSource` and use `MediaKit` / `AudioKit` from `deskhand-common` instead
   of registering media and audio entities yourself.
4. Add a cargo feature on `deskhand` (`gnome = ["linux", "dep:deskhand-gnome"]`)
   and extend `Setup::candidates` in `crates/deskhand/src/wiring.rs`, listing the
   backend before the generic Linux candidates.

## Pull requests

- Run `just ci` (at least `cargo fmt` and `cargo clippy`) before pushing.
- Describe how you tested it, especially on hardware or desktops CI can't run.
- Contributions are dual-licensed under MIT OR Apache-2.0, like the project.
