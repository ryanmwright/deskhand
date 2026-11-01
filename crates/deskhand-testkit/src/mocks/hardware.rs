//! Mocks for UPower, power-profiles-daemon, NetworkManager (system bus) and
//! GameMode (session bus). Each handle changes state and emits
//! `PropertiesChanged` like the real service.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::TestBus;

fn path(p: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(p.to_owned()).expect("valid object path")
}

// --- UPower ----------------------------------------------------------------

pub const UPOWER: &str = "org.freedesktop.UPower";
pub const UPOWER_PATH: &str = "/org/freedesktop/UPower";
pub const DISPLAY_DEVICE: &str = "/org/freedesktop/UPower/devices/DisplayDevice";

/// UPower device types used by the mocks.
pub mod device_type {
    pub const LINE_POWER: u32 = 1;
    pub const BATTERY: u32 = 2;
    pub const MOUSE: u32 = 5;
    pub const KEYBOARD: u32 = 6;
    pub const HEADSET: u32 = 17;
}

/// One UPower device.
#[derive(Debug, Clone, PartialEq)]
pub struct UPowerDevice {
    pub kind: u32,
    /// 1 charging, 2 discharging, 4 fully charged, ...
    pub state: u32,
    pub percentage: f64,
    pub time_to_empty: i64,
    pub time_to_full: i64,
    pub is_present: bool,
    pub power_supply: bool,
    pub model: String,
    pub serial: String,
}

impl Default for UPowerDevice {
    fn default() -> Self {
        Self {
            kind: device_type::BATTERY,
            state: 2,
            percentage: 50.0,
            time_to_empty: 3600,
            time_to_full: 0,
            is_present: true,
            power_supply: true,
            model: String::new(),
            serial: String::new(),
        }
    }
}

type Devices = Arc<Mutex<HashMap<String, UPowerDevice>>>;

struct UPowerManager {
    on_battery: Arc<Mutex<bool>>,
    devices: Devices,
}

#[zbus::interface(name = "org.freedesktop.UPower")]
impl UPowerManager {
    async fn enumerate_devices(&self) -> Vec<OwnedObjectPath> {
        let mut paths: Vec<String> = self
            .devices
            .lock()
            .unwrap()
            .keys()
            .filter(|p| p.as_str() != DISPLAY_DEVICE)
            .cloned()
            .collect();
        paths.sort();
        paths.iter().map(|p| path(p)).collect()
    }

    async fn get_display_device(&self) -> OwnedObjectPath {
        path(DISPLAY_DEVICE)
    }

    #[zbus(property)]
    async fn on_battery(&self) -> bool {
        *self.on_battery.lock().unwrap()
    }

    #[zbus(signal)]
    async fn device_added(e: &SignalEmitter<'_>, device: OwnedObjectPath) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn device_removed(e: &SignalEmitter<'_>, device: OwnedObjectPath) -> zbus::Result<()>;
}

struct UPowerDeviceObj {
    path: String,
    devices: Devices,
}

impl UPowerDeviceObj {
    fn get(&self) -> UPowerDevice {
        self.devices
            .lock()
            .unwrap()
            .get(&self.path)
            .cloned()
            .unwrap_or_default()
    }
}

#[zbus::interface(name = "org.freedesktop.UPower.Device")]
impl UPowerDeviceObj {
    #[zbus(property, name = "Type")]
    async fn kind(&self) -> u32 {
        self.get().kind
    }
    #[zbus(property)]
    async fn state(&self) -> u32 {
        self.get().state
    }
    #[zbus(property)]
    async fn percentage(&self) -> f64 {
        self.get().percentage
    }
    #[zbus(property)]
    async fn time_to_empty(&self) -> i64 {
        self.get().time_to_empty
    }
    #[zbus(property)]
    async fn time_to_full(&self) -> i64 {
        self.get().time_to_full
    }
    #[zbus(property)]
    async fn is_present(&self) -> bool {
        self.get().is_present
    }
    #[zbus(property)]
    async fn power_supply(&self) -> bool {
        self.get().power_supply
    }
    #[zbus(property)]
    async fn model(&self) -> String {
        self.get().model
    }
    #[zbus(property)]
    async fn serial(&self) -> String {
        self.get().serial
    }
    #[zbus(property)]
    async fn native_path(&self) -> String {
        self.path.rsplit('/').next().unwrap_or("").to_owned()
    }
}

