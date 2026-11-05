//! Peripheral batteries (keyboards, mice, headsets, ...) from any number of
//! sources, each peripheral its own Home Assistant device.
//!
//! A source (UPower on Linux, Logitech HID++, ...) implements
//! [`PeripheralSource`] and reports [`PeripheralReading`]s through a
//! [`PeripheralSink`]. The `peripheral_battery` integration
//! ([`PeripheralBattery`]) runs every enabled source on its own, with its own
//! restarts, merges what the sources report for the same physical device and
//! owns the entities:
//!
//! | entity | kind | |
//! |---|---|---|
//! | `battery_<key>` | sensor, % | attributes: kind, state, source, level, estimated, slot, encrypted |
//! | `battery_<key>_state` | enum sensor | charging, discharging, full, ... |
//! | `battery_<key>_low` | binary sensor (battery) | the device's own flag or level, else ≤ 15 % |
//! | `battery_<key>_connected` | binary sensor (connectivity) | |
//! | `battery_<key>_updated` | timestamp (diagnostic) | last successful battery reading |
//! | `<receiver>_paired` | sensor (diagnostic), on the receiver's device | attribute `slots` |
//!
//! `<key>` is the slug of the model name (else the serial), the same rule
//! for every source, so a device seen by two sources is one device. If the
//! names differ, a matching serial joins them. Sources earlier in the list
//! win while their reading says connected.
//!
//! A sleeping or disconnected peripheral keeps its entities and last values;
//! only `connected` turns off. Entities go when no source reports the device
//! any more (after the hub's grace period).
//!
//! ```toml
//! [integrations.peripheral_battery.sources]
//! upower = "auto"   # "auto" | true | false, per source
//! hidpp = "auto"
//!
//! [integrations.peripheral_battery]
//! battery_refresh = "off"  # or "30m": re-read connected devices of sources
//!                          # that otherwise only listen for changes (HID++)
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use deskhand_core::ctx::{BinarySensor, Sensor};
use deskhand_core::entity::{BinarySensorClass, SensorClass, StateClass, SubDevice};
use deskhand_core::hub::Lifecycle;
use deskhand_core::{
    Attributes, Ctx, Enabled, EntityId, EntitySpec, Integration, IntegrationError, Probe,
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::time::rfc3339;

pub const CAPABILITY: &str = "peripheral_battery";

/// Below this the battery counts as low when the device says nothing itself.
const LOW_PERCENT: f64 = 15.0;
/// How long `ready()` waits for every source's first report.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// How often an unavailable source is probed again (also on resume).
const REPROBE: Duration = Duration::from_secs(300);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A run that lasted this long resets the backoff.
const HEALTHY_RUN: Duration = Duration::from_secs(60);

/// Charging state, in UPower's vocabulary plus `error`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ChargeState {
    #[default]
    Unknown,
    Charging,
    Discharging,
    Empty,
    Full,
    PendingCharge,
    PendingDischarge,
    /// The device reports a charging fault.
    Error,
}

impl ChargeState {
    pub const ALL: [Self; 8] = [
        Self::Unknown,
        Self::Charging,
        Self::Discharging,
        Self::Empty,
        Self::Full,
        Self::PendingCharge,
        Self::PendingDischarge,
        Self::Error,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Charging => "charging",
            Self::Discharging => "discharging",
            Self::Empty => "empty",
            Self::Full => "full",
            Self::PendingCharge => "pending_charge",
            Self::PendingDischarge => "pending_discharge",
            Self::Error => "error",
        }
    }
}

/// A coarse battery level, for devices that report one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BatteryLevel {
    Critical,
    Low,
    Normal,
    Full,
}

impl BatteryLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Low => "low",
            Self::Normal => "normal",
            Self::Full => "full",
        }
    }
}

/// How a peripheral is connected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Link {
    /// Key of the receiver it is paired with ([`ReceiverReading::key`]).
    pub receiver: Option<EntityId>,
    pub slot: Option<u8>,
    pub encrypted: Option<bool>,
}

/// What one source knows about one peripheral.
#[derive(Debug, Clone, PartialEq)]
pub struct PeripheralReading {
    /// See [`PeripheralReading::key_for`].
    pub key: EntityId,
    pub name: String,
    /// `keyboard`, `mouse`, `headset`, ...
    pub kind: Option<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub model_id: Option<String>,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub link: Link,
    pub percentage: Option<f64>,
    /// The percentage is estimated (from a voltage or a level).
    pub estimated: bool,
    pub level: Option<BatteryLevel>,
    pub state: ChargeState,
    /// The device's own low-battery flag.
    pub low: Option<bool>,
    pub connected: bool,
    /// When the battery was last read successfully.
    pub read_at: Option<SystemTime>,
}

