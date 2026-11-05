//! Home Assistant device-based discovery.
//!
//! One retained document per machine at
//! `<discovery>/device/deskhand_<node>/config` lists every component. That
//! document doubles as our memory of what was published last time, which is
//! how stale entities are cleaned up without a state file:
//!
//! - a component that disappears is removed by publishing it once as a
//!   `{"platform": ...}` stub, then the document without it;
//! - components of a capability that is enabled but not running right now
//!   (service missing, still probing) are kept as they were, so HA keeps the
//!   entities (and the user's customisations) and shows them unavailable;
//! - components of disabled capabilities are removed.
//!
//! Entities on a [`SubDevice`] (a keyboard, a receiver) go into a document of
//! their own at `<discovery>/device/deskhand_<node>/<key>/config`, tracked the
//! same way; a sub-device document with nothing left in it is deleted, which
//! removes that device from HA.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use deskhand_core::DeviceInfo;
use deskhand_core::entity::{AvailabilityScope, EntityKind, ImageSource, SubDevice};
use deskhand_core::transport::EntityDescriptor;
use serde_json::{Map, Value, json};

use crate::topics::{OFFLINE, ONLINE, Topics};

/// Object id of the synthetic "Deskhand connected" sensor.
pub const CONNECTED_ID: &str = "deskhand_connected";

const ORIGIN_URL: &str = "https://github.com/ryanmwright/deskhand";

/// Renders one entity as a discovery component, or `None` if this
/// transport cannot express it in device discovery (media players).
pub fn component(topics: &Topics, desc: &EntityDescriptor) -> Option<Map<String, Value>> {
    let spec = &desc.spec;
    let id = &spec.id;
    let mut c = Map::new();
    c.insert("platform".into(), spec.kind.platform().into());
    c.insert(
        "unique_id".into(),
        format!("{}_{}", topics.device_id(), id).into(),
    );
    c.insert(
        "name".into(),
        spec.name.clone().map_or(Value::Null, Value::String),
    );
    if let Some(icon) = &spec.icon {
        c.insert("icon".into(), icon.clone().into());
    }
    if let Some(cat) = spec.category {
        c.insert("entity_category".into(), json!(cat));
    }
    if !spec.enabled_by_default {
        c.insert("enabled_by_default".into(), false.into());
    }
    if spec.availability == AvailabilityScope::Integration {
        c.insert(
            "availability".into(),
            json!([
                {"topic": topics.status()},
                {"topic": topics.capability_status(&desc.owner)},
            ]),
        );
        c.insert("availability_mode".into(), "all".into());
        c.insert("payload_available".into(), ONLINE.into());
        c.insert("payload_not_available".into(), OFFLINE.into());
    }

    let state = topics.state(id);
    let command = topics.command(id);
    let mut has_state = true;
    match &spec.kind {
        EntityKind::Sensor {
            device_class,
            state_class,
            unit,
            precision,
            options,
        } => {
            c.insert("state_topic".into(), state.into());
            insert_opt(
                &mut c,
                "device_class",
                device_class.as_ref().map(|d| json!(d)),
            );
            insert_opt(
                &mut c,
                "state_class",
                state_class.as_ref().map(|d| json!(d)),
            );
            insert_opt(
                &mut c,
                "unit_of_measurement",
                unit.as_deref().map(Value::from),
            );
            insert_opt(
                &mut c,
                "suggested_display_precision",
                precision.map(Value::from),
            );
            insert_opt(&mut c, "options", options.as_ref().map(|o| json!(o)));
        }
        EntityKind::BinarySensor { device_class } => {
            c.insert("state_topic".into(), state.into());
            insert_opt(
                &mut c,
                "device_class",
                device_class.as_ref().map(|d| json!(d)),
            );
        }
        EntityKind::Switch { device_class } => {
            c.insert("state_topic".into(), state.into());
            c.insert("command_topic".into(), command.into());
            insert_opt(
                &mut c,
                "device_class",
                device_class.as_ref().map(|d| json!(d)),
            );
        }
        EntityKind::Button { device_class } => {
            has_state = false;
            c.insert("command_topic".into(), command.into());
            insert_opt(
                &mut c,
                "device_class",
                device_class.as_ref().map(|d| json!(d)),
            );
        }
        EntityKind::Number {
            min,
            max,
            step,
            unit,
            mode,
        } => {
            c.insert("state_topic".into(), state.into());
            c.insert("command_topic".into(), command.into());
            c.insert("min".into(), json!(min));
            c.insert("max".into(), json!(max));
            c.insert("step".into(), json!(step));
            c.insert("mode".into(), json!(mode));
            insert_opt(
                &mut c,
                "unit_of_measurement",
                unit.as_deref().map(Value::from),
            );
        }
        EntityKind::Select { options } => {
            c.insert("state_topic".into(), state.into());
            c.insert("command_topic".into(), command.into());
            c.insert("options".into(), json!(options));
        }
        EntityKind::Text { min, max, pattern } => {
            c.insert("state_topic".into(), state.into());
            c.insert("command_topic".into(), command.into());
            c.insert("min".into(), json!(min));
            c.insert("max".into(), json!(max));
            insert_opt(&mut c, "pattern", pattern.clone().map(Value::from));
        }
        EntityKind::Event {
            event_types,
            device_class,
        } => {
            c.insert("state_topic".into(), state.into());
            c.insert("event_types".into(), json!(event_types));
            insert_opt(
                &mut c,
                "device_class",
                device_class.as_ref().map(|d| json!(d)),
            );
        }
        EntityKind::Notify => {
            has_state = false;
            c.insert("command_topic".into(), command.into());
        }
        EntityKind::Lock => {
            c.insert("state_topic".into(), state.into());
            c.insert("command_topic".into(), command.into());
            for s in ["locked", "unlocked", "locking", "unlocking", "jammed"] {
                c.insert(format!("state_{s}"), s.into());
            }
        }
        EntityKind::Image { source } => match source {
            ImageSource::Url => {
                c.insert("url_topic".into(), state.into());
            }
            ImageSource::Bytes { content_type } => {
                c.insert("image_topic".into(), state.into());
                c.insert("content_type".into(), content_type.to_string().into());
            }
        },
        EntityKind::Update => {
            c.insert("state_topic".into(), state.into());
        }
        EntityKind::MediaPlayer => return None,
        other => {
            tracing::warn!(%id, kind = other.platform(), "entity kind not supported by MQTT discovery");
            return None;
        }
    }
    if has_state {
        c.insert("json_attributes_topic".into(), topics.attributes(id).into());
    }
    Some(c)
}

