//! `brightness` capability: one `brightness_<display>` number (0–100 %)
//! per display PowerDevil controls through `org.kde.ScreenBrightness`
//! (Plasma 6.2+): laptop backlights and DDC/CI monitors alike.
//!
//! The id comes from the display's label (`brightness_built_in_screen`,
//! `brightness_dell_u2723qe`; ` 2` for duplicates), which stays the same
//! across hotplugs, unlike PowerDevil's `display<N>` names. Displays that
//! appear or disappear add or remove their entity. Attributes: `display`
//! (PowerDevil's name), `internal`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use deskhand_core::ctx::Number;
use deskhand_core::entity::NumberMode;
use deskhand_core::{
    Attributes, Ctx, EntityId, EntityKind, EntitySpec, Integration, IntegrationError, Probe,
};
use deskhand_linux::LinuxEnv;
use deskhand_linux::util::{Props, changed, dbus_err, get_all, prop, signals_from};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use zbus::Proxy;

pub const SERVICE: &str = "org.kde.ScreenBrightness";
const PATH: &str = "/org/kde/ScreenBrightness";
const IFACE: &str = "org.kde.ScreenBrightness";
const DISPLAY_IFACE: &str = "org.kde.ScreenBrightness.Display";
/// `SetBrightness` flag: no on-screen indicator.
const SUPPRESS_INDICATOR: u32 = 1;
const QUIET: Duration = Duration::from_millis(100);

/// One display as PowerDevil reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    /// PowerDevil's object name (`display0`).
    pub dbus_name: String,
    pub label: String,
    pub internal: bool,
    pub value: i32,
    pub max: i32,
}

impl Display {
    fn percent(&self) -> f64 {
        if self.max <= 0 {
            return 0.0;
        }
        (f64::from(self.value) * 100.0 / f64::from(self.max)).round()
    }

    fn raw(&self, percent: f64) -> i32 {
        let pct = percent.clamp(0.0, 100.0);
        (pct * f64::from(self.max) / 100.0).round() as i32
    }
}

/// Entity id and Home Assistant name per display, in PowerDevil's order.
pub fn entity_names(displays: &[Display]) -> Vec<(EntityId, String)> {
    let mut seen: BTreeMap<String, u32> = BTreeMap::new();
    displays
        .iter()
        .map(|d| {
            let base = if d.label.trim().is_empty() {
                d.dbus_name.clone()
            } else {
                d.label.trim().to_owned()
            };
            let n = seen.entry(base.clone()).or_insert(0);
            *n += 1;
            let label = if *n == 1 { base } else { format!("{base} {n}") };
            let id = EntityId::new(format!("brightness_{}", EntityId::slugify(&label).as_str()))
                .unwrap_or_else(|_| EntityId::slugify(&format!("brightness_{label}")));
            (id, format!("Brightness {label}"))
        })
        .collect()
}

async fn read(conn: &zbus::Connection) -> zbus::Result<Vec<Display>> {
    let root = get_all(conn, SERVICE, PATH, IFACE).await?;
    let names: Vec<String> = prop(&root, "DisplaysDBusNames").unwrap_or_default();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let p: Props = match get_all(conn, SERVICE, &format!("{PATH}/{name}"), DISPLAY_IFACE).await
        {
            Ok(p) => p,
            // Removed between the two calls.
            Err(e) => {
                tracing::debug!(display = %name, error = %e, "cannot read display");
                continue;
            }
        };
        out.push(Display {
            label: prop(&p, "Label").unwrap_or_default(),
            internal: prop(&p, "IsInternal").unwrap_or(false),
            value: prop(&p, "Brightness").unwrap_or(0),
            max: prop(&p, "MaxBrightness").unwrap_or(0),
            dbus_name: name,
        });
    }
    Ok(out)
}

fn spec(id: EntityId, name: String, internal: bool) -> EntitySpec {
    let mut spec = EntitySpec::number(id, name, 0.0, 100.0, 1.0)
        .unit("%")
        .icon(if internal {
            "mdi:brightness-6"
        } else {
            "mdi:monitor"
        });
    if let EntityKind::Number { mode, .. } = &mut spec.kind {
        *mode = NumberMode::Slider;
    }
    spec
}

/// A display's number and the task forwarding its commands.
struct DisplayEntity {
    number: Number,
    forwarder: AbortHandle,
}

impl Drop for DisplayEntity {
    fn drop(&mut self) {
        // The forwarder owns the command stream; stopping it lets the entity unregister.
        self.forwarder.abort();
    }
}

/// Screen brightness through PowerDevil.
#[derive(Debug)]
pub struct Brightness {
    env: Arc<LinuxEnv>,
}

impl Brightness {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }
}

