//! `idle` capability: is someone using this machine?
//!
//! - `user_active` binary sensor: off after `[general] idle_threshold`
//!   without keyboard or mouse input.
//! - `idle_seconds` sensor: 0 while active; while idle, the time since the
//!   last input, refreshed every 30 s.
//!
//! Providers, in priority order:
//! 1. Wayland `ext_idle_notifier_v1` v2 `get_input_idle_notification`
//!    (ignores idle inhibitors, so a playing video still counts as idle).
//! 2. ScreenSaver `GetSessionIdleTime`, polled (X11 sessions).
//! 3. logind `IdleHint`/`IdleSinceHint` (whatever the desktop reports).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deskhand_core::ctx::{BinarySensor, Sensor};
use deskhand_core::entity::{SensorClass, StateClass};
use deskhand_core::{Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::time::Instant;
use zbus::Proxy;

use crate::env::LinuxEnv;
use crate::util::dbus_err;

pub const USER_ACTIVE: EntityId = EntityId::from_static("user_active");
pub const IDLE_SECONDS: EntityId = EntityId::from_static("idle_seconds");

/// How often `idle_seconds` is refreshed while idle.
const IDLE_REFRESH: Duration = Duration::from_secs(30);
/// ScreenSaver polling interval.
const POLL: Duration = Duration::from_secs(5);

/// A change reported by a provider.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Change {
    /// Idle since this instant.
    Idle(Instant),
    Active,
}

/// Owns the entities and turns provider changes into states.
struct Reporter {
    active: BinarySensor,
    seconds: Sensor,
    idle_since: Option<Instant>,
}

impl Reporter {
    fn register(ctx: &Ctx) -> Self {
        let active = ctx.binary_sensor(
            EntitySpec::binary_sensor(USER_ACTIVE, "User active", None).icon("mdi:account-clock"),
        );
        let seconds = ctx.sensor(
            EntitySpec::sensor(IDLE_SECONDS, "Idle time")
                .sensor_class(SensorClass::Duration)
                .unit("s")
                .state_class(StateClass::Measurement)
                .precision(0)
                .icon("mdi:timer-sand")
                .diagnostic(),
        );
        active.set(true);
        seconds.set(0);
        Self {
            active,
            seconds,
            idle_since: None,
        }
    }

    fn apply(&mut self, change: Change) {
        match change {
            Change::Idle(since) => {
                self.idle_since = Some(since);
                self.active.set(false);
                self.refresh();
            }
            Change::Active => {
                self.idle_since = None;
                self.active.set(true);
                self.seconds.set(0);
            }
        }
    }

    fn refresh(&self) {
        if let Some(since) = self.idle_since {
            self.seconds.set(since.elapsed().as_secs());
        }
    }

    /// Runs until cancelled or the provider's channel closes.
    async fn run(
        mut self,
        ctx: &Ctx,
        mut changes: mpsc::UnboundedReceiver<Change>,
    ) -> Result<(), IntegrationError> {
        let cancel = ctx.cancelled();
        let mut tick = tokio::time::interval(IDLE_REFRESH);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                c = changes.recv() => match c {
                    Some(c) => self.apply(c),
                    None => return Err(IntegrationError::msg("idle source stopped")),
                },
                _ = tick.tick() => self.refresh(),
            }
        }
    }
}

// --- Wayland ---------------------------------------------------------------

