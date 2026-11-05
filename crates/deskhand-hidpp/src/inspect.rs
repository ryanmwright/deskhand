//! A one-off look at the Logitech receivers, for `deskhand doctor`: nodes,
//! drivers, access, pairings and, for connected devices, the battery.

use std::time::Duration;

use crate::adapter::{self, Notice, kind_name};
use crate::backend::{Access, HidBackend, HidNode};
use crate::source::candidates;

const DEVICE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct NodeReport {
    pub node: HidNode,
    pub access: Access,
    /// Present when the node is a receiver that could be opened.
    pub receiver: Option<ReceiverReport>,
    /// Why the node could not be inspected.
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReceiverReport {
    pub name: &'static str,
    pub unique_id: Option<String>,
    pub firmware: Option<String>,
    pub devices: Vec<DeviceReport>,
}

#[derive(Debug, Clone)]
pub struct DeviceReport {
    pub slot: u8,
    pub name: Option<String>,
    pub wpid: u16,
    pub kind: Option<&'static str>,
    pub online: bool,
    /// "87 % discharging"; `None` while asleep or without a battery feature.
    pub battery: Option<String>,
}

/// Inspects every Logitech HID++ node. Sleeping devices are not contacted.
pub async fn inspect(backend: &dyn HidBackend) -> std::io::Result<Vec<NodeReport>> {
    let nodes = backend.enumerate()?;
    let mut out = Vec::new();
    for node in candidates(&nodes) {
        let access = backend.access(node);
        let mut report = NodeReport {
            node: node.clone(),
            access: access.clone(),
            receiver: None,
            error: None,
        };
        if access == Access::Ok {
            match inspect_receiver(backend, node).await {
                Ok(r) => report.receiver = r,
                Err(e) => report.error = Some(e),
            }
        }
        out.push(report);
    }
    Ok(out)
}

async fn inspect_receiver(
    backend: &dyn HidBackend,
    node: &HidNode,
) -> Result<Option<ReceiverReport>, String> {
    let raw = backend.open(node).map_err(|e| e.to_string())?;
    let Some((link, mut notices)) = adapter::open_receiver(raw, &node.id, true)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let unique_id = link.unique_id().await.ok();
    let firmware = link.firmware().await;
    if !node.kernel_receiver {
        let _ = link.enable_notifications().await;
    }
    link.trigger_arrival().await.map_err(|e| e.to_string())?;
    let mut devices = Vec::new();
    while let Ok(n) = notices.try_recv() {
        let Notice::Connection {
            slot,
            wpid,
            kind,
            online,
            ..
        } = n
        else {
            continue;
        };
        if devices.iter().any(|d: &DeviceReport| d.slot == slot) {
            continue;
        }
        let mut d = DeviceReport {
            slot,
            name: link.codename(slot).await,
            wpid,
            kind: kind_name(kind),
            online,
            battery: None,
        };
        if online {
            let read = async {
                let dev = link.device(slot).await?;
                Ok::<_, adapter::AdapterError>((
                    dev.identity.name.clone(),
                    dev.read_battery().await?,
                ))
            };
            if let Ok(Ok((name, battery))) = tokio::time::timeout(DEVICE_TIMEOUT, read).await {
                d.name = name.or(d.name);
                d.battery = battery.map(|b| {
                    let pct = b
                        .percentage
                        .map_or_else(|| "?".to_owned(), |p| format!("{p:.0} %"));
                    let est = if b.estimated { " (estimated)" } else { "" };
                    format!("{pct}{est} {}", b.state.as_str())
                });
            }
        }
        devices.push(d);
    }
    Ok(Some(ReceiverReport {
        name: link.kind().name(),
        unique_id,
        firmware,
        devices,
    }))
}