/// Mock `org.freedesktop.UPower`.
#[derive(Debug, Clone)]
pub struct MockUPower {
    conn: zbus::Connection,
    on_battery: Arc<Mutex<bool>>,
    devices: Devices,
}

impl MockUPower {
    /// Starts with a display device; add others with [`MockUPower::add_device`].
    pub async fn start(bus: &TestBus, display: UPowerDevice) -> zbus::Result<Self> {
        let devices: Devices = Arc::default();
        devices
            .lock()
            .unwrap()
            .insert(DISPLAY_DEVICE.into(), display);
        let on_battery = Arc::new(Mutex::new(false));
        let conn = zbus::connection::Builder::address(bus.system_address())?
            .serve_at(
                UPOWER_PATH,
                UPowerManager {
                    on_battery: on_battery.clone(),
                    devices: devices.clone(),
                },
            )?
            .serve_at(
                DISPLAY_DEVICE,
                UPowerDeviceObj {
                    path: DISPLAY_DEVICE.into(),
                    devices: devices.clone(),
                },
            )?
            .name(UPOWER)?
            .build()
            .await?;
        Ok(Self {
            conn,
            on_battery,
            devices,
        })
    }

    /// Adds `/org/freedesktop/UPower/devices/<name>` and emits `DeviceAdded`.
    pub async fn add_device(&self, name: &str, device: UPowerDevice) -> zbus::Result<()> {
        let p = format!("/org/freedesktop/UPower/devices/{name}");
        self.devices.lock().unwrap().insert(p.clone(), device);
        self.conn
            .object_server()
            .at(
                p.as_str(),
                UPowerDeviceObj {
                    path: p.clone(),
                    devices: self.devices.clone(),
                },
            )
            .await?;
        let m = self
            .conn
            .object_server()
            .interface::<_, UPowerManager>(UPOWER_PATH)
            .await?;
        UPowerManager::device_added(m.signal_emitter(), path(&p)).await
    }

    pub async fn remove_device(&self, name: &str) -> zbus::Result<()> {
        let p = format!("/org/freedesktop/UPower/devices/{name}");
        self.devices.lock().unwrap().remove(&p);
        self.conn
            .object_server()
            .remove::<UPowerDeviceObj, _>(p.as_str())
            .await?;
        let m = self
            .conn
            .object_server()
            .interface::<_, UPowerManager>(UPOWER_PATH)
            .await?;
        UPowerManager::device_removed(m.signal_emitter(), path(&p)).await
    }

    /// Changes a device (`"DisplayDevice"` or a name given to `add_device`).
    pub async fn update(&self, name: &str, f: impl FnOnce(&mut UPowerDevice)) -> zbus::Result<()> {
        let p = format!("/org/freedesktop/UPower/devices/{name}");
        if let Some(d) = self.devices.lock().unwrap().get_mut(&p) {
            f(d);
        }
        let iface = self
            .conn
            .object_server()
            .interface::<_, UPowerDeviceObj>(p.as_str())
            .await?;
        let obj = iface.get().await;
        let e = iface.signal_emitter();
        obj.percentage_changed(e).await?;
        obj.state_changed(e).await?;
        obj.time_to_empty_changed(e).await
    }

    pub async fn set_on_battery(&self, on_battery: bool) -> zbus::Result<()> {
        *self.on_battery.lock().unwrap() = on_battery;
        let m = self
            .conn
            .object_server()
            .interface::<_, UPowerManager>(UPOWER_PATH)
            .await?;
        m.get().await.on_battery_changed(m.signal_emitter()).await
    }
}

// --- power-profiles-daemon ---------------------------------------------------

pub const PROFILES: &str = "org.freedesktop.UPower.PowerProfiles";
pub const PROFILES_PATH: &str = "/org/freedesktop/UPower/PowerProfiles";

