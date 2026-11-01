//! `bluetooth` capability (BlueZ): a `bluetooth_power` switch for the first
//! adapter, and a connect switch per device (`bt_<mac>`, attribute
//! `battery` when the device reports one).
//!
//! ```toml
//! [integrations.bluetooth]
//! devices = "paired"            # default: every paired device
//! # devices = ["AA:BB:CC:DD:EE:FF"]
//! ```
//! Devices are dynamic entities: pairing adds one, removing the pairing
//! removes it.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use deskhand_core::ctx::Switch;
use deskhand_core::{Attributes, Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use zbus::Proxy;
use zbus::zvariant::{OwnedObjectPath, Value};

use crate::env::LinuxEnv;
use crate::util::{Props, changed, dbus_err, prop, signals_from};

const BLUEZ: &str = "org.bluez";
const ADAPTER_IFACE: &str = "org.bluez.Adapter1";
const DEVICE_IFACE: &str = "org.bluez.Device1";
const BATTERY_IFACE: &str = "org.bluez.Battery1";
/// RSSI updates during discovery are frequent; batch them.
const QUIET: Duration = Duration::from_millis(400);

pub const BLUETOOTH_POWER: EntityId = EntityId::from_static("bluetooth_power");

/// `devices = "paired"` or a list of addresses.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum DeviceFilter {
    Mode(String),
    Addresses(Vec<String>),
}

impl Default for DeviceFilter {
    fn default() -> Self {
        Self::Mode("paired".into())
    }
}

/// `[integrations.bluetooth]` options.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BluetoothOptions {
    pub devices: DeviceFilter,
}

impl DeviceFilter {
    fn wants(&self, address: &str, paired: bool) -> bool {
        match self {
            Self::Mode(m) if m == "paired" => paired,
            Self::Mode(_) => false,
            Self::Addresses(list) => list.iter().any(|a| a.eq_ignore_ascii_case(address)),
        }
    }
}

pub fn device_entity_id(address: &str) -> EntityId {
    EntityId::from_static("bt").join(address)
}

#[derive(Debug, Clone, PartialEq)]
struct DeviceInfo {
    path: OwnedObjectPath,
    address: String,
    alias: String,
    paired: bool,
    connected: bool,
    battery: Option<u8>,
    icon: Option<String>,
}

#[derive(Debug, Default)]
struct Reading {
    adapter: Option<(OwnedObjectPath, bool)>,
    devices: Vec<DeviceInfo>,
}

type Managed = HashMap<OwnedObjectPath, HashMap<String, Props>>;

async fn read(om: &zbus::fdo::ObjectManagerProxy<'_>) -> zbus::Result<Reading> {
    let objects = om.get_managed_objects().await?;
    let objects: Managed = objects
        .into_iter()
        .map(|(p, ifaces)| {
            (
                p,
                ifaces
                    .into_iter()
                    .map(|(name, props)| (name.to_string(), props))
                    .collect(),
            )
        })
        .collect();
    let mut adapters: Vec<(OwnedObjectPath, bool)> = objects
        .iter()
        .filter_map(|(p, i)| {
            i.get(ADAPTER_IFACE)
                .map(|a| (p.clone(), prop::<bool>(a, "Powered").unwrap_or(false)))
        })
        .collect();
    adapters.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    let mut devices: Vec<DeviceInfo> = objects
        .iter()
        .filter_map(|(p, i)| {
            let d = i.get(DEVICE_IFACE)?;
            let address: String = prop(d, "Address")?;
            Some(DeviceInfo {
                path: p.clone(),
                alias: prop::<String>(d, "Alias").unwrap_or_else(|| address.clone()),
                address,
                paired: prop(d, "Paired").unwrap_or(false),
                connected: prop(d, "Connected").unwrap_or(false),
                battery: i
                    .get(BATTERY_IFACE)
                    .and_then(|b| prop::<u8>(b, "Percentage")),
                icon: prop(d, "Icon"),
            })
        })
        .collect();
    devices.sort_by(|a, b| a.address.cmp(&b.address));
    Ok(Reading {
        adapter: adapters.into_iter().next(),
        devices,
    })
}

/// A device switch and the task forwarding its commands.
struct DeviceEntity {
    switch: Switch,
    forwarder: AbortHandle,
}

impl Drop for DeviceEntity {
    fn drop(&mut self) {
        // The forwarder owns the command stream; stopping it lets the entity unregister.
        self.forwarder.abort();
    }
}

/// Bluetooth adapter power and device connections through BlueZ.
#[derive(Debug)]
pub struct Bluetooth {
    env: Arc<LinuxEnv>,
}

impl Bluetooth {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }
}

