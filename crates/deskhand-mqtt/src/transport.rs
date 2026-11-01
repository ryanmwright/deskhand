//! The MQTT [`Transport`] implementation.
//!
//! Two tasks:
//! - the *poll task* owns rumqttc's `EventLoop`: it keeps the connection up
//!   (reconnecting with jittered backoff), forwards incoming packets, and
//!   stops reconnecting while the machine sleeps;
//! - the *main loop* ([`MqttTransport::run`]) handles hub events and incoming
//!   messages. It only uses non-blocking `try_*` client calls, so a dead
//!   connection can never stall it.
//!
//! Connection sequence: connect → (first time only) read our retained device
//! document to learn what a previous run published → tell the hub we are
//! connected → the hub sends a snapshot → publish discovery (with removal
//! stubs if needed), availability, states → a second later, the document
//! without stubs.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use deskhand_core::entity::EntityKind;
use deskhand_core::transport::TransportError;
use deskhand_core::transport::{EntityDescriptor, Snapshot};
use deskhand_core::{DeviceInfo, EntityId, HubEvent, Transport, TransportEvent, TransportLink};
use rumqttc::{AsyncClient, Event, EventLoop, LastWill, MqttOptions, Outgoing, Packet, QoS};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::config::{MqttConfig, MqttConfigError};
use crate::discovery::DiscoveryPlanner;
use crate::payload;
use crate::topics::{OFFLINE, ONLINE, Topics};

/// Largest packet in either direction. rumqttc's 10 KiB default silently
/// breaks discovery documents and album art.
pub const MAX_PACKET_SIZE: usize = 4 * 1024 * 1024;

/// Requests queued towards the event loop. Publishing uses `try_publish`, so
/// this bounds memory while disconnected instead of blocking.
const REQUEST_CAPACITY: usize = 2048;

/// Timing knobs.
#[derive(Debug, Clone)]
pub struct MqttTimings {
    /// How long to wait for our retained device document on first connect.
    pub retained_read_timeout: Duration,
    /// Delay between the document with removal stubs and the final one.
    pub stub_delay: Duration,
    /// Upper bound of the random delay before resyncing after HA restarts.
    pub birth_jitter: Duration,
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
}

impl Default for MqttTimings {
    fn default() -> Self {
        Self {
            retained_read_timeout: Duration::from_millis(1500),
            stub_delay: Duration::from_secs(1),
            birth_jitter: Duration::from_secs(5),
            reconnect_min: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(30),
        }
    }
}

/// Publishes the hub's entities to an MQTT broker with HA discovery.
pub struct MqttTransport {
    config: MqttConfig,
    password: Option<String>,
    tls: Option<Arc<rustls::ClientConfig>>,
    device: DeviceInfo,
    topics: Topics,
    planner: DiscoveryPlanner,
    timings: MqttTimings,
}

impl std::fmt::Debug for MqttTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MqttTransport")
            .field("host", &self.config.host)
            .field("port", &self.config.effective_port())
            .field("node_id", &self.device.node_id)
            .finish_non_exhaustive()
    }
}

impl MqttTransport {
    /// Validates the config, resolves the password and prepares TLS.
    /// `disabled_capabilities` are integrations turned off in config: their
    /// previously published entities are removed instead of kept.
    pub fn new(
        config: MqttConfig,
        device: DeviceInfo,
        disabled_capabilities: BTreeSet<String>,
    ) -> Result<Self, MqttConfigError> {
        config.validate()?;
        let password = config.resolve_password()?;
        let tls = if config.tls {
            Some(config.tls_config()?)
        } else {
            None
        };
        let topics = Topics::new(
            &config.topic_prefix,
            &config.discovery_prefix,
            &device.node_id,
        );
        Ok(Self {
            config,
            password,
            tls,
            device,
            topics,
            planner: DiscoveryPlanner::new(disabled_capabilities),
            timings: MqttTimings::default(),
        })
    }

    pub fn with_timings(mut self, timings: MqttTimings) -> Self {
        self.timings = timings;
        self
    }

    pub fn topics(&self) -> &Topics {
        &self.topics
    }

    fn mqtt_options(&self) -> MqttOptions {
        let client_id = self
            .config
            .client_id
            .clone()
            .unwrap_or_else(|| format!("deskhand-{}", self.device.node_id));
        let mut o = self
            .config
            .mqtt_options(client_id, self.password.clone(), self.tls.clone());
        o.set_last_will(LastWill::new(
            self.topics.status(),
            OFFLINE,
            QoS::AtLeastOnce,
            true,
        ));
        o
    }
}

