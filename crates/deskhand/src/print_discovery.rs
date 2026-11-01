//! `deskhand print-discovery [--all]`: runs the integrations without a
//! broker and prints the discovery document the daemon would publish
//! (`--all`: every topic and payload, including states).

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use deskhand_core::{
    HubEvent, HubOptions, Snapshot, Supervisor, SupervisorOptions, TransportEvent,
};
use deskhand_mqtt::Topics;
use deskhand_mqtt::discovery::DiscoveryPlanner;
use deskhand_mqtt::topics::{OFFLINE, ONLINE};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::wiring::Setup;

pub async fn print(config_path: &Path, all: bool) -> anyhow::Result<()> {
    let setup = Setup::load(config_path)?;
    crate::logging::init("warn");
    setup.log_warnings();
    setup
        .mqtt
        .validate()
        .context("invalid [mqtt] configuration")?;
    let snapshot = collect_snapshot(&setup).await?;
    let topics = Topics::new(
        &setup.mqtt.topic_prefix,
        &setup.mqtt.discovery_prefix,
        &setup.device.node_id,
    );
    let out = render(&topics, &setup.device, &snapshot, all);
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// Runs every integration until the hub's startup barrier releases, then
/// returns the first snapshot and stops them again.
async fn collect_snapshot(setup: &Setup) -> anyhow::Result<Snapshot> {
    let (hub, handle, mut link) = deskhand_core::Hub::new(HubOptions::default());
    let hub_task = tokio::spawn(hub.run());
    let cancel = CancellationToken::new();
    let supervisor = Supervisor::new(
        handle.clone(),
        &setup.config,
        setup.candidates(),
        SupervisorOptions::default(),
        cancel.clone(),
    );
    let supervisor_task = tokio::spawn(supervisor.run());
    // Pretend to be a connected transport.
    let _ = link.to_hub.send(TransportEvent::Connected);
    let wait = async {
        while let Some(ev) = link.events.recv().await {
            if let HubEvent::Snapshot(s) = ev {
                return Some(s);
            }
        }
        None
    };
    let snapshot = tokio::time::timeout(Duration::from_secs(30), wait).await;
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(7), supervisor_task).await;
    handle.shutdown();
    let _ = hub_task.await;
    snapshot
        .context("integrations did not finish starting within 30 s")?
        .context("the hub stopped before publishing")
}

fn render(
    topics: &Topics,
    device: &deskhand_core::DeviceInfo,
    snap: &Snapshot,
    all: bool,
) -> Value {
    let live = snap.availability.keys().map(|c| c.to_string()).collect();
    let rendered = DiscoveryPlanner::default().render(topics, device, &snap.entities, &live);
    if !all {
        return rendered.first;
    }
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    out.insert(topics.device_config(), rendered.first);
    out.insert(topics.status(), ONLINE.into());
    for (cap, available) in &snap.availability {
        let status = if *available { ONLINE } else { OFFLINE };
        out.insert(topics.capability_status(cap), status.into());
    }
    for desc in &snap.entities {
        let id = &desc.spec.id;
        if let Some(state) = snap.states.get(id) {
            let payload = deskhand_mqtt::payload::encode_state(&desc.spec.kind, state);
            let text = String::from_utf8_lossy(&payload).into_owned();
            let value = serde_json::from_str::<Value>(&text)
                .ok()
                .filter(Value::is_object)
                .unwrap_or(Value::String(text));
            out.insert(topics.state(id), value);
        }
        if let Some(attrs) = snap.attributes.get(id) {
            out.insert(topics.attributes(id), Value::Object(attrs.clone()));
        }
    }
    serde_json::to_value(out).unwrap_or_default()
}
