//! Mock BlueZ (`org.bluez`, system bus): an ObjectManager at `/`, adapter
//! `hci0` and devices with `Device1` (and optionally `Battery1`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use zbus::fdo;
use zbus::object_server::SignalEmitter;

use crate::TestBus;

pub const BLUEZ: &str = "org.bluez";
pub const ADAPTER: &str = "/org/bluez/hci0";

#[derive(Debug, Clone)]
struct DeviceState {
    address: String,
    alias: String,
    paired: bool,
    connected: bool,
}

#[derive(Debug, Default)]
struct State {
    powered: bool,
    devices: BTreeMap<String, DeviceState>,
    calls: Vec<String>,
}

type Shared = Arc<Mutex<State>>;

/// `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF`
pub fn device_path(mac: &str) -> String {
    format!("{ADAPTER}/dev_{}", mac.replace(':', "_"))
}

struct Adapter {
    state: Shared,
}

#[zbus::interface(name = "org.bluez.Adapter1")]
impl Adapter {
    #[zbus(property)]
    async fn powered(&self) -> bool {
        self.state.lock().unwrap().powered
    }

    #[zbus(property)]
    async fn set_powered(&mut self, on: bool) {
        let mut s = self.state.lock().unwrap();
        s.calls.push(format!("Powered={on}"));
        s.powered = on;
        if !on {
            for d in s.devices.values_mut() {
                d.connected = false;
            }
        }
    }

    #[zbus(property)]
    async fn address(&self) -> String {
        "00:11:22:33:44:55".into()
    }

    #[zbus(property)]
    async fn alias(&self) -> String {
        "test-adapter".into()
    }
}

struct Device {
    path: String,
    state: Shared,
}

impl Device {
    fn get(&self) -> DeviceState {
        self.state.lock().unwrap().devices[&self.path].clone()
    }

    fn set_connected(&self, connected: bool) {
        if let Some(d) = self.state.lock().unwrap().devices.get_mut(&self.path) {
            d.connected = connected;
        }
    }
}

#[zbus::interface(name = "org.bluez.Device1")]
impl Device {
    async fn connect(&self, #[zbus(signal_emitter)] e: SignalEmitter<'_>) -> fdo::Result<()> {
        if !self.state.lock().unwrap().powered {
            return Err(fdo::Error::Failed("org.bluez.Error.NotReady".into()));
        }
        let address = self.get().address;
        self.state
            .lock()
            .unwrap()
            .calls
            .push(format!("Connect({address})"));
        self.set_connected(true);
        self.connected_changed(&e).await?;
        Ok(())
    }

    async fn disconnect(&self, #[zbus(signal_emitter)] e: SignalEmitter<'_>) -> fdo::Result<()> {
        let address = self.get().address;
        self.state
            .lock()
            .unwrap()
            .calls
            .push(format!("Disconnect({address})"));
        self.set_connected(false);
        self.connected_changed(&e).await?;
        Ok(())
    }

    #[zbus(property)]
    async fn address(&self) -> String {
        self.get().address
    }
    #[zbus(property)]
    async fn alias(&self) -> String {
        self.get().alias
    }
    #[zbus(property)]
    async fn name(&self) -> String {
        self.get().alias
    }
    #[zbus(property)]
    async fn paired(&self) -> bool {
        self.get().paired
    }
    #[zbus(property)]
    async fn connected(&self) -> bool {
        self.get().connected
    }
    #[zbus(property)]
    async fn icon(&self) -> String {
        "audio-headset".into()
    }
}

struct Battery {
    percentage: Arc<Mutex<u8>>,
}

#[zbus::interface(name = "org.bluez.Battery1")]
impl Battery {
    #[zbus(property)]
    async fn percentage(&self) -> u8 {
        *self.percentage.lock().unwrap()
    }
}

/// Mock BlueZ daemon.
#[derive(Debug, Clone)]
pub struct MockBluez {
    conn: zbus::Connection,
    state: Shared,
}

impl MockBluez {
    pub async fn start(bus: &TestBus, powered: bool) -> zbus::Result<Self> {
        let state: Shared = Arc::new(Mutex::new(State {
            powered,
            ..State::default()
        }));
        let conn = zbus::connection::Builder::address(bus.system_address())?
            .serve_at("/", fdo::ObjectManager)?
            .serve_at(
                ADAPTER,
                Adapter {
                    state: state.clone(),
                },
            )?
            .name(BLUEZ)?
            .build()
            .await?;
        Ok(Self { conn, state })
    }

    pub fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn powered(&self) -> bool {
        self.state.lock().unwrap().powered
    }

    /// Adds a device (InterfacesAdded is emitted by the ObjectManager).
    pub async fn add_device(
        &self,
        mac: &str,
        alias: &str,
        paired: bool,
        battery: Option<u8>,
    ) -> zbus::Result<()> {
        let p = device_path(mac);
        self.state.lock().unwrap().devices.insert(
            p.clone(),
            DeviceState {
                address: mac.into(),
                alias: alias.into(),
                paired,
                connected: false,
            },
        );
        let server = self.conn.object_server();
        server
            .at(
                p.as_str(),
                Device {
                    path: p.clone(),
                    state: self.state.clone(),
                },
            )
            .await?;
        if let Some(b) = battery {
            server
                .at(
                    p.as_str(),
                    Battery {
                        percentage: Arc::new(Mutex::new(b)),
                    },
                )
                .await?;
        }
        Ok(())
    }

    pub async fn remove_device(&self, mac: &str) -> zbus::Result<()> {
        let p = device_path(mac);
        let server = self.conn.object_server();
        let _ = server.remove::<Battery, _>(p.as_str()).await;
        server.remove::<Device, _>(p.as_str()).await?;
        self.state.lock().unwrap().devices.remove(&p);
        Ok(())
    }

    /// Simulates the device connecting or disconnecting by itself.
    pub async fn set_connected(&self, mac: &str, connected: bool) -> zbus::Result<()> {
        let p = device_path(mac);
        if let Some(d) = self.state.lock().unwrap().devices.get_mut(&p) {
            d.connected = connected;
        }
        let i = self
            .conn
            .object_server()
            .interface::<_, Device>(p.as_str())
            .await?;
        i.get().await.connected_changed(i.signal_emitter()).await
    }

    pub fn connected(&self, mac: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .devices
            .get(&device_path(mac))
            .is_some_and(|d| d.connected)
    }
}
