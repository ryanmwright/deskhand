//! `power` capability (logind): power buttons, sleep handling, lid and dock
//! state, and who is blocking sleep. Plus the `keep_awake` capability.
//!
//! Sleep: Deskhand holds a logind *delay* inhibitor. On
//! `PrepareForSleep(true)` it fires `power_event: sleep`, lets integrations
//! publish their sleeping state ([`Ctx::notify_prepare_sleep`]), tells the
//! transport to go offline and disconnect ([`Ctx::notify_sleep`]), then
//! releases the inhibitor so the machine can sleep. On resume it reconnects
//! and takes a new inhibitor.

use std::sync::Arc;
use std::time::Duration;

use deskhand_core::ctx::{BinarySensor, Event};
use deskhand_core::entity::ButtonClass;
use deskhand_core::{
    Attributes, Commands, Ctx, EntityId, EntityKind, EntitySpec, Integration, IntegrationError,
    Probe,
};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::task::JoinSet;
use zbus::Proxy;
use zbus::zvariant::OwnedFd;

use crate::env::{LOGIN1, LinuxEnv};
use crate::util::dbus_err;

pub const POWER_EVENT: EntityId = EntityId::from_static("power_event");
pub const LID_CLOSED: EntityId = EntityId::from_static("lid_closed");
pub const DOCKED: EntityId = EntityId::from_static("docked");
pub const SLEEP_INHIBITED: EntityId = EntityId::from_static("sleep_inhibited");
pub const KEEP_AWAKE: EntityId = EntityId::from_static("keep_awake");

/// Who we are in logind's inhibitor list.
const WHO: &str = "Deskhand";

/// Time for `PrepareSleep` states to reach the transport before it disconnects.
const PREPARE_GRACE: Duration = Duration::from_millis(300);
/// Time for the transport to publish offline and disconnect before we let go.
const DISCONNECT_GRACE: Duration = Duration::from_millis(1200);

/// `[integrations.power]` options.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PowerOptions {
    /// Buttons to offer, if logind allows them: suspend, hibernate,
    /// hybrid_sleep, suspend_then_hibernate, reboot, poweroff. `logout` is
    /// offered by the desktop backend (KDE), not here.
    ///
    /// None by default: Home Assistant powering the computer down is
    /// opt-in, like remote unlock and logout (`deskhand setup` asks).
    pub buttons: Vec<String>,
}

/// (config name, Can* method, action method, entity id, label, icon, device class)
type ButtonDef = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    Option<ButtonClass>,
);

const BUTTONS: &[ButtonDef] = &[
    (
        "suspend",
        "CanSuspend",
        "Suspend",
        "suspend",
        "Suspend",
        "mdi:power-sleep",
        None,
    ),
    (
        "hibernate",
        "CanHibernate",
        "Hibernate",
        "hibernate",
        "Hibernate",
        "mdi:snowflake",
        None,
    ),
    (
        "hybrid_sleep",
        "CanHybridSleep",
        "HybridSleep",
        "hybrid_sleep",
        "Hybrid sleep",
        "mdi:power-sleep",
        None,
    ),
    (
        "suspend_then_hibernate",
        "CanSuspendThenHibernate",
        "SuspendThenHibernate",
        "suspend_then_hibernate",
        "Suspend then hibernate",
        "mdi:power-sleep",
        None,
    ),
    (
        "reboot",
        "CanReboot",
        "Reboot",
        "reboot",
        "Restart",
        "mdi:restart",
        Some(ButtonClass::Restart),
    ),
    (
        "poweroff",
        "CanPowerOff",
        "PowerOff",
        "power_off",
        "Shut down",
        "mdi:power",
        None,
    ),
];

/// Whether this machine has a lid switch.
fn has_lid() -> bool {
    std::fs::read_dir("/proc/acpi/button/lid").is_ok_and(|mut d| d.next().is_some())
        || std::path::Path::new("/sys/class/input")
            .read_dir()
            .is_ok_and(|d| {
                d.flatten().any(|e| {
                    std::fs::read_to_string(e.path().join("name"))
                        .or_else(|_| std::fs::read_to_string(e.path().join("device/name")))
                        .is_ok_and(|n| n.trim() == "Lid Switch")
                })
            })
}