/// What the poll task reports.
#[derive(Debug)]
enum NetEvent {
    Connected,
    Message {
        topic: String,
        payload: Bytes,
        retain: bool,
    },
    Disconnected(String),
}

async fn poll_loop(
    mut eventloop: EventLoop,
    tx: mpsc::UnboundedSender<NetEvent>,
    mut active: watch::Receiver<bool>,
    timings: MqttTimings,
) {
    let mut attempt: u32 = 0;
    loop {
        // The main loop is gone (shutdown or panic): drop the connection so the
        // broker publishes our last will instead of keeping a zombie session alive.
        if tx.is_closed() {
            return;
        }
        // `active` only gates *re*connecting (sleep). Requests already queued,
        // such as the offline status and DISCONNECT sent on sleep, still go out.
        let ev = match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                attempt = 0;
                NetEvent::Connected
            }
            Ok(Event::Incoming(Packet::Publish(p))) => NetEvent::Message {
                topic: p.topic,
                payload: p.payload,
                retain: p.retain,
            },
            Ok(Event::Outgoing(Outgoing::Disconnect)) => {
                // We asked to disconnect (sleep or shutdown): drop the socket now.
                eventloop.clean();
                eventloop.pending.clear();
                if tx
                    .send(NetEvent::Disconnected("disconnect requested".into()))
                    .is_err()
                    || !wait_until_active(&mut active).await
                {
                    return;
                }
                attempt = 0;
                continue;
            }
            Ok(_) => continue,
            Err(e) => {
                // Everything queued is stale: the next connect resends a full snapshot.
                eventloop.pending.clear();
                if tx.send(NetEvent::Disconnected(e.to_string())).is_err() {
                    return;
                }
                if *active.borrow_and_update() {
                    attempt = attempt.saturating_add(1);
                    let delay = backoff(attempt, timings.reconnect_min, timings.reconnect_max);
                    tracing::debug!(error = %e, retry_in = ?delay, "MQTT connection failed");
                    tokio::select! {
                        () = tokio::time::sleep(delay) => {}
                        // Sleep or resume: re-evaluate right away (a resume reconnects now).
                        _ = active.changed() => { attempt = 0; }
                    }
                }
                if !wait_until_active(&mut active).await {
                    return;
                }
                continue;
            }
        };
        if tx.send(ev).is_err() {
            return;
        }
    }
}

/// Waits while the machine sleeps. False once the transport is gone.
async fn wait_until_active(active: &mut watch::Receiver<bool>) -> bool {
    active.wait_for(|a| *a).await.is_ok()
}

/// `min * 2^(n-1)` capped at `max`, with up to 25% random jitter.
fn backoff(attempt: u32, min: Duration, max: Duration) -> Duration {
    let base = min
        .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1).min(16)))
        .min(max);
    base + base.mul_f64(random_fraction() * 0.25)
}

/// A random number in `[0, 1)`, good enough for jitter.
fn random_fraction() -> f64 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    (h.finish() >> 11) as f64 / (1u64 << 53) as f64
}

/// State of the main loop.
struct Session {
    client: AsyncClient,
    active: watch::Sender<bool>,
    topics: Topics,
    device: DeviceInfo,
    planner: DiscoveryPlanner,
    timings: MqttTimings,
    to_hub: mpsc::UnboundedSender<TransportEvent>,

    connected: bool,
    paused: bool,
    /// The previous run's document has been read (or found absent).
    adopted: bool,
    awaiting_retained: Option<Instant>,
    followup: Option<(Instant, Value)>,
    birth_at: Option<Instant>,

    entities: Vec<EntityDescriptor>,
    kinds: HashMap<EntityId, EntityKind>,
    retain: HashMap<EntityId, bool>,
    json_commands: BTreeSet<EntityId>,
    availability: BTreeMap<Arc<str>, bool>,
    /// Field payloads last published per compat media player.
    media_sent: HashMap<EntityId, HashMap<&'static str, Bytes>>,
}

