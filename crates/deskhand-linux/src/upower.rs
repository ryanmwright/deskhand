//! UPower: the laptop battery and AC power (`battery` capability), peripheral
//! batteries as a source for the `peripheral_battery` integration
//! ([`UPowerPeripherals`]), and power-profiles-daemon (`power_profile`).
//!
//! Peripherals (mouse, keyboard, headset, ...) become devices of their own
//! through `deskhand_common::peripherals`, keyed by serial
//! (`battery_<serial>`), else by model name.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deskhand_common::peripherals::{
    BatteryLevel, ChargeState, PeripheralReading, PeripheralSink, PeripheralSource,
    is_placeholder_name,
};
use deskhand_core::ctx::{BinarySensor, Sensor};
use deskhand_core::entity::{BinarySensorClass, SensorClass, StateClass};
use deskhand_core::{Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;
use zbus::Proxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::env::LinuxEnv;
use crate::util::{Props, changed, dbus_err, get_all, prop, signals_from};

const UPOWER: &str = "org.freedesktop.UPower";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";
const DEVICE_IFACE: &str = "org.freedesktop.UPower.Device";
const QUIET: Duration = Duration::from_millis(300);

pub const BATTERY_LEVEL: EntityId = EntityId::from_static("battery_level");
pub const BATTERY_STATE: EntityId = EntityId::from_static("battery_state");
pub const BATTERY_TIME: EntityId = EntityId::from_static("battery_time_remaining");
pub const ON_AC_POWER: EntityId = EntityId::from_static("on_ac_power");
pub const POWER_PROFILE: EntityId = EntityId::from_static("power_profile");

const STATES: &[&str] = &[
    "unknown",
    "charging",
    "discharging",
    "empty",
    "full",
    "pending_charge",
    "pending_discharge",
];

fn state_name(code: u32) -> &'static str {
    STATES.get(code as usize).copied().unwrap_or("unknown")
}

/// UPower device type names for peripheral attributes.
fn kind_name(kind: u32) -> &'static str {
    match kind {
        5 => "mouse",
        6 => "keyboard",
        8 => "phone",
        10 => "tablet",
        12 => "gaming_input",
        13 => "pen",
        14 => "touchpad",
        17 => "headset",
        18 => "speakers",
        19 => "headphones",
        22 => "remote_control",
        26 => "wearable",
        _ => "other",
    }
}

/// The laptop battery (UPower's display device).
#[derive(Debug, Clone, PartialEq)]
struct Laptop {
    percentage: f64,
    state: &'static str,
    /// Seconds until empty (discharging) or full (charging).
    seconds: Option<i64>,
}

fn laptop_from(props: &Props) -> Option<Laptop> {
    let present: bool = prop(props, "IsPresent").unwrap_or(false);
    let kind: u32 = prop(props, "Type").unwrap_or(0);
    if !present || kind != 2 {
        return None;
    }
    let state = state_name(prop(props, "State").unwrap_or(0));
    let seconds = match state {
        "discharging" => prop::<i64>(props, "TimeToEmpty"),
        "charging" => prop::<i64>(props, "TimeToFull"),
        _ => None,
    }
    .filter(|s| *s > 0);
    Some(Laptop {
        percentage: prop(props, "Percentage").unwrap_or(0.0),
        state,
        seconds,
    })
}

fn charge_state(code: u32) -> ChargeState {
    match code {
        1 => ChargeState::Charging,
        2 => ChargeState::Discharging,
        3 => ChargeState::Empty,
        4 => ChargeState::Full,
        5 => ChargeState::PendingCharge,
        6 => ChargeState::PendingDischarge,
        _ => ChargeState::Unknown,
    }
}

/// UPower's `BatteryLevel`, for devices that only report coarse levels.
fn battery_level(code: u32) -> Option<BatteryLevel> {
    match code {
        3 => Some(BatteryLevel::Low),
        4 => Some(BatteryLevel::Critical),
        6 | 7 => Some(BatteryLevel::Normal),
        8 => Some(BatteryLevel::Full),
        _ => None,
    }
}