/// Power buttons, sleep lifecycle, lid/dock and sleep inhibitors via logind.
#[derive(Debug)]
pub struct PowerLogind {
    env: Arc<LinuxEnv>,
    /// Overrides lid detection (tests).
    lid: Option<bool>,
}

impl PowerLogind {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env, lid: None }
    }

    /// Forces lid sensors on or off instead of detecting a lid switch.
    pub fn with_lid(mut self, lid: bool) -> Self {
        self.lid = Some(lid);
        self
    }
}

async fn take_delay_inhibitor(manager: &Proxy<'_>) -> Option<OwnedFd> {
    match manager
        .call::<_, _, OwnedFd>(
            "Inhibit",
            &(
                "sleep",
                WHO,
                "Publish the sleep state to Home Assistant",
                "delay",
            ),
        )
        .await
    {
        Ok(fd) => Some(fd),
        Err(e) => {
            tracing::warn!(error = %e, "cannot take a sleep delay inhibitor; sleep may not be reported");
            None
        }
    }
}

/// Updates `sleep_inhibited` from logind's inhibitor list.
async fn refresh_inhibited(manager: &Proxy<'_>, sensor: &BinarySensor) {
    let list: Vec<(String, String, String, String, u32, u32)> =
        match manager.call("ListInhibitors", &()).await {
            Ok(l) => l,
            Err(e) => {
                tracing::debug!(error = %e, "ListInhibitors failed");
                return;
            }
        };
    let blocking: Vec<_> = list
        .into_iter()
        .filter(|(what, _, _, mode, _, _)| mode == "block" && what.split(':').any(|w| w == "sleep"))
        .map(
            |(what, who, why, _, _, pid)| json!({"who": who, "why": why, "what": what, "pid": pid}),
        )
        .collect();
    let mut attrs = Attributes::new();
    sensor.set(!blocking.is_empty());
    attrs.insert("inhibitors".into(), blocking.into());
    sensor.set_attributes(attrs);
}

