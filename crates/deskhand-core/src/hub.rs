//! The hub: a single actor that owns every entity's spec and state and talks
//! to the transport.
//!
//! Responsibilities:
//! - dedupe unchanged states and attributes, throttle per entity (`min_interval`);
//! - hold back the first publication until every started integration is
//!   ready (or a timeout), so HA sees one complete device instead of a trickle;
//! - batch entity additions/removals into one discovery update;
//! - keep dropped entities for a grace period so restarts don't churn HA;
//! - route commands from the transport to typed handles;
//! - pause, sleep/resume and shutdown.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::IntegrationConfig;
use crate::ctx::{Ctx, Watches};
use crate::entity::{Attributes, CommandValue, EntityId, EntitySpec, LockCmd, State};
use crate::transport::{EntityDescriptor, HubEvent, Snapshot, TransportEvent, TransportLink};

/// Machine power lifecycle, broadcast to integrations and the supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lifecycle {
    Sleep,
    Resume,
}

/// Where a command for an entity goes.
#[derive(Debug)]
pub(crate) enum CommandSink {
    Unit(mpsc::Sender<()>),
    Bool(mpsc::Sender<bool>),
    Number(mpsc::Sender<f64>),
    Text(mpsc::Sender<String>),
    Lock(mpsc::Sender<LockCmd>),
    Raw(mpsc::Sender<CommandValue>),
}

/// Messages from handles, contexts and the supervisor.
#[derive(Debug)]
pub(crate) enum HubMsg {
    /// New entity, or an update to an existing one. `sink: None` with the
    /// current generation keeps the existing sink (used by `Select::set_options`).
    Register {
        owner: Arc<str>,
        spec: EntitySpec,
        sink: Option<CommandSink>,
        generation: u64,
    },
    Unregister {
        id: EntityId,
        generation: u64,
    },
    SetState {
        id: EntityId,
        state: State,
    },
    SetAttributes {
        id: EntityId,
        attributes: Attributes,
    },
    FireEvent {
        id: EntityId,
        event_type: String,
        attributes: Attributes,
    },
    RegisterJson {
        owner: Arc<str>,
        id: EntityId,
        sink: mpsc::Sender<serde_json::Value>,
        generation: u64,
    },
    Started {
        capability: Arc<str>,
    },
    Ready {
        capability: Arc<str>,
    },
    Stopped {
        capability: Arc<str>,
    },
    StartupComplete,
    Availability {
        capability: Arc<str>,
        available: bool,
    },
    Pause(bool),
    Sleep,
    Resume,
    Shutdown,
}

/// Timing knobs (defaults per `docs/v1-plan.md`; tests shorten nothing, they use paused time).
#[derive(Debug, Clone)]
pub struct HubOptions {
    /// After the supervisor has started everything, how long to wait for
    /// integrations to call `ready()` before publishing anyway.
    pub startup_timeout: Duration,
    /// Upper bound on the startup barrier measured from hub creation, in case
    /// probing itself hangs.
    pub max_startup: Duration,
    /// Entity changes within this window are batched into one discovery update.
    pub discovery_debounce: Duration,
    /// How long a dropped entity stays published before it is removed.
    pub removal_grace: Duration,
}

impl Default for HubOptions {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(5),
            max_startup: Duration::from_secs(15),
            discovery_debounce: Duration::from_secs(1),
            removal_grace: Duration::from_secs(30),
        }
    }
}

/// Cloneable access to a running hub.
#[derive(Debug, Clone)]
pub struct HubHandle {
    tx: mpsc::UnboundedSender<HubMsg>,
    watches: Watches,
    lifecycle: broadcast::Sender<Lifecycle>,
    released: watch::Receiver<bool>,
}

impl HubHandle {
    /// Creates the context for one integration run.
    pub fn ctx(&self, config: IntegrationConfig, cancel: CancellationToken) -> Ctx {
        Ctx::new(
            self.tx.clone(),
            self.watches.clone(),
            self.lifecycle.clone(),
            config,
            cancel,
        )
    }

    fn send(&self, msg: HubMsg) {
        let _ = self.tx.send(msg);
    }

    /// The supervisor started an integration for `capability`.
    pub fn integration_started(&self, capability: &str) {
        self.send(HubMsg::Started {
            capability: Arc::from(capability),
        });
    }

