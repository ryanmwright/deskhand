//! `deskhand uninstall`: removes everything this device published to the
//! broker (discovery, states, availability), which makes Home Assistant
//! delete the device. Local files are left alone and listed at the end.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use deskhand_mqtt::AdminClient;
use deskhand_mqtt::topics::ONLINE;

use crate::wiring::Setup;

#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Don't ask for confirmation.
    pub yes: bool,
    /// Proceed even though a daemon for this device is online.
    pub force: bool,
    /// Only list what would be removed.
    pub dry_run: bool,
}

pub async fn uninstall(config_path: &Path, opts: Options) -> anyhow::Result<()> {
    let setup = Setup::load(config_path)?;
    crate::logging::init("warn");
    let mut client =
        AdminClient::connect(&setup.mqtt, &setup.device.node_id, Duration::from_secs(10))
            .await
            .with_context(|| {
                format!(
                    "cannot connect to the MQTT broker {}:{}",
                    setup.mqtt.host,
                    setup.mqtt.effective_port()
                )
            })?;
    let topics = client.topics().clone();
    let filters = client.owned_filters();
    let retained = client
        .read_retained(
            &filters,
            Duration::from_millis(750),
            Duration::from_secs(15),
        )
        .await
        .context("reading retained messages failed")?;

    if retained
        .get(&topics.status())
        .is_some_and(|p| p == ONLINE.as_bytes())
        && !opts.force
    {
        client.disconnect().await;
        bail!(
            "Deskhand is still running for this device (node id {}) and would publish everything again.\n\
             Stop it first (systemctl --user stop deskhand.service), or pass --force.",
            setup.device.node_id
        );
    }

    let list: Vec<String> = retained.keys().cloned().collect();
    println!(
        "Device \"{}\" (node id {}) on {}:{}",
        setup.device.name,
        setup.device.node_id,
        setup.mqtt.host,
        setup.mqtt.effective_port()
    );
    if list.is_empty() {
        println!("Nothing is retained on the broker for this device.");
        client.disconnect().await;
        print_local_hint(config_path);
        return Ok(());
    }
    println!("{} retained message(s) to remove:", list.len());
    for t in &list {
        println!("  {t}");
    }
    if opts.dry_run {
        client.disconnect().await;
        return Ok(());
    }
    if !opts.yes && !confirm("Remove them? Home Assistant will delete the device. [y/N] ")? {
        client.disconnect().await;
        println!("Nothing removed.");
        return Ok(());
    }
    client
        .clear(&list, Duration::from_secs(30))
        .await
        .context("removing retained messages failed")?;
    client.disconnect().await;
    println!("Removed {} retained message(s).", list.len());
    print_local_hint(config_path);
    Ok(())
}

fn confirm(prompt: &str) -> anyhow::Result<bool> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        bail!("not a terminal; pass --yes to confirm");
    }
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

fn print_local_hint(config_path: &Path) {
    println!();
    println!("Local files were not touched. To remove Deskhand completely:");
    println!("  systemctl --user disable --now deskhand.service   (if installed as a service)");
    println!("  rm {}", config_path.display());
    let user = std::env::var("USER").unwrap_or_default();
    if crate::doctor::linger_enabled(&user) == Some(true) {
        println!(
            "  loginctl disable-linger {user}   (if lingering was only for Deskhand; \
             on NixOS, unset users.users.{user}.linger)"
        );
    }
}