mod wayland {
    use std::os::fd::AsFd;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use rustix::event::{PollFd, PollFlags, Timespec};
    use tokio::sync::mpsc;
    use tokio::time::Instant;
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_registry, wl_seat};
    use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
    use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
        self, ExtIdleNotificationV1,
    };
    use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

    use super::Change;

    pub(super) struct State {
        tx: mpsc::UnboundedSender<Change>,
        timeout: Duration,
    }

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<wl_seat::WlSeat, ()> for State {
        fn event(
            _: &mut Self,
            _: &wl_seat::WlSeat,
            _: wl_seat::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ExtIdleNotifierV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &ExtIdleNotifierV1,
            _: <ExtIdleNotifierV1 as Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ExtIdleNotificationV1, ()> for State {
        fn event(
            state: &mut Self,
            _: &ExtIdleNotificationV1,
            event: ext_idle_notification_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            // The compositor counts the timeout from the last input
            // (docs/dev/spikes.md §1), so idle started `timeout` ago.
            let change = match event {
                ext_idle_notification_v1::Event::Idled => Change::Idle(
                    Instant::now()
                        .checked_sub(state.timeout)
                        .unwrap_or_else(Instant::now),
                ),
                ext_idle_notification_v1::Event::Resumed => Change::Active,
                _ => return,
            };
            let _ = state.tx.send(change);
        }
    }

    /// The notifier's version, if the compositor offers it.
    pub(super) fn probe() -> Result<u32, String> {
        let conn = Connection::connect_to_env().map_err(|e| format!("no Wayland display: {e}"))?;
        let (globals, _queue) = registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
        globals
            .contents()
            .clone_list()
            .iter()
            .find(|g| g.interface == "ext_idle_notifier_v1")
            .map(|g| g.version)
            .ok_or_else(|| "compositor does not offer ext_idle_notifier_v1".into())
    }

    /// Blocking event loop; returns when `stop` is set or the connection fails.
    pub(super) fn run(
        timeout: Duration,
        tx: mpsc::UnboundedSender<Change>,
        stop: Arc<AtomicBool>,
    ) -> Result<(), String> {
        let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).map_err(|e| e.to_string())?;
        let notifier: ExtIdleNotifierV1 =
            globals.bind(&qh, 1..=2, ()).map_err(|e| e.to_string())?;
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let _notification = if notifier.version() >= 2 {
            notifier.get_input_idle_notification(ms, &seat, &qh, ())
        } else {
            tracing::warn!("ext_idle_notifier_v1 v1 only: a playing video will count as activity");
            notifier.get_idle_notification(ms, &seat, &qh, ())
        };
        let mut state = State { tx, timeout };
        let half_second = Timespec {
            tv_sec: 0,
            tv_nsec: 500_000_000,
        };
        while !stop.load(Ordering::Relaxed) && !state.tx.is_closed() {
            queue
                .dispatch_pending(&mut state)
                .map_err(|e| e.to_string())?;
            queue.flush().map_err(|e| e.to_string())?;
            let Some(guard) = queue.prepare_read() else {
                continue;
            };
            let readable = {
                let fd = guard.connection_fd();
                let fd = fd.as_fd();
                let mut fds = [PollFd::new(&fd, PollFlags::IN)];
                matches!(rustix::event::poll(&mut fds, Some(&half_second)), Ok(n) if n > 0)
            };
            if readable {
                guard.read().map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
}

/// Wayland `ext_idle_notifier_v1` (on its own thread).
#[derive(Debug)]
pub struct IdleWayland {
    threshold: Duration,
}

impl IdleWayland {
    pub fn new(threshold: Duration) -> Self {
        Self { threshold }
    }
}

#[async_trait::async_trait]
impl Integration for IdleWayland {
    fn id(&self) -> &'static str {
        "idle.wayland"
    }

    fn capability(&self) -> &'static str {
        "idle"
    }

    fn description(&self) -> &'static str {
        "Idle detection through Wayland ext-idle-notify"
    }

    async fn probe(&self) -> Probe {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Probe::unavailable("not a Wayland session (WAYLAND_DISPLAY unset)");
        }
        match tokio::task::spawn_blocking(wayland::probe).await {
            Ok(Ok(v)) => Probe::available_with(format!("ext_idle_notifier_v1 v{v}")),
            Ok(Err(e)) => Probe::unavailable(e),
            Err(e) => Probe::unavailable(e.to_string()),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let threshold = self.threshold;
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("deskhand-idle".into())
            .spawn(move || {
                if let Err(e) = wayland::run(threshold, tx, thread_stop) {
                    tracing::warn!(error = %e, "Wayland idle thread stopped");
                }
            })
            .map_err(IntegrationError::other)?;
        let reporter = Reporter::register(&ctx);
        ctx.ready();
        let res = reporter.run(&ctx, rx).await;
        stop.store(true, Ordering::Relaxed);
        let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        res
    }
}

