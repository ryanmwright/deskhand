//! [`HidppSource`]: the `hidpp` peripheral battery source. It opens every
//! Logitech receiver a [`HidBackend`] finds, runs one [`ReceiverTask`] per
//! receiver and follows hotplug.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use deskhand_common::peripherals::{PeripheralSink, PeripheralSource};
use deskhand_core::{IntegrationError, Probe};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::adapter::{self, ReceiverLink};
use crate::backend::{Access, HidBackend, HidNode, LOGITECH_VENDOR};
use crate::receiver::{ReceiverTask, Timing};

/// A node that failed to open is tried again after this (or on hotplug).
const REOPEN: Duration = Duration::from_secs(30);
/// How long the first report waits for the receivers' start-up and the
/// awake devices' first read (the first request can take 1.4 s, M16).
/// Within the hub's 5 s startup barrier.
const READY_WAIT: Duration = Duration::from_millis(4500);

/// The nodes Deskhand talks to: Logitech, HID++, and not owned by a kernel
/// driver that reports the battery itself.
pub fn candidates(nodes: &[HidNode]) -> impl Iterator<Item = &HidNode> {
    nodes
        .iter()
        .filter(|n| n.vendor == LOGITECH_VENDOR && !n.kernel_owned)
}

/// Logitech HID++ receivers (Bolt, Unifying) as a peripheral battery source.
pub struct HidppSource {
    backend: Arc<dyn HidBackend>,
    timing: Timing,
}

impl std::fmt::Debug for HidppSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HidppSource").finish_non_exhaustive()
    }
}

