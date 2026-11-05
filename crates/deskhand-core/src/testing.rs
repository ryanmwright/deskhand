//! Test helpers (enabled for this crate's tests and with the `test-util`
//! feature). Re-exported by `deskhand-testkit`.

use std::collections::HashMap;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::IntegrationConfig;
use crate::entity::{Attributes, CommandValue, EntityId, EntitySpec, State};
use crate::hub::{Hub, HubOptions};
use crate::integration::Integration;
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

/// Runs one integration against a hub and a connected [`FakeTransport`],
/// remembering the latest published states so waits never miss a value
/// that arrived in an earlier event (such as the startup snapshot).
#[derive(Debug)]
pub struct TestRig {
    t: FakeTransport,
    states: HashMap<EntityId, State>,
    attrs: HashMap<EntityId, Attributes>,
    events: Vec<(EntityId, String, Attributes)>,
    /// Entity ids of the latest snapshot or discovery update.
    pub entities: Vec<EntityId>,
    /// Specs of the latest snapshot or discovery update.
    pub specs: HashMap<EntityId, EntitySpec>,
}

impl TestRig {
    pub fn command(&self, id: &str, value: CommandValue) {
        self.t.command(id, value);
    }

    fn absorb(&mut self, ev: &HubEvent) {
        match ev {
            HubEvent::Snapshot(s) => {
                self.states.extend(s.states.clone());
                self.attrs.extend(s.attributes.clone());
                self.entities = s.entities.iter().map(|d| d.spec.id.clone()).collect();
                self.specs = s
                    .entities
                    .iter()
                    .map(|d| (d.spec.id.clone(), d.spec.clone()))
                    .collect();
            }
            HubEvent::Discovery { entities, .. } => {
                self.entities = entities.iter().map(|d| d.spec.id.clone()).collect();
                self.specs = entities
                    .iter()
                    .map(|d| (d.spec.id.clone(), d.spec.clone()))
                    .collect();
            }
            HubEvent::State { id, state, .. } => {
                self.states.insert(id.clone(), state.clone());
            }
            HubEvent::Attributes { id, attributes } => {
                self.attrs.insert(id.clone(), attributes.clone());
            }
            HubEvent::Event {
                id,
                event_type,
                attributes,
            } => {
                self.events
                    .push((id.clone(), event_type.clone(), attributes.clone()));
            }
            _ => {}
        }
    }

    /// The latest published state of `id`.
    pub fn state(&self, id: &EntityId) -> Option<&State> {
        self.states.get(id)
    }

    /// The latest published attributes of `id`.
    pub fn attributes(&self, id: &EntityId) -> Option<&Attributes> {
        self.attrs.get(id)
    }

    /// Removes and returns the events fired so far that no wait consumed.
    pub fn take_events(&mut self) -> Vec<(EntityId, String, Attributes)> {
        std::mem::take(&mut self.events)
    }

    /// Lets queued events arrive without waiting for anything in particular.
    pub async fn settle(&mut self, wait: Duration) {
        while let Some(ev) = self.t.next_within(wait).await {
            self.absorb(&ev);
        }
    }

    /// Processes events until `done` holds (checked before each event).
    async fn until(&mut self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        let limit = Duration::from_secs(10);
        let res = tokio::time::timeout(limit, async {
            while !done(self) {
                match self.t.next_within(limit).await {
                    Some(ev) => self.absorb(&ev),
                    None => return,
                }
            }
        })
        .await;
        if res.is_err() || !done(self) {
            panic!("timed out waiting for {what}; states: {:?}", self.states);
        }
    }
}

/// Starts `integration` with default config and a connected fake transport.
pub fn rig(integration: impl Integration, capability: &str) -> (TestRig, CancellationToken) {
    rig_with(integration, IntegrationConfig::new(capability))
}

pub fn rig_with(
    integration: impl Integration,
    config: IntegrationConfig,
) -> (TestRig, CancellationToken) {
    // Short removal grace so dynamic-entity tests don't wait 30 s.
    let (hub, handle, link) = Hub::new(HubOptions {
        removal_grace: Duration::from_millis(500),
        ..HubOptions::default()
    });
    tokio::spawn(hub.run());
    let t = FakeTransport::new(link);
    let cancel = CancellationToken::new();
    let capability = config.capability.to_string();
    let ctx = handle.ctx(config, cancel.clone());
    handle.integration_started(&capability);
    tokio::spawn(async move {
        if let Err(e) = integration.run(ctx).await {
            panic!("integration failed: {e}");
        }
    });
    handle.startup_complete();
    t.connect();
    let rig = TestRig {
        t,
        states: HashMap::new(),
        attrs: HashMap::new(),
        events: Vec::new(),
        entities: Vec::new(),
        specs: HashMap::new(),
    };
    (rig, cancel)
}

/// Waits until `id` has `want`.
pub async fn wait_state(r: &mut TestRig, id: &EntityId, want: State) {
    let what = format!("{id} = {want:?}");
    r.until(&what, |r| r.states.get(id) == Some(&want)).await;
}

/// Waits until attribute `key` of `id` is `want`.
pub async fn wait_attr(r: &mut TestRig, id: &EntityId, key: &str, want: serde_json::Value) {
    let what = format!("{id}.{key} = {want}");
    r.until(&what, |r| {
        r.attrs.get(id).and_then(|a| a.get(key)) == Some(&want)
    })
    .await;
}

/// Waits until the published spec of `id` satisfies `pred`.
pub async fn wait_spec(
    r: &mut TestRig,
    id: &EntityId,
    what: &str,
    pred: impl Fn(&EntitySpec) -> bool,
) {
    let what = format!("{id}: {what}");
    r.until(&what, |r| r.specs.get(id).is_some_and(&pred)).await;
}

/// Waits until the published entity set does (or doesn't) contain `id`.
pub async fn wait_entity(r: &mut TestRig, id: &EntityId, present: bool) {
    let what = format!("{id} present = {present}");
    r.until(&what, |r| r.entities.contains(id) == present).await;
}

/// Waits until `id` fired an event of `event_type`, and returns its attributes.
/// Each event is returned once.
pub async fn wait_event(r: &mut TestRig, id: &EntityId, event_type: &str) -> Attributes {
    let what = format!("event {id}: {event_type}");
    let pos = |r: &TestRig| {
        r.events
            .iter()
            .position(|(i, t, _)| i == id && t == event_type)
    };
    r.until(&what, |r| pos(r).is_some()).await;
    let i = pos(r).expect("event present");
    r.events.remove(i).2
}
