//! M5 spike: Plasma's Do Not Disturb through the notification server's
//! `Inhibited` property and `Inhibit`/`UnInhibit` methods.
//!
//! ```text
//! dnd_probe status
//! dnd_probe watch [SECONDS=20]          # print Inhibited changes
//! dnd_probe inhibit [SECONDS=5]         # Inhibit, hold, UnInhibit
//! dnd_probe inhibit-and-exit [SECONDS=5] # Inhibit, hold, exit WITHOUT UnInhibit
//! ```

use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt;
use zbus::Proxy;
use zbus::zvariant::Value;

async fn status(p: &Proxy<'_>) -> zbus::Result<bool> {
    p.get_property::<bool>("Inhibited").await
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("status");
    let secs: u64 = args.get(1).map(|s| s.parse()).transpose()?.unwrap_or(5);
    let conn = zbus::Connection::session().await?;
    let p = Proxy::new(
        &conn,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await?;
    let caps: Vec<String> = p.call("GetCapabilities", &()).await?;
    println!("capabilities: {caps:?}");
    println!("Inhibited = {}", status(&p).await?);

    match cmd {
        "status" => {}
        "watch" => {
            let mut changes = p.receive_property_changed::<bool>("Inhibited").await;
            let end = tokio::time::sleep(Duration::from_secs(secs));
            tokio::pin!(end);
            loop {
                tokio::select! {
                    () = &mut end => break,
                    Some(c) = changes.next() => println!("Inhibited -> {:?}", c.get().await),
                }
            }
        }
        "inhibit" | "inhibit-and-exit" => {
            let hints: HashMap<&str, Value<'_>> = HashMap::new();
            let cookie: u32 = p
                .call(
                    "Inhibit",
                    &(
                        "io.github.ryanmwright.Deskhand",
                        "Deskhand DnD probe",
                        hints,
                    ),
                )
                .await?;
            println!("Inhibit -> cookie {cookie}");
            tokio::time::sleep(Duration::from_millis(300)).await;
            println!("Inhibited = {}", status(&p).await?);
            tokio::time::sleep(Duration::from_secs(secs)).await;
            if cmd == "inhibit" {
                let () = p.call("UnInhibit", &(cookie,)).await?;
                tokio::time::sleep(Duration::from_millis(300)).await;
                println!("after UnInhibit: Inhibited = {}", status(&p).await?);
            } else {
                println!("exiting without UnInhibit");
            }
        }
        other => return Err(format!("unknown command {other}").into()),
    }
    Ok(())
}