// --- ScreenSaver polling ---------------------------------------------------

/// Polls `org.freedesktop.ScreenSaver.GetSessionIdleTime` (milliseconds).
#[derive(Debug)]
pub struct IdleScreenSaver {
    env: Arc<LinuxEnv>,
    threshold: Duration,
}

impl IdleScreenSaver {
    pub fn new(env: Arc<LinuxEnv>, threshold: Duration) -> Self {
        Self { env, threshold }
    }

    async fn proxy(&self) -> zbus::Result<Proxy<'static>> {
        Proxy::new(
            &self.env.session_bus().await?,
            "org.freedesktop.ScreenSaver",
            "/ScreenSaver",
            "org.freedesktop.ScreenSaver",
        )
        .await
    }
}

#[async_trait::async_trait]
impl Integration for IdleScreenSaver {
    fn id(&self) -> &'static str {
        "idle.screensaver"
    }

    fn capability(&self) -> &'static str {
        "idle"
    }

    fn description(&self) -> &'static str {
        "Idle detection by polling org.freedesktop.ScreenSaver"
    }

    async fn probe(&self) -> Probe {
        match self.proxy().await {
            Ok(p) => match p.call::<_, _, u32>("GetSessionIdleTime", &()).await {
                Ok(_) => Probe::available(),
                Err(e) => Probe::unavailable(format!("GetSessionIdleTime: {e}")),
            },
            Err(e) => Probe::unavailable(format!("session bus: {e}")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let proxy = self.proxy().await.map_err(dbus_err)?;
        let (tx, rx) = mpsc::unbounded_channel();
        let reporter = Reporter::register(&ctx);
        ctx.ready();
        let threshold = self.threshold;
        let poller = async move {
            let mut idle = false;
            let mut tick = tokio::time::interval(POLL);
            loop {
                tick.tick().await;
                let ms: u32 = match proxy.call("GetSessionIdleTime", &()).await {
                    Ok(ms) => ms,
                    Err(e) => return Err(dbus_err(e)),
                };
                let idle_for = Duration::from_millis(u64::from(ms));
                let now_idle = idle_for >= threshold;
                if now_idle != idle {
                    idle = now_idle;
                    let change = if idle {
                        Change::Idle(
                            Instant::now()
                                .checked_sub(idle_for)
                                .unwrap_or_else(Instant::now),
                        )
                    } else {
                        Change::Active
                    };
                    if tx.send(change).is_err() {
                        return Ok(());
                    }
                }
            }
        };
        tokio::select! {
            r = reporter.run(&ctx, rx) => r,
            r = poller => r,
        }
    }
}

// --- logind ----------------------------------------------------------------

/// logind `IdleHint` on the graphical session (set by the desktop).
#[derive(Debug)]
pub struct IdleLogind {
    env: Arc<LinuxEnv>,
}

impl IdleLogind {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }
}

/// Converts logind's realtime microseconds into an `Instant`.
fn instant_from_usec(usec: u64) -> Instant {
    let at = UNIX_EPOCH + Duration::from_micros(usec);
    let ago = SystemTime::now().duration_since(at).unwrap_or_default();
    Instant::now().checked_sub(ago).unwrap_or_else(Instant::now)
}

