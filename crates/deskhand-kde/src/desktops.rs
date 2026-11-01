//! `virtual_desktop` capability: a select with the virtual desktops by
//! name (` 2` appended to duplicates), from KWin's VirtualDesktopManager.
//! Attributes: `number` (1-based position), `count`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use deskhand_core::{Attributes, Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use deskhand_linux::LinuxEnv;
use deskhand_linux::util::{changed, dbus_err, signals_from_path};
use serde_json::json;
use zbus::Proxy;

use crate::{KWIN, kwin_session};

pub const VIRTUAL_DESKTOP: EntityId = EntityId::from_static("virtual_desktop");
const VDM_PATH: &str = "/VirtualDesktopManager";
const VDM_IFACE: &str = "org.kde.KWin.VirtualDesktopManager";

/// Virtual desktops.
#[derive(Debug)]
pub struct VirtualDesktops {
    env: Arc<LinuxEnv>,
}

impl VirtualDesktops {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }
}

/// `(position, id, name)` → option name → id, in position order.
pub fn option_names(mut desktops: Vec<(i32, String, String)>) -> Vec<(String, String)> {
    desktops.sort_by_key(|d| d.0);
    let mut seen: BTreeMap<String, u32> = BTreeMap::new();
    let mut out = Vec::new();
    for (_, id, name) in desktops {
        let base = if name.trim().is_empty() {
            id.clone()
        } else {
            name
        };
        let n = seen.entry(base.clone()).or_insert(0);
        *n += 1;
        let option = if *n == 1 { base } else { format!("{base} {n}") };
        out.push((option, id));
    }
    out
}

/// `desktops` is `a(uss)` on the wire although KWin's introspection says
/// `a(iss)` (Plasma 6.6); accept both.
fn parse_desktops(v: zbus::zvariant::OwnedValue) -> Vec<(i32, String, String)> {
    if let Some(d) = v
        .try_clone()
        .ok()
        .and_then(|c| Vec::<(u32, String, String)>::try_from(c).ok())
    {
        return d.into_iter().map(|(p, i, n)| (p as i32, i, n)).collect();
    }
    Vec::<(i32, String, String)>::try_from(v).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "unexpected type of VirtualDesktopManager.desktops");
        Vec::new()
    })
}

#[async_trait::async_trait]
impl Integration for VirtualDesktops {
    fn id(&self) -> &'static str {
        "virtual_desktop.kwin"
    }

    fn capability(&self) -> &'static str {
        "virtual_desktop"
    }

    fn description(&self) -> &'static str {
        "Current virtual desktop (switchable) from KWin"
    }

    async fn probe(&self) -> Probe {
        match kwin_session(&self.env).await {
            Ok(_) => Probe::available(),
            Err(p) => p,
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let conn = self.env.session_bus().await.map_err(dbus_err)?;
        let vdm = zbus::proxy::Builder::<Proxy<'_>>::new(&conn)
            .destination(KWIN)
            .map_err(dbus_err)?
            .path(VDM_PATH)
            .map_err(dbus_err)?
            .interface(VDM_IFACE)
            .map_err(dbus_err)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .map_err(dbus_err)?;
        let mut signals = signals_from_path(&conn, KWIN, Some(VDM_PATH))
            .await
            .map_err(dbus_err)?;
        let (select, mut commands) = ctx.select(
            EntitySpec::select(VIRTUAL_DESKTOP, "Virtual desktop", Vec::new())
                .icon("mdi:view-dashboard-variant"),
        );
        let mut options: Vec<(String, String)> = Vec::new();
        let refresh = async |options: &mut Vec<(String, String)>| -> Result<(), IntegrationError> {
            let desktops = parse_desktops(
                vdm.get_property::<zbus::zvariant::OwnedValue>("desktops")
                    .await
                    .map_err(dbus_err)?,
            );
            let current: String = vdm.get_property("current").await.map_err(dbus_err)?;
            *options = option_names(desktops);
            select.set_options(options.iter().map(|(o, _)| o.clone()).collect());
            if let Some(pos) = options.iter().position(|(_, id)| *id == current) {
                let mut attrs = Attributes::new();
                attrs.insert("number".into(), json!(pos + 1));
                attrs.insert("count".into(), json!(options.len()));
                select.set_attributes(attrs);
                select.set(options[pos].0.clone());
            }
            Ok(())
        };
        refresh(&mut options).await?;
        ctx.ready();

        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                c = changed(&mut signals, Duration::from_millis(100)) => {
                    if c.is_none() {
                        return Err(IntegrationError::msg("KWin restarted or went away"));
                    }
                }
                Some(option) = commands.recv() => {
                    match options.iter().find(|(o, _)| *o == option) {
                        Some((_, id)) => {
                            if let Err(e) = vdm.set_property("current", id.as_str()).await {
                                tracing::warn!(error = %e, "switching virtual desktop failed");
                            }
                        }
                        None => tracing::warn!(option, "unknown virtual desktop"),
                    }
                }
            }
            refresh(&mut options).await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::testing::{rig, wait_attr, wait_state};
    use deskhand_core::{CommandValue, State};
    use deskhand_testkit::mocks::MockKWin;
    use deskhand_testkit::{TestBus, require_dbus};

    use super::*;

    #[test]
    fn duplicate_names() {
        let o = option_names(vec![
            (1, "b".into(), "Work".into()),
            (0, "a".into(), "Work".into()),
            (2, "c".into(), "".into()),
        ]);
        assert_eq!(
            o,
            vec![
                ("Work".to_owned(), "a".to_owned()),
                ("Work 2".to_owned(), "b".to_owned()),
                ("c".to_owned(), "c".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn follows_and_switches_desktops() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let kwin = MockKWin::start(&bus).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &["KDE"]);
        let (mut t, _cancel) = rig(VirtualDesktops::new(env), "virtual_desktop");
        wait_state(&mut t, &VIRTUAL_DESKTOP, State::Text("Desktop 1".into())).await;

        kwin.set_desktops(&[("d1", "Desktop 1"), ("d2", "Mail")], "d2")
            .await
            .unwrap();
        wait_state(&mut t, &VIRTUAL_DESKTOP, State::Text("Mail".into())).await;
        wait_attr(&mut t, &VIRTUAL_DESKTOP, "number", json!(2)).await;

        t.command(
            VIRTUAL_DESKTOP.as_str(),
            CommandValue::Text("Desktop 1".into()),
        );
        wait_state(&mut t, &VIRTUAL_DESKTOP, State::Text("Desktop 1".into())).await;
        assert!(kwin.calls().contains(&"current=d1".to_owned()));
    }
}