#[async_trait::async_trait]
impl Integration for Brightness {
    fn id(&self) -> &'static str {
        "brightness.powerdevil"
    }

    fn capability(&self) -> &'static str {
        "brightness"
    }

    fn description(&self) -> &'static str {
        "Per-display brightness (backlight and DDC/CI) through PowerDevil"
    }

    async fn probe(&self) -> Probe {
        if !self.env.desktop_is("KDE") {
            return Probe::unavailable("not a KDE Plasma session");
        }
        let conn = match self.env.session_bus().await {
            Ok(c) => c,
            Err(e) => return Probe::unavailable(format!("session bus: {e}")),
        };
        if !LinuxEnv::has_owner(&conn, SERVICE).await {
            return Probe::unavailable(
                "PowerDevil's ScreenBrightness is not on the session bus (Plasma 6.2+)",
            );
        }
        match read(&conn).await {
            Ok(d) => Probe::available_with(format!("{} display(s)", d.len())),
            Err(e) => Probe::unavailable(format!("ScreenBrightness: {e}")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let conn = self.env.session_bus().await.map_err(dbus_err)?;
        let mut signals = signals_from(&conn, SERVICE).await.map_err(dbus_err)?;
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<(EntityId, f64)>();
        let mut entities: BTreeMap<EntityId, DisplayEntity> = BTreeMap::new();

        let reconcile = |read: Vec<Display>,
                         entities: &mut BTreeMap<EntityId, DisplayEntity>|
         -> Vec<(EntityId, Display)> {
            let names = entity_names(&read);
            entities.retain(|id, _| names.iter().any(|(n, _)| n == id));
            let mut out = Vec::with_capacity(read.len());
            for ((id, name), d) in names.into_iter().zip(read) {
                let entity = entities.entry(id.clone()).or_insert_with(|| {
                    tracing::info!(display = %d.dbus_name, label = %d.label, entity = %id, "brightness control added");
                    let (number, mut commands) = ctx.number(spec(id.clone(), name, d.internal));
                    let tx = cmd_tx.clone();
                    let entity_id = id.clone();
                    let forwarder = tokio::spawn(async move {
                        while let Some(v) = commands.recv().await {
                            if tx.send((entity_id.clone(), v)).is_err() {
                                break;
                            }
                        }
                    })
                    .abort_handle();
                    DisplayEntity { number, forwarder }
                });
                let mut attrs = Attributes::new();
                attrs.insert("display".into(), json!(d.dbus_name));
                attrs.insert("internal".into(), json!(d.internal));
                entity.number.set_attributes(attrs);
                entity.number.set(d.percent());
                out.push((id, d));
            }
            out
        };
        let mut displays = reconcile(read(&conn).await.map_err(dbus_err)?, &mut entities);
        ctx.ready();

        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                c = changed(&mut signals, QUIET) => {
                    if c.is_none() {
                        return Err(IntegrationError::msg("PowerDevil restarted or went away"));
                    }
                    match read(&conn).await {
                        Ok(r) => displays = reconcile(r, &mut entities),
                        Err(e) => tracing::debug!(error = %e, "reading displays failed"),
                    }
                }
                Some((id, percent)) = cmd_rx.recv() => {
                    let Some((_, d)) = displays.iter().find(|(i, _)| *i == id) else { continue };
                    let raw = d.raw(percent);
                    tracing::info!(display = %d.dbus_name, percent, raw, "brightness from Home Assistant");
                    let path = format!("{PATH}/{}", d.dbus_name);
                    let res = async {
                        Proxy::new(&conn, SERVICE, path.as_str(), DISPLAY_IFACE)
                            .await?
                            .call::<_, _, ()>("SetBrightness", &(raw, SUPPRESS_INDICATOR))
                            .await
                    }
                    .await;
                    if let Err(e) = res {
                        tracing::warn!(display = %d.dbus_name, error = %e, "setting brightness failed");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::testing::{rig, wait_attr, wait_entity, wait_state};
    use deskhand_core::{CommandValue, State};
    use deskhand_testkit::mocks::MockScreenBrightness;
    use deskhand_testkit::{TestBus, require_dbus};

    use super::*;

    fn display(name: &str, label: &str) -> Display {
        Display {
            dbus_name: name.into(),
            label: label.into(),
            internal: false,
            value: 0,
            max: 100,
        }
    }

    #[test]
    fn ids_from_labels() {
        let names = entity_names(&[
            display("display0", "Built-in Screen"),
            display("display1", "DELL U2723QE"),
            display("display2", "DELL U2723QE"),
            display("display3", ""),
        ]);
        let ids: Vec<&str> = names.iter().map(|(i, _)| i.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "brightness_built_in_screen",
                "brightness_dell_u2723qe",
                "brightness_dell_u2723qe_2",
                "brightness_display3"
            ]
        );
        assert_eq!(names[2].1, "Brightness DELL U2723QE 2");
    }

    #[test]
    fn percent_conversion() {
        let d = Display {
            max: 19393,
            value: 9697,
            ..display("d", "x")
        };
        assert_eq!(d.percent(), 50.0);
        assert_eq!(d.raw(100.0), 19393);
        assert_eq!(d.raw(150.0), 19393);
        assert_eq!(d.raw(25.0), 4848);
        assert_eq!(Display { max: 0, ..d }.percent(), 0.0);
    }

    #[tokio::test]
    async fn follows_sets_and_tracks_displays() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let mock = MockScreenBrightness::start(&bus).await.unwrap();
        mock.add_display("display0", "Built-in Screen", true, 1000, 500)
            .await
            .unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &["KDE"]);
        let integration = Brightness::new(env);
        assert!(integration.probe().await.is_available());
        let (mut t, _cancel) = rig(integration, "brightness");
        let builtin = EntityId::from_static("brightness_built_in_screen");
        wait_state(&mut t, &builtin, State::Float(50.0)).await;
        wait_attr(&mut t, &builtin, "internal", json!(true)).await;

        mock.set_brightness("display0", 250).await.unwrap();
        wait_state(&mut t, &builtin, State::Float(25.0)).await;

        t.command(builtin.as_str(), CommandValue::Number(80.0));
        wait_state(&mut t, &builtin, State::Float(80.0)).await;
        assert!(
            mock.calls()
                .contains(&"display0.SetBrightness(800,1)".to_owned())
        );

        mock.add_display("display1", "DELL U2723QE", false, 100, 30)
            .await
            .unwrap();
        let dell = EntityId::from_static("brightness_dell_u2723qe");
        wait_state(&mut t, &dell, State::Float(30.0)).await;
        mock.remove_display("display1").await.unwrap();
        wait_entity(&mut t, &dell, false).await;
    }
}