fn insert_opt(c: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(v) = value {
        c.insert(key.to_owned(), v);
    }
}

/// The "Deskhand connected" diagnostic sensor: it reads the status topic
/// directly, so it is never unavailable itself.
pub fn connected_component(topics: &Topics) -> Map<String, Value> {
    let Value::Object(c) = json!({
        "platform": "binary_sensor",
        "unique_id": format!("{}_{CONNECTED_ID}", topics.device_id()),
        "name": "Connected",
        "device_class": "connectivity",
        "entity_category": "diagnostic",
        "state_topic": topics.status(),
        "payload_on": ONLINE,
        "payload_off": OFFLINE,
    }) else {
        unreachable!()
    };
    c
}

/// The `device` block of Deskhand's own document.
fn main_device_block(topics: &Topics, device: &DeviceInfo) -> Value {
    let mut dev = json!({
        "identifiers": [topics.device_id()],
        "name": device.name,
        "manufacturer": device.manufacturer,
        "model": device.model,
        "sw_version": device.sw_version,
    });
    if let Some(hw) = &device.hw_version {
        dev["hw_version"] = hw.clone().into();
    }
    if !device.connections.is_empty() {
        dev["connections"] = json!(
            device
                .connections
                .iter()
                .map(|(k, v)| [k, v])
                .collect::<Vec<_>>()
        );
    }
    dev
}

/// The `device` block of a sub-device document, linked to its parent with `via_device`.
pub fn sub_device_block(topics: &Topics, sub: &SubDevice) -> Value {
    let mut dev = Map::new();
    dev.insert(
        "identifiers".into(),
        json!([topics.sub_device_id(sub.key.as_str())]),
    );
    dev.insert("name".into(), sub.name.clone().into());
    for (key, value) in [
        ("manufacturer", &sub.manufacturer),
        ("model", &sub.model),
        ("model_id", &sub.model_id),
        ("serial_number", &sub.serial_number),
        ("sw_version", &sub.sw_version),
        ("hw_version", &sub.hw_version),
    ] {
        insert_opt(&mut dev, key, value.clone().map(Value::from));
    }
    let via = match &sub.via {
        Some(parent) => topics.sub_device_id(parent.as_str()),
        None => topics.device_id(),
    };
    dev.insert("via_device".into(), via.into());
    Value::Object(dev)
}

fn document(device_block: Value, sw_version: &str, components: &BTreeMap<String, Value>) -> Value {
    json!({
        "device": device_block,
        "origin": {
            "name": "Deskhand",
            "sw_version": sw_version,
            "support_url": ORIGIN_URL,
        },
        "components": components,
    })
}

