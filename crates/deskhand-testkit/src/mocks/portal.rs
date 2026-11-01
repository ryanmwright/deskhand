//! Mock XDG desktop portal Settings (`org.freedesktop.portal.Desktop`),
//! with the `org.freedesktop.appearance` keys.

use std::sync::{Arc, Mutex};

use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};

use crate::TestBus;

pub const BUS_NAME: &str = "org.freedesktop.portal.Desktop";
pub const PATH: &str = "/org/freedesktop/portal/desktop";
const NAMESPACE: &str = "org.freedesktop.appearance";

#[derive(Debug)]
struct State {
    color_scheme: u32,
    accent: (f64, f64, f64),
}

type Shared = Arc<Mutex<State>>;

struct Settings {
    state: Shared,
}

impl Settings {
    fn value(&self, namespace: &str, key: &str) -> zbus::fdo::Result<OwnedValue> {
        let s = self.state.lock().unwrap();
        let v = match (namespace, key) {
            (NAMESPACE, "color-scheme") => Value::from(s.color_scheme),
            (NAMESPACE, "accent-color") => Value::from(s.accent),
            _ => {
                return Err(zbus::fdo::Error::Failed(format!(
                    "Requested setting not found: {namespace} {key}"
                )));
            }
        };
        v.try_to_owned()
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }
}

#[zbus::interface(name = "org.freedesktop.portal.Settings")]
impl Settings {
    async fn read_one(&self, namespace: &str, key: &str) -> zbus::fdo::Result<OwnedValue> {
        self.value(namespace, key)
    }

    #[zbus(signal)]
    async fn setting_changed(
        e: &SignalEmitter<'_>,
        namespace: &str,
        key: &str,
        value: Value<'_>,
    ) -> zbus::Result<()>;

    #[zbus(property, name = "version")]
    async fn version(&self) -> u32 {
        2
    }
}

/// Mock portal: color-scheme 2 (prefer light), accent #3daee9.
#[derive(Debug, Clone)]
pub struct MockPortal {
    conn: zbus::Connection,
    state: Shared,
}

impl MockPortal {
    pub async fn start(bus: &TestBus) -> zbus::Result<Self> {
        let state: Shared = Arc::new(Mutex::new(State {
            color_scheme: 2,
            accent: (0.239216, 0.682353, 0.913725),
        }));
        let conn = zbus::connection::Builder::address(bus.session_address())?
            .serve_at(
                PATH,
                Settings {
                    state: state.clone(),
                },
            )?
            .name(BUS_NAME)?
            .build()
            .await?;
        Ok(Self { conn, state })
    }

    /// 0 no preference, 1 dark, 2 light.
    pub async fn set_color_scheme(&self, value: u32) -> zbus::Result<()> {
        self.state.lock().unwrap().color_scheme = value;
        let e = SignalEmitter::new(&self.conn, PATH)?;
        Settings::setting_changed(&e, NAMESPACE, "color-scheme", Value::from(value)).await
    }

    /// Components 0..=1, or out of range for "unset".
    pub async fn set_accent(&self, rgb: (f64, f64, f64)) -> zbus::Result<()> {
        self.state.lock().unwrap().accent = rgb;
        let e = SignalEmitter::new(&self.conn, PATH)?;
        Settings::setting_changed(&e, NAMESPACE, "accent-color", Value::from(rgb)).await
    }
}