#[async_trait::async_trait]
impl Transport for MqttTransport {
    async fn run(self: Box<Self>, link: TransportLink) -> Result<(), TransportError> {
        let this = *self;
        let (client, eventloop) = AsyncClient::new(this.mqtt_options(), REQUEST_CAPACITY);
        let (net_tx, mut net_rx) = mpsc::unbounded_channel();
        let (active, active_rx) = watch::channel(true);
        let poll = AbortOnDrop(tokio::spawn(poll_loop(
            eventloop,
            net_tx,
            active_rx,
            this.timings.clone(),
        )));
        tracing::info!(
            host = %this.config.host,
            port = this.config.effective_port(),
            tls = this.config.tls,
            node_id = %this.device.node_id,
            "connecting to MQTT broker"
        );

        let TransportLink { mut events, to_hub } = link;
        let mut s = Session {
            client,
            active,
            topics: this.topics,
            device: this.device,
            planner: this.planner,
            timings: this.timings,
            to_hub,
            connected: false,
            paused: false,
            adopted: false,
            awaiting_retained: None,
            followup: None,
            birth_at: None,
            entities: Vec::new(),
            kinds: HashMap::new(),
            retain: HashMap::new(),
            json_commands: BTreeSet::new(),
            availability: BTreeMap::new(),
            media_sent: HashMap::new(),
        };

        loop {
            let deadline = [
                s.awaiting_retained,
                s.followup.as_ref().map(|f| f.0),
                s.birth_at,
            ]
            .into_iter()
            .flatten()
            .min();
            tokio::select! {
                ev = events.recv() => match ev {
                    Some(HubEvent::Shutdown) | None => break,
                    Some(ev) => s.on_hub_event(ev),
                },
                Some(ev) = net_rx.recv() => s.on_net_event(ev),
                () = sleep_until_opt(deadline) => s.on_timer(Instant::now()),
            }
        }

        // Shutdown: say goodbye explicitly (the LWT only covers crashes).
        tracing::info!("disconnecting from MQTT broker");
        s.publish_status(OFFLINE);
        let _ = s.active.send(false);
        let _ = s.client.try_disconnect();
        if s.connected {
            let wait = async {
                while let Some(ev) = net_rx.recv().await {
                    if matches!(ev, NetEvent::Disconnected(_)) {
                        break;
                    }
                }
            };
            let _ = tokio::time::timeout(Duration::from_secs(2), wait).await;
        }
        drop(poll);
        Ok(())
    }
}

impl Session {
    fn publish(&self, topic: String, payload: impl Into<Vec<u8>>, retain: bool, qos: QoS) {
        if let Err(e) = self.client.try_publish(topic, qos, retain, payload) {
            tracing::warn!(error = %e, "dropping MQTT publish: request queue full or closed");
        }
    }

    fn publish_status(&self, status: &str) {
        self.publish(self.topics.status(), status, true, QoS::AtLeastOnce);
    }

    fn report(&self, ev: TransportEvent) {
        let _ = self.to_hub.send(ev);
    }

    fn on_net_event(&mut self, ev: NetEvent) {
        match ev {
            NetEvent::Connected => {
                tracing::info!("connected to MQTT broker");
                self.connected = true;
                self.subscribe(self.topics.command_filter());
                for filter in self.topics.media_command_filters() {
                    self.subscribe(filter);
                }
                self.subscribe(self.topics.ha_status());
                if self.adopted {
                    self.report(TransportEvent::Connected);
                } else {
                    // Before the device document, so its retained copy arrives first.
                    self.subscribe(self.topics.media_player_config());
                    self.subscribe(self.topics.device_config());
                    self.awaiting_retained =
                        Some(Instant::now() + self.timings.retained_read_timeout);
                }
            }
            NetEvent::Disconnected(reason) => {
                if self.connected {
                    tracing::warn!(%reason, "disconnected from MQTT broker");
                    self.connected = false;
                    self.awaiting_retained = None;
                    self.followup = None;
                    self.birth_at = None;
                    self.media_sent.clear();
                    self.report(TransportEvent::Disconnected);
                }
            }
            NetEvent::Message {
                topic,
                payload,
                retain,
            } => self.on_message(&topic, &payload, retain),
        }
    }

    fn subscribe(&self, filter: String) {
        if let Err(e) = self.client.try_subscribe(filter, QoS::AtLeastOnce) {
            tracing::warn!(error = %e, "MQTT subscribe failed");
        }
    }