/// The whole document of Deskhand's own device.
pub fn device_config(
    topics: &Topics,
    device: &DeviceInfo,
    components: &BTreeMap<String, Value>,
) -> Value {
    document(
        main_device_block(topics, device),
        &device.sw_version,
        components,
    )
}

/// Key in the compat media player document naming its owner capability
/// (the `mqtt_media_player` component ignores unknown keys).
const MEDIA_OWNER_KEY: &str = "deskhand_capability";

/// The compat media player document for the HACS `mqtt_media_player`
/// component, published at [`Topics::media_player_config`].
///
/// It has a single availability topic, so it follows Deskhand's global
/// status (which the LWT covers) rather than the media integration's.
pub fn media_player_config(topics: &Topics, device: &DeviceInfo, desc: &EntityDescriptor) -> Value {
    let id = &desc.spec.id;
    let name = match &desc.spec.name {
        Some(n) => format!("{} {n}", device.name),
        None => device.name.clone(),
    };
    let field = |f: &str| Value::from(topics.media_field(id, f));
    let set = topics.command(id);
    let mut c = json!({
        "name": name,
        "device": {
            "identifiers": [topics.device_id()],
            "name": device.name,
            "manufacturer": device.manufacturer,
            "model": device.model,
            "sw_version": device.sw_version,
        },
        "availability": {
            "topic": topics.status(),
            "payload_available": ONLINE,
            "payload_not_available": OFFLINE,
        },
        "state_state_topic": topics.state(id),
        "command_volume_topic": field(crate::topics::MEDIA_VOLUME),
        "command_seek_topic": field(crate::topics::MEDIA_SEEK),
        "command_play_topic": set,
        "command_play_payload": "Play",
        "command_pause_topic": set,
        "command_pause_payload": "Pause",
        "command_playpause_topic": set,
        "command_playpause_payload": "PlayPause",
        "command_next_topic": set,
        "command_next_payload": "Next",
        "command_previous_topic": set,
        "command_previous_payload": "Previous",
        MEDIA_OWNER_KEY: desc.owner.as_ref(),
    });
    for f in [
        "title",
        "artist",
        "album",
        "duration",
        "position",
        "volume",
        "albumart",
        "mediatype",
    ] {
        c[format!("state_{f}_topic")] = field(f);
    }
    c
}

/// Result of rendering: publish `first` now and, if set, `followup` about a
/// second later (once HA has processed the removal stubs). The top-level
/// fields are Deskhand's own document.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub first: Value,
    pub followup: Option<Value>,
    pub removed: Vec<String>,
    /// Sub-device documents, in key order. Publish them after `first`, so an
    /// entity that moved to a sub-device is removed from its old document first.
    pub sub_devices: Vec<RenderedSub>,
}

/// One sub-device document.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedSub {
    pub key: String,
    /// `None`: delete the document (an empty retained payload), which removes
    /// the device and its entities from HA.
    pub first: Option<Value>,
    pub followup: Option<Value>,
    pub removed: Vec<String>,
}

/// What was published in one discovery document.
#[derive(Debug, Clone, Default)]
struct DocState {
    /// Previously published components kept for capabilities that are not running.
    kept: BTreeMap<String, (Option<String>, Value)>,
    /// id → platform of every component in the last published document.
    last_published: BTreeMap<String, String>,
    /// The `device` block last published (sub-devices), so a kept document
    /// can be republished without its entities running.
    device: Option<Value>,
}