impl PeripheralReading {
    pub fn new(key: EntityId, name: impl Into<String>) -> Self {
        Self {
            key,
            name: name.into(),
            kind: None,
            manufacturer: None,
            model: None,
            model_id: None,
            serial: None,
            firmware: None,
            link: Link::default(),
            percentage: None,
            estimated: false,
            level: None,
            state: ChargeState::Unknown,
            low: None,
            connected: true,
            read_at: None,
        }
    }

    /// The key every source uses: the model name, else the serial, else
    /// `fallback` (a device path), slugified.
    pub fn key_for(model: &str, serial: &str, fallback: &str) -> EntityId {
        let text = [model, serial]
            .into_iter()
            .map(str::trim)
            .find(|s| !s.is_empty())
            .unwrap_or(fallback);
        EntityId::slugify(text)
    }

    /// Whether the battery is low: the device's flag, else its level, else
    /// the percentage against 15 %.
    fn is_low(&self) -> Option<bool> {
        self.low
            .or_else(|| {
                self.level
                    .map(|l| matches!(l, BatteryLevel::Critical | BatteryLevel::Low))
            })
            .or_else(|| self.percentage.map(|p| p <= LOW_PERCENT))
    }
}

/// A receiver (Logi Bolt, Unifying, ...) and what is paired to it.
#[derive(Debug, Clone, PartialEq)]
pub struct ReceiverReading {
    pub key: EntityId,
    pub name: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub slots: Vec<PairedSlot>,
}

/// One pairing slot of a receiver.
#[derive(Debug, Clone, PartialEq)]
pub struct PairedSlot {
    pub slot: u8,
    pub name: String,
    pub model_id: Option<String>,
    pub online: bool,
}

enum Msg {
    Update(&'static str, PeripheralReading),
    Remove(&'static str, EntityId),
    Receiver(&'static str, ReceiverReading),
    RemoveReceiver(&'static str, EntityId),
    /// The first full report is done, or the source is unavailable.
    Settled(&'static str),
    /// The source stopped: forget everything it reported.
    Stopped(&'static str),
}

/// Where a source reports. Cheap to clone; usable from any thread.
#[derive(Clone)]
pub struct PeripheralSink {
    source: &'static str,
    tx: mpsc::UnboundedSender<Msg>,
    battery_refresh: Option<Duration>,
}

impl std::fmt::Debug for PeripheralSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeripheralSink")
            .field("source", &self.source)
            .finish()
    }
}

impl PeripheralSink {
    fn send(&self, msg: Msg) {
        // The integration only goes away when it stops; reports after that are moot.
        let _ = self.tx.send(msg);
    }

    /// Adds or updates a peripheral.
    pub fn update(&self, reading: PeripheralReading) {
        self.send(Msg::Update(self.source, reading));
    }

    /// This source no longer sees the peripheral (unpaired, unplugged).
    pub fn remove(&self, key: &EntityId) {
        self.send(Msg::Remove(self.source, key.clone()));
    }

    pub fn receiver(&self, reading: ReceiverReading) {
        self.send(Msg::Receiver(self.source, reading));
    }

    pub fn remove_receiver(&self, key: &EntityId) {
        self.send(Msg::RemoveReceiver(self.source, key.clone()));
    }

    /// Call once the first full report is done (releases the startup barrier).
    pub fn ready(&self) {
        self.send(Msg::Settled(self.source));
    }

    /// `battery_refresh`: how often to re-read connected devices, for
    /// sources that otherwise only listen for changes. `None` (the default):
    /// never.
    pub fn battery_refresh(&self) -> Option<Duration> {
        self.battery_refresh
    }
}

/// One way of finding peripheral batteries.
#[async_trait::async_trait]
pub trait PeripheralSource: Send + Sync + 'static {
    /// Short id, also the key under `[integrations.peripheral_battery.sources]`.
    fn id(&self) -> &'static str;
    async fn probe(&self) -> Probe;
    /// Reports until `cancel` (return `Ok`) or a failure (`Err`, retried
    /// with backoff). Everything the source reported is dropped when it
    /// returns, unless another source reports the same device.
    async fn run(
        &self,
        sink: PeripheralSink,
        cancel: CancellationToken,
    ) -> Result<(), IntegrationError>;
}

/// `[integrations.peripheral_battery]` options.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PeripheralOptions {
    /// Per source id: `auto` (default), `true` or `false`.
    pub sources: BTreeMap<String, Enabled>,
    /// `"off"` (default) or a duration such as `"30m"`.
    #[serde(deserialize_with = "off_or_duration")]
    pub battery_refresh: Option<Duration>,
}

/// `"off"` → `None`; otherwise a humantime duration (at least a minute).
fn off_or_duration<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    let text = String::deserialize(d)?;
    if text.trim() == "off" {
        return Ok(None);
    }
    let every = humantime_serde::re::humantime::parse_duration(text.trim())
        .map_err(serde::de::Error::custom)?;
    if every < Duration::from_secs(60) {
        return Err(serde::de::Error::custom(
            "battery_refresh must be at least 1m",
        ));
    }
    Ok(Some(every))
}

/// The `peripheral_battery` integration. Sources earlier in the list win
/// when two report the same device.
pub struct PeripheralBattery {
    sources: Vec<Arc<dyn PeripheralSource>>,
}

impl std::fmt::Debug for PeripheralBattery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ids: Vec<&str> = self.sources.iter().map(|s| s.id()).collect();
        f.debug_struct("PeripheralBattery")
            .field("sources", &ids)
            .finish()
    }
}

