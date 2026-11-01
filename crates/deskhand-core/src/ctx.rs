//! What an integration gets to work with: a [`Ctx`] that hands out typed
//! entity handles.
//!
//! Handles are cheap to clone and usable from synchronous code (for example a
//! libpulse or Wayland thread): `set` never blocks. Dropping the last clone of
//! a handle (and its command stream) unregisters the entity after a grace
//! period, so an integration restart does not churn Home Assistant.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::config::{ConfigError, IntegrationConfig};
use crate::entity::{
    Attributes, CommandValue, EntityId, EntityKind, EntitySpec, LockCmd, MediaCmd,
    MediaPlayerState, State,
};
use crate::hub::{CommandSink, HubMsg, Lifecycle};

/// Capacity of each command stream. Commands beyond this are dropped with a warning.
pub const COMMAND_CAPACITY: usize = 8;

/// Shared latest-state registry used by [`Ctx::observe`].
pub(crate) type Watches = Arc<Mutex<HashMap<EntityId, watch::Sender<State>>>>;

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// One live registration. Dropping it unregisters the entity.
struct Registration {
    id: EntityId,
    generation: u64,
    hub: mpsc::UnboundedSender<HubMsg>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = self.hub.send(HubMsg::Unregister {
            id: self.id.clone(),
            generation: self.generation,
        });
    }
}

impl Registration {
    fn send(&self, msg: HubMsg) {
        // The hub only goes away at shutdown; updates after that are moot.
        let _ = self.hub.send(msg);
    }

    fn set_state(&self, state: State) {
        self.send(HubMsg::SetState {
            id: self.id.clone(),
            state,
        });
    }
}

/// A stream of commands from Home Assistant for one entity.
pub struct Commands<T> {
    rx: mpsc::Receiver<T>,
    _registration: Arc<Registration>,
}

impl<T> Commands<T> {
    /// Waits for the next command. Returns `None` once the hub has shut down.
    pub async fn recv(&mut self) -> Option<T> {
        self.rx.recv().await
    }

    /// Returns a pending command without waiting.
    pub fn try_recv(&mut self) -> Option<T> {
        self.rx.try_recv().ok()
    }
}

impl<T> fmt::Debug for Commands<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Commands")
            .field("id", &self._registration.id)
            .finish_non_exhaustive()
    }
}

/// Generates a handle type with the shared methods.
macro_rules! handle {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone)]
        pub struct $name {
            reg: Arc<Registration>,
        }

        impl $name {
            pub fn id(&self) -> &EntityId {
                &self.reg.id
            }

            /// Replaces the entity's attributes.
            pub fn set_attributes(&self, attributes: Attributes) {
                self.reg.send(HubMsg::SetAttributes {
                    id: self.reg.id.clone(),
                    attributes,
                });
            }

            /// Sets the state back to unknown.
            pub fn set_unknown(&self) {
                self.reg.set_state(State::Unknown);
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name)).field("id", &self.reg.id).finish()
            }
        }
    };
}

handle!(
    /// A read-only value.
    Sensor
);
impl Sensor {
    pub fn set(&self, state: impl Into<State>) {
        self.reg.set_state(state.into());
    }
}

handle!(
    /// An on/off value.
    BinarySensor
);
impl BinarySensor {
    pub fn set(&self, on: bool) {
        self.reg.set_state(State::Bool(on));
    }
}

handle!(
    /// The state side of a switch. Report the *real* state after acting on a command.
    Switch
);
impl Switch {
    pub fn set(&self, on: bool) {
        self.reg.set_state(State::Bool(on));
    }
}

handle!(
    /// The state side of a number entity.
    Number
);
impl Number {
    pub fn set(&self, value: f64) {
        self.reg.set_state(State::Float(value));
    }
}

handle!(
    /// The state side of a text entity.
    Text
);
impl Text {
    pub fn set(&self, value: impl Into<String>) {
        self.reg.set_state(State::Text(value.into()));
    }
}

/// The state side of a select entity. Its options can change at runtime.
#[derive(Clone)]
pub struct Select {
    reg: Arc<Registration>,
    owner: Arc<str>,
    spec: Arc<Mutex<EntitySpec>>,
}

