//! Mock systemd user manager (`org.freedesktop.systemd1` on the session bus)
//! with a fixed set of units.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use zbus::fdo;
use zbus::zvariant::OwnedObjectPath;

use crate::TestBus;

pub const SYSTEMD: &str = "org.freedesktop.systemd1";
pub const MANAGER_PATH: &str = "/org/freedesktop/systemd1";

#[derive(Debug, Default)]
struct State {
    units: BTreeMap<String, String>,
    calls: Vec<String>,
    subscribed: bool,
}

type Shared = Arc<Mutex<State>>;

/// systemd's object path escaping (`foo.service` → `foo_2eservice`).
pub fn unit_path(name: &str) -> String {
    let mut out = String::from("/org/freedesktop/systemd1/unit/");
    for (i, b) in name.bytes().enumerate() {
        if b.is_ascii_alphanumeric() && !(i == 0 && b.is_ascii_digit()) {
            out.push(b as char);
        } else {
            out.push_str(&format!("_{b:02x}"));
        }
    }
    out
}

struct Manager {
    state: Shared,
}

impl Manager {
    fn unit(&self, name: &str) -> fdo::Result<OwnedObjectPath> {
        if self.state.lock().unwrap().units.contains_key(name) {
            Ok(OwnedObjectPath::try_from(unit_path(name)).expect("valid path"))
        } else {
            Err(fdo::Error::Failed(format!("Unit {name} not loaded.")))
        }
    }

    async fn set(
        &self,
        conn: &zbus::Connection,
        name: &str,
        state: &str,
    ) -> fdo::Result<OwnedObjectPath> {
        self.unit(name)?;
        self.state
            .lock()
            .unwrap()
            .units
            .insert(name.into(), state.into());
        emit(conn, name).await?;
        Ok(OwnedObjectPath::try_from("/org/freedesktop/systemd1/job/1").expect("valid path"))
    }
}

async fn emit(conn: &zbus::Connection, name: &str) -> zbus::Result<()> {
    let i = conn
        .object_server()
        .interface::<_, Unit>(unit_path(name).as_str())
        .await?;
    i.get().await.active_state_changed(i.signal_emitter()).await
}

#[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
impl Manager {
    async fn subscribe(&self) {
        self.state.lock().unwrap().subscribed = true;
    }

    async fn get_unit(&self, name: &str) -> fdo::Result<OwnedObjectPath> {
        self.unit(name)
    }

    async fn load_unit(&self, name: &str) -> fdo::Result<OwnedObjectPath> {
        self.unit(name)
    }

    async fn start_unit(
        &self,
        name: &str,
        mode: &str,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        self.state
            .lock()
            .unwrap()
            .calls
            .push(format!("StartUnit({name},{mode})"));
        self.set(conn, name, "active").await
    }

    async fn stop_unit(
        &self,
        name: &str,
        mode: &str,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        self.state
            .lock()
            .unwrap()
            .calls
            .push(format!("StopUnit({name},{mode})"));
        self.set(conn, name, "inactive").await
    }
}

struct Unit {
    name: String,
    state: Shared,
}

#[zbus::interface(name = "org.freedesktop.systemd1.Unit")]
impl Unit {
    #[zbus(property)]
    async fn active_state(&self) -> String {
        self.state
            .lock()
            .unwrap()
            .units
            .get(&self.name)
            .cloned()
            .unwrap_or_default()
    }

    #[zbus(property)]
    async fn id(&self) -> String {
        self.name.clone()
    }

    #[zbus(property)]
    async fn description(&self) -> String {
        format!("Test unit {}", self.name)
    }
}

/// Mock systemd user manager.
#[derive(Debug, Clone)]
pub struct MockSystemd {
    conn: zbus::Connection,
    state: Shared,
}

impl MockSystemd {
    /// `units`: (name, ActiveState).
    pub async fn start(bus: &TestBus, units: &[(&str, &str)]) -> zbus::Result<Self> {
        let state: Shared = Arc::default();
        let mut b = zbus::connection::Builder::address(bus.session_address())?.serve_at(
            MANAGER_PATH,
            Manager {
                state: state.clone(),
            },
        )?;
        for (name, active) in units {
            state
                .lock()
                .unwrap()
                .units
                .insert((*name).into(), (*active).into());
            b = b.serve_at(
                unit_path(name),
                Unit {
                    name: (*name).into(),
                    state: state.clone(),
                },
            )?;
        }
        let conn = b.name(SYSTEMD)?.build().await?;
        Ok(Self { conn, state })
    }

    pub fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn subscribed(&self) -> bool {
        self.state.lock().unwrap().subscribed
    }

    /// Changes a unit's state as if it started or crashed by itself.
    pub async fn set_state(&self, name: &str, active_state: &str) -> zbus::Result<()> {
        self.state
            .lock()
            .unwrap()
            .units
            .insert(name.into(), active_state.into());
        emit(&self.conn, name).await
    }
}