impl PeripheralBattery {
    pub fn new(sources: Vec<Arc<dyn PeripheralSource>>) -> Self {
        Self { sources }
    }
}

#[async_trait::async_trait]
impl Integration for PeripheralBattery {
    fn id(&self) -> &'static str {
        "peripheral_battery.merged"
    }

    fn capability(&self) -> &'static str {
        CAPABILITY
    }

    fn description(&self) -> &'static str {
        "Keyboard, mouse and headset batteries, each its own device"
    }

    async fn probe(&self) -> Probe {
        let mut available = Vec::new();
        let mut reasons = Vec::new();
        for s in &self.sources {
            match s.probe().await {
                Probe::Available { detail } => available.push(match detail {
                    Some(d) => format!("{} ({d})", s.id()),
                    None => s.id().to_owned(),
                }),
                Probe::Unavailable { reason } => reasons.push(format!("{}: {reason}", s.id())),
            }
        }
        if available.is_empty() {
            return Probe::unavailable(if reasons.is_empty() {
                "no sources".to_owned()
            } else {
                reasons.join("; ")
            });
        }
        let mut detail = format!("sources: {}", available.join(", "));
        if !reasons.is_empty() {
            detail.push_str(&format!("; not available: {}", reasons.join("; ")));
        }
        Probe::available_with(detail)
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let options = ctx.config::<PeripheralOptions>()?;
        for id in options.sources.keys() {
            if !self.sources.iter().any(|s| s.id() == id) {
                tracing::warn!(source = %id, "unknown peripheral battery source in config");
            }
        }
        let cancel = ctx.cancelled();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut pending: BTreeSet<&'static str> = BTreeSet::new();
        let mut order = Vec::new();
        for source in &self.sources {
            let enabled = options
                .sources
                .get(source.id())
                .copied()
                .unwrap_or_default();
            if enabled == Enabled::Off {
                tracing::info!(source = source.id(), "peripheral battery source disabled");
                continue;
            }
            order.push(source.id());
            pending.insert(source.id());
            let sink = PeripheralSink {
                source: source.id(),
                tx: tx.clone(),
                battery_refresh: options.battery_refresh,
            };
            tokio::spawn(supervise(
                source.clone(),
                enabled,
                sink,
                cancel.child_token(),
                ctx.lifecycle(),
            ));
        }
        drop(tx);

        let mut kit = Kit::new(order);
        let ready_at = tokio::time::Instant::now() + READY_TIMEOUT;
        let mut ready = false;
        if pending.is_empty() {
            ctx.ready();
            ready = true;
        }
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                () = tokio::time::sleep_until(ready_at), if !ready => {
                    tracing::debug!(waiting = ?pending, "peripheral sources not ready in time");
                    ctx.ready();
                    ready = true;
                }
                msg = rx.recv() => {
                    let Some(msg) = msg else { return Ok(()) };
                    if let Msg::Settled(source) = &msg {
                        pending.remove(source);
                    }
                    kit.apply(&ctx, msg);
                    if !ready && pending.is_empty() {
                        ctx.ready();
                        ready = true;
                    }
                }
            }
        }
    }
}

