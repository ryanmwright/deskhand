//! Key sequences for kglobalaccel: `"Meta+Shift+F12"` ↔ Qt key codes.
//!
//! On the wire a sequence is `(ai)` with **exactly four ints**, one per
//! chord, padded with 0. KF6 demarshals a `QKeySequence` as four ints, and a
//! shorter array makes libdbus abort inside KWin, which hosts kglobalaccel
//! (docs/dev/spikes.md §3). [`KeySequence`] can only be built from this
//! module's parser, and [`KeySequence::wire`] always yields four ints.

use std::fmt;

const SHIFT: i32 = 0x0200_0000;
const CTRL: i32 = 0x0400_0000;
const ALT: i32 = 0x0800_0000;
const META: i32 = 0x1000_0000;
const KEYPAD: i32 = 0x2000_0000;
const MODIFIER_MASK: i32 = SHIFT | CTRL | ALT | META | KEYPAD;

/// In display order, with accepted aliases (lowercase).
const MODIFIERS: &[(i32, &str, &[&str])] = &[
    (META, "Meta", &["meta", "super", "win"]),
    (CTRL, "Ctrl", &["ctrl", "control"]),
    (ALT, "Alt", &["alt"]),
    (SHIFT, "Shift", &["shift"]),
    (KEYPAD, "Num", &["num", "keypad"]),
];

/// Named Qt keys (`Qt::Key`), with accepted aliases (lowercase). Printable
/// ASCII keys are their (uppercase) character code.
const NAMED: &[(i32, &str, &[&str])] = &[
    (0x0100_0000, "Esc", &["esc", "escape"]),
    (0x0100_0001, "Tab", &["tab"]),
    (0x0100_0003, "Backspace", &["backspace"]),
    (0x0100_0004, "Return", &["return"]),
    (0x0100_0005, "Enter", &["enter"]),
    (0x0100_0006, "Ins", &["ins", "insert"]),
    (0x0100_0007, "Del", &["del", "delete"]),
    (0x0100_0008, "Pause", &["pause"]),
    (0x0100_0009, "Print", &["print", "printscreen"]),
    (0x0100_000a, "SysReq", &["sysreq"]),
    (0x0100_0010, "Home", &["home"]),
    (0x0100_0011, "End", &["end"]),
    (0x0100_0012, "Left", &["left"]),
    (0x0100_0013, "Up", &["up"]),
    (0x0100_0014, "Right", &["right"]),
    (0x0100_0015, "Down", &["down"]),
    (0x0100_0016, "PgUp", &["pgup", "pageup"]),
    (0x0100_0017, "PgDown", &["pgdown", "pagedown"]),
    (0x0100_0024, "CapsLock", &["capslock"]),
    (0x0100_0025, "NumLock", &["numlock"]),
    (0x0100_0026, "ScrollLock", &["scrolllock"]),
    (0x0100_0055, "Menu", &["menu"]),
    (0x0100_0058, "Help", &["help"]),
    (0x0100_0070, "Volume Down", &["volume down", "volumedown"]),
    (0x0100_0071, "Volume Mute", &["volume mute", "volumemute"]),
    (0x0100_0072, "Volume Up", &["volume up", "volumeup"]),
    (0x0100_0080, "Media Play", &["media play", "mediaplay"]),
    (0x0100_0081, "Media Stop", &["media stop", "mediastop"]),
    (
        0x0100_0082,
        "Media Previous",
        &["media previous", "mediaprevious"],
    ),
    (0x0100_0083, "Media Next", &["media next", "medianext"]),
    (
        0x0100_0084,
        "Media Record",
        &["media record", "mediarecord"],
    ),
    (0x0100_0085, "Media Pause", &["media pause", "mediapause"]),
    (
        0x0100_0086,
        "Toggle Media Play/Pause",
        &[
            "toggle media play/pause",
            "mediatoggleplaypause",
            "mediaplaypause",
        ],
    ),
    (0x20, "Space", &["space"]),
];

