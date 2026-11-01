//! Encoding states and decoding commands, per Home Assistant's MQTT conventions.

use base64::Engine;
use bytes::Bytes;
use deskhand_core::entity::{Attributes, EntityKind, LockCmd, MediaCmd, MediaPlayerState};
use deskhand_core::{CommandValue, State};

use crate::topics::{MEDIA_SEEK, MEDIA_VOLUME};

/// HA treats the payload `None` as "unknown" for state topics.
pub const UNKNOWN: &str = "None";

/// Encodes a state for an entity's state topic.
pub fn encode_state(kind: &EntityKind, state: &State) -> Bytes {
    match state {
        // An image has no "unknown" payload; empty clears the retained image.
        State::Unknown if matches!(kind, EntityKind::Image { .. }) => Bytes::new(),
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
        // Media players are published field by field (`media_fields`).
        State::Media(m) => Bytes::from(m.state.clone()),
    }
}

/// The compat media player's fields as `(topic suffix, payload)`. An empty
/// payload means "none" to the `mqtt_media_player` component.
pub fn media_fields(state: &State) -> Vec<(&'static str, Bytes)> {
    let empty = MediaPlayerState::default();
    let m = match state {
        State::Media(m) => m.as_ref(),
        _ => &empty,
    };
    let text = |v: &Option<String>| Bytes::from(v.clone().unwrap_or_default());
    let num = |v: Option<u64>| Bytes::from(v.map(|n| n.to_string()).unwrap_or_default());
    vec![
        ("state", Bytes::from(m.state.clone())),
        ("title", text(&m.title)),
        ("artist", text(&m.artist)),
        ("album", text(&m.album)),
        ("duration", num(m.duration)),
        ("position", num(m.position)),
        (
            "volume",
            Bytes::from(m.volume.map(|v| v.to_string()).unwrap_or_default()),
        ),
        ("mediatype", text(&m.media_type)),
        (
            "albumart",
            Bytes::from(
                m.art
                    .as_ref()
                    .map(|a| base64::engine::general_purpose::STANDARD.encode(a))
                    .unwrap_or_default(),
            ),
        ),
    ]
}

/// Decodes a command on one of the compat media player's topics: `set`
/// (play/pause/next/previous/stop), `seek` (seconds) or `set_volume` (0-1).
pub fn decode_media_command(command: &str, payload: &[u8]) -> Option<CommandValue> {
    let text = std::str::from_utf8(payload).ok()?.trim();
    let number = || text.parse::<f64>().ok().filter(|n| n.is_finite());
    let cmd = match command {
        MEDIA_SEEK => MediaCmd::Seek(number()?.max(0.0)),
        MEDIA_VOLUME => MediaCmd::Volume(number()?.clamp(0.0, 1.0)),
        _ => match text {
            "Play" => MediaCmd::Play,
            "Pause" => MediaCmd::Pause,
            "PlayPause" => MediaCmd::PlayPause,
            "Stop" => MediaCmd::Stop,
            "Next" => MediaCmd::Next,
            "Previous" => MediaCmd::Previous,
            _ => return None,
        },
    };
    Some(CommandValue::Media(cmd))
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
        | EntityKind::Update => Some(CommandValue::Text(text.to_owned())),
        EntityKind::MediaPlayer => decode_media_command("set", payload),
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

    #[test]
    fn media_commands() {
        let media = |c| Some(CommandValue::Media(c));
        assert_eq!(
            decode_command(&EntityKind::MediaPlayer, b"PlayPause"),
            media(MediaCmd::PlayPause)
        );
        assert_eq!(decode_command(&EntityKind::MediaPlayer, b"Rewind"), None);
        assert_eq!(
            decode_media_command(MEDIA_SEEK, b"93.5"),
            media(MediaCmd::Seek(93.5))
        );
        assert_eq!(
            decode_media_command(MEDIA_VOLUME, b"1.7"),
            media(MediaCmd::Volume(1.0))
        );
        assert_eq!(decode_media_command(MEDIA_SEEK, b"inf"), None);
    }

    #[test]
    fn media_field_payloads() {
        let state = State::from(MediaPlayerState {
            state: "playing".into(),
            title: Some("Song".into()),
            duration: Some(200),
            position: Some(12),
            volume: Some(0.5),
            art: Some(Bytes::from_static(b"\xff\xd8art")),
            ..Default::default()
        });
        let fields: std::collections::BTreeMap<_, _> = media_fields(&state).into_iter().collect();
        assert_eq!(fields["state"], "playing");
        assert_eq!(fields["title"], "Song");
        assert_eq!(fields["artist"], "");
        assert_eq!(fields["duration"], "200");
        assert_eq!(fields["position"], "12");
        assert_eq!(fields["volume"], "0.5");
        assert_eq!(fields["albumart"], "/9hhcnQ=");
        let unknown = media_fields(&State::Unknown);
        assert!(unknown.iter().all(|(_, v)| v.is_empty()));
    }
}