    /// The integration for `capability` stopped (returned or failed).
    pub fn integration_stopped(&self, capability: &str) {
        self.send(HubMsg::Stopped {
            capability: Arc::from(capability),
        });
    }

    /// The supervisor finished its initial probe-and-start pass.
    pub fn startup_complete(&self) {
        self.send(HubMsg::StartupComplete);
    }

    /// Pauses or resumes publishing (tray toggle).
    pub fn set_paused(&self, paused: bool) {
        self.send(HubMsg::Pause(paused));
    }

    pub fn sleep(&self) {
        self.send(HubMsg::Sleep);
    }

    pub fn resume(&self) {
        self.send(HubMsg::Resume);
    }

    /// Tells the transport to go offline and stops the hub.
    pub fn shutdown(&self) {
        self.send(HubMsg::Shutdown);
    }

    pub fn lifecycle(&self) -> broadcast::Receiver<Lifecycle> {
        self.lifecycle.subscribe()
    }

    /// Resolves once the startup barrier has released (every started
    /// integration is ready, or the timeout passed). Used for systemd's
    /// `READY=1`. Also resolves if the hub stops first.
    pub async fn wait_released(&self) {
        let mut rx = self.released.clone();
        let _ = rx.wait_for(|r| *r).await;
    }
}

struct EntityEntry {
    owner: Arc<str>,
    spec: EntitySpec,
    sink: Option<CommandSink>,
    generation: u64,
    state: Option<State>,
    attributes: Option<Attributes>,
    last_sent: Option<Instant>,
    /// A throttled state update is waiting.
    pending: bool,
    /// Set when the last handle was dropped.
    remove_at: Option<Instant>,
}

impl EntityEntry {
    fn throttle_deadline(&self) -> Option<Instant> {
        match (self.pending, self.last_sent, self.spec.min_interval) {
            (true, Some(last), Some(min)) => Some(last + min),
            _ => None,
        }
    }
}

struct JsonEntry {
    owner: Arc<str>,
    sink: mpsc::Sender<serde_json::Value>,
    generation: u64,
}

/// The hub actor. Create with [`Hub::new`], then `tokio::spawn(hub.run())`.
pub struct Hub {
    opts: HubOptions,
    rx: mpsc::UnboundedReceiver<HubMsg>,
    transport_rx: Option<mpsc::UnboundedReceiver<TransportEvent>>,
    to_transport: mpsc::UnboundedSender<HubEvent>,
    watches: Watches,
    lifecycle: broadcast::Sender<Lifecycle>,
    released_tx: watch::Sender<bool>,

    entities: BTreeMap<EntityId, EntityEntry>,
    json: BTreeMap<EntityId, JsonEntry>,
    availability: BTreeMap<Arc<str>, bool>,
    started: BTreeSet<Arc<str>>,
    ready: BTreeSet<Arc<str>>,

    startup_complete: bool,
    released: bool,
    barrier_deadline: Instant,
    discovery_deadline: Option<Instant>,
    connected: bool,
    paused: bool,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub")
            .field("entities", &self.entities.len())
            .field("released", &self.released)
            .field("connected", &self.connected)
            .field("paused", &self.paused)
            .finish_non_exhaustive()
    }
}

enum Flow {
    Continue,
    Stop,
}

