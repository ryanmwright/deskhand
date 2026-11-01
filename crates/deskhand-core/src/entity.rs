//! Entity model: what an integration exposes, independent of any transport.
//!
//! An [`EntitySpec`] describes one entity (a sensor, switch, button, ...). Its
//! [`EntityKind`] maps 1:1 onto a Home Assistant entity platform, but nothing
//! here knows about MQTT topics or JSON layouts; that is the transport's job.

use std::borrow::Cow;
use std::fmt;
use std::time::Duration;

use serde::Serialize;

/// Free-form attributes published next to an entity's state.
pub type Attributes = serde_json::Map<String, serde_json::Value>;

/// Stable identifier of an entity within one device: `[a-z0-9_]+`, at most 64 bytes.
///
/// Ids become part of topics and HA unique ids, so they must never change for
/// the same logical entity. By convention they are namespaced by capability,
/// for example `audio_volume` or `media_state`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct EntityId(Cow<'static, str>);

/// Error returned for an id that is empty, too long or has characters outside `[a-z0-9_]`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid entity id {0:?}: must be 1-64 characters of [a-z0-9_]")]
pub struct InvalidEntityId(pub String);

impl EntityId {
    pub const MAX_LEN: usize = 64;

    /// Creates an id from a literal. Panics (at compile time in const context)
    /// if the literal is not a valid id.
    pub const fn from_static(s: &'static str) -> Self {
        assert!(is_valid_id(s), "invalid entity id literal");
        Self(Cow::Borrowed(s))
    }

    /// Validates a runtime string.
    pub fn new(s: impl Into<String>) -> Result<Self, InvalidEntityId> {
        let s = s.into();
        if is_valid_id(&s) {
            Ok(Self(Cow::Owned(s)))
        } else {
            Err(InvalidEntityId(s))
        }
    }

    /// Turns arbitrary text (a device name, a MAC address) into a valid id.
    ///
    /// Lowercases ASCII, maps every other character run to a single `_`, trims
    /// underscores and truncates to [`Self::MAX_LEN`]. Empty input becomes `unnamed`.
    pub fn slugify(text: &str) -> Self {
        let mut out = String::with_capacity(text.len());
        for c in text.chars() {
            if c.is_ascii_alphanumeric() {
                out.push(c.to_ascii_lowercase());
            } else if !out.ends_with('_') {
                out.push('_');
            }
        }
        let mut out = out.trim_matches('_').to_owned();
        out.truncate(Self::MAX_LEN);
        let out = out.trim_end_matches('_');
        if out.is_empty() {
            Self(Cow::Borrowed("unnamed"))
        } else {
            Self(Cow::Owned(out.to_owned()))
        }
    }

    /// Joins `self` and a suffix with `_`, slugifying the suffix.
    pub fn join(&self, suffix: &str) -> Self {
        Self::slugify(&format!("{}_{}", self.0, suffix))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

const fn is_valid_id(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > EntityId::MAX_LEN {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_') {
            return false;
        }
        i += 1;
    }
    true
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for EntityId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Generates a Home Assistant string enum with a `Custom` escape hatch.
macro_rules! ha_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $value:literal),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum $name {
            $($variant,)*
            /// Any value HA accepts that has no variant here.
            Custom(String),
        }

        impl $name {
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $value,)*
                    Self::Custom(s) => s,
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }
    };
}

ha_enum!(
    /// HA `sensor` device class.
    SensorClass {
        Battery => "battery",
        Current => "current",
        DataRate => "data_rate",
        DataSize => "data_size",
        Duration => "duration",
        Energy => "energy",
        Enum => "enum",
        Frequency => "frequency",
        Power => "power",
        SignalStrength => "signal_strength",
        Temperature => "temperature",
        Timestamp => "timestamp",
        Voltage => "voltage",
    }
);

