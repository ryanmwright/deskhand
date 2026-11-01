//! Mock KWin (`org.kde.KWin` on the session bus): scripting,
//! VirtualDesktopManager and NightLight.
//!
//! Scripts are not executed. A loaded script must declare its callback
//! target on one line as `var TARGET = {"service":…,"path":…,"iface":…};`;
//! [`MockKWin::script_event`] then calls `Event(json)` on that target from
//! KWin's own connection, like the real `callDBus` would.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use zbus::fdo;
use zbus::object_server::SignalEmitter;

use crate::TestBus;

pub const BUS_NAME: &str = "org.kde.KWin";
pub const VDM_PATH: &str = "/VirtualDesktopManager";
pub const NIGHT_LIGHT_PATH: &str = "/org/kde/KWin/NightLight";

#[derive(Debug, Default)]
struct State {
    /// plugin → (id, script text)
    scripts: BTreeMap<String, (i32, String)>,
    running: Vec<i32>,
    next_id: i32,
    /// (id, name)
    desktops: Vec<(String, String)>,
    current: String,
    night_running: bool,
    night_temperature: u32,
    inhibit_cookies: Vec<u32>,
    next_cookie: u32,
    calls: Vec<String>,
}

type Shared = Arc<Mutex<State>>;

struct Scripting {
    state: Shared,
}

#[zbus::interface(name = "org.kde.kwin.Scripting")]
impl Scripting {
    #[zbus(name = "loadScript")]
    async fn load_script(
        &self,
        path: &str,
        plugin: &str,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> fdo::Result<i32> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| fdo::Error::Failed(format!("cannot read {path}: {e}")))?;
        let id = {
            let mut s = self.state.lock().unwrap();
            if s.scripts.contains_key(plugin) {
                return Ok(-1);
            }
            s.next_id += 1;
            let id = s.next_id;
            s.scripts.insert(plugin.to_owned(), (id, text));
            id
        };
        server
            .at(
                format!("/Scripting/Script{id}"),
                Script {
                    id,
                    state: self.state.clone(),
                },
            )
            .await?;
        Ok(id)
    }

    #[zbus(name = "unloadScript")]
    async fn unload_script(&self, plugin: &str) -> bool {
        let mut s = self.state.lock().unwrap();
        s.calls.push(format!("unloadScript({plugin})"));
        match s.scripts.remove(plugin) {
            Some((id, _)) => {
                s.running.retain(|r| *r != id);
                true
            }
            None => false,
        }
    }

    #[zbus(name = "isScriptLoaded")]
    async fn is_script_loaded(&self, plugin: &str) -> bool {
        self.state.lock().unwrap().scripts.contains_key(plugin)
    }
}

struct Script {
    id: i32,
    state: Shared,
}

#[zbus::interface(name = "org.kde.kwin.Script")]
impl Script {
    #[zbus(name = "run")]
    async fn run(&self) {
        self.state.lock().unwrap().running.push(self.id);
    }
}

struct Vdm {
    state: Shared,
}

#[zbus::interface(name = "org.kde.KWin.VirtualDesktopManager")]
impl Vdm {
    #[zbus(property, name = "count")]
    async fn count(&self) -> u32 {
        self.state.lock().unwrap().desktops.len() as u32
    }

    #[zbus(property, name = "current")]
    async fn current(&self) -> String {
        self.state.lock().unwrap().current.clone()
    }

    #[zbus(property, name = "current")]
    async fn set_current(&mut self, id: String) -> fdo::Result<()> {
        let mut s = self.state.lock().unwrap();
        if !s.desktops.iter().any(|(d, _)| *d == id) {
            return Err(fdo::Error::InvalidArgs(format!("no desktop {id}")));
        }
        s.calls.push(format!("current={id}"));
        s.current = id;
        Ok(())
    }

    #[zbus(property, name = "desktops")]
    // Real KWin sends a(uss) although its introspection says a(iss).
    async fn desktops(&self) -> Vec<(u32, String, String)> {
        let s = self.state.lock().unwrap();
        s.desktops
            .iter()
            .enumerate()
            .map(|(i, (id, name))| (i as u32, id.clone(), name.clone()))
            .collect()
    }

    /// KWin's own `currentChanged(s)`, besides PropertiesChanged.
    #[zbus(signal, name = "currentChanged")]
    async fn current_changed_signal(e: &SignalEmitter<'_>, id: &str) -> zbus::Result<()>;
}

struct NightLight {
    state: Shared,
}

#[zbus::interface(name = "org.kde.KWin.NightLight")]
impl NightLight {
    #[zbus(name = "inhibit")]
    async fn inhibit(&self, #[zbus(signal_emitter)] e: SignalEmitter<'_>) -> fdo::Result<u32> {
        let cookie = {
            let mut s = self.state.lock().unwrap();
            s.next_cookie += 1;
            let c = s.next_cookie;
            s.inhibit_cookies.push(c);
            s.calls.push("inhibit".into());
            c
        };
        self.inhibited_changed(&e).await?;
        Ok(cookie)
    }