impl DocState {
    fn adopt(&mut self, topics: &Topics, disabled: &BTreeSet<String>, previous: &Value) {
        let components = previous
            .get("components")
            .or_else(|| previous.get("cmps"))
            .and_then(Value::as_object);
        let Some(components) = components else {
            return;
        };
        for (id, c) in components {
            let Some(platform) = c
                .get("platform")
                .or_else(|| c.get("p"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            // A stub left over from an interrupted removal: nothing to keep.
            let is_stub = c.as_object().is_some_and(|o| o.len() == 1);
            self.last_published.insert(id.clone(), platform.to_owned());
            if is_stub {
                continue;
            }
            let capability = owner_capability(topics, c);
            if capability
                .as_ref()
                .is_some_and(|cap| !disabled.contains(cap))
            {
                self.kept.insert(id.clone(), (capability, c.clone()));
            }
        }
        self.device = previous
            .get("device")
            .or_else(|| previous.get("dev"))
            .cloned();
    }

    /// Adds the kept components to `full` and works out the removals:
    /// returns the components to publish first (with removal stubs), the
    /// follow-up without the stubs (if anything was removed), and the removed ids.
    #[allow(clippy::type_complexity)]
    fn plan(
        &mut self,
        mut full: BTreeMap<String, Value>,
        live: &BTreeSet<String>,
    ) -> (
        BTreeMap<String, Value>,
        Option<BTreeMap<String, Value>>,
        Vec<String>,
    ) {
        self.kept.retain(|id, (cap, _)| {
            !full.contains_key(id) && cap.as_ref().is_none_or(|c| !live.contains(c))
        });
        for (id, (_, c)) in &self.kept {
            full.insert(id.clone(), c.clone());
        }
        let removed: Vec<String> = self
            .last_published
            .keys()
            .filter(|id| !full.contains_key(*id))
            .cloned()
            .collect();
        let followup = (!removed.is_empty()).then(|| full.clone());
        let mut first = full.clone();
        for id in &removed {
            first.insert(id.clone(), json!({ "platform": self.last_published[id] }));
        }
        self.last_published = full
            .iter()
            .filter_map(|(id, c)| {
                c.get("platform")
                    .and_then(Value::as_str)
                    .map(|p| (id.clone(), p.to_owned()))
            })
            .collect();
        (first, followup, removed)
    }
}

/// Tracks what was published to compute removals and kept components.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryPlanner {
    disabled: BTreeSet<String>,
    main: DocState,
    subs: BTreeMap<String, DocState>,
    /// A compat media player document is on the broker; its owner capability.
    media_on_broker: Option<Option<String>>,
}

impl DiscoveryPlanner {
    pub fn new(disabled_capabilities: BTreeSet<String>) -> Self {
        Self {
            disabled: disabled_capabilities,
            ..Self::default()
        }
    }

    /// Takes over the document a previous run left on the broker.
    pub fn adopt_previous(&mut self, topics: &Topics, previous: &Value) {
        self.main.adopt(topics, &self.disabled, previous);
    }

    /// Takes over a sub-device document a previous run left on the broker.
    pub fn adopt_previous_sub(&mut self, topics: &Topics, key: &str, previous: &Value) {
        self.subs
            .entry(key.to_owned())
            .or_default()
            .adopt(topics, &self.disabled, previous);
    }

    /// Takes over a compat media player document a previous run left behind.
    pub fn adopt_previous_media(&mut self, previous: &Value) {
        let owner = previous
            .get(MEDIA_OWNER_KEY)
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.media_on_broker = Some(owner);
    }

    /// What to publish on the compat media player's discovery topic: a
    /// document, an empty payload to remove it, or `None` for no change.
    /// Like device components, a document of a capability that is enabled
    /// but not running is kept.
    pub fn render_media(
        &mut self,
        topics: &Topics,
        device: &DeviceInfo,
        entities: &[EntityDescriptor],
        live: &BTreeSet<String>,
    ) -> Option<String> {
        if let Some(desc) = entities
            .iter()
            .find(|d| matches!(d.spec.kind, EntityKind::MediaPlayer))
        {
            self.media_on_broker = Some(Some(desc.owner.to_string()));
            return Some(media_player_config(topics, device, desc).to_string());
        }
        let owner = self.media_on_broker.as_ref()?;
        let keep = owner
            .as_ref()
            .is_some_and(|c| !self.disabled.contains(c) && !live.contains(c));
        if keep {
            return None;
        }
        self.media_on_broker = None;
        Some(String::new())
    }

    /// Capabilities whose components are being kept (their status topics should read offline).
    pub fn kept_capabilities(&self) -> BTreeSet<String> {
        std::iter::once(&self.main)
            .chain(self.subs.values())
            .flat_map(|d| d.kept.values().filter_map(|(c, _)| c.clone()))
            .collect()
    }