/// Runs one source: probe, run, restart with backoff; re-probe an
/// unavailable source every 5 minutes and on resume.
async fn supervise(
    source: Arc<dyn PeripheralSource>,
    enabled: Enabled,
    sink: PeripheralSink,
    cancel: CancellationToken,
    mut lifecycle: broadcast::Receiver<Lifecycle>,
) {
    let id = source.id();
    let mut backoff = BACKOFF_MIN;
    let mut warned = false;
    loop {
        match source.probe().await {
            Probe::Available { .. } => {
                warned = false;
                tracing::info!(source = id, "peripheral battery source started");
                let started = tokio::time::Instant::now();
                let result = source.run(sink.clone(), cancel.child_token()).await;
                sink.send(Msg::Stopped(id));
                if cancel.is_cancelled() {
                    return;
                }
                match result {
                    Ok(()) => tracing::info!(source = id, "peripheral battery source stopped"),
                    Err(e) => {
                        tracing::warn!(source = id, error = %e, "peripheral battery source failed")
                    }
                }
                if started.elapsed() >= HEALTHY_RUN {
                    backoff = BACKOFF_MIN;
                }
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
            Probe::Unavailable { reason } => {
                if enabled == Enabled::On && !warned {
                    tracing::warn!(source = id, %reason, "peripheral battery source enabled but unavailable");
                } else {
                    tracing::debug!(source = id, %reason, "peripheral battery source unavailable");
                }
                warned = true;
                sink.ready();
                let wait = tokio::time::sleep(REPROBE);
                tokio::pin!(wait);
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => return,
                        () = &mut wait => break,
                        ev = lifecycle.recv() => match ev {
                            Ok(Lifecycle::Resume) => break,
                            Err(broadcast::error::RecvError::Closed) => return,
                            _ => {}
                        },
                    }
                }
            }
        }
    }
}

/// The entities of one peripheral.
struct PeripheralEntities {
    device: Arc<SubDevice>,
    battery: Sensor,
    state: Sensor,
    low: BinarySensor,
    connected: BinarySensor,
    updated: Sensor,
}

impl PeripheralEntities {
    fn register(ctx: &Ctx, key: &EntityId, device: Arc<SubDevice>) -> Self {
        let id = battery_id(key);
        let on = |spec: EntitySpec| spec.on_device(device.clone());
        Self {
            battery: ctx.sensor(on(EntitySpec::sensor(id.clone(), "Battery")
                .sensor_class(SensorClass::Battery)
                .unit("%")
                .state_class(StateClass::Measurement)
                .precision(0))),
            state: ctx.sensor(on(EntitySpec::sensor(id.join("state"), "Battery state")
                .enum_options(
                    ChargeState::ALL
                        .iter()
                        .map(|s| s.as_str().to_owned())
                        .collect(),
                )
                .icon("mdi:battery-charging"))),
            low: ctx.binary_sensor(on(EntitySpec::binary_sensor(
                id.join("low"),
                "Battery low",
                Some(BinarySensorClass::Battery),
            ))),
            connected: ctx.binary_sensor(on(EntitySpec::binary_sensor(
                id.join("connected"),
                "Connected",
                Some(BinarySensorClass::Connectivity),
            ))),
            updated: ctx.sensor(on(EntitySpec::sensor(
                id.join("updated"),
                "Battery updated",
            )
            .sensor_class(SensorClass::Timestamp)
            .diagnostic())),
            device,
        }
    }

    fn publish(&self, r: &PeripheralReading, source: &str) {
        self.battery.set(r.percentage.map(f64::round));
        let mut attrs = Attributes::new();
        if let Some(kind) = &r.kind {
            attrs.insert("kind".into(), json!(kind));
        }
        attrs.insert("state".into(), json!(r.state.as_str()));
        attrs.insert("source".into(), json!(source));
        if let Some(level) = r.level {
            attrs.insert("level".into(), json!(level.as_str()));
        }
        if r.estimated {
            attrs.insert("estimated".into(), json!(true));
        }
        if let Some(slot) = r.link.slot {
            attrs.insert("slot".into(), json!(slot));
        }
        if let Some(encrypted) = r.link.encrypted {
            attrs.insert("encrypted".into(), json!(encrypted));
        }
        self.battery.set_attributes(attrs);
        self.state.set(r.state.as_str());
        match r.is_low() {
            Some(low) => self.low.set(low),
            None => self.low.set_unknown(),
        }
        self.connected.set(r.connected);
        self.updated.set(r.read_at.map(rfc3339));
    }
}

/// `battery_<key>`.
pub fn battery_id(key: &EntityId) -> EntityId {
    EntityId::from_static("battery").join(key.as_str())
}

struct ReceiverEntities {
    device: Arc<SubDevice>,
    paired: Sensor,
}

/// Merges the sources' reports and keeps the entities in step.
struct Kit {
    /// Source ids, highest priority first.
    order: Vec<&'static str>,
    /// Merged key → source → reading.
    readings: BTreeMap<EntityId, BTreeMap<&'static str, PeripheralReading>>,
    /// (source, the key it reported) → merged key, for serial matches.
    aliases: BTreeMap<(&'static str, EntityId), EntityId>,
    receivers: BTreeMap<EntityId, BTreeMap<&'static str, ReceiverReading>>,
    entities: BTreeMap<EntityId, PeripheralEntities>,
    receiver_entities: BTreeMap<EntityId, ReceiverEntities>,
}

impl Kit {
    fn new(order: Vec<&'static str>) -> Self {
        Self {
            order,
            readings: BTreeMap::new(),
            aliases: BTreeMap::new(),
            receivers: BTreeMap::new(),
            entities: BTreeMap::new(),
            receiver_entities: BTreeMap::new(),
        }
    }

