//! Mock Plasma session services: kglobalaccel (`org.kde.kglobalaccel`),
//! PowerDevil's `org.kde.ScreenBrightness` and `org.kde.Shutdown`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use zbus::fdo;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedObjectPath;

use crate::TestBus;

type Calls = Arc<Mutex<Vec<String>>>;

// ---------------------------------------------------------------- kglobalaccel

#[derive(Debug, Default)]
struct AccelState {
    /// component → action → active keys (each exactly four ints)
    components: BTreeMap<String, BTreeMap<String, Vec<Vec<i32>>>>,
    calls: Vec<String>,
}

type SharedAccel = Arc<Mutex<AccelState>>;

fn component_path(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("/component/{safe}")
}

/// Rejects what would abort KWin: a key sequence that is not four ints.
fn check_keys(keys: &[(Vec<i32>,)]) -> fdo::Result<()> {
    match keys.iter().find(|(k,)| k.len() != 4) {
        Some((k,)) => Err(fdo::Error::InvalidArgs(format!(
            "key sequence with {} ints would abort KWin",
            k.len()
        ))),
        None => Ok(()),
    }
}

fn action_parts(id: &[String]) -> fdo::Result<(String, String)> {
    match id {
        [c, a, ..] => Ok((c.clone(), a.clone())),
        _ => Err(fdo::Error::InvalidArgs("action id needs 4 strings".into())),
    }
}

struct Accel {
    state: SharedAccel,
}

impl Accel {
    async fn ensure_component(&self, server: &zbus::ObjectServer, name: &str) -> fdo::Result<()> {
        let new = {
            let mut s = self.state.lock().unwrap();
            let new = !s.components.contains_key(name);
            s.components.entry(name.to_owned()).or_default();
            new
        };
        if new {
            server
                .at(
                    component_path(name),
                    Component {
                        name: name.to_owned(),
                        state: self.state.clone(),
                    },
                )
                .await?;
        }
        Ok(())
    }
}

#[zbus::interface(name = "org.kde.KGlobalAccel")]
impl Accel {
    #[zbus(name = "doRegister")]
    async fn do_register(
        &self,
        id: Vec<String>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> fdo::Result<()> {
        let (c, a) = action_parts(&id)?;
        self.ensure_component(server, &c).await?;
        let mut s = self.state.lock().unwrap();
        s.calls.push(format!("doRegister({c},{a})"));
        s.components
            .get_mut(&c)
            .expect("component")
            .entry(a)
            .or_default();
        Ok(())
    }

    #[zbus(name = "setShortcutKeys")]
    async fn set_shortcut_keys(
        &self,
        id: Vec<String>,
        keys: Vec<(Vec<i32>,)>,
        flags: u32,
    ) -> fdo::Result<Vec<(Vec<i32>,)>> {
        check_keys(&keys)?;
        let (c, a) = action_parts(&id)?;
        let mut s = self.state.lock().unwrap();
        s.calls.push(format!("setShortcutKeys({c},{a},{flags})"));
        let action = s
            .components
            .get_mut(&c)
            .and_then(|c| c.get_mut(&a))
            .ok_or_else(|| fdo::Error::InvalidArgs("not registered".into()))?;
        *action = keys.into_iter().map(|(k,)| k).collect();
        Ok(action.iter().map(|k| (k.clone(),)).collect())
    }

    #[zbus(name = "getComponent")]
    async fn get_component(&self, name: &str) -> fdo::Result<OwnedObjectPath> {
        if !self.state.lock().unwrap().components.contains_key(name) {
            return Err(fdo::Error::Failed(format!("no component {name}")));
        }
        OwnedObjectPath::try_from(component_path(name))
            .map_err(|e| fdo::Error::Failed(e.to_string()))
    }

    #[zbus(name = "unregister")]
    async fn unregister(&self, component: &str, action: &str) -> bool {
        let mut s = self.state.lock().unwrap();
        s.calls.push(format!("unregister({component},{action})"));
        s.components
            .get_mut(component)
            .is_some_and(|c| c.remove(action).is_some())
    }
}

struct Component {
    name: String,
    state: SharedAccel,
}

#[zbus::interface(name = "org.kde.kglobalaccel.Component")]
impl Component {
    #[zbus(name = "shortcutNames")]
    async fn shortcut_names(&self) -> Vec<String> {
        let s = self.state.lock().unwrap();
        s.components
            .get(&self.name)
            .map(|c| c.keys().cloned().collect())
            .unwrap_or_default()
    }

    #[zbus(name = "invokeShortcut")]
    async fn invoke_shortcut(
        &self,
        action: &str,
        #[zbus(signal_emitter)] e: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        self.state
            .lock()
            .unwrap()
            .calls
            .push(format!("invokeShortcut({},{action})", self.name));
        Ok(Self::global_shortcut_pressed(&e, &self.name, action, 0).await?)
    }

