//! `dnd` capability: Plasma's Do Not Disturb as a switch.
//!
//! - **Read:** the notification server's `Inhibited` property, which is on
//!   for the user's own DnD and for apps that inhibit notifications
//!   (docs/dev/spikes.md §4).
//! - **Write:** `plasmanotifyrc [DoNotDisturb] Until` through `kwriteconfig6
//!   --notify`, like the tray toggle: on writes a far-future date (it
//!   survives Deskhand restarts), off deletes the key (which also ends a DnD
//!   the user started). If that fails, on falls back to `Inhibit`, which
//!   lasts while Deskhand runs.
//!
//! While an app inhibits notifications the switch stays on, even after
//! turning it off.

use std::sync::Arc;
use std::time::Duration;

use deskhand_core::{Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use deskhand_linux::LinuxEnv;
use deskhand_linux::util::{changed, dbus_err, signals_from_path};
use zbus::Proxy;
use zbus::zvariant::Value;

use crate::{kwin_session, run_tool};

pub const DND: EntityId = EntityId::from_static("dnd");
const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
/// KConfig date-time far enough ahead to mean "until turned off".
pub const FOREVER: &str = "2100,1,1,0,0,0.000";

/// The `kwriteconfig6` arguments for on or off.
pub fn kwriteconfig_args(on: bool) -> Vec<&'static str> {
    let mut args = vec![
        "--file",
        "plasmanotifyrc",
        "--group",
        "DoNotDisturb",
        "--key",
        "Until",
        "--notify",
    ];
    if on {
        args.push(FOREVER);
    } else {
        args.push("--delete");
    }
    args
}

/// Plasma Do Not Disturb.
#[derive(Debug)]
pub struct Dnd {
    env: Arc<LinuxEnv>,
    kwriteconfig: String,
}

impl Dnd {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self::with_tool(env, "kwriteconfig6")
    }

    /// Another `kwriteconfig6` (tests).
    pub fn with_tool(env: Arc<LinuxEnv>, kwriteconfig: impl Into<String>) -> Self {
        Self {
            env,
            kwriteconfig: kwriteconfig.into(),
        }
    }
}

#[async_trait::async_trait]
impl Integration for Dnd {
    fn id(&self) -> &'static str {
        "dnd.plasma"
    }

    fn capability(&self) -> &'static str {
        "dnd"
    }

    fn description(&self) -> &'static str {
        "Plasma Do Not Disturb switch"
    }

    async fn probe(&self) -> Probe {
        let conn = match kwin_session(&self.env).await {
            Ok(c) => c,
            Err(p) => return p,
        };
        if LinuxEnv::has_owner(&conn, NOTIFICATIONS).await {
            Probe::available()
        } else {
            Probe::unavailable("no notification server (plasmashell) on the session bus")
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let conn = self.env.session_bus().await.map_err(dbus_err)?;
        let proxy = zbus::proxy::Builder::<Proxy<'_>>::new(&conn)
            .destination(NOTIFICATIONS)
            .map_err(dbus_err)?
            .path(PATH)
            .map_err(dbus_err)?
            .interface(NOTIFICATIONS)
            .map_err(dbus_err)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .map_err(dbus_err)?;
        let mut signals = signals_from_path(&conn, NOTIFICATIONS, Some(PATH))
            .await
            .map_err(dbus_err)?;
        let (switch, mut commands) =
            ctx.switch(EntitySpec::switch(DND, "Do not disturb").icon("mdi:bell-off"));
        let refresh = async || match proxy.get_property::<bool>("Inhibited").await {
            Ok(v) => switch.set(v),
            Err(e) => {
                tracing::debug!(error = %e, "reading Inhibited failed");
                switch.set_unknown();
            }
        };
        refresh().await;
        ctx.ready();

        let mut cookie: Option<u32> = None;
        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                c = changed(&mut signals, Duration::from_millis(100)) => {
                    if c.is_none() {
                        return Err(IntegrationError::msg("notification server restarted or went away"));
                    }
                }
                Some(on) = commands.recv() => {
                    tracing::info!(on, "Do Not Disturb from Home Assistant");
                    if !on && let Some(c) = cookie.take() {
                        let _ = proxy.call::<_, _, ()>("UnInhibit", &(c,)).await;
                    }
                    if let Err(e) = run_tool(&self.kwriteconfig, &kwriteconfig_args(on)).await {
                        tracing::warn!(error = %e, "writing plasmanotifyrc failed");
                        if on && cookie.is_none() {
                            let hints: std::collections::HashMap<&str, Value<'_>> = Default::default();
                            match proxy
                                .call::<_, _, u32>("Inhibit", &(deskhand_core::APP_ID, "Do Not Disturb from Home Assistant", hints))
                                .await
                            {
                                Ok(c) => cookie = Some(c),
                                Err(e) => tracing::warn!(error = %e, "Inhibit failed too"),
                            }
                        }
                    }
                }
            }
            refresh().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::testing::{rig, wait_state};
    use deskhand_core::{CommandValue, State};
    use deskhand_testkit::mocks::{MockKWin, MockNotifications};
    use deskhand_testkit::{TestBus, require_dbus};

    use super::*;

    async fn wait_log(log: &std::path::Path, want: &str) {
        for _ in 0..200 {
            if std::fs::read_to_string(log)
                .unwrap_or_default()
                .contains(want)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{want:?} not in {:?}", std::fs::read_to_string(log));
    }

    #[tokio::test]
    async fn writes_plasmanotifyrc_and_follows_inhibited() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let _kwin = MockKWin::start(&bus).await.unwrap();
        let server = MockNotifications::start(&bus).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &["KDE"]);
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let tool = dir.path().join("kwriteconfig6");
        std::fs::write(
            &tool,
            format!("#!/bin/sh\necho \"$@\" >> {}\n", log.display()),
        )
        .unwrap();
        std::os::unix::fs::PermissionsExt::set_mode(
            &mut std::fs::metadata(&tool).unwrap().permissions(),
            0o755,
        );
        std::fs::set_permissions(&tool, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let integration = Dnd::with_tool(env, tool.to_string_lossy());
        assert!(integration.probe().await.is_available());
        let (mut t, _cancel) = rig(integration, "dnd");
        wait_state(&mut t, &DND, State::Bool(false)).await;

        t.command(DND.as_str(), CommandValue::Bool(true));
        wait_log(&log, &format!("--key Until --notify {FOREVER}")).await;
        // plasmashell picks up the config change.
        server.set_user_dnd(true).await.unwrap();
        wait_state(&mut t, &DND, State::Bool(true)).await;
        t.command(DND.as_str(), CommandValue::Bool(false));
        wait_log(&log, "--notify --delete").await;
        server.set_user_dnd(false).await.unwrap();
        wait_state(&mut t, &DND, State::Bool(false)).await;
        assert_eq!(server.inhibitions(), 0);
    }

    #[tokio::test]
    async fn falls_back_to_inhibit() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let _kwin = MockKWin::start(&bus).await.unwrap();
        let server = MockNotifications::start(&bus).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &["KDE"]);
        let (mut t, _cancel) = rig(Dnd::with_tool(env, "/nonexistent/kwriteconfig6"), "dnd");
        wait_state(&mut t, &DND, State::Bool(false)).await;
        t.command(DND.as_str(), CommandValue::Bool(true));
        wait_state(&mut t, &DND, State::Bool(true)).await;
        assert_eq!(server.inhibitions(), 1);
        t.command(DND.as_str(), CommandValue::Bool(false));
        wait_state(&mut t, &DND, State::Bool(false)).await;
        assert_eq!(server.inhibitions(), 0);
    }
}