ha_enum!(
    /// HA `binary_sensor` device class.
    BinarySensorClass {
        Battery => "battery",
        BatteryCharging => "battery_charging",
        Connectivity => "connectivity",
        Lock => "lock",
        Occupancy => "occupancy",
        Plug => "plug",
        Power => "power",
        Presence => "presence",
        Problem => "problem",
        Running => "running",
        Sound => "sound",
        Update => "update",
    }
);

ha_enum!(
    /// HA sensor `state_class`.
    StateClass {
        Measurement => "measurement",
        Total => "total",
        TotalIncreasing => "total_increasing",
    }
);

ha_enum!(
    /// HA `switch` device class.
    SwitchClass {
        Outlet => "outlet",
        Switch => "switch",
    }
);

ha_enum!(
    /// HA `button` device class.
    ButtonClass {
        Identify => "identify",
        Restart => "restart",
        Update => "update",
    }
);

ha_enum!(
    /// HA `event` device class.
    EventClass {
        Button => "button",
        Doorbell => "doorbell",
        Motion => "motion",
    }
);

/// HA `entity_category`: hides an entity from default dashboards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityCategory {
    Config,
    Diagnostic,
}

/// How HA renders a `number` entity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NumberMode {
    #[default]
    Auto,
    Box,
    Slider,
}

/// Where an `image` entity's content comes from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ImageSource {
    /// State is a URL HA fetches itself.
    Url,
    /// State is raw image bytes ([`State::Bytes`]) of this content type.
    Bytes { content_type: Cow<'static, str> },
}

/// Which availability an entity follows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum AvailabilityScope {
    /// Unavailable when Deskhand is offline or the owning integration is down.
    #[default]
    Integration,
    /// Always available, so it can show a meaningful state (`sleeping`, `off`)
    /// while Deskhand itself is offline.
    Always,
}

/// The kind of entity, mapping 1:1 onto a Home Assistant entity platform.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum EntityKind {
    Sensor {
        device_class: Option<SensorClass>,
        state_class: Option<StateClass>,
        unit: Option<Cow<'static, str>>,
        precision: Option<u8>,
        /// Allowed values for `SensorClass::Enum` sensors.
        options: Option<Vec<String>>,
    },
    BinarySensor {
        device_class: Option<BinarySensorClass>,
    },
    Switch {
        device_class: Option<SwitchClass>,
    },
    Button {
        device_class: Option<ButtonClass>,
    },
    Number {
        min: f64,
        max: f64,
        step: f64,
        unit: Option<Cow<'static, str>>,
        mode: NumberMode,
    },
    Select {
        options: Vec<String>,
    },
    Text {
        min: u16,
        max: u16,
        pattern: Option<String>,
    },
    Event {
        event_types: Vec<String>,
        device_class: Option<EventClass>,
    },
    Notify,
    Lock,
    Image {
        source: ImageSource,
    },
    Update,
    /// Rendered only by transports that support a media player
    /// (the HACS `mqtt_media_player` compatibility mode).
    MediaPlayer,
}

impl EntityKind {
    /// The HA platform name (`sensor`, `binary_sensor`, ...).
    pub fn platform(&self) -> &'static str {
        match self {
            Self::Sensor { .. } => "sensor",
            Self::BinarySensor { .. } => "binary_sensor",
            Self::Switch { .. } => "switch",
            Self::Button { .. } => "button",
            Self::Number { .. } => "number",
            Self::Select { .. } => "select",
            Self::Text { .. } => "text",
            Self::Event { .. } => "event",
            Self::Notify => "notify",
            Self::Lock => "lock",
            Self::Image { .. } => "image",
            Self::Update => "update",
            Self::MediaPlayer => "media_player",
        }
    }

    /// Whether HA can send commands to this kind of entity.
    pub fn accepts_commands(&self) -> bool {
        matches!(
            self,
            Self::Switch { .. }
                | Self::Button { .. }
                | Self::Number { .. }
                | Self::Select { .. }
                | Self::Text { .. }
                | Self::Notify
                | Self::Lock
                | Self::MediaPlayer
        )
    }

    /// A plain sensor; refine with the `EntitySpec` builder methods.
    pub fn sensor() -> Self {
        Self::Sensor {
            device_class: None,
            state_class: None,
            unit: None,
            precision: None,
            options: None,
        }
    }
}