impl Select {
    pub fn id(&self) -> &EntityId {
        &self.reg.id
    }

    pub fn set(&self, option: impl Into<String>) {
        self.reg.set_state(State::Text(option.into()));
    }

    pub fn set_attributes(&self, attributes: Attributes) {
        self.reg.send(HubMsg::SetAttributes {
            id: self.reg.id.clone(),
            attributes,
        });
    }

    /// Replaces the option list (triggers a discovery update if it changed).
    pub fn set_options(&self, options: Vec<String>) {
        let spec = {
            let mut spec = self.spec.lock().unwrap_or_else(|e| e.into_inner());
            match &mut spec.kind {
                EntityKind::Select { options: current } if *current != options => {
                    *current = options;
                }
                _ => return,
            }
            spec.clone()
        };
        self.reg.send(HubMsg::Register {
            owner: self.owner.clone(),
            spec,
            sink: None,
            generation: self.reg.generation,
        });
    }
}

impl fmt::Debug for Select {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Select").field("id", &self.reg.id).finish()
    }
}

/// State a lock entity can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LockState {
    Locked,
    Unlocked,
    Locking,
    Unlocking,
    Jammed,
}

impl LockState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Locked => "locked",
            Self::Unlocked => "unlocked",
            Self::Locking => "locking",
            Self::Unlocking => "unlocking",
            Self::Jammed => "jammed",
        }
    }
}

handle!(
    /// The state side of a lock entity.
    Lock
);
impl Lock {
    pub fn set(&self, state: LockState) {
        self.reg.set_state(State::Text(state.as_str().to_owned()));
    }
}

handle!(
    /// An image entity.
    Image
);
impl Image {
    pub fn set_url(&self, url: impl Into<String>) {
        self.reg.set_state(State::Text(url.into()));
    }

    pub fn set_bytes(&self, bytes: bytes::Bytes) {
        self.reg.set_state(State::Bytes(bytes));
    }
}

handle!(
    /// An event entity: fires momentary events, has no persistent state.
    Event
);
impl Event {
    pub fn fire(&self, event_type: impl Into<String>, attributes: Attributes) {
        self.reg.send(HubMsg::FireEvent {
            id: self.reg.id.clone(),
            event_type: event_type.into(),
            attributes,
        });
    }
}

handle!(
    /// A media player entity, rendered only by transports that support one
    /// (the HACS `mqtt_media_player` compatibility mode).
    MediaPlayer
);
impl MediaPlayer {
    pub fn set(&self, state: MediaPlayerState) {
        self.reg.set_state(state.into());
    }
}

handle!(
    /// Escape hatch for kinds without a dedicated handle (update).
    RawEntity
);
impl RawEntity {
    pub fn set(&self, state: impl Into<State>) {
        self.reg.set_state(state.into());
    }
}

/// Lifecycle notifications broadcast to integrations.
pub type LifecycleReceiver = broadcast::Receiver<Lifecycle>;

/// Handed to each integration run.
#[derive(Clone)]
pub struct Ctx {
    inner: Arc<CtxInner>,
}

struct CtxInner {
    capability: Arc<str>,
    hub: mpsc::UnboundedSender<HubMsg>,
    watches: Watches,
    lifecycle: broadcast::Sender<Lifecycle>,
    config: IntegrationConfig,
    cancel: CancellationToken,
}

impl fmt::Debug for Ctx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ctx")
            .field("capability", &self.inner.capability)
            .finish_non_exhaustive()
    }
}