#[async_trait::async_trait]
impl Integration for PowerLogind {
    fn id(&self) -> &'static str {
        "power.logind"
    }

    fn capability(&self) -> &'static str {
        "power"
    }

    fn description(&self) -> &'static str {
        "Power buttons, sleep/resume, lid and sleep inhibitors through logind"
    }

    async fn probe(&self) -> Probe {
        match self.env.system_bus().await {
            Ok(c) if LinuxEnv::has_owner(&c, LOGIN1).await => Probe::available(),
            Ok(_) => Probe::unavailable("logind is not running"),
            Err(e) => Probe::unavailable(format!("system bus: {e}")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let opts: PowerOptions = ctx.config()?;
        let manager = self.env.login1_manager().await.map_err(dbus_err)?;
        let mut sleeps = manager
            .receive_signal("PrepareForSleep")
            .await
            .map_err(dbus_err)?;
        let mut block_changes = manager
            .receive_property_changed::<String>("BlockInhibited")
            .await;

        let mut buttons = JoinSet::new();
        for name in &opts.buttons {
            let Some((_, can, action, id, label, icon, class)) =
                BUTTONS.iter().find(|b| b.0 == name).cloned()
            else {
                if name != "logout" {
                    tracing::warn!(button = %name, "integrations.power.buttons: unknown button");
                }
                continue;
            };
            let answer: String = manager.call(can, &()).await.unwrap_or_else(|_| "na".into());
            if answer != "yes" {
                tracing::info!(button = %name, logind = %answer, "power button not offered");
                continue;
            }
            let mut spec = EntitySpec::button(EntityId::from_static(id), label).icon(icon);
            if let EntityKind::Button { device_class } = &mut spec.kind {
                *device_class = class;
            }
            let presses = ctx.button(spec);
            buttons.spawn(press_loop(presses, manager.clone(), action));
        }

        let power_event = ctx.event(
            EntitySpec::event(
                POWER_EVENT,
                "Power event",
                vec!["sleep".into(), "resume".into()],
            )
            .icon("mdi:power-settings"),
        );
        let sleep_inhibited = ctx.binary_sensor(
            EntitySpec::binary_sensor(SLEEP_INHIBITED, "Sleep inhibited", None)
                .icon("mdi:sleep-off")
                .diagnostic(),
        );
        refresh_inhibited(&manager, &sleep_inhibited).await;

        let lid = if self.lid.unwrap_or_else(has_lid) {
            let closed = ctx.binary_sensor(
                EntitySpec::binary_sensor(LID_CLOSED, "Lid closed", None).icon("mdi:laptop"),
            );
            let docked = ctx.binary_sensor(
                EntitySpec::binary_sensor(DOCKED, "Docked", None).icon("mdi:desktop-tower-monitor"),
            );
            closed.set(
                manager
                    .get_property::<bool>("LidClosed")
                    .await
                    .unwrap_or(false),
            );
            docked.set(
                manager
                    .get_property::<bool>("Docked")
                    .await
                    .unwrap_or(false),
            );
            Some((
                closed,
                docked,
                manager.receive_property_changed::<bool>("LidClosed").await,
                manager.receive_property_changed::<bool>("Docked").await,
            ))
        } else {
            None
        };
        let (lid_closed, docked, mut lid_changes, mut dock_changes) = match lid {
            Some((c, d, lc, dc)) => (Some(c), Some(d), Some(lc), Some(dc)),
            None => (None, None, None, None),
        };

        let mut inhibitor = take_delay_inhibitor(&manager).await;
        ctx.ready();

        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                msg = sleeps.next() => {
                    let Some(msg) = msg else {
                        return Err(IntegrationError::msg("logind went away"));
                    };
                    let Ok((start,)) = msg.body().deserialize::<(bool,)>() else { continue };
                    if start {
                        on_sleep(&ctx, &power_event, inhibitor.take()).await;
                    } else {
                        tracing::info!("resumed from sleep");
                        ctx.notify_resume();
                        power_event.fire("resume", Attributes::new());
                        inhibitor = take_delay_inhibitor(&manager).await;
                    }
                }
                Some(_) = block_changes.next() => refresh_inhibited(&manager, &sleep_inhibited).await,
                Some(c) = next_opt(lid_changes.as_mut()) => {
                    if let (Some(s), Ok(v)) = (&lid_closed, c.get().await) { s.set(v) }
                }
                Some(c) = next_opt(dock_changes.as_mut()) => {
                    if let (Some(s), Ok(v)) = (&docked, c.get().await) { s.set(v) }
                }
            }
        }
    }
}

async fn next_opt<S: futures_util::Stream + Unpin>(s: Option<&mut S>) -> Option<S::Item> {
    match s {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}

async fn on_sleep(ctx: &Ctx, power_event: &Event, inhibitor: Option<OwnedFd>) {
    tracing::info!("going to sleep");
    power_event.fire("sleep", Attributes::new());
    ctx.notify_prepare_sleep();
    tokio::time::sleep(PREPARE_GRACE).await;
    ctx.notify_sleep();
    tokio::time::sleep(DISCONNECT_GRACE).await;
    drop(inhibitor);
}

async fn press_loop(mut presses: Commands<()>, manager: Proxy<'static>, action: &'static str) {
    while presses.recv().await.is_some() {
        tracing::info!(action, "power action from Home Assistant");
        if let Err(e) = manager.call_method(action, &(false,)).await {
            tracing::warn!(action, error = %e, "power action failed");
        }
    }
}

/// `keep_awake` switch: blocks sleep and idle in logind and inhibits the
/// screensaver (screen dimming and locking) while on.
#[derive(Debug)]
pub struct KeepAwake {
    env: Arc<LinuxEnv>,
}

impl KeepAwake {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }
}

/// What keep-awake currently holds.
#[derive(Default)]
struct Held {
    logind: Option<OwnedFd>,
    screensaver: Option<u32>,
}