struct PowerProfiles {
    active: Arc<Mutex<String>>,
}

#[zbus::interface(name = "org.freedesktop.UPower.PowerProfiles")]
impl PowerProfiles {
    #[zbus(property)]
    async fn active_profile(&self) -> String {
        self.active.lock().unwrap().clone()
    }

    #[zbus(property)]
    async fn set_active_profile(&mut self, profile: String) -> zbus::fdo::Result<()> {
        if !["power-saver", "balanced", "performance"].contains(&profile.as_str()) {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "no profile {profile}"
            )));
        }
        *self.active.lock().unwrap() = profile;
        Ok(())
    }

    #[zbus(property)]
    async fn profiles(&self) -> Vec<HashMap<String, OwnedValue>> {
        ["power-saver", "balanced", "performance"]
            .iter()
            .map(|p| {
                HashMap::from([(
                    "Profile".to_owned(),
                    OwnedValue::try_from(Value::from(*p)).expect("string value"),
                )])
            })
            .collect()
    }
}

/// Mock power-profiles-daemon (the new `org.freedesktop.UPower.PowerProfiles` name).
#[derive(Debug, Clone)]
pub struct MockPowerProfiles {
    conn: zbus::Connection,
    active: Arc<Mutex<String>>,
}

impl MockPowerProfiles {
    pub async fn start(bus: &TestBus) -> zbus::Result<Self> {
        let active = Arc::new(Mutex::new("balanced".to_owned()));
        let conn = zbus::connection::Builder::address(bus.system_address())?
            .serve_at(
                PROFILES_PATH,
                PowerProfiles {
                    active: active.clone(),
                },
            )?
            .name(PROFILES)?
            .build()
            .await?;
        Ok(Self { conn, active })
    }

    pub fn active(&self) -> String {
        self.active.lock().unwrap().clone()
    }

    /// Changes the profile as if the user picked it in the desktop.
    pub async fn set_active(&self, profile: &str) -> zbus::Result<()> {
        *self.active.lock().unwrap() = profile.to_owned();
        let i = self
            .conn
            .object_server()
            .interface::<_, PowerProfiles>(PROFILES_PATH)
            .await?;
        i.get()
            .await
            .active_profile_changed(i.signal_emitter())
            .await
    }
}

// --- NetworkManager ----------------------------------------------------------

pub const NM: &str = "org.freedesktop.NetworkManager";
pub const NM_PATH: &str = "/org/freedesktop/NetworkManager";
pub const NM_WIFI_DEVICE: &str = "/org/freedesktop/NetworkManager/Devices/1";
pub const NM_AP: &str = "/org/freedesktop/NetworkManager/AccessPoint/1";

#[derive(Debug, Default)]
struct NmState {
    connectivity: u32,
    ap: Option<(String, u8)>,
}

type Nm = Arc<Mutex<NmState>>;

struct NmManager {
    state: Nm,
}

#[zbus::interface(name = "org.freedesktop.NetworkManager")]
impl NmManager {
    #[zbus(property)]
    async fn connectivity(&self) -> u32 {
        self.state.lock().unwrap().connectivity
    }

    #[zbus(property)]
    async fn devices(&self) -> Vec<OwnedObjectPath> {
        vec![path(NM_WIFI_DEVICE)]
    }
}

struct NmDevice;

#[zbus::interface(name = "org.freedesktop.NetworkManager.Device")]
impl NmDevice {
    #[zbus(property)]
    async fn device_type(&self) -> u32 {
        2 // NM_DEVICE_TYPE_WIFI
    }

    #[zbus(property)]
    async fn interface(&self) -> String {
        "wlan0".into()
    }
}

struct NmWireless {
    state: Nm,
}

#[zbus::interface(name = "org.freedesktop.NetworkManager.Device.Wireless")]
impl NmWireless {
    #[zbus(property)]
    async fn active_access_point(&self) -> OwnedObjectPath {
        path(if self.state.lock().unwrap().ap.is_some() {
            NM_AP
        } else {
            "/"
        })
    }
}

struct NmAccessPoint {
    state: Nm,
}

