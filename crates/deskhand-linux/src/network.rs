//! `network` capability (NetworkManager): `network_connectivity` and, on
//! machines with Wi-Fi, `wifi_ssid` (attribute `signal`, percent).

use std::sync::Arc;
use std::time::Duration;

use deskhand_core::ctx::Sensor;
use deskhand_core::{Attributes, Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use serde_json::json;
use zbus::zvariant::OwnedObjectPath;

use crate::env::LinuxEnv;
use crate::util::{changed, dbus_err, get_all, prop, signals_from};

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
/// NetworkManager is chatty (access point strengths); batch its signals.
const QUIET: Duration = Duration::from_secs(1);

pub const CONNECTIVITY: EntityId = EntityId::from_static("network_connectivity");
pub const WIFI_SSID: EntityId = EntityId::from_static("wifi_ssid");

const CONNECTIVITY_STATES: &[&str] = &["unknown", "none", "portal", "limited", "full"];

#[derive(Debug, Clone, PartialEq)]
struct Reading {
    connectivity: &'static str,
    /// The Wi-Fi device's interface and current network, if there is a Wi-Fi device.
    wifi: Option<(String, Option<(String, u8)>)>,
}

async fn read(conn: &zbus::Connection) -> zbus::Result<Reading> {
    let manager = get_all(conn, NM, NM_PATH, NM).await?;
    let connectivity = CONNECTIVITY_STATES
        .get(prop::<u32>(&manager, "Connectivity").unwrap_or(0) as usize)
        .copied()
        .unwrap_or("unknown");
    let devices: Vec<OwnedObjectPath> = prop(&manager, "Devices").unwrap_or_default();
    let mut wifi = None;
    for d in devices {
        let dev = get_all(
            conn,
            NM,
            d.as_str(),
            "org.freedesktop.NetworkManager.Device",
        )
        .await?;
        if prop::<u32>(&dev, "DeviceType") != Some(2) {
            continue;
        }
        let iface: String = prop(&dev, "Interface").unwrap_or_default();
        let wl = get_all(
            conn,
            NM,
            d.as_str(),
            "org.freedesktop.NetworkManager.Device.Wireless",
        )
        .await?;
        let ap: Option<OwnedObjectPath> = prop(&wl, "ActiveAccessPoint");
        let network = match ap.filter(|p| p.as_str() != "/") {
            Some(ap) => {
                let a = get_all(
                    conn,
                    NM,
                    ap.as_str(),
                    "org.freedesktop.NetworkManager.AccessPoint",
                )
                .await?;
                let ssid: Vec<u8> = prop(&a, "Ssid").unwrap_or_default();
                Some((
                    String::from_utf8_lossy(&ssid).into_owned(),
                    prop::<u8>(&a, "Strength").unwrap_or(0),
                ))
            }
            None => None,
        };
        wifi = Some((iface, network));
        break;
    }
    Ok(Reading { connectivity, wifi })
}

/// Connectivity and Wi-Fi network through NetworkManager.
#[derive(Debug)]
pub struct Network {
    env: Arc<LinuxEnv>,
}

impl Network {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }
}

#[async_trait::async_trait]
impl Integration for Network {
    fn id(&self) -> &'static str {
        "network.networkmanager"
    }

    fn capability(&self) -> &'static str {
        "network"
    }

    fn description(&self) -> &'static str {
        "Connectivity and Wi-Fi network through NetworkManager"
    }

    async fn probe(&self) -> Probe {
        match self.env.system_bus().await {
            Ok(c) if LinuxEnv::has_owner(&c, NM).await => Probe::available(),
            Ok(_) => Probe::unavailable("NetworkManager is not running"),
            Err(e) => Probe::unavailable(format!("system bus: {e}")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let conn = self.env.system_bus().await.map_err(dbus_err)?;
        let mut signals = signals_from(&conn, NM).await.map_err(dbus_err)?;
        let first = read(&conn).await.map_err(dbus_err)?;
        let connectivity = ctx.sensor(
            EntitySpec::sensor(CONNECTIVITY, "Network connectivity")
                .enum_options(CONNECTIVITY_STATES.iter().map(|s| s.to_string()).collect())
                .icon("mdi:web"),
        );
        let ssid: Option<Sensor> = first
            .wifi
            .as_ref()
            .map(|_| ctx.sensor(EntitySpec::sensor(WIFI_SSID, "Wi-Fi network").icon("mdi:wifi")));
        let publish = |r: &Reading| {
            connectivity.set(r.connectivity);
            if let (Some(sensor), Some((iface, network))) = (&ssid, &r.wifi) {
                let mut attrs = Attributes::new();
                attrs.insert("interface".into(), json!(iface));
                attrs.insert("signal".into(), json!(network.as_ref().map(|n| n.1)));
                sensor.set_attributes(attrs);
                sensor.set(network.as_ref().map(|n| n.0.clone()));
            }
        };
        publish(&first);
        ctx.ready();
        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                c = changed(&mut signals, QUIET) => {
                    if c.is_none() {
                        return Err(IntegrationError::msg("service restarted or went away"));
                    }
                    match read(&conn).await {
                        Ok(r) => publish(&r),
                        Err(e) => tracing::debug!(error = %e, "NetworkManager read failed"),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::State;
    use deskhand_testkit::mocks::MockNetworkManager;
    use deskhand_testkit::{TestBus, require_dbus};

    use super::*;
    use crate::testutil::{rig, wait_attr, wait_state};

    #[tokio::test]
    async fn connectivity_and_ssid() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let nm = MockNetworkManager::start(&bus).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &[]);
        let (mut t, _cancel) = rig(Network::new(env), "network");
        wait_state(&mut t, &CONNECTIVITY, "full".into()).await;
        wait_state(&mut t, &WIFI_SSID, State::Unknown).await;

        nm.set_access_point(Some(("HomeNet", 81))).await.unwrap();
        wait_state(&mut t, &WIFI_SSID, "HomeNet".into()).await;
        wait_attr(&mut t, &WIFI_SSID, "signal", json!(81)).await;

        nm.set_connectivity(3).await.unwrap();
        wait_state(&mut t, &CONNECTIVITY, "limited".into()).await;
    }
}
