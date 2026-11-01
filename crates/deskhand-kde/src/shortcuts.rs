//! `shortcuts` capability through kglobalaccel (hosted by KWin).
//!
//! | entity | |
//! |---|---|
//! | `shortcut_<id>` | event `pressed`, one per `[[shortcut]]` |
//! | `kde_shortcut_<id>` | button that invokes an allowlisted KDE shortcut (`[integrations.shortcuts] invoke`) |
//!
//! Custom shortcuts are actions of the kglobalaccel component `deskhand`
//! (fixed, not the app id, so renaming the app never orphans the user's
//! bindings). They show up in System Settings → Shortcuts, where the user
//! can rebind them; the binding persists in `kglobalshortcutsrc`. Actions
//! of that component that are no longer in the config are unregistered.
//!
//! Home Assistant can only invoke the KDE shortcuts listed in the config:
//!
//! ```toml
//! [integrations.shortcuts]
//! invoke = [{ component = "kwin", action = "Overview", name = "Overview" }]
//! ```

use std::collections::BTreeSet;
use std::sync::Arc;

use deskhand_common::shortcuts::Shortcut;
use deskhand_core::{Attributes, Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use deskhand_linux::LinuxEnv;
use deskhand_linux::util::{dbus_err, signals_from_path};
use serde::Deserialize;
use tokio::sync::mpsc;
use zbus::Proxy;
use zbus::zvariant::OwnedObjectPath;

use crate::keys::{self, KeySequence, WireSequence};

pub const KGLOBALACCEL: &str = "org.kde.kglobalaccel";
const ACCEL_PATH: &str = "/kglobalaccel";
const ACCEL_IFACE: &str = "org.kde.KGlobalAccel";
const COMPONENT_IFACE: &str = "org.kde.kglobalaccel.Component";
/// Our kglobalaccel component. Never derive it from `APP_ID`.
pub const COMPONENT: &str = "deskhand";
const COMPONENT_FRIENDLY: &str = "Deskhand";

// KGlobalAccel::SetShortcutFlag
const SET_PRESENT: u32 = 2;
const IS_DEFAULT: u32 = 8;

/// `[integrations.shortcuts]` options.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ShortcutsOptions {
    /// KDE shortcuts Home Assistant may invoke, one button each.
    pub invoke: Vec<InvokeConfig>,
}

/// One allowlisted KDE shortcut.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InvokeConfig {
    /// kglobalaccel component, e.g. `kwin`, `org_kde_powerdevil`.
    pub component: String,
    /// Action name as kglobalaccel knows it, e.g. `Overview`.
    pub action: String,
    /// Button name (default: the action).
    pub name: Option<String>,
    /// Entity id suffix (default: from the action): `kde_shortcut_<id>`.
    pub id: Option<String>,
}

/// A `[[shortcut]]` with parsed keys.
#[derive(Debug, Clone)]
struct Custom {
    shortcut: Shortcut,
    keys: Vec<KeySequence>,
}

/// Custom global shortcuts and invoke buttons.
#[derive(Debug)]
pub struct Shortcuts {
    env: Arc<LinuxEnv>,
    custom: Vec<Custom>,
    invoke: Vec<InvokeConfig>,
}

impl Shortcuts {
    /// Fails if a `default_keys` entry does not parse.
    pub fn new(
        env: Arc<LinuxEnv>,
        shortcuts: Vec<Shortcut>,
        invoke: Vec<InvokeConfig>,
    ) -> Result<Self, String> {
        let custom = shortcuts
            .into_iter()
            .map(|s| {
                let keys = keys::parse_all(&s.default_keys)
                    .map_err(|e| format!("[[shortcut]] {:?}: default_keys {e}", s.id))?;
                Ok(Custom { shortcut: s, keys })
            })
            .collect::<Result<_, String>>()?;
        Ok(Self {
            env,
            custom,
            invoke,
        })
    }
}