    fn rank(&self, source: &str) -> usize {
        self.order
            .iter()
            .position(|s| *s == source)
            .unwrap_or(usize::MAX)
    }

    /// The merged key for a reading: its own key if known, else the key of
    /// a device with the same serial reported by another source.
    fn resolve(&mut self, source: &'static str, r: &PeripheralReading) -> EntityId {
        if let Some(key) = self.aliases.get(&(source, r.key.clone())) {
            return key.clone();
        }
        let key = if self.readings.contains_key(&r.key) {
            r.key.clone()
        } else {
            r.serial
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .and_then(|serial| {
                    self.readings.iter().find_map(|(k, by_source)| {
                        by_source
                            .values()
                            .any(|o| o.serial.as_deref() == Some(serial))
                            .then(|| k.clone())
                    })
                })
                .unwrap_or_else(|| r.key.clone())
        };
        self.aliases.insert((source, r.key.clone()), key.clone());
        key
    }

    fn apply(&mut self, ctx: &Ctx, msg: Msg) {
        match msg {
            Msg::Update(source, r) => {
                let key = self.resolve(source, &r);
                self.readings
                    .entry(key.clone())
                    .or_default()
                    .insert(source, r);
                self.refresh(ctx, &key);
            }
            Msg::Remove(source, reported) => {
                let key = self
                    .aliases
                    .remove(&(source, reported.clone()))
                    .unwrap_or(reported);
                if let Some(by_source) = self.readings.get_mut(&key) {
                    by_source.remove(source);
                }
                self.refresh(ctx, &key);
            }
            Msg::Receiver(source, r) => {
                let key = r.key.clone();
                self.receivers
                    .entry(key.clone())
                    .or_default()
                    .insert(source, r);
                self.refresh_receiver(ctx, &key);
                // Peripherals linked to it can now point at its device.
                let linked: Vec<EntityId> = self
                    .readings
                    .iter()
                    .filter(|(_, rs)| rs.values().any(|r| r.link.receiver.as_ref() == Some(&key)))
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in linked {
                    self.refresh(ctx, &k);
                }
            }
            Msg::RemoveReceiver(source, key) => {
                if let Some(by_source) = self.receivers.get_mut(&key) {
                    by_source.remove(source);
                }
                self.refresh_receiver(ctx, &key);
            }
            Msg::Settled(_) => {}
            Msg::Stopped(source) => {
                let keys: Vec<EntityId> = self.readings.keys().cloned().collect();
                for key in keys {
                    if let Some(by_source) = self.readings.get_mut(&key)
                        && by_source.remove(source).is_some()
                    {
                        self.refresh(ctx, &key);
                    }
                }
                self.aliases.retain(|(s, _), _| *s != source);
                let keys: Vec<EntityId> = self.receivers.keys().cloned().collect();
                for key in keys {
                    if let Some(by_source) = self.receivers.get_mut(&key)
                        && by_source.remove(source).is_some()
                    {
                        self.refresh_receiver(ctx, &key);
                    }
                }
            }
        }
    }