/// A peripheral battery (not a power supply) as a [`PeripheralReading`].
fn peripheral_from(path: &str, props: &Props) -> Option<PeripheralReading> {
    let kind: u32 = prop(props, "Type").unwrap_or(0);
    let power_supply: bool = prop(props, "PowerSupply").unwrap_or(true);
    let present: bool = prop(props, "IsPresent").unwrap_or(true);
    if power_supply || kind <= 2 || !present {
        return None;
    }
    let text = |name| {
        prop::<String>(props, name)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    // The kernel's stand-in name for a device that was asleep when it was
    // set up is no name: it changes once the device wakes.
    let model = text("Model").filter(|m| !is_placeholder_name(m));
    let serial = text("Serial");
    let last_segment = path.rsplit('/').next().unwrap_or("device");
    let key = PeripheralReading::key_for(
        serial.as_deref().unwrap_or(""),
        model.as_deref().unwrap_or(""),
        last_segment,
    );
    let kind = kind_name(kind);
    let name = model.clone().unwrap_or_else(|| {
        let mut n = kind.replace('_', " ");
        if let Some(first) = n.get_mut(0..1) {
            first.make_ascii_uppercase();
        }
        n
    });
    let level = battery_level(prop(props, "BatteryLevel").unwrap_or(0));
    let low = match prop::<u32>(props, "WarningLevel").unwrap_or(0) {
        1 | 2 => Some(false),
        3..=5 => Some(true),
        _ => None,
    };
    let read_at = prop::<u64>(props, "UpdateTime")
        .filter(|t| *t > 0)
        .map(|t| UNIX_EPOCH + Duration::from_secs(t));
    Some(PeripheralReading {
        kind: Some(kind.to_owned()),
        manufacturer: text("Vendor"),
        model,
        serial,
        percentage: prop(props, "Percentage"),
        // A level-only device: UPower's percentage is derived from the level.
        estimated: level.is_some(),
        level,
        state: charge_state(prop(props, "State").unwrap_or(0)),
        low,
        connected: true,
        read_at: read_at.or_else(|| Some(SystemTime::now())),
        ..PeripheralReading::new(key, name)
    })
}

struct LaptopEntities {
    level: Sensor,
    state: Sensor,
    time: Sensor,
    on_ac: BinarySensor,
}

impl LaptopEntities {
    fn register(ctx: &Ctx) -> Self {
        Self {
            level: ctx.sensor(
                EntitySpec::sensor(BATTERY_LEVEL, "Battery")
                    .sensor_class(SensorClass::Battery)
                    .unit("%")
                    .state_class(StateClass::Measurement)
                    .precision(0),
            ),
            state: ctx.sensor(
                EntitySpec::sensor(BATTERY_STATE, "Battery state")
                    .enum_options(STATES.iter().map(|s| s.to_string()).collect())
                    .icon("mdi:battery-charging"),
            ),
            time: ctx.sensor(
                EntitySpec::sensor(BATTERY_TIME, "Battery time remaining")
                    .sensor_class(SensorClass::Duration)
                    .unit("min")
                    .precision(0)
                    .icon("mdi:battery-clock"),
            ),
            on_ac: ctx.binary_sensor(EntitySpec::binary_sensor(
                ON_AC_POWER,
                "On AC power",
                Some(BinarySensorClass::Plug),
            )),
        }
    }

    fn publish(&self, l: &Laptop, on_battery: bool) {
        self.level.set(l.percentage.round());
        self.state.set(l.state);
        self.time.set(l.seconds.map(|s| (s as f64 / 60.0).round()));
        self.on_ac.set(!on_battery);
    }
}

/// Laptop battery and AC power via UPower.
#[derive(Debug)]
pub struct Battery {
    env: Arc<LinuxEnv>,
}

impl Battery {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }
}

struct Snapshot {
    laptop: Option<Laptop>,
    on_battery: bool,
}

async fn read(conn: &zbus::Connection, manager: &Proxy<'_>) -> zbus::Result<Snapshot> {
    let display: OwnedObjectPath = manager.call("GetDisplayDevice", &()).await?;
    let laptop = laptop_from(&get_all(conn, UPOWER, display.as_str(), DEVICE_IFACE).await?);
    let on_battery = prop::<bool>(
        &get_all(conn, UPOWER, UPOWER_PATH, UPOWER).await?,
        "OnBattery",
    )
    .unwrap_or(false);
    Ok(Snapshot { laptop, on_battery })
}

