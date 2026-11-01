//! `deskhand run`: load config, build the hub, transport and supervisor,
//! run until SIGINT/SIGTERM, then shut down cleanly (offline status published).

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use deskhand_core::{
    Config, DeviceInfo, Enabled, Hub, HubOptions, Integration, Supervisor, SupervisorOptions,
    Transport,
};
use deskhand_mqtt::{MqttConfig, MqttTransport};
use tokio_util::sync::CancellationToken;

/// Top-level config sections owned by crates other than the core.
const KNOWN_SECTIONS: &[&str] = &["mqtt"];

/// Every integration compiled in, in priority order per capability.
fn candidates() -> Vec<Box<dyn Integration>> {
    let list: Vec<Box<dyn Integration>> =
        vec![Box::new(deskhand_common::info::DeskhandInfo::new())];
    list
}

fn platform_identity() -> deskhand_core::PlatformIdentity {
    #[cfg(feature = "linux")]
    {
        deskhand_linux::platform_identity()
    }
    #[cfg(not(feature = "linux"))]
    {
        deskhand_core::PlatformIdentity {
            hostname: std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".into()),
            user: std::env::var("USER").unwrap_or_else(|_| "user".into()),
            model: std::env::consts::OS.into(),
            ..Default::default()
        }
    }
}

fn warn_unknown(keys: &[String]) {
    for key in keys {
        tracing::warn!(key, "unknown config key ignored");
    }
}

pub async fn run(config_path: &Path) -> anyhow::Result<()> {
    let config = Config::load(config_path).with_context(|| {
        format!(
            "could not load the config file (create {} or pass --config)",
            config_path.display()
        )
    })?;
    let general = config.general()?;
    crate::logging::init(&general.value.log);
    tracing::info!(version = deskhand_core::VERSION, config = %config_path.display(), "starting deskhand");
    warn_unknown(&general.unknown_keys);
    for section in config.unknown_sections(KNOWN_SECTIONS) {
        tracing::warn!(section, "unknown config section ignored");
    }

    let device_cfg = config.device()?;
    warn_unknown(&device_cfg.unknown_keys);
    let device = DeviceInfo::resolve(&platform_identity(), &device_cfg.value)?;
    tracing::info!(name = %device.name, node_id = %device.node_id, "device identity");

    let mqtt = config.section::<MqttConfig>("mqtt")?;
    warn_unknown(&mqtt.unknown_keys);
    let disabled: BTreeSet<String> = config
        .configured_integrations()
        .into_iter()
        .filter(|c| {
            config
                .integration(c)
                .is_ok_and(|i| i.enabled == Enabled::Off)
        })
        .collect();
    let transport =
        MqttTransport::new(mqtt.value, device, disabled).context("invalid [mqtt] configuration")?;

    let (hub, hub_handle, link) = Hub::new(HubOptions::default());
    let hub_task = tokio::spawn(hub.run());
    let mut transport_task = tokio::spawn(Box::new(transport).run(link));

    let cancel = CancellationToken::new();
    let supervisor = Supervisor::new(
        hub_handle.clone(),
        &config,
        candidates(),
        SupervisorOptions::default(),
        cancel.clone(),
    );
    let supervisor_task = tokio::spawn(supervisor.run());

    tokio::select! {
        signal = shutdown_signal() => tracing::info!(signal, "shutting down"),
        res = &mut transport_task => {
            // The transport only returns on shutdown; anything else is fatal.
            cancel.cancel();
            hub_handle.shutdown();
            return match res {
                Ok(Ok(())) => Err(anyhow::anyhow!("MQTT transport stopped unexpectedly")),
                Ok(Err(e)) => Err(e).context("MQTT transport failed"),
                Err(e) => Err(e).context("MQTT transport panicked"),
            };
        }
    }

    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(7), supervisor_task).await;
    hub_handle.shutdown();
    match tokio::time::timeout(Duration::from_secs(5), transport_task).await {
        Ok(Ok(Err(e))) => tracing::warn!(error = %e, "transport error during shutdown"),
        Err(_) => tracing::warn!("transport did not stop in time"),
        _ => {}
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), hub_task).await;
    Ok(())
}

async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = term.recv() => "SIGTERM",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl-C"
    }
}