/// Full description of one entity.
#[derive(Debug, Clone, PartialEq)]
pub struct EntitySpec {
    pub id: EntityId,
    /// Display name. `None` makes this the device's main entity (named after the device).
    pub name: Option<String>,
    pub kind: EntityKind,
    /// Material Design icon, for example `mdi:monitor`.
    pub icon: Option<String>,
    pub category: Option<EntityCategory>,
    pub enabled_by_default: bool,
    pub availability: AvailabilityScope,
    /// Whether state messages are retained. Events are never retained.
    pub retain: bool,
    /// Minimum time between two published state updates; extra updates are coalesced.
    pub min_interval: Option<Duration>,
}

impl EntitySpec {
    pub fn new(id: EntityId, name: impl Into<String>, kind: EntityKind) -> Self {
        let retain = !matches!(kind, EntityKind::Event { .. });
        Self {
            id,
            name: Some(name.into()),
            kind,
            icon: None,
            category: None,
            enabled_by_default: true,
            availability: AvailabilityScope::Integration,
            retain,
            min_interval: None,
        }
    }

    pub fn sensor(id: EntityId, name: impl Into<String>) -> Self {
        Self::new(id, name, EntityKind::sensor())
    }

    pub fn binary_sensor(
        id: EntityId,
        name: impl Into<String>,
        device_class: Option<BinarySensorClass>,
    ) -> Self {
        Self::new(id, name, EntityKind::BinarySensor { device_class })
    }

    pub fn switch(id: EntityId, name: impl Into<String>) -> Self {
        Self::new(id, name, EntityKind::Switch { device_class: None })
    }

    pub fn button(id: EntityId, name: impl Into<String>) -> Self {
        Self::new(id, name, EntityKind::Button { device_class: None })
    }

    pub fn number(id: EntityId, name: impl Into<String>, min: f64, max: f64, step: f64) -> Self {
        Self::new(
            id,
            name,
            EntityKind::Number {
                min,
                max,
                step,
                unit: None,
                mode: NumberMode::Auto,
            },
        )
    }

    pub fn select(id: EntityId, name: impl Into<String>, options: Vec<String>) -> Self {
        Self::new(id, name, EntityKind::Select { options })
    }

    pub fn text(id: EntityId, name: impl Into<String>) -> Self {
        Self::new(
            id,
            name,
            EntityKind::Text {
                min: 0,
                max: 255,
                pattern: None,
            },
        )
    }

    pub fn event(id: EntityId, name: impl Into<String>, event_types: Vec<String>) -> Self {
        Self::new(
            id,
            name,
            EntityKind::Event {
                event_types,
                device_class: None,
            },
        )
    }

    pub fn notify(id: EntityId, name: impl Into<String>) -> Self {
        Self::new(id, name, EntityKind::Notify)
    }

    pub fn lock(id: EntityId, name: impl Into<String>) -> Self {
        Self::new(id, name, EntityKind::Lock)
    }

    pub fn image(id: EntityId, name: impl Into<String>, source: ImageSource) -> Self {
        Self::new(id, name, EntityKind::Image { source })
    }

    /// Makes this the device's main entity (no own name).
    pub fn unnamed(mut self) -> Self {
        self.name = None;
        self
    }

    pub fn icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    pub fn category(mut self, category: EntityCategory) -> Self {
        self.category = Some(category);
        self
    }

    pub fn diagnostic(self) -> Self {
        self.category(EntityCategory::Diagnostic)
    }

    pub fn disabled_by_default(mut self) -> Self {
        self.enabled_by_default = false;
        self
    }

