//! M5 spike: which logind session does Deskhand see, especially when it runs
//! as a systemd user service (no session of its own)?
//!
//! ```text
//! logind_probe [--watch SECONDS]
//! systemd-run --user --wait --pipe -p Type=exec .../logind_probe   # as a user service
//! ```
//! Prints GetSession("auto"), GetSessionByPID(self), the session's
//! properties and the manager's Can* answers. With `--watch` it then prints
//! Lock/Unlock signals and LockedHint/IdleHint changes.

use std::time::Duration;

use futures_util::StreamExt;
use zbus::Proxy;
use zbus::zvariant::OwnedObjectPath;

const DEST: &str = "org.freedesktop.login1";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let watch = args
        .iter()
        .position(|a| a == "--watch")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.parse::<u64>())
        .transpose()?;

    println!(
        "XDG_SESSION_ID={:?} INVOCATION_ID set: {}",
        std::env::var("XDG_SESSION_ID").ok(),
        std::env::var_os("INVOCATION_ID").is_some()
    );
    let conn = zbus::Connection::system().await?;
    let manager = Proxy::new(
        &conn,
        DEST,
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;

    let auto: Result<OwnedObjectPath, _> = manager.call("GetSession", &("auto",)).await;
    println!("GetSession(auto) = {auto:?}");
    let by_pid: Result<OwnedObjectPath, _> = manager
        .call("GetSessionByPID", &(std::process::id(),))
        .await;
    println!("GetSessionByPID(self) = {by_pid:?}");
    let user: Result<OwnedObjectPath, _> =
        manager.call("GetUserByPID", &(std::process::id(),)).await;
    if let Ok(user) = &user {
        let u = Proxy::new(&conn, DEST, user.as_str(), "org.freedesktop.login1.User").await?;
        let display: Result<(String, OwnedObjectPath), _> = u.get_property("Display").await;
        println!("User.Display = {display:?}");
    }

    for action in ["CanSuspend", "CanHibernate", "CanReboot", "CanPowerOff"] {
        let r: Result<String, _> = manager.call(action, &()).await;
        println!("{action} = {r:?}");
    }
    let lid: Result<bool, _> = manager.get_property("LidClosed").await;
    println!("LidClosed = {lid:?}");

    let Ok(path) = auto else {
        return Ok(());
    };
    let session = Proxy::new(&conn, DEST, path.as_str(), "org.freedesktop.login1.Session").await?;
    for prop in ["Id", "Type", "Class", "Desktop", "State"] {
        let v: Result<String, _> = session.get_property(prop).await;
        println!("Session.{prop} = {v:?}");
    }
    for prop in ["Active", "Remote", "LockedHint", "IdleHint"] {
        let v: Result<bool, _> = session.get_property(prop).await;
        println!("Session.{prop} = {v:?}");
    }

    if let Some(secs) = watch {
        println!("watching {secs} s (path {path})");
        let mut lock = session.receive_signal("Lock").await?;
        let mut unlock = session.receive_signal("Unlock").await?;
        let mut locked = session.receive_property_changed::<bool>("LockedHint").await;
        let mut idle = session.receive_property_changed::<bool>("IdleHint").await;
        let end = tokio::time::sleep(Duration::from_secs(secs));
        tokio::pin!(end);
        loop {
            tokio::select! {
                () = &mut end => break,
                Some(_) = lock.next() => println!("signal Lock"),
                Some(_) = unlock.next() => println!("signal Unlock"),
                Some(c) = locked.next() => println!("LockedHint -> {:?}", c.get().await),
                Some(c) = idle.next() => println!("IdleHint -> {:?}", c.get().await),
            }
        }
    }
    Ok(())
}