    #[zbus(name = "uninhibit")]
    async fn uninhibit(
        &self,
        cookie: u32,
        #[zbus(signal_emitter)] e: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        {
            let mut s = self.state.lock().unwrap();
            s.inhibit_cookies.retain(|c| *c != cookie);
            s.calls.push(format!("uninhibit({cookie})"));
        }
        self.inhibited_changed(&e).await?;
        Ok(())
    }

    #[zbus(property, name = "available")]
    async fn available(&self) -> bool {
        true
    }

    #[zbus(property, name = "enabled")]
    async fn enabled(&self) -> bool {
        true
    }

    #[zbus(property, name = "running")]
    async fn running(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.night_running && s.inhibit_cookies.is_empty()
    }

    #[zbus(property, name = "inhibited")]
    async fn inhibited(&self) -> bool {
        !self.state.lock().unwrap().inhibit_cookies.is_empty()
    }

    #[zbus(property, name = "currentTemperature")]
    async fn current_temperature(&self) -> u32 {
        let s = self.state.lock().unwrap();
        if s.night_running && s.inhibit_cookies.is_empty() {
            s.night_temperature
        } else {
            6500
        }
    }

    #[zbus(property, name = "targetTemperature")]
    async fn target_temperature(&self) -> u32 {
        self.state.lock().unwrap().night_temperature
    }

    #[zbus(property, name = "mode")]
    async fn mode(&self) -> u32 {
        0
    }
}

/// Mock KWin with one virtual desktop ("Desktop 1") and night light off.
#[derive(Debug, Clone)]
pub struct MockKWin {
    conn: zbus::Connection,
    state: Shared,
}

impl MockKWin {
    pub async fn start(bus: &TestBus) -> zbus::Result<Self> {
        let state: Shared = Arc::new(Mutex::new(State {
            desktops: vec![("d1".into(), "Desktop 1".into())],
            current: "d1".into(),
            night_temperature: 4500,
            ..State::default()
        }));
        let conn = zbus::connection::Builder::address(bus.session_address())?
            .serve_at(
                "/Scripting",
                Scripting {
                    state: state.clone(),
                },
            )?
            .serve_at(
                VDM_PATH,
                Vdm {
                    state: state.clone(),
                },
            )?
            .serve_at(
                NIGHT_LIGHT_PATH,
                NightLight {
                    state: state.clone(),
                },
            )?
            .name(BUS_NAME)?
            .build()
            .await?;
        Ok(Self { conn, state })
    }

    pub fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }

    /// Text of a loaded script, if it is loaded and running.
    pub fn running_script(&self, plugin: &str) -> Option<String> {
        let s = self.state.lock().unwrap();
        let (id, text) = s.scripts.get(plugin)?;
        s.running.contains(id).then(|| text.clone())
    }

    /// Calls `Event(json)` on the target a running script declared, from
    /// KWin's connection.
    pub async fn script_event(&self, plugin: &str, json: &str) -> zbus::Result<()> {
        let text = self
            .running_script(plugin)
            .ok_or_else(|| zbus::Error::Failure(format!("script {plugin} is not running")))?;
        let target = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("var TARGET = "))
            .and_then(|l| l.strip_suffix(';'))
            .ok_or_else(|| zbus::Error::Failure("script declares no TARGET".into()))?;
        let field = |name: &str| -> zbus::Result<String> {
            let key = format!("\"{name}\":\"");
            let start = target
                .find(&key)
                .ok_or_else(|| zbus::Error::Failure(format!("TARGET has no {name}")))?
                + key.len();
            let end = target[start..]
                .find('"')
                .ok_or_else(|| zbus::Error::Failure("bad TARGET".into()))?;
            Ok(target[start..start + end].to_owned())
        };
        let (service, path, iface) = (field("service")?, field("path")?, field("iface")?);
        let proxy = zbus::Proxy::new(&self.conn, service, path, iface).await?;
        proxy.call::<_, _, ()>("Event", &(json,)).await
    }

    /// Replaces the virtual desktops: `(id, name)`, the first is current
    /// unless `current` names another.
    pub async fn set_desktops(&self, desktops: &[(&str, &str)], current: &str) -> zbus::Result<()> {
        {
            let mut s = self.state.lock().unwrap();
            s.desktops = desktops
                .iter()
                .map(|(i, n)| ((*i).to_owned(), (*n).to_owned()))
                .collect();
            s.current = current.to_owned();
        }
        let vdm = self
            .conn
            .object_server()
            .interface::<_, Vdm>(VDM_PATH)
            .await?;
        let v = vdm.get().await;
        v.desktops_changed(vdm.signal_emitter()).await?;
        v.count_changed(vdm.signal_emitter()).await?;
        v.current_changed(vdm.signal_emitter()).await?;
        Vdm::current_changed_signal(vdm.signal_emitter(), current).await
    }

    /// Night light starts or stops (as at sunset or sunrise).
    pub async fn set_night_running(&self, running: bool) -> zbus::Result<()> {
        self.state.lock().unwrap().night_running = running;
        let nl = self
            .conn
            .object_server()
            .interface::<_, NightLight>(NIGHT_LIGHT_PATH)
            .await?;
        let n = nl.get().await;
        n.running_changed(nl.signal_emitter()).await?;
        n.current_temperature_changed(nl.signal_emitter()).await
    }
}