impl KeepAwake {
    async fn screensaver(&self) -> Option<Proxy<'static>> {
        let conn = self.env.session_bus().await.ok()?;
        if !LinuxEnv::has_owner(&conn, "org.freedesktop.ScreenSaver").await {
            return None;
        }
        Proxy::new(
            &conn,
            "org.freedesktop.ScreenSaver",
            "/org/freedesktop/ScreenSaver",
            "org.freedesktop.ScreenSaver",
        )
        .await
        .ok()
    }

    async fn acquire(&self, manager: &Proxy<'_>) -> Held {
        let logind = manager
            .call::<_, _, OwnedFd>(
                "Inhibit",
                &("sleep:idle", WHO, "Keep awake (Home Assistant)", "block"),
            )
            .await
            .inspect_err(|e| tracing::warn!(error = %e, "logind keep-awake inhibitor failed"))
            .ok();
        let screensaver = match self.screensaver().await {
            Some(p) => p
                .call::<_, _, u32>("Inhibit", &(WHO, "Keep awake (Home Assistant)"))
                .await
                .inspect_err(|e| tracing::warn!(error = %e, "ScreenSaver.Inhibit failed"))
                .ok(),
            None => None,
        };
        Held {
            logind,
            screensaver,
        }
    }

    async fn release(&self, held: Held) {
        drop(held.logind);
        if let (Some(cookie), Some(p)) = (held.screensaver, self.screensaver().await) {
            let _ = p.call::<_, _, ()>("UnInhibit", &(cookie,)).await;
        }
    }
}