    /// The winning reading (first connected source in priority order, else
    /// the first source) with identity gaps filled from the others.
    fn merged(&self, key: &EntityId) -> Option<(&'static str, PeripheralReading)> {
        let by_source = self.readings.get(key)?;
        let mut ranked: Vec<(&'static str, &PeripheralReading)> =
            by_source.iter().map(|(s, r)| (*s, r)).collect();
        ranked.sort_by_key(|(s, r)| (!r.connected, self.rank(s)));
        let (source, winner) = ranked.first()?;
        let mut m = (*winner).clone();
        for (_, other) in &ranked[1..] {
            fill(&mut m.kind, &other.kind);
            fill(&mut m.manufacturer, &other.manufacturer);
            fill(&mut m.model, &other.model);
            fill(&mut m.model_id, &other.model_id);
            fill(&mut m.serial, &other.serial);
            fill(&mut m.firmware, &other.firmware);
            if m.link.receiver.is_none() {
                m.link = other.link.clone();
            }
        }
        Some((source, m))
    }

    fn sub_device(&self, key: &EntityId, r: &PeripheralReading) -> SubDevice {
        SubDevice {
            manufacturer: r.manufacturer.clone(),
            model: r.model.clone().or_else(|| Some(r.name.clone())),
            model_id: r.model_id.clone(),
            serial_number: r.serial.clone(),
            sw_version: r.firmware.clone(),
            // Only link to a receiver whose device exists.
            via: r
                .link
                .receiver
                .clone()
                .filter(|rk| self.receiver_entities.contains_key(rk)),
            ..SubDevice::new(key.clone(), r.name.clone())
        }
    }

    fn refresh(&mut self, ctx: &Ctx, key: &EntityId) {
        let Some((source, r)) = self.merged(key) else {
            self.readings.remove(key);
            if self.entities.remove(key).is_some() {
                tracing::info!(%key, "peripheral gone");
            }
            return;
        };
        let device = self.sub_device(key, &r);
        let stale = self.entities.get(key).is_none_or(|e| *e.device != device);
        if stale {
            // New handles first, then drop the old ones: the hub sees a
            // re-registration, not a removal.
            if !self.entities.contains_key(key) {
                tracing::info!(%key, name = %r.name, source, "peripheral appeared");
            }
            let entities = PeripheralEntities::register(ctx, key, Arc::new(device));
            self.entities.insert(key.clone(), entities);
        }
        if let Some(e) = self.entities.get(key) {
            e.publish(&r, source);
        }
    }

    fn refresh_receiver(&mut self, ctx: &Ctx, key: &EntityId) {
        let r = self.receivers.get(key).and_then(|by_source| {
            by_source
                .iter()
                .min_by_key(|(s, _)| self.rank(s))
                .map(|(_, r)| r.clone())
        });
        let Some(r) = r else {
            self.receivers.remove(key);
            self.receiver_entities.remove(key);
            return;
        };
        let device = SubDevice {
            manufacturer: r.manufacturer.clone(),
            model: r.model.clone().or_else(|| Some(r.name.clone())),
            serial_number: r.serial.clone(),
            sw_version: r.firmware.clone(),
            ..SubDevice::new(key.clone(), r.name.clone())
        };
        let stale = self
            .receiver_entities
            .get(key)
            .is_none_or(|e| *e.device != device);
        if stale {
            let device = Arc::new(device);
            let paired = ctx.sensor(
                EntitySpec::sensor(key.join("paired"), "Paired devices")
                    .icon("mdi:usb")
                    .diagnostic()
                    .on_device(device.clone()),
            );
            self.receiver_entities
                .insert(key.clone(), ReceiverEntities { device, paired });
        }
        if let Some(e) = self.receiver_entities.get(key) {
            e.paired.set(r.slots.len() as u64);
            let slots: Vec<_> = r
                .slots
                .iter()
                .map(|s| {
                    json!({
                        "slot": s.slot,
                        "name": s.name,
                        "model_id": s.model_id,
                        "online": s.online,
                    })
                })
                .collect();
            let mut attrs = Attributes::new();
            attrs.insert("slots".into(), json!(slots));
            e.paired.set_attributes(attrs);
        }
    }
}

fn fill(slot: &mut Option<String>, other: &Option<String>) {
    if slot.is_none() {
        slot.clone_from(other);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use deskhand_core::testing::{rig_with, wait_attr, wait_entity, wait_spec, wait_state};
    use deskhand_core::{Config, State};

    use super::*;

    /// A source driven by the test: everything sent on the channel is
    /// forwarded to the sink; dropping the sender ends the run.
    struct FakeSource {
        id: &'static str,
        available: bool,
        rx: Mutex<Option<mpsc::UnboundedReceiver<FakeMsg>>>,
    }

    enum FakeMsg {
        Update(PeripheralReading),
        Remove(EntityId),
        Receiver(ReceiverReading),
    }

    #[async_trait::async_trait]
    impl PeripheralSource for FakeSource {
        fn id(&self) -> &'static str {
            self.id
        }

        async fn probe(&self) -> Probe {
            if self.available {
                Probe::available()
            } else {
                Probe::unavailable("not here")
            }
        }

        async fn run(
            &self,
            sink: PeripheralSink,
            cancel: CancellationToken,
        ) -> Result<(), IntegrationError> {
            let Some(mut rx) = self.rx.lock().unwrap().take() else {
                // Second run after the test ended the first: stay idle.
                cancel.cancelled().await;
                return Ok(());
            };
            sink.ready();
            loop {
                tokio::select! {
                    () = cancel.cancelled() => return Ok(()),
                    msg = rx.recv() => match msg {
                        Some(FakeMsg::Update(r)) => sink.update(r),
                        Some(FakeMsg::Remove(k)) => sink.remove(&k),
                        Some(FakeMsg::Receiver(r)) => sink.receiver(r),
                        None => return Ok(()),
                    },
                }
            }
        }
    }

    fn source(id: &'static str) -> (Arc<FakeSource>, mpsc::UnboundedSender<FakeMsg>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let s = Arc::new(FakeSource {
            id,
            available: true,
            rx: Mutex::new(Some(rx)),
        });
        (s, tx)
    }