    #[zbus(signal, name = "globalShortcutPressed")]
    async fn global_shortcut_pressed(
        e: &SignalEmitter<'_>,
        component: &str,
        action: &str,
        timestamp: i64,
    ) -> zbus::Result<()>;
}

/// Mock kglobalaccel. Key lists that are not four ints per sequence are
/// rejected (real KWin aborts).
#[derive(Debug, Clone)]
pub struct MockKGlobalAccel {
    conn: zbus::Connection,
    state: SharedAccel,
}

impl MockKGlobalAccel {
    pub async fn start(bus: &TestBus) -> zbus::Result<Self> {
        let state = SharedAccel::default();
        let conn = zbus::connection::Builder::address(bus.session_address())?
            .serve_at(
                "/kglobalaccel",
                Accel {
                    state: state.clone(),
                },
            )?
            .name("org.kde.kglobalaccel")?
            .build()
            .await?;
        Ok(Self { conn, state })
    }

    async fn accel(&self) -> zbus::Result<zbus::object_server::InterfaceRef<Accel>> {
        self.conn
            .object_server()
            .interface::<_, Accel>("/kglobalaccel")
            .await
    }

    /// Another app's component with these actions.
    pub async fn add_component(&self, name: &str, actions: &[&str]) -> zbus::Result<()> {
        for a in actions {
            self.add_action(name, a).await?;
        }
        Ok(())
    }

    pub async fn add_action(&self, component: &str, action: &str) -> zbus::Result<()> {
        let accel = self.accel().await?;
        accel
            .get()
            .await
            .ensure_component(self.conn.object_server(), component)
            .await?;
        self.state
            .lock()
            .unwrap()
            .components
            .get_mut(component)
            .expect("component")
            .entry(action.to_owned())
            .or_default();
        Ok(())
    }

    /// The active keys of an action; `None` if not registered.
    pub fn keys(&self, component: &str, action: &str) -> Option<Vec<Vec<i32>>> {
        let s = self.state.lock().unwrap();
        s.components.get(component)?.get(action).cloned()
    }

    /// The user presses an action's keys.
    pub async fn press(&self, component: &str, action: &str) -> zbus::Result<()> {
        let e = SignalEmitter::new(&self.conn, component_path(component))?;
        Component::global_shortcut_pressed(&e, component, action, 0).await
    }

    pub fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }
}

// ------------------------------------------------------------ ScreenBrightness

pub const BRIGHTNESS_PATH: &str = "/org/kde/ScreenBrightness";

#[derive(Debug, Clone)]
struct DisplayState {
    label: String,
    internal: bool,
    max: i32,
    value: i32,
}

#[derive(Debug, Default)]
struct BrightnessState {
    displays: BTreeMap<String, DisplayState>,
    calls: Vec<String>,
}

type SharedBrightness = Arc<Mutex<BrightnessState>>;

struct BrightnessRoot {
    state: SharedBrightness,
}

#[zbus::interface(name = "org.kde.ScreenBrightness")]
impl BrightnessRoot {
    #[zbus(property, name = "DisplaysDBusNames")]
    async fn displays_dbus_names(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .displays
            .keys()
            .cloned()
            .collect()
    }

