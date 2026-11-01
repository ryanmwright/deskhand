//! `night_light` capability: KWin's night light.
//!
//! | entity | |
//! |---|---|
//! | `night_light` | binary sensor: the colour shift is active. Attributes: `enabled`, `mode`, `target_temperature` |
//! | `night_light_temperature` | current screen temperature in K |
//! | `night_light_inhibit` | switch: suspend night light (like the tray's "Suspend") |
//!
//! The inhibition belongs to Deskhand's bus connection, so it ends when
//! Deskhand stops. Turning the switch off only ends Deskhand's own
//! inhibition; one made in the tray stays (and keeps the switch on).
//! Plasma 6.6 has no D-Bus call to force night light on (`activateManual`
//! is newer), so there is no switch for that yet.

use std::sync::Arc;
use std::time::Duration;

use deskhand_core::{Attributes, Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use deskhand_linux::LinuxEnv;
use deskhand_linux::util::{changed, dbus_err, get_all, prop, signals_from_path};
use serde_json::json;
use zbus::Proxy;

use crate::{KWIN, kwin_session};

pub const NIGHT_LIGHT: EntityId = EntityId::from_static("night_light");
pub const NIGHT_LIGHT_TEMPERATURE: EntityId = EntityId::from_static("night_light_temperature");
pub const NIGHT_LIGHT_INHIBIT: EntityId = EntityId::from_static("night_light_inhibit");
const PATH: &str = "/org/kde/KWin/NightLight";
const IFACE: &str = "org.kde.KWin.NightLight";

/// KWin night light.
#[derive(Debug)]
pub struct NightLight {
    env: Arc<LinuxEnv>,
}

impl NightLight {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }
}

#[async_trait::async_trait]
impl Integration for NightLight {
    fn id(&self) -> &'static str {
        "night_light.kwin"
    }

    fn capability(&self) -> &'static str {
        "night_light"
    }

    fn description(&self) -> &'static str {
        "KWin night light: active, temperature, inhibit switch"
    }

    async fn probe(&self) -> Probe {
        let conn = match kwin_session(&self.env).await {
            Ok(c) => c,
            Err(p) => return p,
        };
        match get_all(&conn, KWIN, PATH, IFACE).await {
            Ok(p) if prop::<bool>(&p, "available") == Some(true) => Probe::available(),
            Ok(_) => Probe::unavailable("night light is not available (no supported output)"),
            Err(e) => Probe::unavailable(format!("no KWin night light: {e}")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let conn = self.env.session_bus().await.map_err(dbus_err)?;
        let proxy = Proxy::new(&conn, KWIN, PATH, IFACE)
            .await
            .map_err(dbus_err)?;
        let mut signals = signals_from_path(&conn, KWIN, Some(PATH))
            .await
            .map_err(dbus_err)?;
        let active = ctx.binary_sensor(
            EntitySpec::binary_sensor(NIGHT_LIGHT, "Night light", None).icon("mdi:weather-night"),
        );
        let temperature = ctx.sensor(
            EntitySpec::sensor(NIGHT_LIGHT_TEMPERATURE, "Night light temperature")
                .unit("K")
                .icon("mdi:thermometer"),
        );
        let (inhibit, mut commands) = ctx.switch(
            EntitySpec::switch(NIGHT_LIGHT_INHIBIT, "Suspend night light")
                .icon("mdi:lightbulb-off"),
        );
        let refresh = async || -> Result<(), IntegrationError> {
            let p = get_all(&conn, KWIN, PATH, IFACE).await.map_err(dbus_err)?;
            let mut attrs = Attributes::new();
            attrs.insert("enabled".into(), json!(prop::<bool>(&p, "enabled")));
            attrs.insert("mode".into(), json!(prop::<u32>(&p, "mode")));
            attrs.insert(
                "target_temperature".into(),
                json!(prop::<u32>(&p, "targetTemperature")),
            );
            active.set_attributes(attrs);
            active.set(prop::<bool>(&p, "running").unwrap_or(false));
            match prop::<u32>(&p, "currentTemperature") {
                Some(t) => temperature.set(t),
                None => temperature.set_unknown(),
            }
            inhibit.set(prop::<bool>(&p, "inhibited").unwrap_or(false));
            Ok(())
        };
        refresh().await?;
        ctx.ready();

        let mut cookie: Option<u32> = None;
        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                c = changed(&mut signals, Duration::from_millis(100)) => {
                    if c.is_none() {
                        return Err(IntegrationError::msg("KWin restarted or went away"));
                    }
                }
                Some(on) = commands.recv() => match (on, cookie) {
                    (true, None) => match proxy.call::<_, _, u32>("inhibit", &()).await {
                        Ok(c) => cookie = Some(c),
                        Err(e) => tracing::warn!(error = %e, "inhibiting night light failed"),
                    },
                    (false, Some(c)) => {
                        if let Err(e) = proxy.call::<_, _, ()>("uninhibit", &(c,)).await {
                            tracing::warn!(error = %e, "uninhibiting night light failed");
                        }
                        cookie = None;
                    }
                    (false, None) => tracing::info!("night light is suspended elsewhere (tray); Deskhand holds no inhibition"),
                    (true, Some(_)) => {}
                },
            }
            refresh().await?;
        }
        if let Some(c) = cookie {
            let _ = proxy.call::<_, _, ()>("uninhibit", &(c,)).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::testing::{rig, wait_state};
    use deskhand_core::{CommandValue, State};
    use deskhand_testkit::mocks::MockKWin;
    use deskhand_testkit::{TestBus, require_dbus};

    use super::*;

    #[tokio::test]
    async fn follows_and_inhibits() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let kwin = MockKWin::start(&bus).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &["KDE"]);
        let integration = NightLight::new(env);
        assert!(integration.probe().await.is_available());
        let (mut t, _cancel) = rig(integration, "night_light");
        wait_state(&mut t, &NIGHT_LIGHT, State::Bool(false)).await;
        wait_state(&mut t, &NIGHT_LIGHT_TEMPERATURE, State::Int(6500)).await;

        kwin.set_night_running(true).await.unwrap();
        wait_state(&mut t, &NIGHT_LIGHT, State::Bool(true)).await;
        wait_state(&mut t, &NIGHT_LIGHT_TEMPERATURE, State::Int(4500)).await;

        t.command(NIGHT_LIGHT_INHIBIT.as_str(), CommandValue::Bool(true));
        wait_state(&mut t, &NIGHT_LIGHT_INHIBIT, State::Bool(true)).await;
        wait_state(&mut t, &NIGHT_LIGHT, State::Bool(false)).await;
        t.command(NIGHT_LIGHT_INHIBIT.as_str(), CommandValue::Bool(false));
        wait_state(&mut t, &NIGHT_LIGHT_INHIBIT, State::Bool(false)).await;
        wait_state(&mut t, &NIGHT_LIGHT, State::Bool(true)).await;
        assert_eq!(
            kwin.calls(),
            vec!["inhibit".to_owned(), "uninhibit(1)".to_owned()]
        );
    }
}