impl Hub {
    pub fn new(opts: HubOptions) -> (Self, HubHandle, TransportLink) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (to_transport, events) = mpsc::unbounded_channel();
        let (to_hub, transport_rx) = mpsc::unbounded_channel();
        let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
        let (lifecycle, _) = broadcast::channel(16);
        let (released_tx, released) = watch::channel(false);
        let handle = HubHandle {
            tx,
            watches: watches.clone(),
            lifecycle: lifecycle.clone(),
            released,
        };
        let barrier_deadline = Instant::now() + opts.max_startup;
        let hub = Self {
            opts,
            rx,
            transport_rx: Some(transport_rx),
            to_transport,
            watches,
            lifecycle,
            released_tx,
            entities: BTreeMap::new(),
            json: BTreeMap::new(),
            availability: BTreeMap::new(),
            started: BTreeSet::new(),
            ready: BTreeSet::new(),
            startup_complete: false,
            released: false,
            barrier_deadline,
            discovery_deadline: None,
            connected: false,
            paused: false,
        };
        (hub, handle, TransportLink { events, to_hub })
    }

    /// Runs until [`HubHandle::shutdown`] or until every handle is dropped.
    pub async fn run(mut self) {
        loop {
            let deadline = self.next_deadline();
            let flow = tokio::select! {
                msg = self.rx.recv() => match msg {
                    Some(msg) => self.handle_msg(msg),
                    None => Flow::Stop,
                },
                ev = recv_opt(&mut self.transport_rx) => {
                    match ev {
                        Some(ev) => self.handle_transport(ev),
                        None => {
                            tracing::debug!("transport channel closed");
                            self.transport_rx = None;
                            self.connected = false;
                        }
                    }
                    Flow::Continue
                }
                () = sleep_until_opt(deadline) => {
                    self.on_timer(Instant::now());
                    Flow::Continue
                }
            };
            if let Flow::Stop = flow {
                break;
            }
        }
        tracing::debug!("hub stopped");
    }

    /// Whether states and events should go to the transport right now.
    fn live(&self) -> bool {
        self.connected && self.released && !self.paused
    }

    fn emit(&self, event: HubEvent) {
        let _ = self.to_transport.send(event);
    }

    fn next_deadline(&self) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let mut consider = |t: Option<Instant>| {
            if let Some(t) = t {
                next = Some(next.map_or(t, |n| n.min(t)));
            }
        };
        if !self.released {
            consider(Some(self.barrier_deadline));
        }
        consider(self.discovery_deadline);
        for e in self.entities.values() {
            consider(e.remove_at);
            consider(e.throttle_deadline());
        }
        next
    }

    fn on_timer(&mut self, now: Instant) {
        self.check_barrier(now);

        let expired: Vec<EntityId> = self
            .entities
            .iter()
            .filter(|(_, e)| e.remove_at.is_some_and(|t| t <= now))
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            tracing::debug!(%id, "removing entity after grace period");
            self.entities.remove(&id);
            self.mark_discovery_dirty(now);
        }

        let due: Vec<EntityId> = self
            .entities
            .iter()
            .filter(|(_, e)| e.throttle_deadline().is_some_and(|t| t <= now))
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            self.flush_state(&id, now);
        }

        if self.discovery_deadline.is_some_and(|t| t <= now) {
            self.discovery_deadline = None;
            if self.live() {
                self.emit(HubEvent::Discovery {
                    entities: self.descriptors(),
                    json_commands: self.json.keys().cloned().collect(),
                });
            }
        }
    }

    fn check_barrier(&mut self, now: Instant) {
        if self.released {
            return;
        }
        let all_ready = self.startup_complete && self.started.is_subset(&self.ready);
        if all_ready || now >= self.barrier_deadline {
            if !all_ready {
                let waiting: Vec<&str> =
                    self.started.difference(&self.ready).map(|c| &**c).collect();
                tracing::warn!(
                    ?waiting,
                    "publishing before all integrations reported ready"
                );
            }
            self.released = true;
            self.released_tx.send_replace(true);
            self.discovery_deadline = None;
            if self.connected && !self.paused {
                self.emit(HubEvent::Snapshot(self.snapshot()));
            }
        }
    }

    fn mark_discovery_dirty(&mut self, now: Instant) {
        if self.released && self.discovery_deadline.is_none() {
            self.discovery_deadline = Some(now + self.opts.discovery_debounce);
        }
    }

    fn descriptors(&self) -> Vec<EntityDescriptor> {
        self.entities
            .values()
            .map(|e| EntityDescriptor {
                spec: e.spec.clone(),
                owner: e.owner.clone(),
            })
            .collect()
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            entities: self.descriptors(),
            states: self
                .entities
                .iter()
                .filter_map(|(id, e)| e.state.clone().map(|s| (id.clone(), s)))
                .collect(),
            attributes: self
                .entities
                .iter()
                .filter_map(|(id, e)| e.attributes.clone().map(|a| (id.clone(), a)))
                .collect(),
            availability: self.availability.clone(),
            json_commands: self.json.keys().cloned().collect(),
        }
    }

    fn set_availability(&mut self, capability: Arc<str>, available: bool) {
        let changed = self.availability.get(&capability) != Some(&available);
        self.availability.insert(capability.clone(), available);
        if changed && self.live() {
            self.emit(HubEvent::Availability {
                capability,
                available,
            });
        }
    }

    fn flush_state(&mut self, id: &EntityId, now: Instant) {
        let live = self.live();
        let Some(e) = self.entities.get_mut(id) else {
            return;
        };
        e.pending = false;
        if !live {
            return;
        }
        if let Some(state) = e.state.clone() {
            e.last_sent = Some(now);
            let retain = e.spec.retain;
            self.emit(HubEvent::State {
                id: id.clone(),
                state,
                retain,
            });
        }
    }

    fn handle_msg(&mut self, msg: HubMsg) -> Flow {
        let now = Instant::now();
        match msg {
            HubMsg::Register {
                owner,
                spec,
                sink,
                generation,
            } => self.register(owner, spec, sink, generation, now),
            HubMsg::Unregister { id, generation } => {
                if let Some(e) = self.entities.get_mut(&id) {
                    if e.generation == generation {
                        e.remove_at = Some(now + self.opts.removal_grace);
                        e.sink = None;
                    }
                } else if self
                    .json
                    .get(&id)
                    .is_some_and(|j| j.generation == generation)
                {
                    self.json.remove(&id);
                    self.mark_discovery_dirty(now);
                }
            }
            HubMsg::SetState { id, state } => self.set_state(id, state, now),
            HubMsg::SetAttributes { id, attributes } => {
                let live = self.live();
                if let Some(e) = self.entities.get_mut(&id)
                    && e.attributes.as_ref() != Some(&attributes)
                {
                    e.attributes = Some(attributes.clone());
                    if live {
                        self.emit(HubEvent::Attributes { id, attributes });
                    }
                }
            }
            HubMsg::FireEvent {
                id,
                event_type,
                attributes,
            } => {
                if self.live() && self.entities.contains_key(&id) {
                    self.emit(HubEvent::Event {
                        id,
                        event_type,
                        attributes,
                    });
                }
            }
            HubMsg::RegisterJson {
                owner,
                id,
                sink,
                generation,
            } => {
                let is_new = !self.json.contains_key(&id);
                self.json.insert(
                    id,
                    JsonEntry {
                        owner,
                        sink,
                        generation,
                    },
                );
                if is_new {
                    self.mark_discovery_dirty(now);
                }
            }
            HubMsg::Started { capability } => {
                self.ready.remove(&capability);
                self.started.insert(capability.clone());
                self.set_availability(capability, false);
            }
            HubMsg::Ready { capability } => {
                self.ready.insert(capability.clone());
                self.set_availability(capability, true);
                self.check_barrier(now);
            }
            HubMsg::Stopped { capability } => {
                self.started.remove(&capability);
                self.ready.remove(&capability);
                self.set_availability(capability, false);
                self.check_barrier(now);
            }
            HubMsg::StartupComplete => {
                self.startup_complete = true;
                self.barrier_deadline = self.barrier_deadline.min(now + self.opts.startup_timeout);
                self.check_barrier(now);
            }
            HubMsg::Availability {
                capability,
                available,
            } => self.set_availability(capability, available),
            HubMsg::Pause(paused) => {
                if paused != self.paused {
                    self.paused = paused;
                    tracing::info!(paused, "publishing paused state changed");
                    if self.connected {
                        self.emit(HubEvent::Pause(paused));
                        if !paused && self.released {
                            self.emit(HubEvent::Snapshot(self.snapshot()));
                        }
                    }
                }
            }
            HubMsg::Sleep => {
                let _ = self.lifecycle.send(Lifecycle::Sleep);
                self.emit(HubEvent::Sleep);
            }
            HubMsg::Resume => {
                let _ = self.lifecycle.send(Lifecycle::Resume);
                self.emit(HubEvent::Resume);
            }
            HubMsg::Shutdown => {
                self.emit(HubEvent::Shutdown);
                return Flow::Stop;
            }
        }
        Flow::Continue
    }

    fn register(
        &mut self,
        owner: Arc<str>,
        spec: EntitySpec,
        sink: Option<CommandSink>,
        generation: u64,
        now: Instant,
    ) {
        let id = spec.id.clone();
        match self.entities.get_mut(&id) {
            Some(e) => {
                if e.owner != owner && e.remove_at.is_none() {
                    tracing::warn!(%id, old = %e.owner, new = %owner, "entity id registered by two integrations; last one wins");
                }
                let spec_changed = e.spec != spec;
                let same_registration = e.generation == generation;
                e.owner = owner;
                e.spec = spec;
                e.remove_at = None;
                if !(same_registration && sink.is_none()) {
                    e.sink = sink;
                }
                e.generation = generation;
                if spec_changed {
                    self.mark_discovery_dirty(now);
                }
            }
            None => {
                self.entities.insert(
                    id,
                    EntityEntry {
                        owner,
                        spec,
                        sink,
                        generation,
                        state: None,
                        attributes: None,
                        last_sent: None,
                        pending: false,
                        remove_at: None,
                    },
                );
                self.mark_discovery_dirty(now);
            }
        }
    }

    fn set_state(&mut self, id: EntityId, state: State, now: Instant) {
        let live = self.live();
        let Some(e) = self.entities.get_mut(&id) else {
            tracing::trace!(%id, "state for unknown entity ignored");
            return;
        };
        if e.state.as_ref() == Some(&state) {
            return;
        }
        e.state = Some(state.clone());
        {
            let mut watches = self.watches.lock().unwrap_or_else(|e| e.into_inner());
            watches
                .entry(id.clone())
                .or_insert_with(|| tokio::sync::watch::channel(State::Unknown).0)
                .send_replace(state.clone());
        }
        if !live {
            return;
        }
        if let (Some(last), Some(min)) = (e.last_sent, e.spec.min_interval)
            && now < last + min
        {
            e.pending = true;
            return;
        }
        e.last_sent = Some(now);
        e.pending = false;
        let retain = e.spec.retain;
        self.emit(HubEvent::State { id, state, retain });
    }

    fn handle_transport(&mut self, ev: TransportEvent) {
        match ev {
            TransportEvent::Connected => {
                self.connected = true;
                if self.paused {
                    self.emit(HubEvent::Pause(true));
                } else if self.released {
                    self.discovery_deadline = None;
                    self.emit(HubEvent::Snapshot(self.snapshot()));
                }
            }
            TransportEvent::Disconnected => self.connected = false,
            TransportEvent::HaOnline => {
                if self.live() {
                    self.discovery_deadline = None;
                    self.emit(HubEvent::Snapshot(self.snapshot()));
                }
            }
            TransportEvent::Command { id, value } => self.route_command(id, value),
        }
    }

    fn route_command(&mut self, id: EntityId, value: CommandValue) {
        if self.paused {
            tracing::debug!(%id, "command ignored while paused");
            return;
        }
        if let Some(j) = self.json.get(&id) {
            match value {
                CommandValue::Json(v) => warn_full(&id, j.sink.try_send(v).err()),
                other => {
                    tracing::warn!(%id, ?other, owner = %j.owner, "non-JSON command for JSON endpoint dropped")
                }
            }
            return;
        }
        let Some(sink) = self.entities.get(&id).and_then(|e| e.sink.as_ref()) else {
            tracing::warn!(%id, "command for entity without a command handler dropped");
            return;
        };
        let full = match (sink, value) {
            (CommandSink::Raw(tx), v) => tx.try_send(v).err().map(drop),
            (CommandSink::Unit(tx), CommandValue::Press) => tx.try_send(()).err().map(drop),
            (CommandSink::Bool(tx), CommandValue::Bool(b)) => tx.try_send(b).err().map(drop),
            (CommandSink::Number(tx), CommandValue::Number(n)) => tx.try_send(n).err().map(drop),
            (CommandSink::Text(tx), CommandValue::Text(t)) => tx.try_send(t).err().map(drop),
            (CommandSink::Lock(tx), CommandValue::Lock(l)) => tx.try_send(l).err().map(drop),
            (sink, value) => {
                tracing::warn!(%id, ?value, ?sink, "command of the wrong type dropped");
                return;
            }
        };
        warn_full(&id, full);
    }
}

fn warn_full<E>(id: &EntityId, err: Option<E>) {
    if err.is_some() {
        tracing::warn!(%id, "command dropped: handler is not keeping up or has stopped");
    }
}

async fn recv_opt<T>(rx: &mut Option<mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}