impl Ctx {
    pub(crate) fn new(
        hub: mpsc::UnboundedSender<HubMsg>,
        watches: Watches,
        lifecycle: broadcast::Sender<Lifecycle>,
        config: IntegrationConfig,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            inner: Arc::new(CtxInner {
                capability: config.capability.clone(),
                hub,
                watches,
                lifecycle,
                config,
                cancel,
            }),
        }
    }

    pub fn capability(&self) -> &str {
        &self.inner.capability
    }

    fn register(&self, spec: EntitySpec, sink: Option<CommandSink>) -> Arc<Registration> {
        let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        let reg = Arc::new(Registration {
            id: spec.id.clone(),
            generation,
            hub: self.inner.hub.clone(),
        });
        let _ = self.inner.hub.send(HubMsg::Register {
            owner: self.inner.capability.clone(),
            spec,
            sink,
            generation,
        });
        reg
    }

    fn register_with_commands<T>(
        &self,
        spec: EntitySpec,
        make_sink: impl FnOnce(mpsc::Sender<T>) -> CommandSink,
    ) -> (Arc<Registration>, Commands<T>) {
        let (tx, rx) = mpsc::channel(COMMAND_CAPACITY);
        let reg = self.register(spec, Some(make_sink(tx)));
        let commands = Commands {
            rx,
            _registration: reg.clone(),
        };
        (reg, commands)
    }

    fn expect_kind(spec: &EntitySpec, ok: bool, wanted: &str) {
        debug_assert!(
            ok,
            "entity {} registered as {wanted} with kind {:?}",
            spec.id, spec.kind
        );
        if !ok {
            tracing::warn!(id = %spec.id, kind = spec.kind.platform(), "entity registered as {wanted} with mismatched kind");
        }
    }

    pub fn sensor(&self, spec: EntitySpec) -> Sensor {
        Self::expect_kind(
            &spec,
            matches!(spec.kind, EntityKind::Sensor { .. }),
            "sensor",
        );
        Sensor {
            reg: self.register(spec, None),
        }
    }

    pub fn binary_sensor(&self, spec: EntitySpec) -> BinarySensor {
        Self::expect_kind(
            &spec,
            matches!(spec.kind, EntityKind::BinarySensor { .. }),
            "binary_sensor",
        );
        BinarySensor {
            reg: self.register(spec, None),
        }
    }

    pub fn switch(&self, spec: EntitySpec) -> (Switch, Commands<bool>) {
        Self::expect_kind(
            &spec,
            matches!(spec.kind, EntityKind::Switch { .. }),
            "switch",
        );
        let (reg, cmds) = self.register_with_commands(spec, CommandSink::Bool);
        (Switch { reg }, cmds)
    }

    pub fn button(&self, spec: EntitySpec) -> Commands<()> {
        Self::expect_kind(
            &spec,
            matches!(spec.kind, EntityKind::Button { .. }),
            "button",
        );
        self.register_with_commands(spec, CommandSink::Unit).1
    }

    pub fn number(&self, spec: EntitySpec) -> (Number, Commands<f64>) {
        Self::expect_kind(
            &spec,
            matches!(spec.kind, EntityKind::Number { .. }),
            "number",
        );
        let (reg, cmds) = self.register_with_commands(spec, CommandSink::Number);
        (Number { reg }, cmds)
    }

    pub fn select(&self, spec: EntitySpec) -> (Select, Commands<String>) {
        Self::expect_kind(
            &spec,
            matches!(spec.kind, EntityKind::Select { .. }),
            "select",
        );
        let shared = Arc::new(Mutex::new(spec.clone()));
        let (reg, cmds) = self.register_with_commands(spec, CommandSink::Text);
        let select = Select {
            reg,
            owner: self.inner.capability.clone(),
            spec: shared,
        };
        (select, cmds)
    }

    pub fn text(&self, spec: EntitySpec) -> (Text, Commands<String>) {
        Self::expect_kind(&spec, matches!(spec.kind, EntityKind::Text { .. }), "text");
        let (reg, cmds) = self.register_with_commands(spec, CommandSink::Text);
        (Text { reg }, cmds)
    }

    pub fn event(&self, spec: EntitySpec) -> Event {
        Self::expect_kind(
            &spec,
            matches!(spec.kind, EntityKind::Event { .. }),
            "event",
        );
        Event {
            reg: self.register(spec, None),
        }
    }

    /// A notify entity: each command is one message text.
    pub fn notify(&self, spec: EntitySpec) -> Commands<String> {
        Self::expect_kind(&spec, matches!(spec.kind, EntityKind::Notify), "notify");
        self.register_with_commands(spec, CommandSink::Text).1
    }

    pub fn lock(&self, spec: EntitySpec) -> (Lock, Commands<LockCmd>) {
        Self::expect_kind(&spec, matches!(spec.kind, EntityKind::Lock), "lock");
        let (reg, cmds) = self.register_with_commands(spec, CommandSink::Lock);
        (Lock { reg }, cmds)
    }

    pub fn image(&self, spec: EntitySpec) -> Image {
        Self::expect_kind(
            &spec,
            matches!(spec.kind, EntityKind::Image { .. }),
            "image",
        );
        Image {
            reg: self.register(spec, None),
        }
    }

    pub fn media_player(&self, spec: EntitySpec) -> (MediaPlayer, Commands<MediaCmd>) {
        Self::expect_kind(
            &spec,
            matches!(spec.kind, EntityKind::MediaPlayer),
            "media_player",
        );
        let (reg, cmds) = self.register_with_commands(spec, CommandSink::Media);
        (MediaPlayer { reg }, cmds)
    }

    /// Any kind, with untyped commands. Prefer the typed constructors.
    pub fn raw(&self, spec: EntitySpec) -> (RawEntity, Commands<CommandValue>) {
        let (reg, cmds) = self.register_with_commands(spec, CommandSink::Raw);
        (RawEntity { reg }, cmds)
    }

    /// A JSON command endpoint that is not an entity (for example rich
    /// notifications). The transport decides its topic.
    pub fn json_command(&self, id: EntityId) -> Commands<serde_json::Value> {
        let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        let reg = Arc::new(Registration {
            id: id.clone(),
            generation,
            hub: self.inner.hub.clone(),
        });
        let (tx, rx) = mpsc::channel(COMMAND_CAPACITY);
        let _ = self.inner.hub.send(HubMsg::RegisterJson {
            owner: self.inner.capability.clone(),
            id,
            sink: tx,
            generation,
        });
        Commands {
            rx,
            _registration: reg,
        }
    }

    /// Watches the latest state of any entity (also other integrations'),
    /// for derived sensors. Starts as `Unknown` if the entity has no state yet.
    pub fn observe(&self, id: &EntityId) -> watch::Receiver<State> {
        let mut watches = self.inner.watches.lock().unwrap_or_else(|e| e.into_inner());
        watches
            .entry(id.clone())
            .or_insert_with(|| watch::channel(State::Unknown).0)
            .subscribe()
    }

    /// Marks this integration's entities available or unavailable.
    pub fn set_available(&self, available: bool) {
        let _ = self.inner.hub.send(HubMsg::Availability {
            capability: self.inner.capability.clone(),
            available,
        });
    }

    /// Call once the initial entities are registered and have their first
    /// states. Marks the integration available and releases the startup barrier.
    pub fn ready(&self) {
        let _ = self.inner.hub.send(HubMsg::Ready {
            capability: self.inner.capability.clone(),
        });
    }

    /// Deserializes this integration's `[integrations.<capability>]` options,
    /// warning about unknown keys.
    pub fn config<T: DeserializeOwned + Default>(&self) -> Result<T, ConfigError> {
        let parsed = self.inner.config.options::<T>()?;
        for key in &parsed.unknown_keys {
            tracing::warn!(key, "unknown config key ignored");
        }
        Ok(parsed.value)
    }

    /// Cancelled when the integration should stop.
    pub fn cancelled(&self) -> CancellationToken {
        self.inner.cancel.clone()
    }

    /// Announces an imminent sleep: integrations get [`Lifecycle::PrepareSleep`]
    /// and can still publish (for example `session_state=sleeping`). Follow
    /// with [`Ctx::notify_sleep`] after a short pause.
    ///
    /// [`Lifecycle::PrepareSleep`]: crate::hub::Lifecycle::PrepareSleep
    pub fn notify_prepare_sleep(&self) {
        let _ = self.inner.hub.send(HubMsg::PrepareSleep);
    }

    /// Reports that the machine is about to sleep (from the platform's power
    /// integration): the transport publishes offline and disconnects.
    pub fn notify_sleep(&self) {
        let _ = self.inner.hub.send(HubMsg::Sleep);
    }

    /// Reports that the machine woke up.
    pub fn notify_resume(&self) {
        let _ = self.inner.hub.send(HubMsg::Resume);
    }

    /// Subscribes to sleep/resume notifications.
    pub fn lifecycle(&self) -> LifecycleReceiver {
        self.inner.lifecycle.subscribe()
    }
}