const F1: i32 = 0x0100_0030;
const F35: i32 = 0x0100_0052;

/// One key sequence: up to four chords (`"Ctrl+K, Ctrl+C"`), each a key
/// with modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeySequence([i32; 4]);

/// The D-Bus form of a sequence: `(ai)`, always four ints.
pub type WireSequence = (Vec<i32>,);

impl KeySequence {
    /// Parses `"Meta+Shift+F12"`. Chords are separated by `", "`; modifiers
    /// and named keys ignore case.
    pub fn parse(text: &str) -> Result<Self, String> {
        let chords: Vec<&str> = text.split(", ").map(str::trim).collect();
        if chords.len() > 4 {
            return Err(format!("{text:?}: at most 4 chords"));
        }
        let mut out = [0; 4];
        for (slot, chord) in out.iter_mut().zip(&chords) {
            *slot = parse_chord(chord).map_err(|e| format!("{text:?}: {e}"))?;
        }
        Ok(Self(out))
    }

    /// The `(ai)` struct kglobalaccel expects: exactly four ints.
    pub fn wire(&self) -> WireSequence {
        (self.0.to_vec(),)
    }

    /// Reads a sequence kglobalaccel sent. Extra ints are ignored, missing
    /// ones are 0.
    pub fn from_wire(seq: &WireSequence) -> Self {
        let mut out = [0; 4];
        for (slot, v) in out.iter_mut().zip(&seq.0) {
            *slot = *v;
        }
        Self(out)
    }

    pub fn is_empty(&self) -> bool {
        self.0[0] == 0
    }
}

fn parse_chord(chord: &str) -> Result<i32, String> {
    if chord.is_empty() {
        return Err("empty key".into());
    }
    // "Ctrl++" and "+" mean the plus key.
    let (mods, key) = if chord == "+" {
        ("", "+")
    } else if let Some(m) = chord.strip_suffix("++") {
        (m, "+")
    } else {
        match chord.rsplit_once('+') {
            Some((m, k)) => (m, k),
            None => ("", chord),
        }
    };
    let mut code = 0;
    if !mods.is_empty() {
        for m in mods.split('+') {
            let lower = m.trim().to_ascii_lowercase();
            let (bit, _, _) = MODIFIERS
                .iter()
                .find(|(_, _, aliases)| aliases.contains(&lower.as_str()))
                .ok_or_else(|| format!("unknown modifier {m:?}"))?;
            code |= bit;
        }
    }
    Ok(code | parse_key(key.trim())?)
}

fn parse_key(key: &str) -> Result<i32, String> {
    let lower = key.to_ascii_lowercase();
    if let Some((code, _, _)) = NAMED
        .iter()
        .find(|(_, _, aliases)| aliases.contains(&lower.as_str()))
    {
        return Ok(*code);
    }
    if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<i32>().ok())
        && (1..=35).contains(&n)
    {
        return Ok(F1 + n - 1);
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_graphic() => Ok(c.to_ascii_uppercase() as i32),
        _ => Err(format!("unknown key {key:?}")),
    }
}

fn format_chord(code: i32, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for (bit, name, _) in MODIFIERS {
        if code & bit != 0 {
            write!(f, "{name}+")?;
        }
    }
    let key = code & !MODIFIER_MASK;
    if let Some((_, name, _)) = NAMED.iter().find(|(c, _, _)| *c == key) {
        f.write_str(name)
    } else if (F1..=F35).contains(&key) {
        write!(f, "F{}", key - F1 + 1)
    } else if let Some(c) = u8::try_from(key).ok().filter(u8::is_ascii_graphic) {
        write!(f, "{}", c as char)
    } else {
        write!(f, "0x{key:x}")
    }
}

impl fmt::Display for KeySequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, chord) in self.0.iter().take_while(|c| **c != 0).enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            format_chord(*chord, f)?;
        }
        Ok(())
    }
}

