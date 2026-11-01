//! M5 spike: a KWin script that reports window activations back to us with
//! `callDBus`, and the check that such calls really come from KWin.
//!
//! ```text
//! kwin_script_probe [SECONDS=20]
//! ```
//! 1. Serves `Event(s json)` on our own bus name.
//! 2. Writes a script to $XDG_RUNTIME_DIR, loads it with
//!    `org.kde.kwin.Scripting.loadScript`, and starts it.
//! 3. Prints each event and whether its sender is the owner of `org.kde.KWin`.
//! 4. Calls `Event` itself from a second connection, which must be rejected.
//! 5. Unloads the script.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use zbus::message::Header;
use zbus::{Proxy, fdo};

const NAME: &str = "io.github.ryanmwright.Deskhand.Probe";
const PATH: &str = "/io/github/ryanmwright/Deskhand/KWin";
const IFACE: &str = "io.github.ryanmwright.Deskhand.KWin";
const PLUGIN: &str = "deskhand-probe";

struct Receiver {
    kwin_owner: Arc<Mutex<Option<String>>>,
}

#[zbus::interface(name = "io.github.ryanmwright.Deskhand.KWin")]
impl Receiver {
    async fn event(&self, json: String, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        let owner = self.kwin_owner.lock().await.clone().unwrap_or_default();
        if sender != owner {
            println!("REJECTED event from {sender} (KWin is {owner}): {json}");
            return Err(fdo::Error::AccessDenied("not KWin".into()));
        }
        println!("event from KWin ({sender}): {json}");
        Ok(())
    }
}

fn script() -> String {
    format!(
        r#"
function send(kind, w) {{
    var info = w ? {{
        app_id: String(w.desktopFileName || w.resourceClass),
        resource_class: String(w.resourceClass),
        caption: String(w.caption),
        pid: w.pid,
        fullscreen: w.fullScreen,
        output: w.output ? String(w.output.name) : null
    }} : null;
    callDBus("{NAME}", "{PATH}", "{IFACE}", "Event",
        JSON.stringify({{ kind: kind, window: info, screens: workspace.screens.length }}));
}}
workspace.windowActivated.connect(function (w) {{ send("activated", w); }});
workspace.screensChanged.connect(function () {{ send("screens", workspace.activeWindow); }});
send("initial", workspace.activeWindow);
"#
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let secs: u64 = std::env::args()
        .nth(1)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(20);
    let kwin_owner = Arc::new(Mutex::new(None));
    let conn = zbus::connection::Builder::session()?
        .serve_at(
            PATH,
            Receiver {
                kwin_owner: kwin_owner.clone(),
            },
        )?
        .name(NAME)?
        .build()
        .await?;
    let dbus = fdo::DBusProxy::new(&conn).await?;
    let owner = dbus.get_name_owner("org.kde.KWin".try_into()?).await?;
    println!("org.kde.KWin is owned by {owner}");
    *kwin_owner.lock().await = Some(owner.to_string());

    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let path = format!("{dir}/deskhand-kwin-probe.js");
    std::fs::write(&path, script())?;
    let scripting = Proxy::new(
        &conn,
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting",
    )
    .await?;
    let loaded: bool = scripting.call("isScriptLoaded", &(PLUGIN,)).await?;
    if loaded {
        let _: bool = scripting.call("unloadScript", &(PLUGIN,)).await?;
    }
    let id: i32 = scripting
        .call("loadScript", &(path.as_str(), PLUGIN))
        .await?;
    println!("loadScript -> id {id}");
    // Run just this script (start() would also start every other loaded script).
    let script_path = format!("/Scripting/Script{id}");
    let script_proxy = Proxy::new(
        &conn,
        "org.kde.KWin",
        script_path.as_str(),
        "org.kde.kwin.Script",
    )
    .await?;
    match script_proxy.call::<_, _, ()>("run", &()).await {
        Ok(()) => println!("{script_path}.run ok"),
        Err(e) => {
            println!("{script_path}.run failed ({e}); falling back to start()");
            let () = scripting.call("start", &()).await?;
        }
    }

    // A forged call from another client must be rejected.
    let other = zbus::Connection::session().await?;
    let forged = Proxy::new(&other, NAME, PATH, IFACE).await?;
    let r: Result<(), _> = forged.call("Event", &(r#"{"kind":"forged"}"#,)).await;
    println!("forged call result: {r:?}");

    tokio::time::sleep(Duration::from_secs(secs)).await;
    let unloaded: bool = scripting.call("unloadScript", &(PLUGIN,)).await?;
    println!("unloadScript -> {unloaded}");
    Ok(())
}
