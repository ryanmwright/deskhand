//! `active_window` capability: the active window, whether it is fullscreen,
//! and the number of displays, reported by a small KWin script.
//!
//! The script is loaded with `org.kde.kwin.Scripting.loadScript` and calls
//! back with `callDBus` on our session connection, passing one JSON string
//! (docs/dev/spikes.md §2). Calls are accepted only from the current owner
//! of `org.kde.KWin`. When KWin restarts the integration fails, and the
//! supervisor restarts it, which loads the script again.
//!
//! | entity | |
//! |---|---|
//! | `active_window` | app id (desktop file name, else window class); `none` without an active window. Attributes: `title`, `app_id`, `resource_class`, `pid`, `output` |
//! | `fullscreen` | the active window is fullscreen |
//! | `display_count` | number of screens |
//!
//! ```toml
//! [integrations.active_window]
//! publish_title = true   # false leaves window titles out of Home Assistant
//! ```

use std::sync::{Arc, Mutex};
use std::time::Duration;

use deskhand_core::{Attributes, Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use deskhand_linux::LinuxEnv;
use deskhand_linux::util::dbus_err;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use zbus::message::Header;
use zbus::{Proxy, fdo};

use crate::{KWIN, kwin_session};

pub const ACTIVE_WINDOW: EntityId = EntityId::from_static("active_window");
pub const FULLSCREEN: EntityId = EntityId::from_static("fullscreen");
pub const DISPLAY_COUNT: EntityId = EntityId::from_static("display_count");

/// KWin script plugin name (also used to unload a leftover copy).
pub const PLUGIN: &str = "deskhand-active-window";
/// Object and interface the script calls. Derived from `APP_ID`, but the
/// interface macro needs literals.
pub const CALLBACK_PATH: &str = "/io/github/ryanmwright/Deskhand/KWin";
pub const CALLBACK_IFACE: &str = "io.github.ryanmwright.Deskhand.KWin";

/// `[integrations.active_window]` options.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ActiveWindowOptions {
    pub publish_title: bool,
}

impl Default for ActiveWindowOptions {
    fn default() -> Self {
        Self {
            publish_title: true,
        }
    }
}

/// One report from the script.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Report {
    #[serde(default)]
    pub kind: String,
    pub window: Option<Window>,
    #[serde(default)]
    pub screens: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Window {
    pub app_id: String,
    pub resource_class: String,
    pub caption: String,
    pub pid: Option<i64>,
    pub fullscreen: bool,
    pub output: Option<String>,
}

/// The KWin script. `TARGET` is on one line by itself (the test mock reads it).
pub fn script(service: &str) -> String {
    let target = json!({ "service": service, "path": CALLBACK_PATH, "iface": CALLBACK_IFACE });
    format!(
        r#"// Deskhand: reports the active window. Loaded and unloaded by Deskhand.
var TARGET = {target};
var current = null;
function info(w) {{
    if (!w) return null;
    return {{
        app_id: String(w.desktopFileName || w.resourceClass || ""),
        resource_class: String(w.resourceClass || ""),
        caption: String(w.caption || ""),
        pid: w.pid,
        fullscreen: !!w.fullScreen,
        output: w.output ? String(w.output.name) : null
    }};
}}
function send(kind) {{
    callDBus(TARGET.service, TARGET.path, TARGET.iface, "Event",
        JSON.stringify({{ kind: kind, window: info(workspace.activeWindow), screens: workspace.screens.length }}));
}}
function changed() {{ send("changed"); }}
function track(w) {{
    if (current) {{
        try {{
            current.captionChanged.disconnect(changed);
            current.fullScreenChanged.disconnect(changed);
        }} catch (e) {{}}
    }}
    current = w;
    if (w) {{
        w.captionChanged.connect(changed);
        w.fullScreenChanged.connect(changed);
    }}
}}
workspace.windowActivated.connect(function (w) {{ track(w); send("activated"); }});
workspace.screensChanged.connect(function () {{ send("screens"); }});
track(workspace.activeWindow);
send("initial");
"#
    )
}

