//! `lock` capability: whether the screen is locked, and locking it.
//!
//! - `screen_locked` binary sensor.
//! - With `[integrations.lock] allow_remote_unlock = true`: a `screen_lock`
//!   lock entity that can also unlock. Otherwise (the default) only a
//!   `lock_screen` button, so Home Assistant can never unlock the desktop.
//!
//! Providers: logind `LockedHint` on the graphical session (primary), then
//! the freedesktop ScreenSaver interface (read-only state, lock only).

use std::sync::Arc;

use deskhand_core::ctx::LockState;
use deskhand_core::entity::LockCmd;
use deskhand_core::{Commands, Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use futures_util::StreamExt;
use serde::Deserialize;
use zbus::Proxy;

use crate::env::LinuxEnv;
use crate::util::{dbus_err, recv_opt};

pub const SCREEN_LOCKED: EntityId = EntityId::from_static("screen_locked");
pub const SCREEN_LOCK: EntityId = EntityId::from_static("screen_lock");
pub const LOCK_BUTTON: EntityId = EntityId::from_static("lock_screen");

/// `[integrations.lock]` options.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct LockOptions {
    /// Publish a lock entity that Home Assistant can also *unlock* with.
    pub allow_remote_unlock: bool,
}

/// The control side: a lock entity (unlock allowed) or a lock button.
struct Control {
    lock: Option<(deskhand_core::ctx::Lock, Commands<LockCmd>)>,
    button: Option<Commands<()>>,
}

impl Control {
    fn register(ctx: &Ctx, allow_unlock: bool) -> Self {
        if allow_unlock {
            let lock =
                ctx.lock(EntitySpec::lock(SCREEN_LOCK, "Screen lock").icon("mdi:monitor-lock"));
            Self {
                lock: Some(lock),
                button: None,
            }
        } else {
            let button =
                ctx.button(EntitySpec::button(LOCK_BUTTON, "Lock screen").icon("mdi:monitor-lock"));
            Self {
                lock: None,
                button: Some(button),
            }
        }
    }

    fn set(&self, locked: bool) {
        if let Some((lock, _)) = &self.lock {
            lock.set(if locked {
                LockState::Locked
            } else {
                LockState::Unlocked
            });
        }
    }

    /// The next command: `Some(true)` lock, `Some(false)` unlock.
    async fn next(&mut self) -> Option<bool> {
        let lock = self.lock.as_mut().map(|(_, c)| c);
        tokio::select! {
            cmd = recv_opt(lock) => match cmd? {
                LockCmd::Lock => Some(true),
                LockCmd::Unlock | LockCmd::Open => Some(false),
            },
            press = recv_opt(self.button.as_mut()) => press.map(|()| true),
        }
    }
}

fn screen_locked_spec() -> EntitySpec {
    EntitySpec::binary_sensor(SCREEN_LOCKED, "Screen locked", None).icon("mdi:monitor-lock")
}

/// logind `LockedHint` + `Session.Lock`/`Unlock`.
#[derive(Debug)]
pub struct LockLogind {
    env: Arc<LinuxEnv>,
}

impl LockLogind {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }
}

#[async_trait::async_trait]
impl Integration for LockLogind {
    fn id(&self) -> &'static str {
        "lock.logind"
    }

    fn capability(&self) -> &'static str {
        "lock"
    }

    fn description(&self) -> &'static str {
        "Screen lock state and control through logind"
    }

    async fn probe(&self) -> Probe {
        let session = match self.env.session_proxy().await {
            Ok(s) => s,
            Err(e) => return Probe::unavailable(format!("no logind session: {e}")),
        };
        match session.get_property::<bool>("LockedHint").await {
            Ok(_) => Probe::available_with(session.path().to_string()),
            Err(e) => Probe::unavailable(format!("LockedHint unreadable: {e}")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let opts: LockOptions = ctx.config()?;
        let session = self.env.session_proxy().await.map_err(dbus_err)?;
        let mut changes = session.receive_property_changed::<bool>("LockedHint").await;
        let locked: bool = session.get_property("LockedHint").await.map_err(dbus_err)?;
        let sensor = ctx.binary_sensor(screen_locked_spec());
        let mut control = Control::register(&ctx, opts.allow_remote_unlock);
        sensor.set(locked);
        control.set(locked);
        ctx.ready();

        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                change = changes.next() => {
                    let Some(change) = change else {
                        return Err(IntegrationError::msg("logind session went away"));
                    };
                    if let Ok(locked) = change.get().await {
                        tracing::debug!(locked, "LockedHint changed");
                        sensor.set(locked);
                        control.set(locked);
                    }
                }
                cmd = control.next() => match cmd {
                    Some(lock) => call(&session, if lock { "Lock" } else { "Unlock" }).await,
                    None => return Ok(()),
                },
            }
        }
    }
}

async fn call(proxy: &Proxy<'_>, method: &str) {
    tracing::info!(method, "screen lock command from Home Assistant");
    if let Err(e) = proxy.call_method(method, &()).await {
        tracing::warn!(method, error = %e, "lock command failed");
    }
}

/// freedesktop ScreenSaver `GetActive`/`ActiveChanged` + `Lock` (no unlock).
#[derive(Debug)]
pub struct LockScreenSaver {
    env: Arc<LinuxEnv>,
}