/// kglobalaccel's action id (`as`, not a struct): `[component, action,
/// component name, action name]`.
fn action_id<'a>(action: &'a str, name: &'a str) -> Vec<&'a str> {
    vec![COMPONENT, action, COMPONENT_FRIENDLY, name]
}

/// The entity id of an invoke entry.
fn invoke_entity(c: &InvokeConfig) -> Result<EntityId, String> {
    let suffix = match &c.id {
        Some(id) => EntityId::new(id.clone())
            .map_err(|_| format!("invoke id {id:?}: lowercase letters, digits and _ only"))?,
        None => EntityId::slugify(&c.action),
    };
    EntityId::new(format!("kde_shortcut_{}", suffix.as_str()))
        .map_err(|_| format!("invoke id {:?} is too long", suffix.as_str()))
}

async fn accel(conn: &zbus::Connection) -> zbus::Result<Proxy<'static>> {
    Proxy::new(conn, KGLOBALACCEL, ACCEL_PATH, ACCEL_IFACE).await
}

async fn component(conn: &zbus::Connection, name: &str) -> zbus::Result<Proxy<'static>> {
    let path: OwnedObjectPath = accel(conn).await?.call("getComponent", &(name,)).await?;
    Proxy::new(conn, KGLOBALACCEL, path, COMPONENT_IFACE).await
}

/// Invokes a KDE global shortcut (as if its keys were pressed).
pub async fn invoke_shortcut(
    conn: &zbus::Connection,
    component_name: &str,
    action: &str,
) -> zbus::Result<()> {
    component(conn, component_name)
        .await?
        .call("invokeShortcut", &(action,))
        .await
}

/// Registers one custom shortcut; returns the active keys (the user's own
/// if they rebound it).
async fn register(accel: &Proxy<'_>, c: &Custom) -> zbus::Result<Vec<WireSequence>> {
    let id = action_id(&c.shortcut.id, &c.shortcut.name);
    let wire = keys::wire_all(&c.keys);
    accel.call::<_, _, ()>("doRegister", &(&id,)).await?;
    accel
        .call::<_, _, Vec<WireSequence>>("setShortcutKeys", &(&id, &wire, IS_DEFAULT))
        .await?;
    accel
        .call("setShortcutKeys", &(&id, &wire, SET_PRESENT))
        .await
}