/// Receives the script's calls; only KWin may call.
struct Callback {
    tx: mpsc::UnboundedSender<String>,
    kwin_owner: Arc<Mutex<String>>,
}

#[zbus::interface(name = "io.github.ryanmwright.Deskhand.KWin")]
impl Callback {
    async fn event(&self, json: String, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        let owner = self
            .kwin_owner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if sender.is_empty() || sender != owner {
            tracing::warn!(%sender, "rejected an active-window report not sent by KWin");
            return Err(fdo::Error::AccessDenied(
                "only KWin may report windows".into(),
            ));
        }
        let _ = self.tx.send(json);
        Ok(())
    }
}

/// Active window through a KWin script.
#[derive(Debug)]
pub struct ActiveWindow {
    env: Arc<LinuxEnv>,
}

impl ActiveWindow {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }
}

struct Entities {
    window: deskhand_core::ctx::Sensor,
    fullscreen: deskhand_core::ctx::BinarySensor,
    displays: deskhand_core::ctx::Sensor,
    publish_title: bool,
}

impl Entities {
    fn publish(&self, r: &Report) {
        self.displays.set(r.screens);
        let mut attrs = Attributes::new();
        match &r.window {
            Some(w) => {
                if self.publish_title {
                    attrs.insert("title".into(), json!(w.caption));
                }
                attrs.insert("app_id".into(), json!(w.app_id));
                attrs.insert("resource_class".into(), json!(w.resource_class));
                attrs.insert("pid".into(), json!(w.pid));
                attrs.insert("output".into(), json!(w.output));
                self.window.set_attributes(attrs);
                let id = if w.app_id.is_empty() {
                    "unknown_app"
                } else {
                    &w.app_id
                };
                self.window.set(id);
                self.fullscreen.set(w.fullscreen);
            }
            None => {
                self.window.set_attributes(attrs);
                self.window.set("none");
                self.fullscreen.set(false);
            }
        }
    }
}

/// Writes the script, loads it under [`PLUGIN`] and runs it.
async fn load_script(
    conn: &zbus::Connection,
    text: &str,
) -> Result<std::path::PathBuf, IntegrationError> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("deskhand-kwin-{}.js", std::process::id()));
    tokio::fs::write(&path, text)
        .await
        .map_err(IntegrationError::other)?;
    let scripting = Proxy::new(conn, KWIN, "/Scripting", "org.kde.kwin.Scripting")
        .await
        .map_err(dbus_err)?;
    let loaded: bool = scripting
        .call("isScriptLoaded", &(PLUGIN,))
        .await
        .map_err(dbus_err)?;
    if loaded {
        // Left over from a previous run (crash or KWin kept it).
        let _: bool = scripting
            .call("unloadScript", &(PLUGIN,))
            .await
            .map_err(dbus_err)?;
    }
    let id: i32 = scripting
        .call("loadScript", &(path.to_string_lossy().as_ref(), PLUGIN))
        .await
        .map_err(dbus_err)?;
    if id < 0 {
        return Err(IntegrationError::msg("KWin refused to load the script"));
    }
    let script_path = format!("/Scripting/Script{id}");
    let script = Proxy::new(conn, KWIN, script_path.as_str(), "org.kde.kwin.Script")
        .await
        .map_err(dbus_err)?;
    script
        .call::<_, _, ()>("run", &())
        .await
        .map_err(dbus_err)?;
    Ok(path)
}

async fn unload_script(conn: &zbus::Connection, path: &std::path::Path) {
    if let Ok(scripting) = Proxy::new(conn, KWIN, "/Scripting", "org.kde.kwin.Scripting").await {
        let r: zbus::Result<bool> = scripting.call("unloadScript", &(PLUGIN,)).await;
        if let Err(e) = r {
            tracing::debug!(error = %e, "unloading the KWin script failed");
        }
    }
    let _ = tokio::fs::remove_file(path).await;
}