    #[zbus(signal, name = "DisplayAdded")]
    async fn display_added(e: &SignalEmitter<'_>, name: &str) -> zbus::Result<()>;

    #[zbus(signal, name = "DisplayRemoved")]
    async fn display_removed(e: &SignalEmitter<'_>, name: &str) -> zbus::Result<()>;

    #[zbus(signal, name = "BrightnessChanged")]
    async fn brightness_changed(
        e: &SignalEmitter<'_>,
        name: &str,
        value: i32,
        client: &str,
        context: &str,
    ) -> zbus::Result<()>;
}

struct BrightnessDisplay {
    name: String,
    state: SharedBrightness,
}

impl BrightnessDisplay {
    fn get(&self) -> DisplayState {
        self.state.lock().unwrap().displays[&self.name].clone()
    }
}

#[zbus::interface(name = "org.kde.ScreenBrightness.Display")]
impl BrightnessDisplay {
    #[zbus(name = "SetBrightness")]
    async fn set_brightness(
        &self,
        value: i32,
        flags: u32,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> fdo::Result<()> {
        {
            let mut s = self.state.lock().unwrap();
            s.calls
                .push(format!("{}.SetBrightness({value},{flags})", self.name));
            let d = s
                .displays
                .get_mut(&self.name)
                .ok_or_else(|| fdo::Error::Failed("gone".into()))?;
            d.value = value.clamp(0, d.max);
        }
        notify_brightness(conn, &self.name).await?;
        Ok(())
    }

    #[zbus(property, name = "Label")]
    async fn label(&self) -> String {
        self.get().label
    }

    #[zbus(property, name = "IsInternal")]
    async fn is_internal(&self) -> bool {
        self.get().internal
    }

    #[zbus(property, name = "Brightness")]
    async fn brightness(&self) -> i32 {
        self.get().value
    }

    #[zbus(property, name = "MaxBrightness")]
    async fn max_brightness(&self) -> i32 {
        self.get().max
    }
}

async fn notify_brightness(conn: &zbus::Connection, name: &str) -> zbus::Result<()> {
    let path = format!("{BRIGHTNESS_PATH}/{name}");
    let iface = conn
        .object_server()
        .interface::<_, BrightnessDisplay>(path.as_str())
        .await?;
    let value = iface.get().await.get().value;
    iface
        .get()
        .await
        .brightness_changed(iface.signal_emitter())
        .await?;
    let root = SignalEmitter::new(conn, BRIGHTNESS_PATH)?;
    BrightnessRoot::brightness_changed(&root, name, value, "", "").await
}

/// Mock PowerDevil ScreenBrightness without displays.
#[derive(Debug, Clone)]
pub struct MockScreenBrightness {
    conn: zbus::Connection,
    state: SharedBrightness,
}

impl MockScreenBrightness {
    pub async fn start(bus: &TestBus) -> zbus::Result<Self> {
        let state = SharedBrightness::default();
        let conn = zbus::connection::Builder::address(bus.session_address())?
            .serve_at(
                BRIGHTNESS_PATH,
                BrightnessRoot {
                    state: state.clone(),
                },
            )?
            .name("org.kde.ScreenBrightness")?
            .build()
            .await?;
        Ok(Self { conn, state })
    }

    async fn displays_changed(&self) -> zbus::Result<()> {
        let root = self
            .conn
            .object_server()
            .interface::<_, BrightnessRoot>(BRIGHTNESS_PATH)
            .await?;
        root.get()
            .await
            .displays_d_bus_names_changed(root.signal_emitter())
            .await
    }

    pub async fn add_display(
        &self,
        name: &str,
        label: &str,
        internal: bool,
        max: i32,
        value: i32,
    ) -> zbus::Result<()> {
        self.state.lock().unwrap().displays.insert(
            name.to_owned(),
            DisplayState {
                label: label.to_owned(),
                internal,
                max,
                value,
            },
        );
        self.conn
            .object_server()
            .at(
                format!("{BRIGHTNESS_PATH}/{name}"),
                BrightnessDisplay {
                    name: name.to_owned(),
                    state: self.state.clone(),
                },
            )
            .await?;
        let root = SignalEmitter::new(&self.conn, BRIGHTNESS_PATH)?;
        BrightnessRoot::display_added(&root, name).await?;
        self.displays_changed().await
    }

    pub async fn remove_display(&self, name: &str) -> zbus::Result<()> {
        self.state.lock().unwrap().displays.remove(name);
        self.conn
            .object_server()
            .remove::<BrightnessDisplay, _>(format!("{BRIGHTNESS_PATH}/{name}"))
            .await?;
        let root = SignalEmitter::new(&self.conn, BRIGHTNESS_PATH)?;
        BrightnessRoot::display_removed(&root, name).await?;
        self.displays_changed().await
    }

    /// Brightness changed outside Deskhand (keys, the tray).
    pub async fn set_brightness(&self, name: &str, value: i32) -> zbus::Result<()> {
        self.state
            .lock()
            .unwrap()
            .displays
            .get_mut(name)
            .expect("display")
            .value = value;
        notify_brightness(&self.conn, name).await
    }

    pub fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }
}

// -------------------------------------------------------------------- Shutdown

struct Shutdown {
    calls: Calls,
}

#[zbus::interface(name = "org.kde.Shutdown")]
impl Shutdown {
    #[zbus(name = "logout")]
    async fn logout(&self) {
        self.calls.lock().unwrap().push("logout".into());
    }

    #[zbus(name = "logoutAndReboot")]
    async fn logout_and_reboot(&self) {
        self.calls.lock().unwrap().push("logoutAndReboot".into());
    }

    #[zbus(name = "logoutAndShutdown")]
    async fn logout_and_shutdown(&self) {
        self.calls.lock().unwrap().push("logoutAndShutdown".into());
    }
}

/// Mock `org.kde.Shutdown` (records calls; logs nobody out).
#[derive(Debug, Clone)]
pub struct MockShutdown {
    _conn: zbus::Connection,
    calls: Calls,
}

impl MockShutdown {
    pub async fn start(bus: &TestBus) -> zbus::Result<Self> {
        let calls = Calls::default();
        let conn = zbus::connection::Builder::address(bus.session_address())?
            .serve_at(
                "/Shutdown",
                Shutdown {
                    calls: calls.clone(),
                },
            )?
            .name("org.kde.Shutdown")?
            .build()
            .await?;
        Ok(Self { _conn: conn, calls })
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}