#[async_trait::async_trait]
impl Integration for IdleLogind {
    fn id(&self) -> &'static str {
        "idle.logind"
    }

    fn capability(&self) -> &'static str {
        "idle"
    }

    fn description(&self) -> &'static str {
        "Idle detection through logind IdleHint (as reported by the desktop)"
    }

    async fn probe(&self) -> Probe {
        match self.env.session_proxy().await {
            Ok(s) => match s.get_property::<bool>("IdleHint").await {
                Ok(_) => Probe::available(),
                Err(e) => Probe::unavailable(format!("IdleHint: {e}")),
            },
            Err(e) => Probe::unavailable(format!("no logind session: {e}")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let session = self.env.session_proxy().await.map_err(dbus_err)?;
        let mut hints = session.receive_property_changed::<bool>("IdleHint").await;
        let (tx, rx) = mpsc::unbounded_channel();
        let mut reporter = Reporter::register(&ctx);
        if session
            .get_property::<bool>("IdleHint")
            .await
            .unwrap_or(false)
        {
            let since: u64 = session.get_property("IdleSinceHint").await.unwrap_or(0);
            reporter.apply(Change::Idle(instant_from_usec(since)));
        }
        ctx.ready();
        let watcher = async move {
            while let Some(change) = hints.next().await {
                let Ok(idle) = change.get().await else {
                    continue;
                };
                let c = if idle {
                    let since: u64 = session.get_property("IdleSinceHint").await.unwrap_or(0);
                    Change::Idle(instant_from_usec(since))
                } else {
                    Change::Active
                };
                if tx.send(c).is_err() {
                    break;
                }
            }
            Err(IntegrationError::msg("logind session went away"))
        };
        tokio::select! {
            r = reporter.run(&ctx, rx) => r,
            r = watcher => r,
        }
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::testing::FakeTransport;
    use deskhand_core::{Hub, HubEvent, HubOptions, IntegrationConfig, State};
    use deskhand_testkit::mocks::{MockLogind, MockScreenSaver};
    use deskhand_testkit::{TestBus, require_dbus};
    use tokio_util::sync::CancellationToken;

    use super::*;

    async fn wait_state(t: &mut FakeTransport, id: &EntityId, want: State) {
        let limit = Duration::from_secs(15);
        tokio::time::timeout(limit, async {
            loop {
                match t.next_within(limit).await {
                    Some(HubEvent::State { id: i, state, .. }) if &i == id && state == want => {
                        return;
                    }
                    Some(HubEvent::Snapshot(s)) if s.states.get(id) == Some(&want) => return,
                    Some(_) => {}
                    None => panic!("event stream ended"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {id} = {want:?}"));
    }

    fn start(integration: impl Integration) -> (FakeTransport, CancellationToken) {
        let (hub, handle, link) = Hub::new(HubOptions::default());
        tokio::spawn(hub.run());
        let t = FakeTransport::new(link);
        let cancel = CancellationToken::new();
        let ctx = handle.ctx(IntegrationConfig::new("idle"), cancel.clone());
        handle.integration_started("idle");
        tokio::spawn(async move { integration.run(ctx).await });
        handle.startup_complete();
        t.connect();
        (t, cancel)
    }

    #[tokio::test]
    async fn logind_idle_hint() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let logind = MockLogind::start(&bus).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &[]);
        let integration = IdleLogind::new(env);
        assert!(integration.probe().await.is_available());
        let (mut t, cancel) = start(integration);
        wait_state(&mut t, &USER_ACTIVE, State::Bool(true)).await;

        let ten_min_ago = SystemTime::now() - Duration::from_secs(600);
        let usec = ten_min_ago.duration_since(UNIX_EPOCH).unwrap().as_micros() as u64;
        logind.set_idle(true, usec).await.unwrap();
        wait_state(&mut t, &USER_ACTIVE, State::Bool(false)).await;
        logind.set_idle(false, 0).await.unwrap();
        wait_state(&mut t, &USER_ACTIVE, State::Bool(true)).await;
        cancel.cancel();
    }

    #[tokio::test]
    async fn screensaver_polling_uses_the_threshold() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let screensaver = MockScreenSaver::start(&bus).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &[]);
        let integration = IdleScreenSaver::new(env, Duration::from_secs(60));
        assert!(integration.probe().await.is_available());
        screensaver.set_idle_time(120_000);
        let (mut t, cancel) = start(integration);
        wait_state(&mut t, &USER_ACTIVE, State::Bool(false)).await;
        cancel.cancel();
    }

    #[test]
    fn reporter_counts_from_idle_start() {
        let past = instant_from_usec(
            (SystemTime::now() - Duration::from_secs(90))
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_micros() as u64,
        );
        let secs = past.elapsed().as_secs();
        assert!((89..=91).contains(&secs), "{secs}");
    }
}
