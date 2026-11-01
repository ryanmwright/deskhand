//! `appearance` capability: `dark_mode` and `accent_colour` from the XDG
//! desktop portal's `org.freedesktop.appearance` settings, which every
//! modern desktop implements.
//!
//! Reading is portable; switching needs a desktop-specific
//! [`ColorSchemeWriter`] (KDE: `plasma-apply-colorscheme`). With a writer
//! `dark_mode` is a switch, without one a binary sensor.
//!
//! ```toml
//! [integrations.appearance]
//! light_scheme = "BreezeLight"   # desktop-specific names used by the writer
//! dark_scheme = "BreezeDark"
//! ```

use std::sync::Arc;

use deskhand_core::ctx::{BinarySensor, Switch};
use deskhand_core::{Attributes, Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use serde::Deserialize;
use serde_json::json;
use zbus::Proxy;
use zbus::zvariant::OwnedValue;

use crate::env::LinuxEnv;
use crate::util::{dbus_err, recv_opt, signals_from_path};

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SETTINGS: &str = "org.freedesktop.portal.Settings";
const NAMESPACE: &str = "org.freedesktop.appearance";

pub const DARK_MODE: EntityId = EntityId::from_static("dark_mode");
pub const ACCENT_COLOUR: EntityId = EntityId::from_static("accent_colour");

/// `[integrations.appearance]` options.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct AppearanceOptions {
    pub light_scheme: Option<String>,
    pub dark_scheme: Option<String>,
}

/// Switches the desktop between light and dark.
#[async_trait::async_trait]
pub trait ColorSchemeWriter: Send + Sync + std::fmt::Debug + 'static {
    async fn set_dark(&self, dark: bool, options: &AppearanceOptions) -> Result<(), String>;
}

/// `#rrggbb` for an accent colour with components in 0..=1; `None` if unset
/// (the portal reports out-of-range values then).
pub fn accent_hex((r, g, b): (f64, f64, f64)) -> Option<String> {
    let ok = |v: f64| (0.0..=1.0).contains(&v);
    if !(ok(r) && ok(g) && ok(b)) {
        return None;
    }
    let c = |v: f64| (v * 255.0).round() as u8;
    Some(format!("#{:02x}{:02x}{:02x}", c(r), c(g), c(b)))
}

/// Dark mode and accent colour.
#[derive(Debug)]
pub struct Appearance {
    env: Arc<LinuxEnv>,
    writer: Option<Arc<dyn ColorSchemeWriter>>,
    id: &'static str,
}

impl Appearance {
    /// Read-only, from the portal.
    pub fn portal(env: Arc<LinuxEnv>) -> Self {
        Self {
            env,
            writer: None,
            id: "appearance.portal",
        }
    }

    /// Read from the portal, write with a desktop-specific writer.
    pub fn with_writer(
        env: Arc<LinuxEnv>,
        id: &'static str,
        writer: Arc<dyn ColorSchemeWriter>,
    ) -> Self {
        Self {
            env,
            writer: Some(writer),
            id,
        }
    }
}

async fn read(settings: &Proxy<'_>, key: &str) -> zbus::Result<OwnedValue> {
    match settings
        .call::<_, _, OwnedValue>("ReadOne", &(NAMESPACE, key))
        .await
    {
        Ok(v) => Ok(v),
        // Portal versions before 2 only have the deprecated Read (value in a variant).
        Err(_) => {
            let v: OwnedValue = settings.call("Read", &(NAMESPACE, key)).await?;
            Ok(match zbus::zvariant::Value::from(v.try_clone()?) {
                zbus::zvariant::Value::Value(inner) => inner.try_to_owned()?,
                _ => v,
            })
        }
    }
}

fn as_dark(v: &OwnedValue) -> Option<bool> {
    // 0: no preference, 1: prefer dark, 2: prefer light.
    u32::try_from(v.try_clone().ok()?).ok().map(|u| u == 1)
}

