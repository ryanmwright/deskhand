//! `logout` capability: a `logout` button that ends the Plasma session
//! without a confirmation prompt (`org.kde.Shutdown.logout`). Unsaved work
//! in open apps can be lost, so it is opt-in: add `"logout"` to
//! `[integrations.power] buttons`.

use std::sync::Arc;

use deskhand_core::{Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use deskhand_linux::LinuxEnv;
use deskhand_linux::util::dbus_err;
use zbus::Proxy;

pub const LOGOUT: EntityId = EntityId::from_static("logout");
pub const SERVICE: &str = "org.kde.Shutdown";
const PATH: &str = "/Shutdown";

/// Plasma logout button.
#[derive(Debug)]
pub struct Logout {
    env: Arc<LinuxEnv>,
    /// `"logout"` is in `[integrations.power] buttons`.
    wanted: bool,
}

impl Logout {
    pub fn new(env: Arc<LinuxEnv>, wanted: bool) -> Self {
        Self { env, wanted }
    }
}

#[async_trait::async_trait]
impl Integration for Logout {
    fn id(&self) -> &'static str {
        "logout.plasma"
    }

    fn capability(&self) -> &'static str {
        "logout"
    }

    fn description(&self) -> &'static str {
        "Log out of Plasma without a prompt (opt-in)"
    }

    async fn probe(&self) -> Probe {
        if !self.wanted {
            return Probe::unavailable("opt-in: add \"logout\" to [integrations.power] buttons");
        }
        if !self.env.desktop_is("KDE") {
            return Probe::unavailable("not a KDE Plasma session");
        }
        match self.env.session_bus().await {
            Ok(c) if LinuxEnv::has_owner(&c, SERVICE).await => Probe::available(),
            Ok(_) => Probe::unavailable("org.kde.Shutdown is not on the session bus"),
            Err(e) => Probe::unavailable(format!("session bus: {e}")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let conn = self.env.session_bus().await.map_err(dbus_err)?;
        let proxy = Proxy::new(&conn, SERVICE, PATH, SERVICE)
            .await
            .map_err(dbus_err)?;
        let mut presses = ctx.button(EntitySpec::button(LOGOUT, "Log out").icon("mdi:logout"));
        ctx.ready();
        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                p = presses.recv() => {
                    if p.is_none() {
                        return Ok(());
                    }
                    tracing::info!("logging out (Home Assistant)");
                    if let Err(e) = proxy.call::<_, _, ()>("logout", &()).await {
                        tracing::warn!(error = %e, "logout failed");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::CommandValue;
    use deskhand_core::testing::{rig, wait_entity};
    use deskhand_testkit::mocks::MockShutdown;
    use deskhand_testkit::{TestBus, require_dbus};

    use super::*;

    #[tokio::test]
    async fn opt_in_and_logs_out() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let mock = MockShutdown::start(&bus).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &["KDE"]);
        assert!(!Logout::new(env.clone(), false).probe().await.is_available());
        let integration = Logout::new(env, true);
        assert!(integration.probe().await.is_available());
        let (mut t, _cancel) = rig(integration, "logout");
        wait_entity(&mut t, &LOGOUT, true).await;
        t.command(LOGOUT.as_str(), CommandValue::Press);
        for _ in 0..50 {
            if mock.calls() == vec!["logout".to_owned()] {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("no logout: {:?}", mock.calls());
    }
}