    pub fn always_available(mut self) -> Self {
        self.availability = AvailabilityScope::Always;
        self
    }

    pub fn no_retain(mut self) -> Self {
        self.retain = false;
        self
    }

    pub fn min_interval(mut self, interval: Duration) -> Self {
        self.min_interval = Some(interval);
        self
    }

    /// Sets the unit of a sensor or number. No effect on other kinds.
    pub fn unit(mut self, unit: impl Into<Cow<'static, str>>) -> Self {
        match &mut self.kind {
            EntityKind::Sensor { unit: u, .. } | EntityKind::Number { unit: u, .. } => {
                *u = Some(unit.into());
            }
            _ => {}
        }
        self
    }

    /// Sets the device class of a sensor. No effect on other kinds.
    pub fn sensor_class(mut self, class: SensorClass) -> Self {
        if let EntityKind::Sensor { device_class, .. } = &mut self.kind {
            *device_class = Some(class);
        }
        self
    }

    /// Sets the state class of a sensor. No effect on other kinds.
    pub fn state_class(mut self, class: StateClass) -> Self {
        if let EntityKind::Sensor { state_class, .. } = &mut self.kind {
            *state_class = Some(class);
        }
        self
    }

    /// Sets the display precision of a sensor. No effect on other kinds.
    pub fn precision(mut self, digits: u8) -> Self {
        if let EntityKind::Sensor { precision, .. } = &mut self.kind {
            *precision = Some(digits);
        }
        self
    }

    /// Turns a sensor into an enum sensor with the given options.
    pub fn enum_options(mut self, values: Vec<String>) -> Self {
        if let EntityKind::Sensor {
            device_class,
            options,
            ..
        } = &mut self.kind
        {
            *device_class = Some(SensorClass::Enum);
            *options = Some(values);
        }
        self
    }
}

/// The current value of an entity.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum State {
    /// No value yet (rendered as HA `unknown`).
    #[default]
    Unknown,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Json(serde_json::Value),
    Bytes(bytes::Bytes),
    /// Full state of a media player entity ([`EntityKind::MediaPlayer`]).
    Media(Box<MediaPlayerState>),
}

/// What a media player entity shows. Transports that render media players
/// publish each field on its own and only when it changed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaPlayerState {
    /// `playing`, `paused`, `stopped` or `idle` (no player).
    pub state: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Track length in whole seconds.
    pub duration: Option<u64>,
    /// Playback position in whole seconds, as of when this state was set.
    pub position: Option<u64>,
    /// 0.0 to 1.0.
    pub volume: Option<f64>,
    /// `music`, `video`, ...
    pub media_type: Option<String>,
    /// Cover art image bytes.
    pub art: Option<bytes::Bytes>,
}

impl From<MediaPlayerState> for State {
    fn from(v: MediaPlayerState) -> Self {
        Self::Media(Box::new(v))
    }
}

impl From<bool> for State {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

macro_rules! state_from_int {
    ($($t:ty),*) => {$(
        impl From<$t> for State {
            fn from(v: $t) -> Self {
                Self::Int(i64::from(v))
            }
        }
    )*};
}
state_from_int!(i8, i16, i32, i64, u8, u16, u32);

impl From<u64> for State {
    fn from(v: u64) -> Self {
        Self::Int(i64::try_from(v).unwrap_or(i64::MAX))
    }
}

impl From<f64> for State {
    fn from(v: f64) -> Self {
        Self::Float(v)
    }
}

impl From<f32> for State {
    fn from(v: f32) -> Self {
        Self::Float(f64::from(v))
    }
}

impl From<String> for State {
    fn from(v: String) -> Self {
        Self::Text(v)
    }
}

impl From<&str> for State {
    fn from(v: &str) -> Self {
        Self::Text(v.to_owned())
    }
}

impl From<serde_json::Value> for State {
    fn from(v: serde_json::Value) -> Self {
        Self::Json(v)
    }
}

impl From<bytes::Bytes> for State {
    fn from(v: bytes::Bytes) -> Self {
        Self::Bytes(v)
    }
}

impl<T: Into<State>> From<Option<T>> for State {
    fn from(v: Option<T>) -> Self {
        v.map_or(Self::Unknown, Into::into)
    }
}

/// A lock command from HA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LockCmd {
    Lock,
    Unlock,
    Open,
}

