//! M5 spike: custom global shortcuts through kglobalaccel's D-Bus API.
//!
//! ```text
//! kglobalaccel_probe register [SECONDS=30]   # register Meta+Shift+F12, then listen
//! kglobalaccel_probe listen [SECONDS=30]     # listen only (persistence check)
//! kglobalaccel_probe show                    # what kglobalaccel has for our action
//! kglobalaccel_probe unregister
//! ```
//! Component `deskhand` (fixed, not the app id; see the plan, M11), action `probe`.

use std::time::Duration;

use futures_util::StreamExt;
use zbus::Proxy;
use zbus::zvariant::OwnedObjectPath;

const COMPONENT: &str = "deskhand";
const ACTION: &str = "probe";

// Qt 6 key codes.
const META: i32 = 0x1000_0000;
const SHIFT: i32 = 0x0200_0000;
const KEY_F12: i32 = 0x0100_003b;

// KGlobalAccel::SetShortcutFlag
const SET_PRESENT: u32 = 2;
const NO_AUTOLOADING: u32 = 4;
const IS_DEFAULT: u32 = 8;

fn action_id() -> Vec<&'static str> {
    vec![COMPONENT, ACTION, "Deskhand", "Deskhand probe shortcut"]
}

async fn listen(conn: &zbus::Connection, accel: &Proxy<'_>, secs: u64) -> zbus::Result<()> {
    let path: OwnedObjectPath = accel.call("getComponent", &(COMPONENT,)).await?;
    println!("component object {path}");
    let component = Proxy::new(
        conn,
        "org.kde.kglobalaccel",
        path.as_str(),
        "org.kde.kglobalaccel.Component",
    )
    .await?;
    let mut pressed = component.receive_signal("globalShortcutPressed").await?;
    let mut released = component.receive_signal("globalShortcutReleased").await?;
    println!("listening {secs} s");
    let end = tokio::time::sleep(Duration::from_secs(secs));
    tokio::pin!(end);
    loop {
        tokio::select! {
            () = &mut end => return Ok(()),
            Some(m) = pressed.next() => {
                let (c, a, ts): (String, String, i64) = m.body().deserialize()?;
                println!("PRESSED {c}/{a} at {ts}");
            }
            Some(m) = released.next() => {
                let (c, a, ts): (String, String, i64) = m.body().deserialize()?;
                println!("released {c}/{a} at {ts}");
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("show");
    let secs: u64 = args.get(1).map(|s| s.parse()).transpose()?.unwrap_or(30);
    let conn = zbus::Connection::session().await?;
    let accel = Proxy::new(
        &conn,
        "org.kde.kglobalaccel",
        "/kglobalaccel",
        "org.kde.KGlobalAccel",
    )
    .await?;
    // KF6 reads a QKeySequence as exactly four ints; a shorter array makes
    // libdbus abort *inside KWin* (which hosts kglobalaccel). Always pad to 4.
    let keys = vec![(vec![META | SHIFT | KEY_F12, 0, 0, 0],)];

    match cmd {
        "register" => {
            let () = accel.call("doRegister", &(action_id(),)).await?;
            let default: Vec<(Vec<i32>,)> = accel
                .call("setShortcutKeys", &(action_id(), &keys, IS_DEFAULT))
                .await?;
            println!("setShortcutKeys(IsDefault) -> {default:x?}");
            let active: Vec<(Vec<i32>,)> = accel
                .call("setShortcutKeys", &(action_id(), &keys, SET_PRESENT))
                .await?;
            println!(
                "setShortcutKeys(SetPresent) -> {active:x?} (autoloaded if the user changed it)"
            );
            listen(&conn, &accel, secs).await?;
        }
        "listen" => listen(&conn, &accel, secs).await?,
        "show" => {
            let current: Vec<(Vec<i32>,)> = accel.call("shortcutKeys", &(action_id(),)).await?;
            let default: Vec<(Vec<i32>,)> =
                accel.call("defaultShortcutKeys", &(action_id(),)).await?;
            println!("shortcutKeys -> {current:x?}, defaultShortcutKeys -> {default:x?}");
            let mains: Vec<Vec<String>> = accel.call("allMainComponents", &()).await?;
            let ours = mains
                .iter()
                .find(|c| c.first().map(String::as_str) == Some(COMPONENT));
            println!("allMainComponents has ours: {ours:?}");
        }
        "unregister" => {
            let ok: bool = accel.call("unregister", &(COMPONENT, ACTION)).await?;
            println!("unregister -> {ok}");
        }
        "setinactive" => {
            let () = accel.call("setInactive", &(action_id(),)).await?;
            println!("setInactive done");
        }
        other => return Err(format!("unknown command {other}").into()),
    }
    let _ = NO_AUTOLOADING;
    Ok(())
}
