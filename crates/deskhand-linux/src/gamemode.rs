//! `gamemode` capability: `gamemode_active` from Feral GameMode's
//! `ClientCount` (attribute `clients`).

use std::sync::Arc;

use deskhand_core::{Attributes, Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use futures_util::StreamExt;
use serde_json::json;
use zbus::Proxy;

use crate::env::LinuxEnv;
use crate::util::dbus_err;

const GAMEMODE: &str = "com.feralinteractive.GameMode";
const GAMEMODE_PATH: &str = "/com/feralinteractive/GameMode";

pub const GAMEMODE_ACTIVE: EntityId = EntityId::from_static("gamemode_active");

/// Whether a game has GameMode enabled.
#[derive(Debug)]
pub struct GameMode {
    env: Arc<LinuxEnv>,
}

impl GameMode {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }
}

#[async_trait::async_trait]
impl Integration for GameMode {
    fn id(&self) -> &'static str {
        "gamemode.feral"
    }

    fn capability(&self) -> &'static str {
        "gamemode"
    }

    fn description(&self) -> &'static str {
        "Whether Feral GameMode is active"
    }

    async fn probe(&self) -> Probe {
        // gamemoded is D-Bus activated; a missing owner is fine if it is activatable.
        let Ok(conn) = self.env.session_bus().await else {
            return Probe::unavailable("no session bus");
        };
        let Ok(dbus) = zbus::fdo::DBusProxy::new(&conn).await else {
            return Probe::unavailable("no session bus");
        };
        let activatable = dbus
            .list_activatable_names()
            .await
            .is_ok_and(|names| names.iter().any(|n| n.as_str() == GAMEMODE));
        if activatable || LinuxEnv::has_owner(&conn, GAMEMODE).await {
            Probe::available()
        } else {
            Probe::unavailable("GameMode is not installed")
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let conn = self.env.session_bus().await.map_err(dbus_err)?;
        let proxy = Proxy::new(&conn, GAMEMODE, GAMEMODE_PATH, GAMEMODE)
            .await
            .map_err(dbus_err)?;
        let mut changes = proxy.receive_property_changed::<i32>("ClientCount").await;
        let sensor = ctx.binary_sensor(
            EntitySpec::binary_sensor(GAMEMODE_ACTIVE, "GameMode", None).icon("mdi:controller"),
        );
        let publish = |clients: i32| {
            let mut attrs = Attributes::new();
            attrs.insert("clients".into(), json!(clients));
            sensor.set_attributes(attrs);
            sensor.set(clients > 0);
        };
        publish(proxy.get_property("ClientCount").await.unwrap_or(0));
        ctx.ready();
        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                c = changes.next() => match c {
                    Some(c) => if let Ok(n) = c.get().await { publish(n) },
                    None => return Err(IntegrationError::msg("GameMode went away")),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::State;
    use deskhand_testkit::mocks::MockGameMode;
    use deskhand_testkit::{TestBus, require_dbus};

    use super::*;
    use crate::testutil::{rig, wait_state};

    #[tokio::test]
    async fn follows_client_count() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let gm = MockGameMode::start(&bus).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &[]);
        let integration = GameMode::new(env);
        assert!(integration.probe().await.is_available());
        let (mut t, _cancel) = rig(integration, "gamemode");
        wait_state(&mut t, &GAMEMODE_ACTIVE, State::Bool(false)).await;
        gm.set_clients(1).await.unwrap();
        wait_state(&mut t, &GAMEMODE_ACTIVE, State::Bool(true)).await;
    }
}