fn icon_for(icon: Option<&str>) -> &'static str {
    match icon.unwrap_or("") {
        i if i.starts_with("audio-head") => "mdi:headphones",
        i if i.starts_with("audio") => "mdi:speaker-bluetooth",
        "input-mouse" => "mdi:mouse",
        "input-keyboard" => "mdi:keyboard",
        "input-gaming" => "mdi:gamepad-variant",
        "phone" => "mdi:cellphone",
        _ => "mdi:bluetooth",
    }
}

#[async_trait::async_trait]
impl Integration for Bluetooth {
    fn id(&self) -> &'static str {
        "bluetooth.bluez"
    }

    fn capability(&self) -> &'static str {
        "bluetooth"
    }

    fn description(&self) -> &'static str {
        "Bluetooth adapter power and device connections through BlueZ"
    }

    async fn probe(&self) -> Probe {
        let conn = match self.env.system_bus().await {
            Ok(c) => c,
            Err(e) => return Probe::unavailable(format!("system bus: {e}")),
        };
        if !LinuxEnv::has_owner(&conn, BLUEZ).await {
            return Probe::unavailable("BlueZ is not running");
        }
        let Ok(om) = object_manager(&conn).await else {
            return Probe::unavailable("BlueZ object manager unavailable");
        };
        match read(&om).await {
            Ok(r) if r.adapter.is_some() => Probe::available(),
            Ok(_) => Probe::unavailable("no Bluetooth adapter"),
            Err(e) => Probe::unavailable(format!("BlueZ: {e}")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let opts: BluetoothOptions = ctx.config()?;
        let conn = self.env.system_bus().await.map_err(dbus_err)?;
        let mut signals = signals_from(&conn, BLUEZ).await.map_err(dbus_err)?;
        let om = object_manager(&conn).await.map_err(dbus_err)?;
        let (power, mut power_cmds) =
            ctx.switch(EntitySpec::switch(BLUETOOTH_POWER, "Bluetooth").icon("mdi:bluetooth"));
        let (dev_tx, mut dev_rx) = mpsc::unbounded_channel::<(String, bool)>();
        let mut devices: BTreeMap<String, DeviceEntity> = BTreeMap::new();
        let mut reading = read(&om).await.map_err(dbus_err)?;

        let reconcile = |r: &Reading, devices: &mut BTreeMap<String, DeviceEntity>| {
            match &r.adapter {
                Some((_, powered)) => power.set(*powered),
                None => power.set_unknown(),
            }
            let wanted: Vec<&DeviceInfo> = r
                .devices
                .iter()
                .filter(|d| opts.devices.wants(&d.address, d.paired))
                .collect();
            devices.retain(|addr, _| wanted.iter().any(|d| &d.address == addr));
            for d in wanted {
                let entity = devices.entry(d.address.clone()).or_insert_with(|| {
                    tracing::info!(address = %d.address, alias = %d.alias, "Bluetooth device added");
                    let (switch, mut commands) = ctx.switch(
                        EntitySpec::switch(device_entity_id(&d.address), d.alias.clone())
                            .icon(icon_for(d.icon.as_deref())),
                    );
                    let tx = dev_tx.clone();
                    let address = d.address.clone();
                    let forwarder = tokio::spawn(async move {
                        while let Some(on) = commands.recv().await {
                            if tx.send((address.clone(), on)).is_err() {
                                break;
                            }
                        }
                    })
                    .abort_handle();
                    DeviceEntity { switch, forwarder }
                });
                let mut attrs = Attributes::new();
                attrs.insert("address".into(), json!(d.address));
                attrs.insert("paired".into(), json!(d.paired));
                if let Some(b) = d.battery {
                    attrs.insert("battery".into(), json!(b));
                }
                entity.switch.set_attributes(attrs);
                entity.switch.set(d.connected);
            }
        };
        reconcile(&reading, &mut devices);
        ctx.ready();

        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                c = changed(&mut signals, QUIET) => {
                    if c.is_none() {
                        return Err(IntegrationError::msg("service restarted or went away"));
                    }
                    match read(&om).await {
                        Ok(r) => { reconcile(&r, &mut devices); reading = r; }
                        Err(e) => tracing::debug!(error = %e, "BlueZ read failed"),
                    }
                }
                cmd = power_cmds.recv() => {
                    let Some(on) = cmd else { return Ok(()) };
                    let Some((path, _)) = &reading.adapter else { continue };
                    tracing::info!(on, "Bluetooth power from Home Assistant");
                    match Proxy::new(&conn, BLUEZ, path.clone(), ADAPTER_IFACE).await {
                        Ok(p) => {
                            if let Err(e) = p.set_property("Powered", Value::from(on)).await {
                                tracing::warn!(error = %e, "setting Bluetooth power failed");
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "Bluetooth adapter proxy failed"),
                    }
                }
                Some((address, on)) = dev_rx.recv() => {
                    let Some(d) = reading.devices.iter().find(|d| d.address == address) else { continue };
                    let method = if on { "Connect" } else { "Disconnect" };
                    tracing::info!(%address, method, "Bluetooth device command from Home Assistant");
                    // Connecting can take seconds; don't hold up other updates.
                    let conn = conn.clone();
                    let path = d.path.clone();
                    tokio::spawn(async move {
                        let res = match Proxy::new(&conn, BLUEZ, path, DEVICE_IFACE).await {
                            Ok(p) => p.call_method(method, &()).await.map(drop),
                            Err(e) => Err(e),
                        };
                        if let Err(e) = res {
                            tracing::warn!(%address, method, error = %e, "Bluetooth command failed");
                        }
                    });
                }
            }
        }
    }
}