    /// Renders the documents for the current entities. `live` holds the
    /// capabilities that are running (or have run) in this session.
    pub fn render(
        &mut self,
        topics: &Topics,
        device: &DeviceInfo,
        entities: &[EntityDescriptor],
        live: &BTreeSet<String>,
    ) -> Rendered {
        let mut main: BTreeMap<String, Value> = BTreeMap::new();
        main.insert(
            CONNECTED_ID.to_owned(),
            Value::Object(connected_component(topics)),
        );
        let mut subs: BTreeMap<String, (Arc<SubDevice>, BTreeMap<String, Value>)> = BTreeMap::new();
        for desc in entities {
            let Some(c) = component(topics, desc) else {
                continue;
            };
            let id = desc.spec.id.to_string();
            match &desc.spec.device {
                None => {
                    main.insert(id, Value::Object(c));
                }
                Some(sub) => {
                    subs.entry(sub.key.to_string())
                        .or_insert_with(|| (sub.clone(), BTreeMap::new()))
                        .1
                        .insert(id, Value::Object(c));
                }
            }
        }

        let (first, followup, removed) = self.main.plan(main, live);
        let block = main_device_block(topics, device);
        let sw = &device.sw_version;
        let mut rendered = Rendered {
            first: document(block.clone(), sw, &first),
            followup: followup.map(|f| document(block, sw, &f)),
            removed,
            sub_devices: Vec::new(),
        };

        let keys: BTreeSet<String> = subs.keys().chain(self.subs.keys()).cloned().collect();
        for key in keys {
            let (sub, full) = match subs.remove(&key) {
                Some((sub, full)) => (Some(sub), full),
                None => (None, BTreeMap::new()),
            };
            let state = self.subs.entry(key.clone()).or_default();
            let was_published = !state.last_published.is_empty();
            let (first, followup, removed) = state.plan(full, live);
            if state.last_published.is_empty() {
                // Nothing left on this device: delete its document.
                self.subs.remove(&key);
                if was_published {
                    rendered.sub_devices.push(RenderedSub {
                        key,
                        first: None,
                        followup: None,
                        removed,
                    });
                }
                continue;
            }
            let block = sub
                .map(|s| sub_device_block(topics, &s))
                .or_else(|| state.device.clone())
                .unwrap_or_else(|| {
                    json!({
                        "identifiers": [topics.sub_device_id(&key)],
                        "name": key,
                        "via_device": topics.device_id(),
                    })
                });
            state.device = Some(block.clone());
            rendered.sub_devices.push(RenderedSub {
                key,
                first: Some(document(block.clone(), sw, &first)),
                followup: followup.map(|f| document(block, sw, &f)),
                removed,
            });
        }
        rendered
    }
}

/// The capability whose status topic a component's availability list names.
fn owner_capability(topics: &Topics, component: &Value) -> Option<String> {
    component
        .get("availability")
        .or_else(|| component.get("avty"))
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|a| {
            a.get("topic")
                .or_else(|| a.get("t"))
                .and_then(Value::as_str)
        })
        .find_map(|t| topics.capability_from_status(t))
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::sync::Arc;

    use deskhand_core::EntityId;
    use deskhand_core::entity::{BinarySensorClass, EntitySpec, SensorClass, StateClass};

    use super::*;

    fn topics() -> Topics {
        Topics::new("deskhand", "homeassistant", "abc123")
    }

    fn device() -> DeviceInfo {
        DeviceInfo {
            node_id: "abc123".into(),
            name: "Desk".into(),
            manufacturer: "Deskhand".into(),
            model: "KDE Plasma 6 on NixOS".into(),
            sw_version: "0.0.0-test".into(),
            hw_version: Some("Linux 6.18".into()),
            connections: vec![("mac".into(), "aa:bb:cc:dd:ee:ff".into())],
        }
    }

    fn desc(owner: &str, spec: EntitySpec) -> EntityDescriptor {
        EntityDescriptor {
            spec,
            owner: Arc::from(owner),
        }
    }

    fn id(s: &'static str) -> EntityId {
        EntityId::from_static(s)
    }

    /// One entity of every kind the renderer supports.
    fn all_kinds() -> Vec<EntityDescriptor> {
        vec![
            desc(
                "metrics",
                EntitySpec::sensor(id("cpu_percent"), "CPU")
                    .unit("%")
                    .state_class(StateClass::Measurement)
                    .precision(1)
                    .icon("mdi:cpu-64-bit"),
            ),
            desc(
                "session",
                EntitySpec::sensor(id("session_state"), "Session")
                    .enum_options(vec!["active".into(), "idle".into()])
                    .always_available(),
            ),
            desc(
                "metrics",
                EntitySpec::sensor(id("boot_time"), "Boot time")
                    .sensor_class(SensorClass::Timestamp)
                    .diagnostic()
                    .disabled_by_default(),
            ),
            desc(
                "camera",
                EntitySpec::binary_sensor(id("camera_in_use"), "Camera in use", None),
            ),
            desc(
                "session",
                EntitySpec::binary_sensor(
                    id("user_active"),
                    "Active",
                    Some(BinarySensorClass::Presence),
                )
                .unnamed(),
            ),
            desc("power", EntitySpec::switch(id("keep_awake"), "Keep awake")),
            desc("power", EntitySpec::button(id("suspend"), "Suspend")),
            desc(
                "audio",
                EntitySpec::number(id("audio_volume"), "Volume", 0.0, 100.0, 1.0).unit("%"),
            ),
            desc(
                "audio",
                EntitySpec::select(
                    id("audio_output"),
                    "Output",
                    vec!["Speakers".into(), "Headphones".into()],
                ),
            ),
            desc("demo", EntitySpec::text(id("demo_text"), "Text")),
            desc(
                "shortcuts",
                EntitySpec::event(id("shortcut_lights"), "Lights", vec!["pressed".into()]),
            ),
            desc("notifications", EntitySpec::notify(id("notify"), "Notify")),
            desc("lock", EntitySpec::lock(id("screen_lock"), "Screen")),
            desc(
                "media",
                EntitySpec::image(
                    id("media_art"),
                    "Album art",
                    ImageSource::Bytes {
                        content_type: Cow::Borrowed("image/png"),
                    },
                ),
            ),
            desc(
                "media",
                EntitySpec::image(id("media_art_url"), "Album art URL", ImageSource::Url),
            ),
        ]
    }

