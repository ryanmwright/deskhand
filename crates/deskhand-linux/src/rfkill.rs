//! Bluetooth rfkill soft block. Plasma (and other desktops) turn Bluetooth
//! off with an rfkill soft block, and BlueZ then refuses `Powered = true`
//! ("Failed" / "Blocked through rfkill"); turning it back on means
//! unblocking first, as Plasma's own toggle does.
//!
//! State is read from `/sys/class/rfkill/*/{type,soft}`. Unblocking writes
//! one `rfkill_event` (`RFKILL_OP_CHANGE_ALL` for Bluetooth, soft = 0) to
//! `/dev/rfkill`, which systemd's `uaccess` rule makes writable for the
//! user of the active local session.

use std::io::Write;
use std::path::PathBuf;

const RFKILL_TYPE_BLUETOOTH: u8 = 2;
const RFKILL_OP_CHANGE_ALL: u8 = 3;

/// The 8-byte `struct rfkill_event` (`idx: u32, type, op, soft, hard: u8`)
/// that sets the soft block of every Bluetooth radio.
pub fn change_all_event(soft_block: bool) -> [u8; 8] {
    let mut e = [0u8; 8];
    // idx (ignored for CHANGE_ALL) stays 0.
    e[4] = RFKILL_TYPE_BLUETOOTH;
    e[5] = RFKILL_OP_CHANGE_ALL;
    e[6] = u8::from(soft_block);
    e
}

/// Where rfkill lives (overridable for tests).
#[derive(Debug, Clone)]
pub struct Rfkill {
    pub sys: PathBuf,
    pub dev: PathBuf,
}

impl Default for Rfkill {
    fn default() -> Self {
        Self {
            sys: "/sys/class/rfkill".into(),
            dev: "/dev/rfkill".into(),
        }
    }
}

impl Rfkill {
    /// Whether any Bluetooth radio is soft-blocked.
    pub fn bluetooth_soft_blocked(&self) -> bool {
        let Ok(entries) = std::fs::read_dir(&self.sys) else {
            return false;
        };
        entries.flatten().any(|e| {
            let read = |f: &str| std::fs::read_to_string(e.path().join(f)).unwrap_or_default();
            read("type").trim() == "bluetooth" && read("soft").trim() == "1"
        })
    }

    /// Lifts the soft block of every Bluetooth radio.
    pub fn unblock_bluetooth(&self) -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).open(&self.dev)?;
        f.write_all(&change_all_event(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_layout() {
        assert_eq!(change_all_event(false), [0, 0, 0, 0, 2, 3, 0, 0]);
        assert_eq!(change_all_event(true)[6], 1);
    }

    #[test]
    fn reads_soft_block_from_sysfs() {
        let dir = tempfile::tempdir().unwrap();
        let radio = |name: &str, ty: &str, soft: &str| {
            let p = dir.path().join(name);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("type"), format!("{ty}\n")).unwrap();
            std::fs::write(p.join("soft"), format!("{soft}\n")).unwrap();
        };
        radio("rfkill0", "wlan", "1");
        radio("rfkill1", "bluetooth", "0");
        let r = Rfkill {
            sys: dir.path().into(),
            dev: dir.path().join("dev"),
        };
        assert!(!r.bluetooth_soft_blocked(), "only wlan is blocked");
        radio("rfkill1", "bluetooth", "1");
        assert!(r.bluetooth_soft_blocked());
        std::fs::write(&r.dev, b"").unwrap();
        r.unblock_bluetooth().unwrap();
        assert_eq!(std::fs::read(&r.dev).unwrap(), change_all_event(false));
        assert!(
            Rfkill {
                sys: "/nonexistent".into(),
                dev: "/nonexistent".into()
            }
            .unblock_bluetooth()
            .is_err()
        );
    }
}