async fn read_peripherals(
    conn: &zbus::Connection,
    manager: &Proxy<'_>,
) -> zbus::Result<Vec<PeripheralReading>> {
    let paths: Vec<OwnedObjectPath> = manager.call("EnumerateDevices", &()).await?;
    let mut peripherals = Vec::new();
    for p in paths {
        // Devices can vanish between the listing and the read.
        if let Ok(props) = get_all(conn, UPOWER, p.as_str(), DEVICE_IFACE).await
            && let Some(per) = peripheral_from(p.as_str(), &props)
        {
            peripherals.push(per);
        }
    }
    Ok(peripherals)
}

async fn probe_upower(env: &LinuxEnv) -> Probe {
    match env.system_bus().await {
        Ok(c) if LinuxEnv::has_owner(&c, UPOWER).await => Probe::available(),
        Ok(_) => Probe::unavailable("UPower is not running"),
        Err(e) => Probe::unavailable(format!("system bus: {e}")),
    }
}

#[async_trait::async_trait]
impl Integration for Battery {
    fn id(&self) -> &'static str {
        "battery.upower"
    }

    fn capability(&self) -> &'static str {
        "battery"
    }

    fn description(&self) -> &'static str {
        "Battery and AC power through UPower"
    }

    async fn probe(&self) -> Probe {
        probe_upower(&self.env).await
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let conn = self.env.system_bus().await.map_err(dbus_err)?;
        let mut signals = signals_from(&conn, UPOWER).await.map_err(dbus_err)?;
        let manager = Proxy::new(&conn, UPOWER, UPOWER_PATH, UPOWER)
            .await
            .map_err(dbus_err)?;
        let first = read(&conn, &manager).await.map_err(dbus_err)?;
        let laptop = first
            .laptop
            .as_ref()
            .map(|_| LaptopEntities::register(&ctx));
        let apply = |snap: &Snapshot| {
            if let (Some(e), Some(l)) = (&laptop, &snap.laptop) {
                e.publish(l, snap.on_battery);
            }
        };
        apply(&first);
        ctx.ready();

        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                c = changed(&mut signals, QUIET) => {
                    if c.is_none() {
                        return Err(IntegrationError::msg("service restarted or went away"));
                    }
                    match read(&conn, &manager).await {
                        Ok(snap) => apply(&snap),
                        Err(e) => tracing::debug!(error = %e, "UPower read failed"),
                    }
                }
            }
        }
    }
}

// --- peripheral batteries -----------------------------------------------------

/// Peripheral batteries from UPower (the kernel's `power_supply` devices,
/// Bluetooth battery reports, ...), as a `peripheral_battery` source.
#[derive(Debug)]
pub struct UPowerPeripherals {
    env: Arc<LinuxEnv>,
}

impl UPowerPeripherals {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }
}

#[async_trait::async_trait]
impl PeripheralSource for UPowerPeripherals {
    fn id(&self) -> &'static str {
        "upower"
    }

    async fn probe(&self) -> Probe {
        probe_upower(&self.env).await
    }

    async fn run(
        &self,
        sink: PeripheralSink,
        cancel: CancellationToken,
    ) -> Result<(), IntegrationError> {
        let conn = self.env.system_bus().await.map_err(dbus_err)?;
        let mut signals = signals_from(&conn, UPOWER).await.map_err(dbus_err)?;
        let manager = Proxy::new(&conn, UPOWER, UPOWER_PATH, UPOWER)
            .await
            .map_err(dbus_err)?;
        let mut known: BTreeSet<EntityId> = BTreeSet::new();
        let report = |list: Vec<PeripheralReading>, known: &mut BTreeSet<EntityId>| {
            let now: BTreeSet<EntityId> = list.iter().map(|r| r.key.clone()).collect();
            for gone in known.difference(&now) {
                sink.remove(gone);
            }
            for r in list {
                sink.update(r);
            }
            *known = now;
        };
        report(
            read_peripherals(&conn, &manager).await.map_err(dbus_err)?,
            &mut known,
        );
        sink.ready();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                c = changed(&mut signals, QUIET) => {
                    if c.is_none() {
                        return Err(IntegrationError::msg("service restarted or went away"));
                    }
                    match read_peripherals(&conn, &manager).await {
                        Ok(list) => report(list, &mut known),
                        Err(e) => tracing::debug!(error = %e, "UPower read failed"),
                    }
                }
            }
        }
    }
}

