//! `deskhand run`: load config, build the hub, transport and supervisor,
//! run until SIGINT/SIGTERM, then shut down cleanly (offline status published).

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use deskhand_core::{Enabled, Hub, HubOptions, Supervisor, SupervisorOptions, Transport};
use deskhand_mqtt::MqttTransport;
use tokio_util::sync::CancellationToken;

use crate::wiring::Setup;

pub async fn run(config_path: &Path) -> anyhow::Result<()> {
    let setup = Setup::load(config_path)?;
    crate::logging::init(&setup.general.log);
    tracing::info!(version = deskhand_core::VERSION, config = %config_path.display(), "starting deskhand");
    setup.log_warnings();
    let device = setup.device.clone();
    tracing::info!(name = %device.name, node_id = %device.node_id, "device identity");

    let disabled: BTreeSet<String> = setup
        .config
        .configured_integrations()
        .into_iter()
        .filter(|c| {
            setup
                .config
                .integration(c)
                .is_ok_and(|i| i.enabled == Enabled::Off)
        })
        .collect();
    let transport = MqttTransport::new(setup.mqtt.clone(), device, disabled)
        .context("invalid [mqtt] configuration")?;

    let (hub, hub_handle, link) = Hub::new(HubOptions::default());
    let hub_task = tokio::spawn(hub.run());
    let mut transport_task = tokio::spawn(Box::new(transport).run(link));

    let cancel = CancellationToken::new();
    let supervisor = Supervisor::new(
        hub_handle.clone(),
        &setup.config,
        setup.candidates(),
        SupervisorOptions::default(),
        cancel.clone(),
    );
    let supervisor_task = tokio::spawn(supervisor.run());

    // systemd (Type=notify): ready once the first complete state is assembled.
    // The broker may be down; the service is still up and will connect later.
    tokio::spawn({
        let hub = hub_handle.clone();
        async move {
            hub.wait_released().await;
            notify_systemd(&[sd_notify::NotifyState::Ready]);
        }
    });

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

    notify_systemd(&[sd_notify::NotifyState::Stopping]);
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

/// Sends a status to systemd when running as a `Type=notify` service; a no-op otherwise.
fn notify_systemd(state: &[sd_notify::NotifyState]) {
    if std::env::var_os("NOTIFY_SOCKET").is_none() {
        return;
    }
    if let Err(e) = sd_notify::notify(state) {
        tracing::debug!(error = %e, "sd_notify failed");
    }
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