    #[test]
    fn device_document_snapshot() {
        let mut planner = DiscoveryPlanner::default();
        let live: BTreeSet<String> = all_kinds().iter().map(|d| d.owner.to_string()).collect();
        let r = planner.render(&topics(), &device(), &all_kinds(), &live);
        assert!(r.followup.is_none());
        insta::assert_json_snapshot!(r.first);
    }

    #[test]
    fn media_player_is_not_rendered_in_device_discovery() {
        let d = desc(
            "media",
            EntitySpec::new(id("media_player"), "Player", EntityKind::MediaPlayer),
        );
        assert!(component(&topics(), &d).is_none());
    }

    #[test]
    fn media_player_document_snapshot() {
        let d = desc(
            "media",
            EntitySpec::new(id("media"), "Media", EntityKind::MediaPlayer),
        );
        insta::assert_json_snapshot!(media_player_config(&topics(), &device(), &d));
    }

    #[test]
    fn media_player_document_lifecycle() {
        let t = topics();
        let player = desc(
            "media",
            EntitySpec::new(id("media"), "Media", EntityKind::MediaPlayer),
        );
        let media = BTreeSet::from(["media".to_owned()]);
        let none = BTreeSet::new();

        // Nothing published, nothing on the broker: nothing to do.
        let mut planner = DiscoveryPlanner::default();
        assert_eq!(planner.render_media(&t, &device(), &[], &none), None);

        let doc = planner
            .render_media(&t, &device(), std::slice::from_ref(&player), &media)
            .unwrap();
        let doc: Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(doc["command_seek_topic"], "deskhand/abc123/media/seek");
        // The entity went away while media runs (compat turned off): remove.
        assert_eq!(
            planner.render_media(&t, &device(), &[], &media).as_deref(),
            Some("")
        );
        assert_eq!(planner.render_media(&t, &device(), &[], &media), None);

        // From a previous run: kept while media is enabled but not running...
        let mut planner = DiscoveryPlanner::default();
        planner.adopt_previous_media(&doc);
        assert_eq!(planner.render_media(&t, &device(), &[], &none), None);
        // ...removed once it runs without a player entity.
        assert_eq!(
            planner.render_media(&t, &device(), &[], &media).as_deref(),
            Some("")
        );

        // Removed right away when media is disabled.
        let mut planner = DiscoveryPlanner::new(media.clone());
        planner.adopt_previous_media(&doc);
        assert_eq!(
            planner.render_media(&t, &device(), &[], &none).as_deref(),
            Some("")
        );
    }