    fn on_message(&mut self, topic: &str, payload: &[u8], retain: bool) {
        if topic == self.topics.media_player_config() {
            if self.awaiting_retained.is_some()
                && let Ok(previous) = serde_json::from_slice::<Value>(payload)
            {
                self.planner.adopt_previous_media(&previous);
            }
            return;
        }
        if topic == self.topics.device_config() {
            if self.awaiting_retained.is_some() {
                if !payload.is_empty() {
                    match serde_json::from_slice::<Value>(payload) {
                        Ok(previous) => self.planner.adopt_previous(&self.topics, &previous),
                        Err(e) => {
                            tracing::warn!(error = %e, "ignoring unparsable previous discovery document")
                        }
                    }
                }
                self.finish_retained_read();
            }
            return;
        }
        if topic == self.topics.ha_status() {
            if payload == ONLINE.as_bytes() && !retain {
                let jitter = self.timings.birth_jitter.mul_f64(random_fraction());
                tracing::info!(resync_in = ?jitter, "Home Assistant came online");
                self.birth_at = Some(Instant::now() + jitter);
            }
            return;
        }
        let media = self.topics.id_from_media_command(topic);
        let Some(id) = media
            .as_ref()
            .map(|(id, _)| id.clone())
            .or_else(|| self.topics.id_from_command(topic))
        else {
            return;
        };
        if retain {
            // A retained command would fire on every reconnect; never act on one.
            tracing::warn!(%topic, "ignoring retained command; clear it with an empty retained publish");
            return;
        }
        if self.paused {
            return;
        }
        let value = if let Some((_, command)) = media {
            match self.kinds.get(&id) {
                Some(EntityKind::MediaPlayer) => payload::decode_media_command(command, payload),
                _ => return,
            }
        } else if self.json_commands.contains(&id) {
            payload::decode_json(payload)
        } else if let Some(kind) = self.kinds.get(&id) {
            payload::decode_command(kind, payload)
        } else {
            tracing::debug!(%id, "command for unknown entity");
            return;
        };
        match value {
            Some(value) => self.report(TransportEvent::Command { id, value }),
            None => {
                tracing::warn!(%id, payload = %String::from_utf8_lossy(payload), "invalid command payload")
            }
        }
    }

    fn finish_retained_read(&mut self) {
        self.awaiting_retained = None;
        self.adopted = true;
        for topic in [
            self.topics.device_config(),
            self.topics.media_player_config(),
        ] {
            if let Err(e) = self.client.try_unsubscribe(topic) {
                tracing::debug!(error = %e, "unsubscribe failed");
            }
        }
        self.report(TransportEvent::Connected);
    }

    fn on_timer(&mut self, now: Instant) {
        if self.awaiting_retained.is_some_and(|t| t <= now) {
            tracing::debug!("no previous discovery document found");
            self.finish_retained_read();
        }
        if self.followup.as_ref().is_some_and(|(t, _)| *t <= now)
            && let Some((_, doc)) = self.followup.take()
            && self.connected
            && !self.paused
        {
            self.publish(
                self.topics.device_config(),
                doc.to_string(),
                true,
                QoS::AtLeastOnce,
            );
        }
        if self.birth_at.is_some_and(|t| t <= now) {
            self.birth_at = None;
            self.report(TransportEvent::HaOnline);
        }
    }

    fn set_entities(&mut self, entities: Vec<EntityDescriptor>, json: Vec<EntityId>) {
        self.kinds = entities
            .iter()
            .map(|d| (d.spec.id.clone(), d.spec.kind.clone()))
            .collect();
        self.retain = entities
            .iter()
            .map(|d| (d.spec.id.clone(), d.spec.retain))
            .collect();
        self.entities = entities;
        self.json_commands = json.into_iter().collect();
    }

    fn live_capabilities(&self) -> BTreeSet<String> {
        self.availability
            .keys()
            .map(|c| c.to_string())
            .chain(self.entities.iter().map(|d| d.owner.to_string()))
            .collect()
    }

    fn publish_discovery(&mut self) {
        let live = self.live_capabilities();
        let rendered = self
            .planner
            .render(&self.topics, &self.device, &self.entities, &live);
        if !rendered.removed.is_empty() {
            tracing::info!(removed = ?rendered.removed, "removing stale entities from Home Assistant");
        }
        self.publish(
            self.topics.device_config(),
            rendered.first.to_string(),
            true,
            QoS::AtLeastOnce,
        );
        self.followup = rendered
            .followup
            .map(|doc| (Instant::now() + self.timings.stub_delay, doc));
        if let Some(doc) =
            self.planner
                .render_media(&self.topics, &self.device, &self.entities, &live)
        {
            self.publish(
                self.topics.media_player_config(),
                doc,
                true,
                QoS::AtLeastOnce,
            );
        }
    }

    fn publish_snapshot(&mut self, snap: Snapshot) {
        self.availability = snap.availability;
        self.set_entities(snap.entities, snap.json_commands);
        if !self.connected || self.paused {
            return;
        }
        self.publish_discovery();
        self.publish_status(ONLINE);
        for (cap, available) in &self.availability {
            self.publish_capability(cap, *available);
        }
        for cap in self.planner.kept_capabilities() {
            if !self.availability.contains_key(cap.as_str()) {
                self.publish_capability(&cap, false);
            }
        }
        self.media_sent.clear();
        for (id, state) in &snap.states {
            self.publish_state(id, state, None);
        }
        for (id, attrs) in &snap.attributes {
            self.publish(
                self.topics.attributes(id),
                Value::Object(attrs.clone()).to_string(),
                true,
                QoS::AtMostOnce,
            );
        }
        tracing::debug!(entities = self.entities.len(), "published snapshot");
    }