// --- power profiles ----------------------------------------------------------

/// Bus names of power-profiles-daemon: the current one, then the legacy one.
const PROFILE_NAMES: [(&str, &str); 2] = [
    (
        "org.freedesktop.UPower.PowerProfiles",
        "/org/freedesktop/UPower/PowerProfiles",
    ),
    ("net.hadess.PowerProfiles", "/net/hadess/PowerProfiles"),
];

/// `power_profile` select through power-profiles-daemon.
#[derive(Debug)]
pub struct PowerProfile {
    env: Arc<LinuxEnv>,
}

impl PowerProfile {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }

    async fn proxy(&self) -> Option<Proxy<'static>> {
        let conn = self.env.system_bus().await.ok()?;
        for (name, path) in PROFILE_NAMES {
            if LinuxEnv::has_owner(&conn, name).await {
                return Proxy::new(&conn, name, path, name).await.ok();
            }
        }
        None
    }
}

fn profile_names(profiles: Vec<std::collections::HashMap<String, OwnedValue>>) -> Vec<String> {
    profiles
        .iter()
        .filter_map(|p| prop::<String>(p, "Profile"))
        .collect()
}

#[async_trait::async_trait]
impl Integration for PowerProfile {
    fn id(&self) -> &'static str {
        "power_profile.ppd"
    }

    fn capability(&self) -> &'static str {
        "power_profile"
    }

    fn description(&self) -> &'static str {
        "Power profile select through power-profiles-daemon"
    }

    async fn probe(&self) -> Probe {
        match self.proxy().await {
            Some(p) => Probe::available_with(p.destination().to_string()),
            None => Probe::unavailable("power-profiles-daemon is not running"),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let proxy = self
            .proxy()
            .await
            .ok_or_else(|| IntegrationError::msg("power-profiles-daemon went away"))?;
        let profiles = profile_names(proxy.get_property("Profiles").await.map_err(dbus_err)?);
        let active: String = proxy
            .get_property("ActiveProfile")
            .await
            .map_err(dbus_err)?;
        let mut changes = proxy
            .receive_property_changed::<String>("ActiveProfile")
            .await;
        let (select, mut commands) = ctx.select(
            EntitySpec::select(POWER_PROFILE, "Power profile", profiles.clone())
                .icon("mdi:speedometer"),
        );
        select.set(active);
        ctx.ready();
        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                Some(c) = changes.next() => {
                    if let Ok(p) = c.get().await { select.set(p) }
                }
                cmd = commands.recv() => {
                    let Some(profile) = cmd else { return Ok(()) };
                    if !profiles.contains(&profile) {
                        tracing::warn!(%profile, "unknown power profile requested");
                        continue;
                    }
                    tracing::info!(%profile, "power profile from Home Assistant");
                    if let Err(e) = proxy.set_property("ActiveProfile", Value::from(profile.as_str())).await {
                        tracing::warn!(error = %e, "setting the power profile failed");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::{CommandValue, State};
    use deskhand_testkit::mocks::hardware::device_type;
    use deskhand_testkit::mocks::{MockPowerProfiles, MockUPower, UPowerDevice};
    use deskhand_testkit::{TestBus, require_dbus};

    use super::*;
    use crate::testutil::{rig, wait_entity, wait_state};

    #[tokio::test]
    async fn laptop_battery_and_ac_power() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let upower = MockUPower::start(&bus, UPowerDevice::default())
            .await
            .unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &[]);
        let (mut t, _cancel) = rig(Battery::new(env), "battery");
        wait_state(&mut t, &BATTERY_LEVEL, State::Float(50.0)).await;
        wait_state(&mut t, &BATTERY_TIME, State::Float(60.0)).await;
        wait_state(&mut t, &ON_AC_POWER, State::Bool(true)).await;

        upower.set_on_battery(true).await.unwrap();
        wait_state(&mut t, &ON_AC_POWER, State::Bool(false)).await;
        upower
            .update("DisplayDevice", |d| d.percentage = 42.4)
            .await
            .unwrap();
        wait_state(&mut t, &BATTERY_LEVEL, State::Float(42.0)).await;
    }

    #[tokio::test]
    async fn peripherals_become_devices_keyed_by_serial() {
        use deskhand_common::peripherals::{CAPABILITY, PeripheralBattery};
        use deskhand_core::testing::{wait_attr, wait_spec};
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let upower = MockUPower::start(&bus, UPowerDevice::default())
            .await
            .unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &[]);
        let source: Arc<dyn PeripheralSource> = Arc::new(UPowerPeripherals::new(env));
        let (mut t, _cancel) = rig(PeripheralBattery::new(vec![source]), CAPABILITY);

        // Asleep when the kernel set it up: a stand-in name for now.
        let mouse = UPowerDevice {
            kind: device_type::MOUSE,
            power_supply: false,
            percentage: 77.0,
            model: "Logitech Wireless Mouse PID:4071".into(),
            vendor: "Logitech".into(),
            serial: "4071-ab-cd".into(),
            state: 1,
            warning_level: 1,
            update_time: 1_793_000_000,
            ..UPowerDevice::default()
        };
        upower.add_device("mouse_hidpp_1", mouse).await.unwrap();
        let id = EntityId::new("battery_4071_ab_cd").unwrap();
        wait_state(&mut t, &id, State::Float(77.0)).await;
        wait_attr(&mut t, &id, "kind", serde_json::json!("mouse")).await;
        wait_attr(&mut t, &id, "state", serde_json::json!("charging")).await;
        let low = EntityId::new("battery_4071_ab_cd_low").unwrap();
        wait_state(&mut t, &low, State::Bool(false)).await;
        let updated = EntityId::new("battery_4071_ab_cd_updated").unwrap();
        wait_state(&mut t, &updated, "2026-10-26T07:33:20+00:00".into()).await;
        wait_spec(&mut t, &id, "on the mouse's device", |s| {
            s.device.as_ref().is_some_and(|d| {
                d.name == "Mouse"
                    && d.model.as_deref() == Some("Mouse")
                    && d.manufacturer.as_deref() == Some("Logitech")
                    && d.serial_number.as_deref() == Some("4071-ab-cd")
            })
        })
        .await;

        // Its real name once it wakes: the same entities, renamed.
        upower
            .update("mouse_hidpp_1", |d| {
                d.model = "MX Master 3".into();
                d.warning_level = 3;
            })
            .await
            .unwrap();
        wait_state(&mut t, &low, State::Bool(true)).await;
        wait_spec(&mut t, &id, "the real name", |s| {
            s.device.as_ref().is_some_and(|d| d.name == "MX Master 3")
        })
        .await;
        assert!(
            t.entities
                .iter()
                .all(|e| !e.as_str().starts_with("battery_")
                    || e.as_str().starts_with("battery_4071_ab_cd")),
            "{:?}",
            t.entities
        );

        // Removing it drops the device (after the hub's grace period).
        upower.remove_device("mouse_hidpp_1").await.unwrap();
        wait_entity(&mut t, &id, false).await;
    }

    #[tokio::test]
    async fn power_profile_select_both_ways() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let ppd = MockPowerProfiles::start(&bus).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &[]);
        let (mut t, _cancel) = rig(PowerProfile::new(env), "power_profile");
        wait_state(&mut t, &POWER_PROFILE, "balanced".into()).await;
        t.command("power_profile", CommandValue::Text("performance".into()));
        wait_state(&mut t, &POWER_PROFILE, "performance".into()).await;
        assert_eq!(ppd.active(), "performance");
        ppd.set_active("power-saver").await.unwrap();
        wait_state(&mut t, &POWER_PROFILE, "power-saver".into()).await;
    }
}
