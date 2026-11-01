//! Encoding states and decoding commands, per Home Assistant's MQTT conventions.

use bytes::Bytes;
use deskhand_core::entity::{Attributes, EntityKind, LockCmd};
use deskhand_core::{CommandValue, State};

/// HA treats the payload `None` as "unknown" for state topics.
pub const UNKNOWN: &str = "None";

/// Encodes a state for an entity's state topic.
pub fn encode_state(kind: &EntityKind, state: &State) -> Bytes {
    match state {
        State::Unknown => Bytes::from_static(UNKNOWN.as_bytes()),
        State::Bool(b) => {
            let on_off = if *b { "ON" } else { "OFF" };
            match kind {
                // Sensors with bool state (rare) read better as true/false.
                EntityKind::Sensor { .. } => Bytes::from(b.to_string()),
                _ => Bytes::from_static(on_off.as_bytes()),
            }
        }
        State::Int(i) => Bytes::from(i.to_string()),
        State::Float(f) if f.is_finite() => Bytes::from(f.to_string()),
        State::Float(_) => Bytes::from_static(UNKNOWN.as_bytes()),
        State::Text(t) => Bytes::from(t.clone()),
        State::Json(v) => Bytes::from(v.to_string()),
        State::Bytes(b) => b.clone(),
    }
}

/// Encodes an event for an `event` entity's state topic.
pub fn encode_event(event_type: &str, attributes: &Attributes) -> Bytes {
    let mut obj = attributes.clone();
    obj.insert("event_type".to_owned(), event_type.into());
    Bytes::from(serde_json::Value::Object(obj).to_string())
}

/// Decodes a command payload for an entity of `kind`. `None` means the
/// payload is not valid for that kind.
pub fn decode_command(kind: &EntityKind, payload: &[u8]) -> Option<CommandValue> {
    let text = std::str::from_utf8(payload).ok()?;
    match kind {
        EntityKind::Button { .. } => Some(CommandValue::Press),
        EntityKind::Switch { .. } => match text.trim() {
            "ON" => Some(CommandValue::Bool(true)),
            "OFF" => Some(CommandValue::Bool(false)),
            _ => None,
        },
        EntityKind::Number { .. } => text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .map(CommandValue::Number),
        EntityKind::Lock => match text.trim() {
            "LOCK" => Some(CommandValue::Lock(LockCmd::Lock)),
            "UNLOCK" => Some(CommandValue::Lock(LockCmd::Unlock)),
            "OPEN" => Some(CommandValue::Lock(LockCmd::Open)),
            _ => None,
        },
        EntityKind::Select { .. }
        | EntityKind::Text { .. }
        | EntityKind::Notify
        | EntityKind::Update
        | EntityKind::MediaPlayer => Some(CommandValue::Text(text.to_owned())),
        _ => None,
    }
}

/// Decodes a payload for a JSON command endpoint.
pub fn decode_json(payload: &[u8]) -> Option<CommandValue> {
    serde_json::from_slice(payload).ok().map(CommandValue::Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn switch() -> EntityKind {
        EntityKind::Switch { device_class: None }
    }

    #[test]
    fn states() {
        let bs = EntityKind::BinarySensor { device_class: None };
        assert_eq!(encode_state(&bs, &State::Bool(true)), "ON");
        assert_eq!(encode_state(&bs, &State::Unknown), "None");
        let s = EntityKind::sensor();
        assert_eq!(encode_state(&s, &State::Float(12.5)), "12.5");
        assert_eq!(encode_state(&s, &State::Float(f64::NAN)), "None");
        assert_eq!(encode_state(&s, &State::Float(1e-7)), "0.0000001");
        assert_eq!(encode_state(&s, &State::Int(-3)), "-3");
        assert_eq!(
            encode_state(&s, &State::Json(serde_json::json!({"a": 1}))),
            r#"{"a":1}"#
        );
    }

    #[test]
    fn events() {
        let mut attrs = Attributes::new();
        attrs.insert("tag".into(), "door".into());
        let v: serde_json::Value = serde_json::from_slice(&encode_event("action", &attrs)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"event_type": "action", "tag": "door"})
        );
    }

    #[test]
    fn commands() {
        assert_eq!(
            decode_command(&switch(), b"ON"),
            Some(CommandValue::Bool(true))
        );
        assert_eq!(decode_command(&switch(), b"maybe"), None);
        let num = EntityKind::Number {
            min: 0.0,
            max: 100.0,
            step: 1.0,
            unit: None,
            mode: Default::default(),
        };
        assert_eq!(
            decode_command(&num, b" 42.5 "),
            Some(CommandValue::Number(42.5))
        );
        assert_eq!(decode_command(&num, b"NaN"), None);
        assert_eq!(
            decode_command(&EntityKind::Button { device_class: None }, b"PRESS"),
            Some(CommandValue::Press)
        );
        assert_eq!(
            decode_command(&EntityKind::Lock, b"UNLOCK"),
            Some(CommandValue::Lock(LockCmd::Unlock))
        );
        assert_eq!(
            decode_command(&EntityKind::Notify, "héllo".as_bytes()),
            Some(CommandValue::Text("héllo".into()))
        );
        assert_eq!(decode_command(&EntityKind::sensor(), b"x"), None);
        assert_eq!(decode_command(&switch(), &[0xff, 0xfe]), None);
        assert!(decode_json(b"{\"title\":1}").is_some());
        assert!(decode_json(b"nope").is_none());
    }
}