async fn object_manager(
    conn: &zbus::Connection,
) -> zbus::Result<zbus::fdo::ObjectManagerProxy<'static>> {
    zbus::fdo::ObjectManagerProxy::builder(conn)
        .destination(BLUEZ)?
        .path("/")?
        .build()
        .await
}

#[cfg(test)]
mod tests {
    use deskhand_core::{CommandValue, Config, State};
    use deskhand_testkit::mocks::MockBluez;
    use deskhand_testkit::{TestBus, require_dbus};

    use super::*;
    use crate::testutil::{rig, rig_with, wait_attr, wait_entity, wait_state};

    const HEADSET: &str = "AA:BB:CC:DD:EE:01";
    const MOUSE: &str = "AA:BB:CC:DD:EE:02";

    #[test]
    fn filter() {
        assert!(DeviceFilter::default().wants(HEADSET, true));
        assert!(!DeviceFilter::default().wants(HEADSET, false));
        let list = DeviceFilter::Addresses(vec!["aa:bb:cc:dd:ee:01".into()]);
        assert!(list.wants(HEADSET, false));
        assert!(!list.wants(MOUSE, true));
        assert_eq!(device_entity_id(HEADSET).as_str(), "bt_aa_bb_cc_dd_ee_01");
    }

    #[tokio::test]
    async fn adapter_power_and_device_connections() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let bluez = MockBluez::start(&bus, true).await.unwrap();
        bluez
            .add_device(HEADSET, "WH-1000XM4", true, Some(80))
            .await
            .unwrap();
        bluez
            .add_device(MOUSE, "Unpaired mouse", false, None)
            .await
            .unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &[]);
        let integration = Bluetooth::new(env);
        assert!(integration.probe().await.is_available());
        let (mut t, _cancel) = rig(integration, "bluetooth");
        let headset = device_entity_id(HEADSET);
        wait_state(&mut t, &BLUETOOTH_POWER, State::Bool(true)).await;
        wait_state(&mut t, &headset, State::Bool(false)).await;
        wait_attr(&mut t, &headset, "battery", json!(80)).await;

        t.command(headset.as_str(), CommandValue::Bool(true));
        wait_state(&mut t, &headset, State::Bool(true)).await;
        assert!(bluez.connected(HEADSET));
        // The unpaired mouse has no entity.
        t.command(device_entity_id(MOUSE).as_str(), CommandValue::Bool(true));

        t.command("bluetooth_power", CommandValue::Bool(false));
        wait_state(&mut t, &BLUETOOTH_POWER, State::Bool(false)).await;
        wait_state(&mut t, &headset, State::Bool(false)).await;
        assert!(!bluez.powered());
        assert!(!bluez.calls().iter().any(|c| c.contains(MOUSE)));

        // A device that disappears is removed.
        bluez.remove_device(HEADSET).await.unwrap();
        wait_entity(&mut t, &headset, false).await;
    }

    #[tokio::test]
    async fn explicit_device_list() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let bluez = MockBluez::start(&bus, true).await.unwrap();
        bluez
            .add_device(HEADSET, "Headset", true, None)
            .await
            .unwrap();
        bluez.add_device(MOUSE, "Mouse", false, None).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &[]);
        let config = Config::from_toml_str(
            &format!("[integrations.bluetooth]\ndevices = ['{MOUSE}']"),
            None,
        )
        .unwrap()
        .integration("bluetooth")
        .unwrap();
        let (mut t, _cancel) = rig_with(Bluetooth::new(env), config);
        wait_entity(&mut t, &device_entity_id(MOUSE), true).await;
        assert!(!t.entities.contains(&device_entity_id(HEADSET)));
    }
}