    fn start(
        toml: &str,
        sources: Vec<Arc<dyn PeripheralSource>>,
    ) -> deskhand_core::testing::TestRig {
        let config = Config::from_toml_str(toml, None)
            .unwrap()
            .integration(CAPABILITY)
            .unwrap();
        rig_with(PeripheralBattery::new(sources), config).0
    }

    fn keyboard(percentage: f64) -> PeripheralReading {
        PeripheralReading {
            kind: Some("keyboard".into()),
            manufacturer: Some("Logitech".into()),
            model: Some("MX Keys S".into()),
            percentage: Some(percentage),
            state: ChargeState::Discharging,
            read_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_000_000)),
            ..PeripheralReading::new(EntityId::slugify("MX Keys S"), "MX Keys S")
        }
    }

    fn id(s: &'static str) -> EntityId {
        EntityId::from_static(s)
    }

    #[tokio::test]
    async fn one_source_gives_a_device_with_five_entities() {
        let (hidpp, tx) = source("hidpp");
        let mut t = start("", vec![hidpp]);
        tx.send(FakeMsg::Update(PeripheralReading {
            firmware: Some("RBK 81.03.B0018".into()),
            level: Some(BatteryLevel::Full),
            link: Link {
                slot: Some(1),
                encrypted: Some(true),
                ..Link::default()
            },
            ..keyboard(100.0)
        }))
        .unwrap();
        wait_state(&mut t, &id("battery_mx_keys_s"), State::Float(100.0)).await;
        wait_state(&mut t, &id("battery_mx_keys_s_state"), "discharging".into()).await;
        wait_state(&mut t, &id("battery_mx_keys_s_low"), State::Bool(false)).await;
        wait_state(
            &mut t,
            &id("battery_mx_keys_s_connected"),
            State::Bool(true),
        )
        .await;
        wait_state(
            &mut t,
            &id("battery_mx_keys_s_updated"),
            "2026-10-26T07:33:20+00:00".into(),
        )
        .await;
        wait_attr(&mut t, &id("battery_mx_keys_s"), "slot", json!(1)).await;
        wait_attr(&mut t, &id("battery_mx_keys_s"), "source", json!("hidpp")).await;
        wait_spec(
            &mut t,
            &id("battery_mx_keys_s"),
            "on the keyboard's device",
            |s| {
                s.device.as_ref().is_some_and(|d| {
                    d.key.as_str() == "mx_keys_s"
                        && d.sw_version.as_deref() == Some("RBK 81.03.B0018")
                        && d.via.is_none()
                })
            },
        )
        .await;
    }

    #[tokio::test]
    async fn asleep_keeps_the_values_and_removal_drops_the_device() {
        let (hidpp, tx) = source("hidpp");
        let mut t = start("", vec![hidpp]);
        tx.send(FakeMsg::Update(keyboard(12.0))).unwrap();
        wait_state(&mut t, &id("battery_mx_keys_s_low"), State::Bool(true)).await;
        tx.send(FakeMsg::Update(PeripheralReading {
            connected: false,
            ..keyboard(12.0)
        }))
        .unwrap();
        wait_state(
            &mut t,
            &id("battery_mx_keys_s_connected"),
            State::Bool(false),
        )
        .await;
        assert_eq!(t.state(&id("battery_mx_keys_s")), Some(&State::Float(12.0)));
        tx.send(FakeMsg::Remove(id("mx_keys_s"))).unwrap();
        wait_entity(&mut t, &id("battery_mx_keys_s"), false).await;
        wait_entity(&mut t, &id("battery_mx_keys_s_updated"), false).await;
    }

    #[tokio::test]
    async fn two_sources_one_device_preferring_the_connected_first_source() {
        let (hidpp, hid_tx) = source("hidpp");
        let (upower, up_tx) = source("upower");
        let mut t = start("", vec![hidpp, upower]);
        up_tx
            .send(FakeMsg::Update(PeripheralReading {
                serial: Some("1234AB567CD8".into()),
                ..keyboard(90.0)
            }))
            .unwrap();
        wait_attr(&mut t, &id("battery_mx_keys_s"), "source", json!("upower")).await;
        // HID++ reports the same keyboard under a different name: the serial joins them.
        hid_tx
            .send(FakeMsg::Update(PeripheralReading {
                serial: Some("1234AB567CD8".into()),
                ..PeripheralReading {
                    key: EntityId::slugify("MX KEYS S BOLT"),
                    ..keyboard(88.0)
                }
            }))
            .unwrap();
        wait_attr(&mut t, &id("battery_mx_keys_s"), "source", json!("hidpp")).await;
        wait_state(&mut t, &id("battery_mx_keys_s"), State::Float(88.0)).await;
        assert!(!t.entities.contains(&id("battery_mx_keys_s_bolt")));

        // HID++ says asleep: UPower's connected reading wins meanwhile.
        hid_tx
            .send(FakeMsg::Update(PeripheralReading {
                key: EntityId::slugify("MX KEYS S BOLT"),
                serial: Some("1234AB567CD8".into()),
                connected: false,
                ..keyboard(88.0)
            }))
            .unwrap();
        wait_attr(&mut t, &id("battery_mx_keys_s"), "source", json!("upower")).await;

        // The HID++ source stops: UPower keeps the device; nothing is removed.
        drop(hid_tx);
        wait_state(&mut t, &id("battery_mx_keys_s"), State::Float(90.0)).await;
        // Longer than the test rig's removal grace: still there.
        t.settle(Duration::from_secs(2)).await;
        assert!(t.entities.contains(&id("battery_mx_keys_s")));
    }

    #[tokio::test]
    async fn receivers_are_devices_and_peripherals_link_to_them() {
        let (hidpp, tx) = source("hidpp");
        let mut t = start("", vec![hidpp]);
        tx.send(FakeMsg::Receiver(ReceiverReading {
            key: id("bolt_aaaabbbbcccc0001"),
            name: "Logi Bolt receiver".into(),
            manufacturer: Some("Logitech".into()),
            model: None,
            serial: Some("AAAABBBBCCCC0001".into()),
            firmware: None,
            slots: vec![PairedSlot {
                slot: 1,
                name: "MX KEYS S".into(),
                model_id: Some("B378".into()),
                online: true,
            }],
        }))
        .unwrap();
        tx.send(FakeMsg::Update(PeripheralReading {
            link: Link {
                receiver: Some(id("bolt_aaaabbbbcccc0001")),
                slot: Some(1),
                encrypted: Some(true),
            },
            ..keyboard(100.0)
        }))
        .unwrap();
        let paired = id("bolt_aaaabbbbcccc0001_paired");
        wait_state(&mut t, &paired, State::Int(1)).await;
        wait_spec(
            &mut t,
            &id("battery_mx_keys_s"),
            "linked to the receiver",
            |s| {
                s.device
                    .as_ref()
                    .is_some_and(|d| d.via.as_ref() == Some(&id("bolt_aaaabbbbcccc0001")))
            },
        )
        .await;
        let slots = t.attributes(&paired).unwrap()["slots"].clone();
        assert_eq!(slots[0]["model_id"], "B378");
    }

    #[tokio::test]
    async fn disabled_and_unavailable_sources_do_not_block_ready() {
        let (hidpp, _hid_tx) = source("hidpp");
        let missing = Arc::new(FakeSource {
            id: "upower",
            available: false,
            rx: Mutex::new(None),
        });
        let (other, other_tx) = source("other");
        let mut t = start(
            "[integrations.peripheral_battery.sources]\nhidpp = false\n",
            vec![hidpp, missing, other],
        );
        other_tx.send(FakeMsg::Update(keyboard(50.0))).unwrap();
        wait_attr(&mut t, &id("battery_mx_keys_s"), "source", json!("other")).await;
    }

    #[test]
    fn battery_refresh_option() {
        let parse = |toml: &str| {
            Config::from_toml_str(toml, None)
                .unwrap()
                .integration(CAPABILITY)
                .unwrap()
                .options::<PeripheralOptions>()
                .map(|p| p.value.battery_refresh)
        };
        assert_eq!(parse("").unwrap(), None);
        let section = "[integrations.peripheral_battery]\nbattery_refresh = ";
        assert_eq!(parse(&format!("{section}\"off\"")).unwrap(), None);
        assert_eq!(
            parse(&format!("{section}\"30m\"")).unwrap(),
            Some(Duration::from_secs(1800))
        );
        assert!(parse(&format!("{section}\"10s\"")).is_err());
        assert!(parse(&format!("{section}\"often\"")).is_err());
    }

    #[test]
    fn keys_and_low_battery_rules() {
        assert_eq!(
            PeripheralReading::key_for(" MX Master 3 ", "SER", "/dev/x").as_str(),
            "mx_master_3"
        );
        assert_eq!(
            PeripheralReading::key_for("", "SER-1", "x").as_str(),
            "ser_1"
        );
        assert_eq!(
            PeripheralReading::key_for("", " ", "hidpp_battery_0").as_str(),
            "hidpp_battery_0"
        );
        let mut r = keyboard(50.0);
        assert_eq!(r.is_low(), Some(false));
        r.level = Some(BatteryLevel::Low);
        assert_eq!(r.is_low(), Some(true));
        r.low = Some(false);
        assert_eq!(r.is_low(), Some(false));
        assert_eq!(battery_id(&id("mx_keys_s")).as_str(), "battery_mx_keys_s");
    }
}