/// A command for a media player entity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MediaCmd {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
    /// Absolute position in seconds.
    Seek(f64),
    /// 0.0 to 1.0.
    Volume(f64),
}

/// A command parsed by the transport, before it is routed to a typed handle.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandValue {
    Press,
    Bool(bool),
    Number(f64),
    Text(String),
    Lock(LockCmd),
    Json(serde_json::Value),
    Media(MediaCmd),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_id_validation() {
        assert!(EntityId::new("cpu_percent").is_ok());
        assert!(EntityId::new("a1").is_ok());
        assert!(EntityId::new("").is_err());
        assert!(EntityId::new("Bad").is_err());
        assert!(EntityId::new("with-dash").is_err());
        assert!(EntityId::new("x".repeat(65)).is_err());
        const ID: EntityId = EntityId::from_static("media_state");
        assert_eq!(ID.as_str(), "media_state");
    }

    #[test]
    fn slugify_produces_valid_ids() {
        assert_eq!(EntityId::slugify("AA:BB:CC:11").as_str(), "aa_bb_cc_11");
        assert_eq!(
            EntityId::slugify("  Sony WH-1000XM4 ").as_str(),
            "sony_wh_1000xm4"
        );
        assert_eq!(EntityId::slugify("Ünïcode!!").as_str(), "n_code");
        assert_eq!(EntityId::slugify("***").as_str(), "unnamed");
        let long = EntityId::slugify(&"ab ".repeat(40));
        assert!(long.as_str().len() <= EntityId::MAX_LEN);
        assert!(!long.as_str().ends_with('_'));
        assert_eq!(
            EntityId::from_static("bt").join("AA:BB").as_str(),
            "bt_aa_bb"
        );
    }

    #[test]
    fn spec_builders_and_defaults() {
        let s = EntitySpec::sensor(EntityId::from_static("cpu"), "CPU")
            .unit("%")
            .state_class(StateClass::Measurement)
            .precision(1);
        assert!(s.retain);
        assert_eq!(s.kind.platform(), "sensor");
        let EntityKind::Sensor {
            unit, state_class, ..
        } = &s.kind
        else {
            panic!("not a sensor");
        };
        assert_eq!(unit.as_deref(), Some("%"));
        assert_eq!(state_class, &Some(StateClass::Measurement));

        let e = EntitySpec::event(EntityId::from_static("ev"), "Ev", vec!["pressed".into()]);
        assert!(!e.retain, "events are never retained");
        assert!(!e.kind.accepts_commands());
        assert!(
            EntitySpec::switch(EntityId::from_static("sw"), "Sw")
                .kind
                .accepts_commands()
        );
    }

    #[test]
    fn ha_enums_serialize_as_strings() {
        assert_eq!(
            serde_json::to_string(&SensorClass::DataRate).unwrap(),
            "\"data_rate\""
        );
        assert_eq!(
            serde_json::to_string(&SensorClass::Custom("ph".into())).unwrap(),
            "\"ph\""
        );
        assert_eq!(
            serde_json::to_string(&EntityCategory::Diagnostic).unwrap(),
            "\"diagnostic\""
        );
    }

    #[test]
    fn state_conversions() {
        assert_eq!(State::from(true), State::Bool(true));
        assert_eq!(State::from(5u32), State::Int(5));
        assert_eq!(State::from(u64::MAX), State::Int(i64::MAX));
        assert_eq!(State::from("x"), State::Text("x".into()));
        assert_eq!(State::from(None::<i32>), State::Unknown);
    }
}
