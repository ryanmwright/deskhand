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

use std::collections::{BTreeMap, BTreeSet};

use deskhand_core::DeviceInfo;
use deskhand_core::entity::{AvailabilityScope, EntityKind, ImageSource};
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

/// The whole device document.
pub fn device_config(
    topics: &Topics,
    device: &DeviceInfo,
    components: &BTreeMap<String, Value>,
) -> Value {
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
    json!({
        "device": dev,
        "origin": {
            "name": "Deskhand",
            "sw_version": device.sw_version,
            "support_url": ORIGIN_URL,
        },
        "components": components,
    })
}

/// Result of rendering: publish `first` now and, if set, `followup` about a
/// second later (once HA has processed the removal stubs).
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub first: Value,
    pub followup: Option<Value>,
    pub removed: Vec<String>,
}

/// Tracks what was published to compute removals and kept components.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryPlanner {
    disabled: BTreeSet<String>,
    /// Previously published components kept for capabilities that are not running.
    kept: BTreeMap<String, (Option<String>, Value)>,
    /// id → platform of every component in the last published document.
    last_published: BTreeMap<String, String>,
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
                .is_some_and(|cap| !self.disabled.contains(cap))
            {
                self.kept.insert(id.clone(), (capability, c.clone()));
            }
        }
    }

    /// Capabilities whose components are being kept (their status topics should read offline).
    pub fn kept_capabilities(&self) -> BTreeSet<String> {
        self.kept.values().filter_map(|(c, _)| c.clone()).collect()
    }

    /// Renders the document for the current entities. `live` holds the
    /// capabilities that are running (or have run) in this session.
    pub fn render(
        &mut self,
        topics: &Topics,
        device: &DeviceInfo,
        entities: &[EntityDescriptor],
        live: &BTreeSet<String>,
    ) -> Rendered {
        let mut full: BTreeMap<String, Value> = BTreeMap::new();
        full.insert(
            CONNECTED_ID.to_owned(),
            Value::Object(connected_component(topics)),
        );
        for desc in entities {
            if let Some(c) = component(topics, desc) {
                full.insert(desc.spec.id.to_string(), Value::Object(c));
            }
        }
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
        let followup = if removed.is_empty() {
            None
        } else {
            Some(device_config(topics, device, &full))
        };
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
        Rendered {
            first: device_config(topics, device, &first),
            followup,
            removed,
        }
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
}