    fn component_ids(v: &Value) -> Vec<String> {
        v["components"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn removed_entities_get_a_stub_then_disappear() {
        let t = topics();
        let mut planner = DiscoveryPlanner::default();
        let live = BTreeSet::from(["a".to_owned()]);
        let a = desc("a", EntitySpec::sensor(id("a_one"), "One"));
        let b = desc("a", EntitySpec::sensor(id("a_two"), "Two"));
        planner.render(&t, &device(), &[a.clone(), b], &live);

        let r = planner.render(&t, &device(), std::slice::from_ref(&a), &live);
        assert_eq!(r.removed, vec!["a_two"]);
        assert_eq!(
            r.first["components"]["a_two"],
            json!({"platform": "sensor"})
        );
        let followup = r.followup.unwrap();
        assert_eq!(component_ids(&followup), vec!["a_one", CONNECTED_ID]);

        // Steady state: nothing more to remove.
        let r = planner.render(&t, &device(), &[a], &live);
        assert!(r.removed.is_empty() && r.followup.is_none());
    }

    #[test]
    fn previous_run_cleanup_keeps_unavailable_and_drops_disabled() {
        let t = topics();
        // What a previous run left behind.
        let mut old = DiscoveryPlanner::default();
        let prev_entities = vec![
            desc("audio", EntitySpec::sensor(id("audio_volume"), "Volume")),
            desc("camera", EntitySpec::sensor(id("camera_in_use"), "Camera")),
            desc("metrics", EntitySpec::sensor(id("cpu_old_name"), "CPU")),
            desc("metrics", EntitySpec::sensor(id("cpu_percent"), "CPU")),
        ];
        let all: BTreeSet<String> = ["audio", "camera", "metrics"].map(String::from).into();
        let previous = old.render(&t, &device(), &prev_entities, &all).first;

        // This run: camera disabled, audio enabled but not running yet,
        // metrics running and renamed one entity.
        let mut planner = DiscoveryPlanner::new(BTreeSet::from(["camera".to_owned()]));
        planner.adopt_previous(&t, &previous);
        let now = vec![desc(
            "metrics",
            EntitySpec::sensor(id("cpu_percent"), "CPU"),
        )];
        let live = BTreeSet::from(["metrics".to_owned()]);
        let r = planner.render(&t, &device(), &now, &live);

        assert_eq!(r.removed, vec!["camera_in_use", "cpu_old_name"]);
        assert_eq!(
            planner.kept_capabilities(),
            BTreeSet::from(["audio".to_owned()])
        );
        assert_eq!(
            component_ids(&r.followup.unwrap()),
            vec!["audio_volume", "cpu_percent", CONNECTED_ID]
        );

        // Audio starts later and registers a different set: the kept component goes.
        let later = vec![
            now[0].clone(),
            desc("audio", EntitySpec::sensor(id("audio_mute"), "Mute")),
        ];
        let live = BTreeSet::from(["metrics".to_owned(), "audio".to_owned()]);
        let r = planner.render(&t, &device(), &later, &live);
        assert_eq!(r.removed, vec!["audio_volume"]);
        assert!(planner.kept_capabilities().is_empty());
    }

    #[test]
    fn adopt_previous_tolerates_abbreviations_and_stubs() {
        let t = topics();
        let previous = json!({
            "cmps": {
                "x": {"p": "sensor", "avty": [{"t": "deskhand/abc123/status/audio"}]},
                "gone": {"p": "button"},
            }
        });
        let mut planner = DiscoveryPlanner::default();
        planner.adopt_previous(&t, &previous);
        let r = planner.render(&t, &device(), &[], &BTreeSet::new());
        assert_eq!(r.removed, vec!["gone"]);
        assert!(r.first["components"].get("x").is_some(), "kept");
    }

    fn keyboard(via: Option<&'static str>) -> Arc<SubDevice> {
        Arc::new(SubDevice {
            manufacturer: Some("Logitech".into()),
            model: Some("MX Keys S".into()),
            model_id: Some("B378".into()),
            serial_number: Some("1234AB567CD8".into()),
            sw_version: Some("RBK 81.03.B0018".into()),
            via: via.map(EntityId::from_static),
            ..SubDevice::new(id("mx_keys_s"), "MX Keys S")
        })
    }

    fn sub_doc<'a>(r: &'a Rendered, key: &str) -> &'a RenderedSub {
        r.sub_devices.iter().find(|s| s.key == key).unwrap()
    }