#[async_trait::async_trait]
impl Integration for KeepAwake {
    fn id(&self) -> &'static str {
        "keep_awake.logind"
    }

    fn capability(&self) -> &'static str {
        "keep_awake"
    }

    fn description(&self) -> &'static str {
        "Keep-awake switch (logind sleep/idle inhibitor and ScreenSaver inhibit)"
    }

    async fn probe(&self) -> Probe {
        match self.env.system_bus().await {
            Ok(c) if LinuxEnv::has_owner(&c, LOGIN1).await => Probe::available(),
            Ok(_) => Probe::unavailable("logind is not running"),
            Err(e) => Probe::unavailable(format!("system bus: {e}")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let manager = self.env.login1_manager().await.map_err(dbus_err)?;
        let (switch, mut commands) =
            ctx.switch(EntitySpec::switch(KEEP_AWAKE, "Keep awake").icon("mdi:coffee"));
        switch.set(false);
        ctx.ready();
        let mut held: Option<Held> = None;
        let cancel = ctx.cancelled();
        loop {
            let on = tokio::select! {
                () = cancel.cancelled() => break,
                cmd = commands.recv() => match cmd { Some(on) => on, None => break },
            };
            match (on, held.take()) {
                (true, None) => {
                    let h = self.acquire(&manager).await;
                    switch.set(h.logind.is_some() || h.screensaver.is_some());
                    held = Some(h);
                }
                (false, Some(h)) => {
                    self.release(h).await;
                    switch.set(false);
                }
                (_, h) => {
                    switch.set(h.is_some());
                    held = h;
                }
            }
        }
        if let Some(h) = held {
            self.release(h).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::testing::FakeTransport;
    use deskhand_core::{
        CommandValue, Hub, HubEvent, HubOptions, IntegrationConfig, Lifecycle, State,
    };
    use deskhand_testkit::mocks::{MockLogind, MockScreenSaver};
    use deskhand_testkit::{TestBus, require_dbus};
    use tokio_util::sync::CancellationToken;

    use super::*;

    struct Rig {
        t: FakeTransport,
        hub: deskhand_core::HubHandle,
        cancel: CancellationToken,
    }

    async fn rig(integration: impl Integration, capability: &str) -> Rig {
        rig_with(integration, IntegrationConfig::new(capability)).await
    }

    async fn rig_with(integration: impl Integration, config: IntegrationConfig) -> Rig {
        let capability = config.capability.to_string();
        let capability = capability.as_str();
        let (hub, handle, link) = Hub::new(HubOptions::default());
        tokio::spawn(hub.run());
        let t = FakeTransport::new(link);
        let cancel = CancellationToken::new();
        let ctx = handle.ctx(config, cancel.clone());
        handle.integration_started(capability);
        tokio::spawn(async move { integration.run(ctx).await.unwrap() });
        handle.startup_complete();
        t.connect();
        Rig {
            t,
            hub: handle,
            cancel,
        }
    }

    async fn snapshot(t: &mut FakeTransport) -> deskhand_core::Snapshot {
        loop {
            match t.next_within(Duration::from_secs(5)).await {
                Some(HubEvent::Snapshot(s)) => return s,
                Some(_) => {}
                None => panic!("no snapshot"),
            }
        }
    }

    #[test]
    fn no_power_buttons_by_default() {
        // Home Assistant powering the computer down is opt-in.
        assert!(PowerOptions::default().buttons.is_empty());
    }

    #[tokio::test]
    async fn buttons_follow_can_and_sleep_is_handled_in_order() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let logind = MockLogind::start(&bus).await.unwrap();
        logind.set_can("Hibernate", "no");
        logind.set_lid_closed(false).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &[]);
        let config = deskhand_core::Config::from_toml_str(
            "[integrations.power]\nbuttons = [\"suspend\", \"hibernate\", \"reboot\", \"poweroff\"]",
            None,
        )
        .unwrap()
        .integration("power")
        .unwrap();
        let mut r = rig_with(PowerLogind::new(env).with_lid(true), config).await;
        let snap = snapshot(&mut r.t).await;
        let ids: Vec<&str> = snap.entities.iter().map(|d| d.spec.id.as_str()).collect();
        assert!(ids.contains(&"suspend") && ids.contains(&"reboot") && ids.contains(&"power_off"));
        assert!(
            !ids.contains(&"hibernate"),
            "CanHibernate=no hides the button"
        );
        assert!(ids.contains(&"lid_closed") && ids.contains(&"docked"));
        assert_eq!(snap.states[&SLEEP_INHIBITED], State::Bool(false));

        // The delay inhibitor is held while awake.
        let delay = |l: &MockLogind| {
            l.inhibitors()
                .iter()
                .filter(|i| i.mode == "delay" && i.active)
                .count()
        };
        assert_eq!(delay(&logind), 1);

        r.t.command("suspend", CommandValue::Press);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(logind.calls().contains(&"Suspend(false)".to_owned()));

        let mut lifecycle = r.hub.lifecycle();
        logind.prepare_for_sleep(true).await.unwrap();
        let mut seen = Vec::new();
        while !seen.contains(&"transport-sleep".to_owned()) {
            match r
                .t
                .next_within(Duration::from_secs(5))
                .await
                .expect("events")
            {
                HubEvent::Event { event_type, .. } => seen.push(format!("event-{event_type}")),
                HubEvent::Sleep => seen.push("transport-sleep".into()),
                _ => {}
            }
        }
        assert_eq!(seen, vec!["event-sleep", "transport-sleep"]);
        assert_eq!(lifecycle.recv().await.unwrap(), Lifecycle::PrepareSleep);
        // The inhibitor is released after the transport disconnected.
        tokio::time::sleep(DISCONNECT_GRACE + Duration::from_millis(500)).await;
        assert_eq!(delay(&logind), 0);

        logind.prepare_for_sleep(false).await.unwrap();
        loop {
            if let Some(HubEvent::Resume) = r.t.next_within(Duration::from_secs(5)).await {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(delay(&logind), 1, "a new delay inhibitor after resume");
        r.cancel.cancel();
    }

    #[tokio::test]
    async fn keep_awake_holds_both_inhibitors() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let logind = MockLogind::start(&bus).await.unwrap();
        let screensaver = MockScreenSaver::start(&bus).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &[]);
        let mut r = rig(KeepAwake::new(env), "keep_awake").await;
        snapshot(&mut r.t).await;
        let blocks = |l: &MockLogind| {
            l.inhibitors()
                .iter()
                .filter(|i| i.mode == "block" && i.active)
                .count()
        };

        r.t.command("keep_awake", CommandValue::Bool(true));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(blocks(&logind), 1);
        assert_eq!(logind.inhibitors()[0].what, "sleep:idle");
        assert_eq!(screensaver.inhibitions().len(), 1);

        r.t.command("keep_awake", CommandValue::Bool(false));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(blocks(&logind), 0);
        assert!(screensaver.inhibitions().is_empty());
        r.cancel.cancel();
    }
}