/// Parses each of `texts`; the D-Bus `a(ai)` list.
pub fn parse_all(texts: &[String]) -> Result<Vec<KeySequence>, String> {
    texts.iter().map(|t| KeySequence::parse(t)).collect()
}

/// `a(ai)` for kglobalaccel, four ints per sequence.
pub fn wire_all(seqs: &[KeySequence]) -> Vec<WireSequence> {
    seqs.iter().map(KeySequence::wire).collect()
}

/// Readable form of what kglobalaccel returned, e.g. `["Meta+Shift+F12"]`.
pub fn describe(seqs: &[WireSequence]) -> Vec<String> {
    seqs.iter()
        .map(KeySequence::from_wire)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use zbus::zvariant::{Type, serialized::Context, to_bytes};

    use super::*;

    #[test]
    fn parses_like_the_spike() {
        // The spike's working call: Meta+Shift+F12 → 0x1300003b.
        let s = KeySequence::parse("Meta+Shift+F12").unwrap();
        assert_eq!(s.wire(), (vec![0x1300_003b, 0, 0, 0],));
        assert_eq!(s, KeySequence::parse("shift+super+f12").unwrap());
    }

    #[test]
    fn every_sequence_is_exactly_four_ints() {
        for text in ["A", "Ctrl+K, Ctrl+C", "Meta+1, 2, 3, 4", "Volume Up"] {
            let (ints,) = KeySequence::parse(text).unwrap().wire();
            assert_eq!(ints.len(), 4, "{text}");
        }
        let (ints,) = KeySequence::from_wire(&(vec![0x41],)).wire();
        assert_eq!(ints, vec![0x41, 0, 0, 0]);
        let (ints,) = KeySequence::from_wire(&(vec![1, 2, 3, 4, 5, 6],)).wire();
        assert_eq!(ints, vec![1, 2, 3, 4]);
    }

    #[test]
    fn wire_format_is_a_struct_of_four_ints() {
        // a(ai) with one sequence: array length, padding, then (ai) holding
        // four 4-byte ints (16 bytes).
        assert_eq!(<Vec<WireSequence>>::SIGNATURE.to_string(), "a(ai)");
        let seqs = wire_all(&parse_all(&["Ctrl+A".into()]).unwrap());
        let bytes = to_bytes(Context::new_dbus(zbus::zvariant::LE, 0), &seqs).unwrap();
        let inner_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        assert_eq!(inner_len, 16);
    }

    #[test]
    fn keys_and_modifiers() {
        let code = |t: &str| KeySequence::parse(t).unwrap().0[0];
        assert_eq!(code("a"), 0x41);
        assert_eq!(code("Ctrl+Alt+Del"), CTRL | ALT | 0x0100_0007);
        assert_eq!(code("Ctrl++"), CTRL | 0x2b);
        assert_eq!(code("+"), 0x2b);
        assert_eq!(code("Meta+,"), META | 0x2c);
        assert_eq!(code("F35"), F35);
        assert_eq!(code("Meta+Space"), META | 0x20);
        assert_eq!(code("Media Play"), 0x0100_0080);
        assert_eq!(code("Num+5"), KEYPAD | 0x35);
        for bad in [
            "",
            "Hyper+A",
            "Ctrl+",
            "F36",
            "Ctrl+Foo",
            "a, b, c, d, e",
            "é",
        ] {
            assert!(KeySequence::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn formats_back() {
        for text in [
            "Meta+Shift+F12",
            "Ctrl+K, Ctrl+C",
            "Ctrl+Alt+Del",
            "Meta+Space",
            "Ctrl++",
            "Volume Up",
        ] {
            assert_eq!(KeySequence::parse(text).unwrap().to_string(), text);
        }
        assert_eq!(
            describe(&[(vec![0x1300_003b, 0, 0, 0],), (vec![0, 0, 0, 0],)]),
            vec!["Meta+Shift+F12"]
        );
    }
}