fn as_accent(v: &OwnedValue) -> Option<(f64, f64, f64)> {
    <(f64, f64, f64)>::try_from(v.try_clone().ok()?).ok()
}

enum DarkHandle {
    Switch(Switch),
    Sensor(BinarySensor),
}

impl DarkHandle {
    fn set(&self, on: Option<bool>) {
        match (self, on) {
            (Self::Switch(s), Some(v)) => s.set(v),
            (Self::Sensor(s), Some(v)) => s.set(v),
            (Self::Switch(s), None) => s.set_unknown(),
            (Self::Sensor(s), None) => s.set_unknown(),
        }
    }
}

#[async_trait::async_trait]
impl Integration for Appearance {
    fn id(&self) -> &'static str {
        self.id
    }

    fn capability(&self) -> &'static str {
        "appearance"
    }

    fn description(&self) -> &'static str {
        if self.writer.is_some() {
            "Dark mode (switchable) and accent colour"
        } else {
            "Dark mode and accent colour from the desktop portal (read-only)"
        }
    }

    async fn probe(&self) -> Probe {
        let Ok(conn) = self.env.session_bus().await else {
            return Probe::unavailable("no session bus");
        };
        if !LinuxEnv::has_owner(&conn, PORTAL).await {
            return Probe::unavailable("no XDG desktop portal on the session bus");
        }
        let Ok(settings) = Proxy::new(&conn, PORTAL, PORTAL_PATH, SETTINGS).await else {
            return Probe::unavailable("no portal Settings interface");
        };
        match read(&settings, "color-scheme").await {
            Ok(_) => Probe::available(),
            Err(e) => Probe::unavailable(format!("portal has no color-scheme setting: {e}")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let options = ctx.config::<AppearanceOptions>()?;
        let conn = self.env.session_bus().await.map_err(dbus_err)?;
        let settings = Proxy::new(&conn, PORTAL, PORTAL_PATH, SETTINGS)
            .await
            .map_err(dbus_err)?;
        let mut signals = signals_from_path(&conn, PORTAL, Some(PORTAL_PATH))
            .await
            .map_err(dbus_err)?;

        let spec = EntitySpec::switch(DARK_MODE, "Dark mode").icon("mdi:theme-light-dark");
        let (dark, mut commands) = match &self.writer {
            Some(_) => {
                let (s, c) = ctx.switch(spec);
                (DarkHandle::Switch(s), Some(c))
            }
            None => {
                let mut spec = spec;
                spec.kind = deskhand_core::EntityKind::BinarySensor { device_class: None };
                (DarkHandle::Sensor(ctx.binary_sensor(spec)), None)
            }
        };
        let accent =
            ctx.sensor(EntitySpec::sensor(ACCENT_COLOUR, "Accent colour").icon("mdi:palette"));
        let set_accent =
            |v: Option<(f64, f64, f64)>| match v.and_then(|c| accent_hex(c).map(|h| (c, h))) {
                Some(((r, g, b), hex)) => {
                    let mut attrs = Attributes::new();
                    let c = |v: f64| (v * 255.0).round() as u8;
                    attrs.insert("rgb".into(), json!([c(r), c(g), c(b)]));
                    accent.set_attributes(attrs);
                    accent.set(hex);
                }
                None => accent.set_unknown(),
            };
        dark.set(
            read(&settings, "color-scheme")
                .await
                .ok()
                .as_ref()
                .and_then(as_dark),
        );
        set_accent(
            read(&settings, "accent-color")
                .await
                .ok()
                .as_ref()
                .and_then(as_accent),
        );
        ctx.ready();

        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                msg = signals.next() => {
                    let Some(msg) = msg else {
                        return Err(IntegrationError::msg("desktop portal restarted or went away"));
                    };
                    let header = msg.header();
                    if header.member().map(|m| m.as_str()) != Some("SettingChanged") {
                        continue;
                    }
                    let Ok((ns, key, value)) = msg.body().deserialize::<(String, String, OwnedValue)>() else {
                        continue;
                    };
                    if ns != NAMESPACE {
                        continue;
                    }
                    match key.as_str() {
                        "color-scheme" => dark.set(as_dark(&value)),
                        "accent-color" => set_accent(as_accent(&value)),
                        _ => {}
                    }
                }
                Some(on) = recv_opt(commands.as_mut()) => {
                    let Some(writer) = &self.writer else { continue };
                    tracing::info!(dark = on, "switching colour scheme from Home Assistant");
                    if let Err(e) = writer.set_dark(on, &options).await {
                        tracing::warn!(error = %e, "switching colour scheme failed");
                    }
                    // The portal reports the real result; re-read in case it did not change.
                    dark.set(read(&settings, "color-scheme").await.ok().as_ref().and_then(as_dark));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use deskhand_core::{CommandValue, State};
    use deskhand_testkit::mocks::MockPortal;
    use deskhand_testkit::{TestBus, require_dbus};

    use super::*;
    use crate::testutil::{rig, rig_with, wait_attr, wait_state};

    #[test]
    fn accent_colours() {
        assert_eq!(
            accent_hex((0.239216, 0.682353, 0.913725)).as_deref(),
            Some("#3daee9")
        );
        assert_eq!(accent_hex((-1.0, -1.0, -1.0)), None);
        assert_eq!(accent_hex((1.0, 0.0, 0.0)).as_deref(), Some("#ff0000"));
    }

    #[derive(Debug)]
    struct FakeWriter {
        portal: MockPortal,
        calls: Mutex<Vec<bool>>,
    }

    #[async_trait::async_trait]
    impl ColorSchemeWriter for FakeWriter {
        async fn set_dark(&self, dark: bool, options: &AppearanceOptions) -> Result<(), String> {
            assert_eq!(options.dark_scheme.as_deref(), Some("Night"));
            self.calls.lock().unwrap().push(dark);
            self.portal
                .set_color_scheme(if dark { 1 } else { 2 })
                .await
                .map_err(|e| e.to_string())
        }
    }

    #[tokio::test]
    async fn follows_and_switches_the_portal() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let portal = MockPortal::start(&bus).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &[]);

        // Read-only: a binary sensor.
        let ro = Appearance::portal(env.clone());
        assert!(ro.probe().await.is_available());
        let (mut t, cancel) = rig(ro, "appearance");
        wait_state(&mut t, &DARK_MODE, State::Bool(false)).await;
        wait_state(&mut t, &ACCENT_COLOUR, State::Text("#3daee9".into())).await;
        wait_attr(&mut t, &ACCENT_COLOUR, "rgb", json!([61, 174, 233])).await;
        portal.set_color_scheme(1).await.unwrap();
        wait_state(&mut t, &DARK_MODE, State::Bool(true)).await;
        portal.set_accent((-1.0, -1.0, -1.0)).await.unwrap();
        wait_state(&mut t, &ACCENT_COLOUR, State::Unknown).await;
        cancel.cancel();

        // With a writer: a switch that goes through it.
        let writer = Arc::new(FakeWriter {
            portal: portal.clone(),
            calls: Mutex::new(Vec::new()),
        });
        let rw = Appearance::with_writer(env, "appearance.test", writer.clone());
        let config = deskhand_core::Config::from_toml_str(
            "[integrations.appearance]\ndark_scheme = 'Night'",
            None,
        )
        .unwrap()
        .integration("appearance")
        .unwrap();
        let (mut t, _cancel) = rig_with(rw, config);
        wait_state(&mut t, &DARK_MODE, State::Bool(true)).await;
        t.command(DARK_MODE.as_str(), CommandValue::Bool(false));
        wait_state(&mut t, &DARK_MODE, State::Bool(false)).await;
        assert_eq!(*writer.calls.lock().unwrap(), vec![false]);
    }
}