impl HidppSource {
    pub fn new(backend: Arc<dyn HidBackend>) -> Self {
        Self {
            backend,
            timing: Timing::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_timing(mut self, timing: Timing) -> Self {
        self.timing = timing;
        self
    }
}

#[async_trait::async_trait]
impl PeripheralSource for HidppSource {
    fn id(&self) -> &'static str {
        "hidpp"
    }

    async fn probe(&self) -> Probe {
        let nodes = match self.backend.enumerate() {
            Ok(n) => n,
            Err(e) => return Probe::unavailable(format!("cannot list HID devices: {e}")),
        };
        let logitech: Vec<&HidNode> = candidates(&nodes).collect();
        if logitech.is_empty() {
            // Available anyway: a receiver plugged in later is picked up.
            return Probe::available_with("no Logitech receiver connected");
        }
        let denied: Vec<&HidNode> = logitech
            .iter()
            .copied()
            .filter(|n| self.backend.access(n) == Access::Denied)
            .collect();
        if denied.len() == logitech.len() {
            let ids: Vec<&str> = denied.iter().map(|n| n.id.as_str()).collect();
            let mut reason = format!(
                "cannot open {} (permission denied); the udev rule for Logitech devices is missing",
                ids.join(", ")
            );
            if let Some(hint) = self.backend.access_hint() {
                reason.push_str(&format!(": {hint}"));
            }
            return Probe::unavailable(reason);
        }
        let ids: Vec<&str> = logitech.iter().map(|n| n.id.as_str()).collect();
        Probe::available_with(format!("Logitech HID++ on {}", ids.join(", ")))
    }

    async fn run(
        &self,
        sink: PeripheralSink,
        cancel: CancellationToken,
    ) -> Result<(), IntegrationError> {
        let mut hotplug = self.backend.hotplug().map_err(IntegrationError::other)?;
        let (done_tx, mut done_rx) = mpsc::unbounded_channel();
        let mut runner = Runner {
            done: done_tx,
            backend: self.backend.clone(),
            sink: sink.clone(),
            timing: self.timing,
            cancel: cancel.clone(),
            running: BTreeMap::new(),
            ignored: BTreeSet::new(),
            retry: BTreeMap::new(),
            warned: BTreeSet::new(),
        };
        let waiting = runner.sync().await;
        let _ = tokio::time::timeout(READY_WAIT, async {
            for w in waiting {
                let _ = w.await;
            }
        })
        .await;
        sink.ready();
        let result = loop {
            let retry_at = runner.retry.values().min().copied();
            tokio::select! {
                () = cancel.cancelled() => break Ok(()),
                r = hotplug.changed() => if let Err(e) = r {
                    break Err(IntegrationError::other(e));
                },
                Some((id, gone)) = done_rx.recv() => {
                    // A receiver task ended by itself: try its node again.
                    if let Some(r) = runner.running.remove(&id) {
                        let _ = r.handle.await;
                    }
                    let wait = if gone { Duration::from_secs(1) } else { REOPEN };
                    runner.retry.insert(id, Instant::now() + wait);
                }
                () = async {
                    match retry_at {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {}
            }
            runner.sync().await;
        };
        runner.stop().await;
        result
    }
}

struct Running {
    link: Arc<ReceiverLink>,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

struct Runner {
    /// Receiver tasks that ended by themselves: (node, the receiver is gone).
    done: mpsc::UnboundedSender<(String, bool)>,
    backend: Arc<dyn HidBackend>,
    sink: PeripheralSink,
    timing: Timing,
    cancel: CancellationToken,
    running: BTreeMap<String, Running>,
    /// Nodes that speak HID++ but are no receiver (a device on a cable).
    ignored: BTreeSet<String>,
    /// Nodes to try again, and when.
    retry: BTreeMap<String, Instant>,
    /// Nodes whose permission problem was logged already.
    warned: BTreeSet<String>,
}

impl Runner {
    /// Matches running receivers to the nodes present. Returns the start-up
    /// signals of receivers it started.
    async fn sync(&mut self) -> Vec<oneshot::Receiver<()>> {
        let nodes = match self.backend.enumerate() {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "cannot list HID devices");
                return Vec::new();
            }
        };
        let present: BTreeSet<&str> = candidates(&nodes).map(|n| n.id.as_str()).collect();
        // Gone, or a dead channel under a reused node name.
        let stale: Vec<String> = self
            .running
            .iter()
            .filter(|(id, r)| {
                !present.contains(id.as_str()) || !r.link.is_connected() || r.handle.is_finished()
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            if let Some(r) = self.running.remove(&id) {
                tracing::info!(node = %id, "HID++ receiver gone");
                r.cancel.cancel();
                let _ = r.handle.await;
            }
        }
        self.ignored.retain(|id| present.contains(id.as_str()));
        self.retry.retain(|id, _| present.contains(id.as_str()));
        self.warned.retain(|id| present.contains(id.as_str()));

        let now = Instant::now();
        let mut waiting = Vec::new();
        for node in candidates(&nodes) {
            if self.running.contains_key(&node.id)
                || self.ignored.contains(&node.id)
                || self.retry.get(&node.id).is_some_and(|at| *at > now)
            {
                continue;
            }
            self.retry.remove(&node.id);
            if let Some(w) = self.start(node).await {
                waiting.push(w);
            }
        }
        waiting
    }

    async fn start(&mut self, node: &HidNode) -> Option<oneshot::Receiver<()>> {
        let raw = match self.backend.open(node) {
            Ok(raw) => raw,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    // Retried when the node's access changes (hotplug).
                    if self.warned.insert(node.id.clone()) {
                        tracing::warn!(
                            node = %node.id,
                            hint = self.backend.access_hint().unwrap_or(""),
                            "cannot open the Logitech receiver: permission denied (udev rule missing)"
                        );
                    }
                } else {
                    tracing::debug!(node = %node.id, error = %e, "cannot open HID node");
                    self.retry.insert(node.id.clone(), Instant::now() + REOPEN);
                }
                return None;
            }
        };
        let (link, notices) = match adapter::open_receiver(raw, &node.id, false).await {
            Ok(Some(r)) => r,
            Ok(None) | Err(adapter::AdapterError::NotHidpp) => {
                tracing::debug!(node = %node.id, name = %node.name, "not a Logitech receiver; ignored");
                self.ignored.insert(node.id.clone());
                return None;
            }
            Err(e) => {
                tracing::debug!(node = %node.id, error = %e, "cannot open HID++ channel");
                self.retry.insert(node.id.clone(), Instant::now() + REOPEN);
                return None;
            }
        };
        let link = Arc::new(link);
        let cancel = self.cancel.child_token();
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = ReceiverTask {
            link: link.clone(),
            notices,
            node: node.id.clone(),
            kernel_receiver: node.kernel_receiver,
            sink: self.sink.clone(),
            timing: self.timing,
        };
        let id = node.id.clone();
        let done = self.done.clone();
        let handle = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let gone = match task.run(cancel.clone(), ready_tx).await {
                    Ok(()) => return,
                    Err(adapter::AdapterError::Gone) => {
                        tracing::info!(node = %id, "HID++ receiver disconnected");
                        true
                    }
                    Err(e) => {
                        tracing::warn!(node = %id, error = %e, "HID++ receiver failed");
                        false
                    }
                };
                if !cancel.is_cancelled() {
                    let _ = done.send((id, gone));
                }
            }
        });
        self.running.insert(
            node.id.clone(),
            Running {
                link,
                cancel,
                handle,
            },
        );
        Some(ready_rx)
    }

    async fn stop(&mut self) {
        for (_, r) in std::mem::take(&mut self.running) {
            r.cancel.cancel();
            let _ = r.handle.await;
        }
    }
}