    fn publish_capability(&self, capability: &str, available: bool) {
        self.publish(
            self.topics.capability_status(capability),
            if available { ONLINE } else { OFFLINE },
            true,
            QoS::AtLeastOnce,
        );
    }

    fn publish_state(&mut self, id: &EntityId, state: &deskhand_core::State, retain: Option<bool>) {
        let Some(kind) = self.kinds.get(id) else {
            return;
        };
        if matches!(kind, EntityKind::MediaPlayer) {
            self.publish_media(id, state);
            return;
        }
        let retain = retain.unwrap_or_else(|| self.retain.get(id).copied().unwrap_or(true));
        self.publish(
            self.topics.state(id),
            payload::encode_state(kind, state),
            retain,
            QoS::AtMostOnce,
        );
    }

    /// Publishes the compat media player's fields that changed. The position
    /// is resent whenever the state changes, because HA extrapolates it from
    /// the time it was received.
    fn publish_media(&mut self, id: &EntityId, state: &deskhand_core::State) {
        let mut sent = self.media_sent.remove(id).unwrap_or_default();
        let fields = payload::media_fields(state);
        let state_changed = fields
            .iter()
            .any(|(f, v)| *f == "state" && sent.get(f) != Some(v));
        for (field, value) in fields {
            if sent.get(field) == Some(&value) && !(state_changed && field == "position") {
                continue;
            }
            let topic = if field == "state" {
                self.topics.state(id)
            } else {
                self.topics.media_field(id, field)
            };
            self.publish(topic, value.clone(), true, QoS::AtMostOnce);
            sent.insert(field, value);
        }
        self.media_sent.insert(id.clone(), sent);
    }

    fn on_hub_event(&mut self, ev: HubEvent) {
        let live = self.connected && !self.paused;
        match ev {
            HubEvent::Snapshot(snap) => self.publish_snapshot(snap),
            HubEvent::Discovery {
                entities,
                json_commands,
            } => {
                self.set_entities(entities, json_commands);
                if live {
                    self.publish_discovery();
                }
            }
            HubEvent::State { id, state, retain } => {
                if live {
                    self.publish_state(&id, &state, Some(retain));
                }
            }
            HubEvent::Attributes { id, attributes } => {
                if live {
                    self.publish(
                        self.topics.attributes(&id),
                        Value::Object(attributes).to_string(),
                        true,
                        QoS::AtMostOnce,
                    );
                }
            }
            HubEvent::Event {
                id,
                event_type,
                attributes,
            } => {
                if live {
                    self.publish(
                        self.topics.state(&id),
                        payload::encode_event(&event_type, &attributes),
                        false,
                        QoS::AtLeastOnce,
                    );
                }
            }
            HubEvent::Availability {
                capability,
                available,
            } => {
                self.availability.insert(capability.clone(), available);
                if live {
                    self.publish_capability(&capability, available);
                }
            }
            HubEvent::Pause(paused) => {
                self.paused = paused;
                if paused && self.connected {
                    self.publish_status(OFFLINE);
                }
                // Unpausing is followed by a snapshot, which publishes online.
            }
            HubEvent::Sleep => {
                tracing::info!("machine going to sleep; disconnecting");
                if self.connected {
                    self.publish_status(OFFLINE);
                }
                let _ = self.active.send(false);
                let _ = self.client.try_disconnect();
            }
            HubEvent::Resume => {
                tracing::info!("machine resumed; reconnecting");
                let _ = self.active.send(true);
            }
            HubEvent::Shutdown => {}
            other => tracing::debug!(?other, "unhandled hub event"),
        }
    }
}

/// Aborts the task when dropped, so the poll task never outlives the transport.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_with_bounded_jitter() {
        let (min, max) = (Duration::from_secs(1), Duration::from_secs(30));
        for attempt in 1..=10 {
            let d = backoff(attempt, min, max);
            let base = min.saturating_mul(2u32.pow(attempt - 1)).min(max);
            assert!(d >= base && d <= base.mul_f64(1.25), "{attempt}: {d:?}");
        }
    }

    #[test]
    fn random_fraction_is_in_range() {
        for _ in 0..100 {
            let f = random_fraction();
            assert!((0.0..1.0).contains(&f));
        }
    }
}
