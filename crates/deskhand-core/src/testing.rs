//! Test helpers (enabled for this crate's tests and with the `test-util`
//! feature). Re-exported by `deskhand-testkit`.

use std::time::Duration;

use crate::entity::{CommandValue, EntityId};
use crate::transport::{HubEvent, TransportEvent, TransportLink};

/// Plays the transport's side of a [`TransportLink`] in tests.
#[derive(Debug)]
pub struct FakeTransport {
    link: TransportLink,
}

impl FakeTransport {
    pub fn new(link: TransportLink) -> Self {
        Self { link }
    }

    pub fn send(&self, event: TransportEvent) {
        self.link
            .to_hub
            .send(event)
            .expect("hub dropped its transport receiver");
    }

    pub fn connect(&self) {
        self.send(TransportEvent::Connected);
    }

    pub fn disconnect(&self) {
        self.send(TransportEvent::Disconnected);
    }

    pub fn command(&self, id: &str, value: CommandValue) {
        self.send(TransportEvent::Command {
            id: EntityId::new(id).expect("valid id"),
            value,
        });
    }

    /// Lets every task run until idle. With paused time this advances the
    /// clock by 1 ms (or to an earlier timer).
    pub async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    /// Everything the hub has sent so far, after letting it settle.
    pub async fn drain(&mut self) -> Vec<HubEvent> {
        self.settle().await;
        let mut out = Vec::new();
        while let Ok(ev) = self.link.events.try_recv() {
            out.push(ev);
        }
        out
    }

    /// Waits up to `timeout` (virtual time when paused) for the next event.
    pub async fn next_within(&mut self, timeout: Duration) -> Option<HubEvent> {
        tokio::time::timeout(timeout, self.link.events.recv())
            .await
            .ok()
            .flatten()
    }
}