impl LockScreenSaver {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }

    async fn proxy(&self) -> zbus::Result<Proxy<'static>> {
        Proxy::new(
            &self.env.session_bus().await?,
            "org.freedesktop.ScreenSaver",
            "/org/freedesktop/ScreenSaver",
            "org.freedesktop.ScreenSaver",
        )
        .await
    }
}

#[async_trait::async_trait]
impl Integration for LockScreenSaver {
    fn id(&self) -> &'static str {
        "lock.screensaver"
    }

    fn capability(&self) -> &'static str {
        "lock"
    }

    fn description(&self) -> &'static str {
        "Screen lock state through org.freedesktop.ScreenSaver (lock only)"
    }

    async fn probe(&self) -> Probe {
        match self.proxy().await {
            Ok(p) => match p.call::<_, _, bool>("GetActive", &()).await {
                Ok(_) => Probe::available(),
                Err(e) => Probe::unavailable(format!("ScreenSaver.GetActive: {e}")),
            },
            Err(e) => Probe::unavailable(format!("session bus: {e}")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let opts: LockOptions = ctx.config()?;
        if opts.allow_remote_unlock {
            tracing::warn!(
                "allow_remote_unlock needs logind; the ScreenSaver fallback can only lock"
            );
        }
        let proxy = self.proxy().await.map_err(dbus_err)?;
        let mut changes = proxy
            .receive_signal("ActiveChanged")
            .await
            .map_err(dbus_err)?;
        let active: bool = proxy.call("GetActive", &()).await.map_err(dbus_err)?;
        let sensor = ctx.binary_sensor(screen_locked_spec());
        let mut control = Control::register(&ctx, false);
        sensor.set(active);
        ctx.ready();

        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                msg = changes.next() => {
                    let Some(msg) = msg else {
                        return Err(IntegrationError::msg("ScreenSaver went away"));
                    };
                    if let Ok((active,)) = msg.body().deserialize::<(bool,)>() {
                        sensor.set(active);
                    }
                }
                cmd = control.next() => match cmd {
                    Some(true) => call(&proxy, "Lock").await,
                    Some(false) => {}
                    None => return Ok(()),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use deskhand_core::testing::FakeTransport;
    use deskhand_core::{CommandValue, Config, Hub, HubEvent, HubOptions, State};
    use deskhand_testkit::mocks::MockLogind;
    use deskhand_testkit::{TestBus, require_dbus};
    use tokio_util::sync::CancellationToken;

    use super::*;

    async fn wait_state(t: &mut FakeTransport, id: &str, want: State) {
        let deadline = Duration::from_secs(5);
        tokio::time::timeout(deadline, async {
            loop {
                match t.next_within(deadline).await {
                    Some(HubEvent::State { id: i, state, .. })
                        if i.as_str() == id && state == want =>
                    {
                        return;
                    }
                    Some(HubEvent::Snapshot(s))
                        if s.states.get(&EntityId::new(id).unwrap()) == Some(&want) =>
                    {
                        return;
                    }
                    Some(_) => {}
                    None => panic!("no {id} = {want:?}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {id} = {want:?}"));
    }

    async fn start(bus: &TestBus, config: &str) -> (FakeTransport, CancellationToken) {
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &[]);
        let integration = LockLogind::new(env);
        assert!(integration.probe().await.is_available());
        let (hub, handle, link) = Hub::new(HubOptions::default());
        tokio::spawn(hub.run());
        let t = FakeTransport::new(link);
        let cancel = CancellationToken::new();
        let cfg = Config::from_toml_str(config, None)
            .unwrap()
            .integration("lock")
            .unwrap();
        let ctx = handle.ctx(cfg, cancel.clone());
        handle.integration_started("lock");
        tokio::spawn(async move { integration.run(ctx).await.unwrap() });
        handle.startup_complete();
        t.connect();
        (t, cancel)
    }

    #[tokio::test]
    async fn follows_locked_hint_and_locks_but_never_unlocks_by_default() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let logind = MockLogind::start(&bus).await.unwrap();
        let (mut t, cancel) = start(&bus, "").await;
        wait_state(&mut t, "screen_locked", State::Bool(false)).await;

        logind.set_locked(true).await.unwrap();
        wait_state(&mut t, "screen_locked", State::Bool(true)).await;

        t.command("lock_screen", CommandValue::Press);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(logind.calls().contains(&"Session.Lock".to_owned()));
        // No lock entity exists, so an unlock command goes nowhere.
        t.command("screen_lock", CommandValue::Lock(LockCmd::Unlock));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!logind.calls().iter().any(|c| c.contains("Unlock")));
        cancel.cancel();
    }

    #[tokio::test]
    async fn remote_unlock_when_allowed() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let logind = MockLogind::start(&bus).await.unwrap();
        let (mut t, cancel) = start(&bus, "[integrations.lock]\nallow_remote_unlock = true").await;
        wait_state(&mut t, "screen_lock", "unlocked".into()).await;
        logind.set_locked(true).await.unwrap();
        wait_state(&mut t, "screen_lock", "locked".into()).await;
        t.command("screen_lock", CommandValue::Lock(LockCmd::Unlock));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(logind.calls().contains(&"Session.Unlock".to_owned()));
        cancel.cancel();
    }
}
