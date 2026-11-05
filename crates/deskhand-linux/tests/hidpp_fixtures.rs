//! The M16 captures (tests/fixtures/hidpp) decode as observed on the host.

use hidpp::feature::backlight::BacklightEvent;
use hidpp::feature::unified_battery::{BatteryEvent, BatteryStatus};

/// (report bytes) of every `rx` line in a capture.
fn reports(name: &str) -> Vec<Vec<u8>> {
    let path = format!("{}/tests/fixtures/hidpp/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (_ms, dir, _node) = (f.next()?, f.next()?, f.next()?);
            (dir == "rx").then(|| f.map(|b| u8::from_str_radix(b, 16).unwrap()).collect())
        })
        .collect()
}

/// Unsolicited HID++ 2.0 long reports from slot 1 at `feature_index`: (function, payload).
fn events(reports: &[Vec<u8>], feature_index: u8) -> Vec<(u8, [u8; 16])> {
    reports
        .iter()
        .filter(|r| r.len() == 20 && r[0] == 0x11 && r[1] == 0x01 && r[2] == feature_index)
        .filter(|r| r[3] & 0x0f == 0) // software id 0: an event, not a reply
        .map(|r| (r[3] >> 4, r[4..20].try_into().unwrap()))
        .collect()
}

#[test]
fn battery_events_follow_the_cable_and_the_wake() {
    let all = reports("mx-keys-s-bolt-events.txt");
    let states: Vec<BatteryStatus> = events(&all, 0x08)
        .into_iter()
        .map(|(f, p)| match BatteryEvent::decode(f, &p) {
            Some(BatteryEvent::InfoUpdate(info)) => {
                assert_eq!(info.charging_percentage, 100);
                info.status
            }
            other => panic!("not a battery event: {other:?}"),
        })
        .collect();
    // Cable in, cable out, then the report sent by itself on wake.
    assert_eq!(
        states,
        [
            BatteryStatus::Charging,
            BatteryStatus::Discharging,
            BatteryStatus::Discharging
        ]
    );
}

#[test]
fn backlight_key_presses_are_events() {
    let all = reports("mx-keys-s-bolt-events.txt");
    let levels: Vec<u8> = events(&all, 0x0c)
        .into_iter()
        .map(|(f, p)| match BacklightEvent::decode(f, &p) {
            Some(BacklightEvent::InfoChanged(info)) => info.current_level,
            other => panic!("not a backlight event: {other:?}"),
        })
        .collect();
    assert_eq!(levels, [2, 2, 1, 0, 1, 2, 3]);
}

#[test]
fn sleep_and_wake_are_link_notifications() {
    let all = reports("mx-keys-s-bolt-events.txt");
    // 0x41 device connection from slot 1: bit 6 of byte 4 set = link down.
    let links: Vec<bool> = all
        .iter()
        .filter(|r| r.len() == 7 && r[0] == 0x10 && r[1] == 0x01 && r[2] == 0x41)
        .map(|r| r[4] & 0x40 == 0)
        .collect();
    assert_eq!(links, [false, true]);
}

#[test]
fn setup_capture_has_the_identity() {
    let all = reports("mx-keys-s-bolt-setup.txt");
    let serial = all
        .iter()
        .find(|r| r.len() == 20 && r[1] == 0x01 && r[2] == 0x02 && r[3] == 0x2b)
        .map(|r| String::from_utf8_lossy(&r[4..16]).into_owned());
    assert_eq!(serial.as_deref(), Some("1234AB567CD8"));
    let codename = all
        .iter()
        .find(|r| r.len() == 20 && r[1] == 0xff && r[3] == 0xb5 && r[4] == 0x61)
        .map(|r| String::from_utf8_lossy(&r[7..7 + usize::from(r[6])]).into_owned());
    assert_eq!(codename.as_deref(), Some("MX KEYS S"));
}