#[async_trait::async_trait]
impl Integration for ActiveWindow {
    fn id(&self) -> &'static str {
        "active_window.kwin"
    }

    fn capability(&self) -> &'static str {
        "active_window"
    }

    fn description(&self) -> &'static str {
        "Active window, fullscreen and display count from a KWin script"
    }

    async fn probe(&self) -> Probe {
        match kwin_session(&self.env).await {
            Ok(_) => Probe::available(),
            Err(p) => p,
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let options = ctx.config::<ActiveWindowOptions>()?;
        let conn = self.env.session_bus().await.map_err(dbus_err)?;
        let dbus = fdo::DBusProxy::new(&conn).await.map_err(dbus_err)?;
        let mut owner_changes = dbus
            .receive_name_owner_changed_with_args(&[(0, KWIN)])
            .await
            .map_err(dbus_err)?;
        let owner = dbus
            .get_name_owner(
                KWIN.try_into()
                    .map_err(|e| dbus_err(zbus::Error::from(e)))?,
            )
            .await
            .map_err(|e| dbus_err(e.into()))?;

        let (tx, mut reports) = mpsc::unbounded_channel();
        let server = conn.object_server();
        // A previous run's object (after a restart) would make `at` a no-op.
        let _ = server.remove::<Callback, _>(CALLBACK_PATH).await;
        server
            .at(
                CALLBACK_PATH,
                Callback {
                    tx,
                    kwin_owner: Arc::new(Mutex::new(owner.to_string())),
                },
            )
            .await
            .map_err(dbus_err)?;

        let e = Entities {
            window: ctx.sensor(
                EntitySpec::sensor(ACTIVE_WINDOW, "Active window")
                    .icon("mdi:application")
                    .min_interval(Duration::from_millis(500)),
            ),
            fullscreen: ctx.binary_sensor(
                EntitySpec::binary_sensor(FULLSCREEN, "Fullscreen", None).icon("mdi:fullscreen"),
            ),
            displays: ctx
                .sensor(EntitySpec::sensor(DISPLAY_COUNT, "Displays").icon("mdi:monitor-multiple")),
            publish_title: options.publish_title,
        };
        let service = conn
            .unique_name()
            .map(|n| n.to_string())
            .ok_or_else(|| IntegrationError::msg("session connection has no unique name"))?;
        let path = load_script(&conn, &script(&service)).await?;

        let result = async {
            // The script reports right away; wait briefly so `ready` comes with a state.
            if let Ok(Some(json)) =
                tokio::time::timeout(Duration::from_secs(2), reports.recv()).await
            {
                handle(&e, &json);
            }
            ctx.ready();
            let cancel = ctx.cancelled();
            loop {
                tokio::select! {
                    () = cancel.cancelled() => return Ok(()),
                    Some(json) = reports.recv() => handle(&e, &json),
                    _ = owner_changes.next() => {
                        return Err(IntegrationError::msg("KWin restarted; reloading the script"));
                    }
                }
            }
        }
        .await;
        unload_script(&conn, &path).await;
        let _ = server.remove::<Callback, _>(CALLBACK_PATH).await;
        result
    }
}

