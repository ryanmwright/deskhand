//! `systemd` capability: one switch per allowlisted systemd *user* unit.
//!
//! ```toml
//! [integrations.systemd]
//! units = ["syncthing.service", "backup.timer"]
//! ```
//! Only units listed here can be started or stopped from Home Assistant.

use std::sync::Arc;
use std::time::Duration;

use deskhand_core::ctx::Switch;
use deskhand_core::{Attributes, Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use zbus::Proxy;
use zbus::zvariant::OwnedObjectPath;

use crate::env::LinuxEnv;
use crate::util::{changed, dbus_err, get_all, prop, signals_from};

const SYSTEMD: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_IFACE: &str = "org.freedesktop.systemd1.Manager";
const QUIET: Duration = Duration::from_millis(250);

/// `[integrations.systemd]` options.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SystemdOptions {
    /// User units to expose (full names, e.g. `syncthing.service`).
    pub units: Vec<String>,
}

/// `syncthing.service` → `systemd_syncthing`; other types keep their suffix.
pub fn unit_entity_id(unit: &str) -> EntityId {
    let base = unit.strip_suffix(".service").unwrap_or(unit);
    EntityId::from_static("systemd").join(base)
}

fn is_on(active_state: &str) -> bool {
    matches!(
        active_state,
        "active" | "activating" | "reloading" | "refreshing"
    )
}

/// Switches for allowlisted systemd user units.
#[derive(Debug)]
pub struct SystemdUnits {
    env: Arc<LinuxEnv>,
    /// From config (read when the binary builds the candidate list, so the
    /// probe can say "nothing configured").
    units: Vec<String>,
}

impl SystemdUnits {
    pub fn new(env: Arc<LinuxEnv>, units: Vec<String>) -> Self {
        Self { env, units }
    }
}

struct Unit {
    name: String,
    path: OwnedObjectPath,
    switch: Switch,
}

async fn refresh(conn: &zbus::Connection, unit: &Unit) {
    match get_all(
        conn,
        SYSTEMD,
        unit.path.as_str(),
        "org.freedesktop.systemd1.Unit",
    )
    .await
    {
        Ok(props) => {
            let state: String = prop(&props, "ActiveState").unwrap_or_default();
            let mut attrs = Attributes::new();
            attrs.insert("unit".into(), json!(unit.name));
            attrs.insert("active_state".into(), json!(state));
            attrs.insert(
                "sub_state".into(),
                json!(prop::<String>(&props, "SubState")),
            );
            unit.switch.set_attributes(attrs);
            unit.switch.set(is_on(&state));
        }
        Err(e) => {
            tracing::debug!(unit = %unit.name, error = %e, "reading unit state failed");
            unit.switch.set_unknown();
        }
    }
}

#[async_trait::async_trait]
impl Integration for SystemdUnits {
    fn id(&self) -> &'static str {
        "systemd.user"
    }

    fn capability(&self) -> &'static str {
        "systemd"
    }

    fn description(&self) -> &'static str {
        "Switches for the systemd user units allowlisted in [integrations.systemd]"
    }

    async fn probe(&self) -> Probe {
        if self.units.is_empty() {
            return Probe::unavailable("no units in [integrations.systemd] units");
        }
        match self.env.session_bus().await {
            Ok(c) if LinuxEnv::has_owner(&c, SYSTEMD).await => {
                Probe::available_with(format!("{} unit(s)", self.units.len()))
            }
            Ok(_) => Probe::unavailable("no systemd user manager on the session bus"),
            Err(e) => Probe::unavailable(format!("session bus: {e}")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let conn = self.env.session_bus().await.map_err(dbus_err)?;
        let mut signals = signals_from(&conn, SYSTEMD).await.map_err(dbus_err)?;
        let manager = Proxy::new(&conn, SYSTEMD, MANAGER_PATH, MANAGER_IFACE)
            .await
            .map_err(dbus_err)?;
        // Without Subscribe, systemd does not send unit change signals.
        let () = manager.call("Subscribe", &()).await.map_err(dbus_err)?;

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<(usize, bool)>();
        let mut units = Vec::new();
        let mut forwarders = tokio::task::JoinSet::new();
        for name in &self.units {
            let path: OwnedObjectPath = match manager.call("LoadUnit", &(name.as_str(),)).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(unit = %name, error = %e, "integrations.systemd: cannot load unit");
                    continue;
                }
            };
            let (switch, mut commands) = ctx.switch(
                EntitySpec::switch(unit_entity_id(name), name.clone()).icon("mdi:cog-play"),
            );
            let idx = units.len();
            let tx = cmd_tx.clone();
            forwarders.spawn(async move {
                while let Some(on) = commands.recv().await {
                    if tx.send((idx, on)).is_err() {
                        break;
                    }
                }
            });
            let unit = Unit {
                name: name.clone(),
                path,
                switch,
            };
            refresh(&conn, &unit).await;
            units.push(unit);
        }
        ctx.ready();

        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                c = changed(&mut signals, QUIET) => {
                    if c.is_none() {
                        return Err(IntegrationError::msg("service restarted or went away"));
                    }
                    for u in &units {
                        refresh(&conn, u).await;
                    }
                }
                Some((idx, on)) = cmd_rx.recv() => {
                    let unit = &units[idx];
                    let method = if on { "StartUnit" } else { "StopUnit" };
                    tracing::info!(unit = %unit.name, method, "systemd unit command from Home Assistant");
                    if let Err(e) = manager
                        .call::<_, _, OwnedObjectPath>(method, &(unit.name.as_str(), "replace"))
                        .await
                    {
                        tracing::warn!(unit = %unit.name, error = %e, "systemd command failed");
                    }
                    refresh(&conn, unit).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::{CommandValue, Config, State};
    use deskhand_testkit::mocks::MockSystemd;
    use deskhand_testkit::{TestBus, require_dbus};

    use super::*;
    use crate::testutil::{rig, wait_state};

    #[test]
    fn entity_ids() {
        assert_eq!(
            unit_entity_id("syncthing.service").as_str(),
            "systemd_syncthing"
        );
        assert_eq!(
            unit_entity_id("backup.timer").as_str(),
            "systemd_backup_timer"
        );
        let cfg = Config::from_toml_str("[integrations.systemd]\nunits = ['a.service']", None)
            .unwrap()
            .integration("systemd")
            .unwrap()
            .options::<SystemdOptions>()
            .unwrap();
        assert_eq!(cfg.value.units, vec!["a.service"]);
    }

    #[tokio::test]
    async fn switches_follow_and_control_units() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let systemd = MockSystemd::start(
            &bus,
            &[("sync.service", "inactive"), ("other.service", "active")],
        )
        .await
        .unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &[]);
        let integration = SystemdUnits::new(env, vec!["sync.service".into()]);
        assert!(integration.probe().await.is_available());
        let (mut t, _cancel) = rig(integration, "systemd");
        let id = unit_entity_id("sync.service");
        wait_state(&mut t, &id, State::Bool(false)).await;
        assert!(systemd.subscribed());

        t.command(id.as_str(), CommandValue::Bool(true));
        wait_state(&mut t, &id, State::Bool(true)).await;
        assert!(
            systemd
                .calls()
                .contains(&"StartUnit(sync.service,replace)".to_owned())
        );

        // Changes made outside Deskhand are followed too.
        systemd.set_state("sync.service", "failed").await.unwrap();
        wait_state(&mut t, &id, State::Bool(false)).await;

        // Units outside the allowlist have no entity, so commands go nowhere.
        t.command("systemd_other", CommandValue::Bool(false));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!systemd.calls().iter().any(|c| c.contains("other")));
    }
}
