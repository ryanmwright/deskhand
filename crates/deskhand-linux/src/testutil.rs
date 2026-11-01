//! Test helpers: run one integration against a hub and a fake transport,
//! remembering the latest published states so waits never miss a value that
//! arrived in an earlier event (such as the startup snapshot).

use std::collections::HashMap;
use std::time::Duration;

use deskhand_core::testing::FakeTransport;
use deskhand_core::{
    Attributes, CommandValue, EntityId, Hub, HubEvent, HubOptions, Integration, IntegrationConfig,
    State,
};
use tokio_util::sync::CancellationToken;

pub(crate) struct TestRig {
    t: FakeTransport,
    states: HashMap<EntityId, State>,
    attrs: HashMap<EntityId, Attributes>,
    /// Entity ids of the latest snapshot or discovery update.
    pub(crate) entities: Vec<EntityId>,
}

impl TestRig {
    pub(crate) fn command(&self, id: &str, value: CommandValue) {
        self.t.command(id, value);
    }

    fn absorb(&mut self, ev: &HubEvent) {
        match ev {
            HubEvent::Snapshot(s) => {
                self.states.extend(s.states.clone());
                self.attrs.extend(s.attributes.clone());
                self.entities = s.entities.iter().map(|d| d.spec.id.clone()).collect();
            }
            HubEvent::Discovery { entities, .. } => {
                self.entities = entities.iter().map(|d| d.spec.id.clone()).collect();
            }
            HubEvent::State { id, state, .. } => {
                self.states.insert(id.clone(), state.clone());
            }
            HubEvent::Attributes { id, attributes } => {
                self.attrs.insert(id.clone(), attributes.clone());
            }
            _ => {}
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
pub(crate) fn rig(integration: impl Integration, capability: &str) -> (TestRig, CancellationToken) {
    rig_with(integration, IntegrationConfig::new(capability))
}

pub(crate) fn rig_with(
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
        entities: Vec::new(),
    };
    (rig, cancel)
}

/// Waits until `id` has `want`.
pub(crate) async fn wait_state(r: &mut TestRig, id: &EntityId, want: State) {
    let what = format!("{id} = {want:?}");
    r.until(&what, |r| r.states.get(id) == Some(&want)).await;
}

/// Waits until attribute `key` of `id` is `want`.
pub(crate) async fn wait_attr(r: &mut TestRig, id: &EntityId, key: &str, want: serde_json::Value) {
    let what = format!("{id}.{key} = {want}");
    r.until(&what, |r| {
        r.attrs.get(id).and_then(|a| a.get(key)) == Some(&want)
    })
    .await;
}

/// Waits until the published entity set does (or doesn't) contain `id`.
pub(crate) async fn wait_entity(r: &mut TestRig, id: &EntityId, present: bool) {
    let what = format!("{id} present = {present}");
    r.until(&what, |r| r.entities.contains(id) == present).await;
}