fn handle(e: &Entities, json: &str) {
    match serde_json::from_str::<Report>(json) {
        Ok(r) => {
            tracing::trace!(kind = %r.kind, "active window report");
            e.publish(&r);
        }
        Err(err) => tracing::warn!(error = %err, "unparsable report from the KWin script"),
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::State;
    use deskhand_core::testing::{rig, wait_attr, wait_state};
    use deskhand_testkit::mocks::MockKWin;
    use deskhand_testkit::{TestBus, require_dbus};

    use super::*;

    #[test]
    fn script_declares_its_target_on_one_line() {
        let s = script(":1.42");
        let line = s.lines().find(|l| l.starts_with("var TARGET = ")).unwrap();
        assert!(line.contains(r#""service":":1.42""#));
        assert!(line.contains(CALLBACK_IFACE));
        assert!(s.contains("captionChanged") && s.contains("fullScreenChanged"));
    }

    fn report(app: &str, caption: &str, fullscreen: bool, screens: u32) -> String {
        json!({
            "kind": "activated",
            "window": {"app_id": app, "resource_class": app, "caption": caption,
                       "pid": 42, "fullscreen": fullscreen, "output": "Virtual-1"},
            "screens": screens,
        })
        .to_string()
    }

    async fn wait_running(kwin: &MockKWin) {
        for _ in 0..200 {
            if kwin.running_script(PLUGIN).is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("script never ran");
    }

    #[tokio::test]
    async fn reports_from_kwin_only() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let kwin = MockKWin::start(&bus).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &["KDE"]);
        let integration = ActiveWindow::new(env.clone());
        assert!(integration.probe().await.is_available());
        let other_desktop = ActiveWindow::new(LinuxEnv::with_addresses(
            bus.session_address(),
            bus.system_address(),
            &["GNOME"],
        ));
        assert!(!other_desktop.probe().await.is_available());

        let (mut t, cancel) = rig(integration, "active_window");
        wait_running(&kwin).await;
        kwin.script_event(PLUGIN, &report("org.kde.konsole", "~ : bash", false, 1))
            .await
            .unwrap();
        wait_state(
            &mut t,
            &ACTIVE_WINDOW,
            State::Text("org.kde.konsole".into()),
        )
        .await;
        wait_attr(&mut t, &ACTIVE_WINDOW, "title", json!("~ : bash")).await;
        wait_state(&mut t, &DISPLAY_COUNT, State::Int(1)).await;
        wait_state(&mut t, &FULLSCREEN, State::Bool(false)).await;

        kwin.script_event(PLUGIN, &report("mpv", "video.mkv", true, 2))
            .await
            .unwrap();
        wait_state(&mut t, &FULLSCREEN, State::Bool(true)).await;
        wait_state(&mut t, &DISPLAY_COUNT, State::Int(2)).await;
        kwin.script_event(PLUGIN, r#"{"kind":"activated","window":null,"screens":2}"#)
            .await
            .unwrap();
        wait_state(&mut t, &ACTIVE_WINDOW, State::Text("none".into())).await;

        // Anyone else calling the callback is rejected.
        let other = zbus::connection::Builder::address(bus.session_address())
            .unwrap()
            .build()
            .await
            .unwrap();
        let service = env
            .session_bus()
            .await
            .unwrap()
            .unique_name()
            .unwrap()
            .to_string();
        let forged = Proxy::new(&other, service, CALLBACK_PATH, CALLBACK_IFACE)
            .await
            .unwrap();
        let r: zbus::Result<()> = forged.call("Event", &(report("evil", "x", true, 9),)).await;
        assert!(r.is_err(), "forged report accepted");

        // Stopping unloads the script.
        cancel.cancel();
        for _ in 0..100 {
            if kwin
                .calls()
                .iter()
                .any(|c| c == &format!("unloadScript({PLUGIN})"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(kwin.running_script(PLUGIN).is_none());
        assert_ne!(t.state(&DISPLAY_COUNT), Some(&State::Int(9)));
    }

    #[tokio::test]
    async fn titles_can_be_withheld() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let kwin = MockKWin::start(&bus).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &["KDE"]);
        let config = deskhand_core::Config::from_toml_str(
            "[integrations.active_window]\npublish_title = false",
            None,
        )
        .unwrap()
        .integration("active_window")
        .unwrap();
        let (mut t, _cancel) = deskhand_core::testing::rig_with(ActiveWindow::new(env), config);
        wait_running(&kwin).await;
        kwin.script_event(PLUGIN, &report("firefox", "Secret", false, 1))
            .await
            .unwrap();
        wait_attr(&mut t, &ACTIVE_WINDOW, "app_id", json!("firefox")).await;
        assert!(t.attributes(&ACTIVE_WINDOW).unwrap().get("title").is_none());
    }
}
