//! The boundary between the hub and a transport (MQTT today, maybe a native
//! Home Assistant integration later).
//!
//! The hub owns all state. A transport is stateless with respect to entities:
//! whenever it (re)connects or Home Assistant restarts, it asks for a
//! [`Snapshot`] and republishes everything.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::entity::{Attributes, CommandValue, EntityId, EntitySpec, State};

/// An entity as the transport sees it: its spec plus the capability that owns
/// it (which selects the availability it follows).
#[derive(Debug, Clone, PartialEq)]
pub struct EntityDescriptor {
    pub spec: EntitySpec,
    pub owner: Arc<str>,
}

/// Everything a transport needs to publish from scratch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// All registered entities, sorted by id.
    pub entities: Vec<EntityDescriptor>,
    /// Latest known state per entity (entities without a state are omitted).
    pub states: BTreeMap<EntityId, State>,
    pub attributes: BTreeMap<EntityId, Attributes>,
    /// Availability per capability.
    pub availability: BTreeMap<Arc<str>, bool>,
    /// Ids of non-entity JSON command endpoints (for example rich notifications).
    pub json_commands: Vec<EntityId>,
}

/// Hub → transport.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum HubEvent {
    /// Publish everything (discovery, states, availability). Sent after the
    /// transport reports [`TransportEvent::Connected`] or
    /// [`TransportEvent::HaOnline`], and after unpausing.
    Snapshot(Snapshot),
    /// The set of entities changed; republish discovery.
    Discovery {
        entities: Vec<EntityDescriptor>,
        json_commands: Vec<EntityId>,
    },
    State {
        id: EntityId,
        state: State,
        retain: bool,
    },
    Attributes {
        id: EntityId,
        attributes: Attributes,
    },
    Event {
        id: EntityId,
        event_type: String,
        attributes: Attributes,
    },
    Availability {
        capability: Arc<str>,
        available: bool,
    },
    /// The user paused publishing (tray). Publish offline and ignore commands
    /// until `Pause(false)`, which is followed by a fresh snapshot.
    Pause(bool),
    /// The machine is about to sleep: publish offline and disconnect cleanly.
    Sleep,
    /// The machine woke up: reconnect immediately.
    Resume,
    /// Deskhand is exiting: publish offline, disconnect and return from `run`.
    Shutdown,
}

/// Transport → hub.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum TransportEvent {
    Connected,
    Disconnected,
    /// Home Assistant (re)started and wants discovery again.
    HaOnline,
    /// A command for an entity or JSON endpoint. Transports must drop
    /// retained command messages before sending this.
    Command {
        id: EntityId,
        value: CommandValue,
    },
}

/// The transport's ends of the hub channels.
#[derive(Debug)]
pub struct TransportLink {
    pub events: mpsc::UnboundedReceiver<HubEvent>,
    pub to_hub: mpsc::UnboundedSender<TransportEvent>,
}

/// Error returned by a transport that cannot continue.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct TransportError(#[from] pub Box<dyn std::error::Error + Send + Sync>);

impl TransportError {
    pub fn msg(msg: impl Into<String>) -> Self {
        Self(msg.into().into())
    }
}

/// A way of getting entities to Home Assistant.
#[async_trait::async_trait]
pub trait Transport: Send + 'static {
    /// Runs until [`HubEvent::Shutdown`] or the hub goes away. Reconnecting
    /// after network failures is the transport's own job.
    async fn run(self: Box<Self>, link: TransportLink) -> Result<(), TransportError>;
}