    #[test]
    fn sub_device_document_snapshot() {
        let receiver = Arc::new(SubDevice {
            manufacturer: Some("Logitech".into()),
            model: Some("Logi Bolt receiver".into()),
            ..SubDevice::new(id("bolt_receiver"), "Logi Bolt receiver")
        });
        let entities = vec![
            desc(
                "peripheral_battery",
                EntitySpec::sensor(id("battery_mx_keys_s"), "Battery")
                    .sensor_class(SensorClass::Battery)
                    .unit("%")
                    .on_device(keyboard(Some("bolt_receiver"))),
            ),
            desc(
                "peripheral_battery",
                EntitySpec::sensor(id("bolt_receiver_paired"), "Paired devices")
                    .diagnostic()
                    .on_device(receiver),
            ),
            desc("metrics", EntitySpec::sensor(id("cpu_percent"), "CPU")),
        ];
        let live = BTreeSet::from(["peripheral_battery".to_owned(), "metrics".to_owned()]);
        let r = DiscoveryPlanner::default().render(&topics(), &device(), &entities, &live);
        assert_eq!(
            component_ids(&r.first),
            vec!["cpu_percent", CONNECTED_ID],
            "sub-device entities stay out of the main document"
        );
        let keys: Vec<&str> = r.sub_devices.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["bolt_receiver", "mx_keys_s"]);
        let receiver_doc = sub_doc(&r, "bolt_receiver").first.as_ref().unwrap();
        assert_eq!(receiver_doc["device"]["via_device"], "deskhand_abc123");
        insta::assert_json_snapshot!(sub_doc(&r, "mx_keys_s").first.as_ref().unwrap());
    }

    #[test]
    fn entity_moving_to_a_sub_device_leaves_a_stub_behind() {
        let t = topics();
        let live = BTreeSet::from(["peripheral_battery".to_owned()]);
        let on_main = desc(
            "peripheral_battery",
            EntitySpec::sensor(id("battery_mx_keys_s"), "MX Keys S battery"),
        );
        let mut planner = DiscoveryPlanner::default();
        planner.render(&t, &device(), std::slice::from_ref(&on_main), &live);

        let moved = desc(
            "peripheral_battery",
            EntitySpec::sensor(id("battery_mx_keys_s"), "Battery").on_device(keyboard(None)),
        );
        let r = planner.render(&t, &device(), &[moved], &live);
        assert_eq!(r.removed, vec!["battery_mx_keys_s"]);
        assert_eq!(
            r.first["components"]["battery_mx_keys_s"],
            json!({"platform": "sensor"})
        );
        let sub = sub_doc(&r, "mx_keys_s");
        assert!(sub.removed.is_empty());
        let doc = sub.first.as_ref().unwrap();
        assert_eq!(component_ids(doc), vec!["battery_mx_keys_s"]);
        assert_eq!(
            doc["components"]["battery_mx_keys_s"]["unique_id"],
            "deskhand_abc123_battery_mx_keys_s",
            "same unique id as before the move"
        );
        assert_eq!(doc["device"]["via_device"], "deskhand_abc123");
    }

    #[test]
    fn empty_sub_device_document_is_deleted() {
        let t = topics();
        let live = BTreeSet::from(["peripheral_battery".to_owned()]);
        let kb = keyboard(None);
        let battery = desc(
            "peripheral_battery",
            EntitySpec::sensor(id("battery_mx_keys_s"), "Battery").on_device(kb.clone()),
        );
        let connected = desc(
            "peripheral_battery",
            EntitySpec::binary_sensor(id("battery_mx_keys_s_connected"), "Connected", None)
                .on_device(kb),
        );
        let mut planner = DiscoveryPlanner::default();
        planner.render(&t, &device(), &[battery.clone(), connected], &live);

        // One entity goes: stub, then the document without it.
        let r = planner.render(&t, &device(), std::slice::from_ref(&battery), &live);
        let sub = sub_doc(&r, "mx_keys_s");
        assert_eq!(sub.removed, vec!["battery_mx_keys_s_connected"]);
        assert_eq!(
            component_ids(sub.followup.as_ref().unwrap()),
            vec!["battery_mx_keys_s"]
        );

        // The last one goes: the whole document is deleted, once.
        let r = planner.render(&t, &device(), &[], &live);
        let sub = sub_doc(&r, "mx_keys_s");
        assert!(sub.first.is_none());
        assert_eq!(sub.removed, vec!["battery_mx_keys_s"]);
        let r = planner.render(&t, &device(), &[], &live);
        assert!(r.sub_devices.is_empty());
    }

    #[test]
    fn previous_run_sub_devices_are_kept_or_deleted() {
        let t = topics();
        let battery = desc(
            "peripheral_battery",
            EntitySpec::sensor(id("battery_mx_keys_s"), "Battery").on_device(keyboard(None)),
        );
        let live = BTreeSet::from(["peripheral_battery".to_owned()]);
        let previous = DiscoveryPlanner::default()
            .render(&t, &device(), &[battery], &live)
            .sub_devices[0]
            .first
            .clone()
            .unwrap();

        // Enabled but not running yet: kept as it was, device block included.
        let mut planner = DiscoveryPlanner::default();
        planner.adopt_previous_sub(&t, "mx_keys_s", &previous);
        let r = planner.render(&t, &device(), &[], &BTreeSet::new());
        let sub = sub_doc(&r, "mx_keys_s");
        assert_eq!(sub.first.as_ref(), Some(&previous));
        assert_eq!(
            planner.kept_capabilities(),
            BTreeSet::from(["peripheral_battery".to_owned()])
        );
        // It runs and the keyboard is gone: deleted.
        let r = planner.render(&t, &device(), &[], &live);
        assert!(sub_doc(&r, "mx_keys_s").first.is_none());

        // Disabled: deleted right away.
        let mut planner = DiscoveryPlanner::new(BTreeSet::from(["peripheral_battery".to_owned()]));
        planner.adopt_previous_sub(&t, "mx_keys_s", &previous);
        let r = planner.render(&t, &device(), &[], &BTreeSet::new());
        assert!(sub_doc(&r, "mx_keys_s").first.is_none());
    }
}