#[async_trait::async_trait]
impl Integration for Shortcuts {
    fn id(&self) -> &'static str {
        "shortcuts.kglobalaccel"
    }

    fn capability(&self) -> &'static str {
        "shortcuts"
    }

    fn description(&self) -> &'static str {
        "Custom global shortcuts ([[shortcut]]) as events and allowlisted KDE shortcuts as buttons"
    }

    async fn probe(&self) -> Probe {
        if self.custom.is_empty() && self.invoke.is_empty() {
            return Probe::unavailable(
                "no [[shortcut]] entries and no [integrations.shortcuts] invoke list",
            );
        }
        if !self.env.desktop_is("KDE") {
            return Probe::unavailable("not a KDE Plasma session");
        }
        match self.env.session_bus().await {
            Ok(c) if LinuxEnv::has_owner(&c, KGLOBALACCEL).await => Probe::available_with(format!(
                "{} shortcut(s), {} invoke button(s)",
                self.custom.len(),
                self.invoke.len()
            )),
            Ok(_) => Probe::unavailable("kglobalaccel is not on the session bus"),
            Err(e) => Probe::unavailable(format!("session bus: {e}")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        // Read for its unknown-key warnings; the list came from the wiring.
        let _: ShortcutsOptions = ctx.config()?;
        let conn = self.env.session_bus().await.map_err(dbus_err)?;
        let accel = accel(&conn).await.map_err(dbus_err)?;

        // Remove stale actions first: one may hold the keys a new one wants.
        let wanted: BTreeSet<&str> = self.custom.iter().map(|c| c.shortcut.id.as_str()).collect();
        if let Ok(ours) = component(&conn, COMPONENT).await {
            let names: Vec<String> = ours.call("shortcutNames", &()).await.map_err(dbus_err)?;
            for stale in names.iter().filter(|n| !wanted.contains(n.as_str())) {
                match accel
                    .call::<_, _, bool>("unregister", &(COMPONENT, stale))
                    .await
                {
                    Ok(_) => {
                        tracing::info!(action = %stale, "removed global shortcut no longer in the config")
                    }
                    Err(e) => {
                        tracing::warn!(action = %stale, error = %e, "cannot remove old global shortcut")
                    }
                }
            }
        }

        let mut events = Vec::new();
        for c in &self.custom {
            let active = keys::describe(&register(&accel, c).await.map_err(dbus_err)?);
            if active.is_empty() && !c.keys.is_empty() {
                tracing::warn!(shortcut = %c.shortcut.id, default_keys = ?c.shortcut.default_keys,
                    "global shortcut has no keys: the default keys are taken by another shortcut; bind it in System Settings");
            } else {
                tracing::info!(shortcut = %c.shortcut.id, keys = ?active, "global shortcut registered");
            }
            let event = ctx.event(
                EntitySpec::event(
                    c.shortcut.entity.clone(),
                    c.shortcut.name.clone(),
                    vec!["pressed".into()],
                )
                .icon("mdi:keyboard"),
            );
            events.push((c.shortcut.id.clone(), event));
        }

        let mut signals = None;
        if !self.custom.is_empty() {
            let ours = component(&conn, COMPONENT).await.map_err(dbus_err)?;
            signals = Some(
                signals_from_path(&conn, KGLOBALACCEL, Some(ours.path().as_str()))
                    .await
                    .map_err(dbus_err)?,
            );
        }

        let (press_tx, mut press_rx) = mpsc::unbounded_channel::<usize>();
        let mut forwarders = tokio::task::JoinSet::new();
        let mut seen = BTreeSet::new();
        for (i, inv) in self.invoke.iter().enumerate() {
            let entity = match invoke_entity(inv) {
                Ok(e) if seen.insert(e.clone()) => e,
                Ok(e) => {
                    tracing::warn!(entity = %e, "integrations.shortcuts.invoke: duplicate id, skipped");
                    continue;
                }
                Err(e) => {
                    tracing::warn!("integrations.shortcuts.invoke: {e}, skipped");
                    continue;
                }
            };
            match component(&conn, &inv.component).await {
                Ok(p) => {
                    let names: Vec<String> = p.call("shortcutNames", &()).await.unwrap_or_default();
                    if !names.contains(&inv.action) {
                        tracing::warn!(component = %inv.component, action = %inv.action, "KDE shortcut not found (yet); the button may not work");
                    }
                }
                Err(e) => {
                    tracing::warn!(component = %inv.component, error = %e, "KDE shortcut component not found (yet); the button may not work")
                }
            }
            let mut presses = ctx.button(
                EntitySpec::button(
                    entity,
                    inv.name.clone().unwrap_or_else(|| inv.action.clone()),
                )
                .icon("mdi:keyboard-outline"),
            );
            let tx = press_tx.clone();
            forwarders.spawn(async move {
                while presses.recv().await.is_some() {
                    if tx.send(i).is_err() {
                        break;
                    }
                }
            });
        }
        ctx.ready();

        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                m = async {
                    match signals.as_mut() {
                        Some(s) => s.next().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let Some(m) = m else {
                        return Err(IntegrationError::msg("kglobalaccel restarted or went away"));
                    };
                    if m.header().member().map(|n| n.as_str()) != Some("globalShortcutPressed") {
                        continue;
                    }
                    let Ok((comp, action, _ts)) = m.body().deserialize::<(String, String, i64)>() else { continue };
                    if comp != COMPONENT {
                        continue;
                    }
                    if let Some((_, event)) = events.iter().find(|(id, _)| *id == action) {
                        tracing::debug!(shortcut = %action, "global shortcut pressed");
                        event.fire("pressed", Attributes::new());
                    }
                }
                Some(i) = press_rx.recv() => {
                    let inv = &self.invoke[i];
                    tracing::info!(component = %inv.component, action = %inv.action, "invoking KDE shortcut");
                    if let Err(e) = invoke_shortcut(&conn, &inv.component, &inv.action).await {
                        tracing::warn!(component = %inv.component, action = %inv.action, error = %e, "invoking KDE shortcut failed");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use deskhand_common::shortcuts::Shortcut;
    use deskhand_core::CommandValue;
    use deskhand_core::testing::{rig, wait_entity, wait_event};
    use deskhand_testkit::mocks::MockKGlobalAccel;
    use deskhand_testkit::{TestBus, require_dbus};

    use super::*;

    fn shortcut(id: &str, keys: &[&str]) -> Shortcut {
        Shortcut {
            id: id.into(),
            entity: EntityId::new(format!("shortcut_{id}")).unwrap(),
            name: id.into(),
            default_keys: keys.iter().map(|k| k.to_string()).collect(),
        }
    }

    #[test]
    fn invoke_ids() {
        let inv = |action: &str, id: Option<&str>| InvokeConfig {
            component: "kwin".into(),
            action: action.into(),
            name: None,
            id: id.map(Into::into),
        };
        assert_eq!(
            invoke_entity(&inv("Turn Off Screen", None))
                .unwrap()
                .as_str(),
            "kde_shortcut_turn_off_screen"
        );
        assert_eq!(
            invoke_entity(&inv("x", Some("overview"))).unwrap().as_str(),
            "kde_shortcut_overview"
        );
        assert!(invoke_entity(&inv("x", Some("Bad Id"))).is_err());
    }

    #[test]
    fn bad_default_keys_are_rejected() {
        let env =
            LinuxEnv::with_addresses("unix:path=/nonexistent", "unix:path=/nonexistent", &["KDE"]);
        let err = Shortcuts::new(env, vec![shortcut("a", &["Hyper+A"])], vec![]).unwrap_err();
        assert!(err.contains("Hyper"), "{err}");
    }

    #[tokio::test]
    async fn registers_fires_and_invokes() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let accel = MockKGlobalAccel::start(&bus).await.unwrap();
        accel.add_component("kwin", &["Overview"]).await.unwrap();
        // Left over from an older config: removed on start.
        accel.add_action(COMPONENT, "old").await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &["KDE"]);
        let integration = Shortcuts::new(
            env,
            vec![shortcut("lights", &["Meta+Shift+L"]), shortcut("free", &[])],
            vec![InvokeConfig {
                component: "kwin".into(),
                action: "Overview".into(),
                name: None,
                id: None,
            }],
        )
        .unwrap();
        assert!(integration.probe().await.is_available());
        let (mut t, _cancel) = rig(integration, "shortcuts");
        wait_entity(
            &mut t,
            &EntityId::from_static("kde_shortcut_overview"),
            true,
        )
        .await;

        // Every key list on the wire had four ints per sequence.
        assert_eq!(
            accel.keys(COMPONENT, "lights"),
            Some(vec![vec![0x1200_004c, 0, 0, 0]])
        );
        assert_eq!(accel.keys(COMPONENT, "free"), Some(vec![]));
        assert_eq!(accel.keys(COMPONENT, "old"), None);
        assert!(
            accel
                .calls()
                .contains(&"unregister(deskhand,old)".to_owned())
        );

        accel.press(COMPONENT, "lights").await.unwrap();
        wait_event(&mut t, &EntityId::from_static("shortcut_lights"), "pressed").await;

        t.command("kde_shortcut_overview", CommandValue::Press);
        for _ in 0..50 {
            if accel
                .calls()
                .contains(&"invokeShortcut(kwin,Overview)".to_owned())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            accel
                .calls()
                .contains(&"invokeShortcut(kwin,Overview)".to_owned()),
            "{:?}",
            accel.calls()
        );
    }
}