#[zbus::interface(name = "org.freedesktop.NetworkManager.AccessPoint")]
impl NmAccessPoint {
    #[zbus(property)]
    async fn ssid(&self) -> Vec<u8> {
        self.state
            .lock()
            .unwrap()
            .ap
            .as_ref()
            .map(|a| a.0.as_bytes().to_vec())
            .unwrap_or_default()
    }

    #[zbus(property)]
    async fn strength(&self) -> u8 {
        self.state.lock().unwrap().ap.as_ref().map_or(0, |a| a.1)
    }
}

/// Mock NetworkManager with one Wi-Fi device.
#[derive(Debug, Clone)]
pub struct MockNetworkManager {
    conn: zbus::Connection,
    state: Nm,
}

impl MockNetworkManager {
    pub async fn start(bus: &TestBus) -> zbus::Result<Self> {
        let state: Nm = Arc::new(Mutex::new(NmState {
            connectivity: 4,
            ap: None,
        }));
        let conn = zbus::connection::Builder::address(bus.system_address())?
            .serve_at(
                NM_PATH,
                NmManager {
                    state: state.clone(),
                },
            )?
            .serve_at(NM_WIFI_DEVICE, NmDevice)?
            .serve_at(
                NM_WIFI_DEVICE,
                NmWireless {
                    state: state.clone(),
                },
            )?
            .serve_at(
                NM_AP,
                NmAccessPoint {
                    state: state.clone(),
                },
            )?
            .name(NM)?
            .build()
            .await?;
        Ok(Self { conn, state })
    }

    /// 1 none, 2 portal, 3 limited, 4 full.
    pub async fn set_connectivity(&self, c: u32) -> zbus::Result<()> {
        self.state.lock().unwrap().connectivity = c;
        let i = self
            .conn
            .object_server()
            .interface::<_, NmManager>(NM_PATH)
            .await?;
        i.get().await.connectivity_changed(i.signal_emitter()).await
    }

    /// The associated network, or `None` when disconnected.
    pub async fn set_access_point(&self, ap: Option<(&str, u8)>) -> zbus::Result<()> {
        self.state.lock().unwrap().ap = ap.map(|(s, st)| (s.to_owned(), st));
        let i = self
            .conn
            .object_server()
            .interface::<_, NmWireless>(NM_WIFI_DEVICE)
            .await?;
        i.get()
            .await
            .active_access_point_changed(i.signal_emitter())
            .await?;
        let a = self
            .conn
            .object_server()
            .interface::<_, NmAccessPoint>(NM_AP)
            .await?;
        a.get().await.strength_changed(a.signal_emitter()).await
    }
}

// --- GameMode ----------------------------------------------------------------

pub const GAMEMODE: &str = "com.feralinteractive.GameMode";
pub const GAMEMODE_PATH: &str = "/com/feralinteractive/GameMode";

struct GameMode {
    clients: Arc<Mutex<i32>>,
}

#[zbus::interface(name = "com.feralinteractive.GameMode")]
impl GameMode {
    #[zbus(property)]
    async fn client_count(&self) -> i32 {
        *self.clients.lock().unwrap()
    }
}

/// Mock GameMode daemon (session bus).
#[derive(Debug, Clone)]
pub struct MockGameMode {
    conn: zbus::Connection,
    clients: Arc<Mutex<i32>>,
}

impl MockGameMode {
    pub async fn start(bus: &TestBus) -> zbus::Result<Self> {
        let clients = Arc::new(Mutex::new(0));
        let conn = zbus::connection::Builder::address(bus.session_address())?
            .serve_at(
                GAMEMODE_PATH,
                GameMode {
                    clients: clients.clone(),
                },
            )?
            .name(GAMEMODE)?
            .build()
            .await?;
        Ok(Self { conn, clients })
    }

    pub async fn set_clients(&self, n: i32) -> zbus::Result<()> {
        *self.clients.lock().unwrap() = n;
        let i = self
            .conn
            .object_server()
            .interface::<_, GameMode>(GAMEMODE_PATH)
            .await?;
        i.get().await.client_count_changed(i.signal_emitter()).await
    }
}
