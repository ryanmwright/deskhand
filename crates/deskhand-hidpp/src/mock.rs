//! A scripted Logi Bolt receiver behind a [`RawHidChannel`], for tests.
//!
//! It answers the HID++ 1.0 register requests Deskhand sends to a receiver
//! and the HID++ 2.0 requests to its online devices, records every request,
//! and lets a test inject reports (link changes, battery events) or replay
//! the M16 captures. Requests to an offline device get no answer, like a
//! sleeping device; tests assert that none are sent.

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use hidpp::channel::RawHidChannel;

/// The receiver's unique id in the M16 captures (placeholder).
pub const UNIQUE_ID: &str = "AAAABBBBCCCC0001";

#[derive(Debug, Clone)]
pub enum MockBattery {
    /// 0x1004: percentage, level bits, status.
    Unified { percent: u8, level: u8, status: u8 },
    /// 0x1000: level, status.
    Status { level: u8, status: u8 },
    /// 0x1001: millivolts, flags.
    Voltage { mv: u16, flags: u8 },
}

/// 0x1982 (v3) state, as the device keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockBacklight {
    pub enabled: bool,
    /// Options byte: WOW, crown, power save, mode in bits 3..=4.
    pub options: u8,
    /// Capability byte (options bits 8..=15).
    pub caps: u8,
    /// The configured (manual) level.
    pub level: u8,
    pub levels: u8,
    /// The current level and status (`getBacklightInfo`).
    pub current: u8,
    pub status: u8,
    /// Hands out, hands in, powered, not powered; 5 s units.
    pub durations: [u16; 4],
}

impl MockBacklight {
    /// The MX Keys S of the M16 setup capture: `01 0d 3d 03 00 02 03 00 06
    /// 00 04 00`, 8 levels, level 2 in automatic mode.
    pub fn mx_keys_s() -> Self {
        Self {
            enabled: true,
            options: 0x0d,
            caps: 0x3d,
            level: 2,
            levels: 8,
            current: 2,
            status: 2,
            durations: [3, 6, 4, 0],
        }
    }

    pub fn mode(&self) -> u8 {
        (self.options >> 3) & 0b11
    }
}

#[derive(Debug, Clone)]
pub struct MockDevice {
    pub wpid: u16,
    pub kind: u8,
    pub codename: String,
    pub name: String,
    pub serial: String,
    pub online: bool,
    pub battery: MockBattery,
    pub backlight: Option<MockBacklight>,
}

impl MockDevice {
    /// The MX Keys S of the M16 captures (100 %, full, discharging).
    pub fn mx_keys_s() -> Self {
        Self {
            wpid: 0xb378,
            kind: 0x01,
            codename: "MX KEYS S".into(),
            name: "MX Keys S".into(),
            serial: "1234AB567CD8".into(),
            online: true,
            battery: MockBattery::Unified {
                percent: 100,
                level: 0x08,
                status: 0x00,
            },
            backlight: Some(MockBacklight::mx_keys_s()),
        }
    }

    /// Feature table: index → id. 0x0003 at 2, 0x0005 at 3, 0x1d4b at 4,
    /// the battery at 8 and the backlight at 0x0c, like the MX Keys S.
    fn features(&self) -> BTreeMap<u8, u16> {
        let battery = match self.battery {
            MockBattery::Unified { .. } => 0x1004,
            MockBattery::Status { .. } => 0x1000,
            MockBattery::Voltage { .. } => 0x1001,
        };
        let mut features = BTreeMap::from([
            (0x01, 0x0001),
            (0x02, 0x0003),
            (0x03, 0x0005),
            (0x04, 0x1d4b),
            (0x08, battery),
        ]);
        if self.backlight.is_some() {
            features.insert(0x0c, 0x1982);
        }
        features
    }
}

#[derive(Debug, Default)]
struct State {
    flags: [u8; 3],
    devices: BTreeMap<u8, MockDevice>,
    /// Every report written to the receiver.
    written: Vec<Vec<u8>>,
}

#[derive(Debug)]
struct Inner {
    state: Mutex<State>,
    tx: async_channel::Sender<Vec<u8>>,
    rx: async_channel::Receiver<Vec<u8>>,
    connected: AtomicBool,
}

/// The receiver; clone it to keep a handle after handing it to the source.
#[derive(Debug, Clone)]
pub struct MockReceiver(Arc<Inner>);

impl MockReceiver {
    pub fn new(devices: impl IntoIterator<Item = (u8, MockDevice)>) -> Self {
        let (tx, rx) = async_channel::unbounded();
        Self(Arc::new(Inner {
            state: Mutex::new(State {
                devices: devices.into_iter().collect(),
                ..State::default()
            }),
            tx,
            rx,
            connected: AtomicBool::new(true),
        }))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap()
    }

    /// Sends a report as if the receiver produced it.
    pub fn inject(&self, report: &[u8]) {
        let _ = self.0.tx.try_send(report.to_vec());
    }

    /// The device in `slot` sleeps or wakes: a 0x41 link notice.
    pub fn set_online(&self, slot: u8, online: bool) {
        let report = {
            let mut s = self.state();
            let Some(d) = s.devices.get_mut(&slot) else {
                return;
            };
            d.online = online;
            connection(slot, d)
        };
        self.inject(&report);
    }

    pub fn set_battery(&self, slot: u8, battery: MockBattery) {
        if let Some(d) = self.state().devices.get_mut(&slot) {
            d.battery = battery;
        }
    }

    /// The device's own battery broadcast (function 0 of its battery feature).
    pub fn battery_event(&self, slot: u8) {
        let report = {
            let s = self.state();
            let Some(d) = s.devices.get(&slot) else {
                return;
            };
            let mut r = vec![0x11, slot, 0x08, 0x00];
            r.extend(battery_payload(&d.battery));
            r.resize(20, 0);
            r
        };
        self.inject(&report);
    }

    /// The device's backlight, as last written.
    pub fn backlight(&self, slot: u8) -> Option<MockBacklight> {
        self.state().devices.get(&slot)?.backlight.clone()
    }

    /// Unpairs the device in `slot` (0x40 notice).
    pub fn unpair(&self, slot: u8) {
        self.state().devices.remove(&slot);
        self.inject(&[0x10, slot, 0x40, 0x02, 0x00, 0x00, 0x00]);
    }

    /// The receiver is unplugged: reads park, writes fail.
    pub fn unplug(&self) {
        self.0.connected.store(false, Ordering::SeqCst);
        self.0.tx.close();
    }

    /// Requests sent to the device in `slot` (HID++ 2.0, any feature).
    pub fn requests_to(&self, slot: u8) -> Vec<Vec<u8>> {
        self.state()
            .written
            .iter()
            .filter(|r| r.get(1) == Some(&slot))
            .cloned()
            .collect()
    }

    /// Battery reads (function 1 of 0x1004, function 0 of 0x1000/0x1001)
    /// sent to `slot`.
    pub fn battery_reads(&self, slot: u8) -> usize {
        self.requests_to(slot)
            .iter()
            .filter(|r| r[2] == 0x08)
            .filter(|r| {
                let f = r[3] >> 4;
                match self.state().devices.get(&slot).map(|d| &d.battery) {
                    Some(MockBattery::Unified { .. }) | None => f == 1,
                    _ => f == 0,
                }
            })
            .count()
    }

    /// The receiver's notification flags register.
    pub fn flags(&self) -> [u8; 3] {
        self.state().flags
    }

    fn reply(&self, req: &[u8]) -> Vec<Vec<u8>> {
        let mut s = self.state();
        s.written.push(req.to_vec());
        let dev = req[1];
        if dev == 0xff {
            return register(&mut s, req);
        }
        let Some(d) = s.devices.get_mut(&dev) else {
            return vec![vec![0x10, dev, 0x8f, req[2], req[3], 0x08, 0x00]];
        };
        if !d.online {
            return Vec::new();
        }
        let (fi, fsw, params) = (req[2], req[3], &req[4..]);
        let data = feature(d, dev, fi, fsw >> 4, params);
        match data {
            Some(data) => {
                let mut r = vec![0x11, dev, fi, fsw];
                r.extend(data);
                r.resize(20, 0);
                vec![r]
            }
            // HID++ 2.0 error: invalid function.
            None => vec![vec![
                0x11, dev, 0xff, fi, fsw, 0x07, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ]],
        }
    }
}

fn connection(slot: u8, d: &MockDevice) -> Vec<u8> {
    let info = d.kind | if d.online { 0 } else { 0x40 };
    let [lo, hi] = d.wpid.to_le_bytes();
    vec![0x10, slot, 0x41, 0x10, info, lo, hi]
}

fn battery_payload(b: &MockBattery) -> Vec<u8> {
    match *b {
        MockBattery::Unified {
            percent,
            level,
            status,
        } => vec![percent, level, status, 0x00],
        MockBattery::Status { level, status } => vec![level, 0, status],
        MockBattery::Voltage { mv, flags } => {
            let [hi, lo] = mv.to_be_bytes();
            vec![hi, lo, flags]
        }
    }
}

/// HID++ 1.0 register access on the receiver.
fn register(s: &mut State, req: &[u8]) -> Vec<Vec<u8>> {
    let (sub, addr) = (req[2], req[3]);
    let ack = |data: [u8; 3]| vec![0x10, 0xff, sub, addr, data[0], data[1], data[2]];
    let long = |data: &[u8]| {
        let mut r = vec![0x11, 0xff, sub, addr];
        r.extend(data);
        r.resize(20, 0);
        r
    };
    let error = |code: u8| vec![0x10, 0xff, 0x8f, sub, addr, code, 0x00];
    match (sub, addr) {
        (0x81, 0x00) => vec![ack(s.flags)],
        (0x80, 0x00) => {
            s.flags = [req[4], req[5], req[6]];
            vec![ack([0, 0, 0])]
        }
        (0x81, 0x02) => vec![ack([0, s.devices.len() as u8, 0])],
        (0x80, 0x02) if req[4] == 0x02 => {
            // Device arrival: one 0x41 per paired device, then the ack.
            let mut out: Vec<Vec<u8>> = s
                .devices
                .iter()
                .map(|(slot, d)| connection(*slot, d))
                .collect();
            out.push(ack([0, 0, 0]));
            out
        }
        (0x81, 0xf1) => match req[4] {
            0x01 => vec![ack([0x01, 0x05, 0x02])],
            0x02 => vec![ack([0x02, 0x00, 0x18])],
            _ => vec![error(0x02)],
        },
        (0x83, 0xfb) => vec![long(UNIQUE_ID.as_bytes())],
        (0x83, 0xb5) => {
            let info = req[4];
            let slot = info & 0x0f;
            let Some(d) = s.devices.get(&slot) else {
                return vec![error(0x08)];
            };
            match info & 0xf0 {
                0x50 => {
                    let [lo, hi] = d.wpid.to_le_bytes();
                    let status = d.kind | if d.online { 0 } else { 0x40 };
                    // Unit id 11 22 33 44 for slot 1, 11 22 33 45 for slot 2, ...
                    vec![long(&[info, status, lo, hi, 0x11, 0x22, 0x33, 0x43 + slot])]
                }
                // Chunk 1: [sub, 1, length, 13 bytes]; chunk 2: [sub, 2, rest].
                0x60 => {
                    let name = d.codename.as_bytes();
                    let data = if req[5] == 0x02 {
                        let mut data = vec![info, 0x02];
                        data.extend(name.get(13..).unwrap_or_default());
                        data
                    } else {
                        let mut data = vec![info, 0x01, name.len() as u8];
                        data.extend(&name[..name.len().min(13)]);
                        data
                    };
                    vec![long(&data)]
                }
                _ => vec![error(0x02)],
            }
        }
        _ => vec![error(0x02)],
    }
}

/// HID++ 2.0 function data of an online device, or `None` (error).
fn feature(
    d: &mut MockDevice,
    slot: u8,
    index: u8,
    function: u8,
    params: &[u8],
) -> Option<Vec<u8>> {
    let features = d.features();
    if index == 0 {
        return match function {
            // getFeature(id)
            0 => {
                let id = u16::from_be_bytes([params[0], params[1]]);
                let found = if id == 0x0000 {
                    Some(0)
                } else {
                    features.iter().find(|(_, f)| **f == id).map(|(i, _)| *i)
                };
                let version = if id == 0x1982 { 3 } else { 0 };
                Some(vec![found.unwrap_or(0), 0x00, version])
            }
            // ping: protocol 4.5
            1 => Some(vec![0x04, 0x05, params[2]]),
            _ => None,
        };
    }
    match (*features.get(&index)?, function) {
        (0x0001, 0) => Some(vec![features.len() as u8]),
        (0x0003, 0) => {
            let [hi, lo] = d.wpid.to_be_bytes();
            // The unit id the receiver has for the slot.
            Some(vec![
                2,
                0x11,
                0x22,
                0x33,
                0x43 + slot,
                0x00,
                0x04,
                hi,
                lo,
                0,
                0,
                0,
                0,
                0x00,
                0x01,
            ])
        }
        (0x0003, 1) => Some(match params[0] {
            0 => vec![0x01, b'B', b'L', b'1', 0x88, 0x03, 0x00, 0x18, 0x00],
            _ => vec![0x00, b'R', b'B', b'K', 0x81, 0x03, 0x00, 0x18, 0x01],
        }),
        (0x0003, 2) => Some(d.serial.as_bytes().to_vec()),
        (0x0005, 0) => Some(vec![d.name.len() as u8]),
        (0x0005, 1) => Some(d.name.as_bytes()[usize::from(params[0])..].to_vec()),
        (0x0005, 2) => Some(vec![0x00]),
        (0x1004, 0) => Some(vec![0x0f, 0x03]),
        (0x1004, 1) | (0x1000, 0) | (0x1001, 0) => Some(battery_payload(&d.battery)),
        (0x1000, 1) => Some(vec![0x04, 0x04]),
        (0x1982, f) => backlight(d.backlight.as_mut()?, f, params),
        _ => None,
    }
}

/// 0x1982: getBacklightConfig, setBacklightConfig, getBacklightInfo.
fn backlight(b: &mut MockBacklight, function: u8, params: &[u8]) -> Option<Vec<u8>> {
    let le = |v: u16| v.to_le_bytes();
    match function {
        0 => {
            let mut r = vec![u8::from(b.enabled), b.options, b.caps, 0x03, 0x00, b.level];
            for d in b.durations {
                r.extend(le(d));
            }
            Some(r)
        }
        1 => {
            let word = |i: usize| u16::from_le_bytes([params[i], params[i + 1]]);
            b.enabled = params[0] != 0;
            b.options = params[1];
            b.level = params[3];
            b.durations = [word(4), word(6), word(8), word(10)];
            // What the keyboard shows now.
            b.status = match (b.enabled, b.mode()) {
                (false, _) => 0,
                (true, 1) => 2,
                (true, 3) => {
                    b.current = b.level;
                    5
                }
                _ => b.status,
            };
            Some(Vec::new())
        }
        2 => {
            let mut r = vec![b.levels, b.current, b.status, 0x00];
            for d in [3, 6, 60, 0] {
                r.extend(le(d));
            }
            Some(r)
        }
        _ => None,
    }
}

#[async_trait::async_trait]
impl RawHidChannel for MockReceiver {
    fn vendor_id(&self) -> u16 {
        0x046d
    }

    fn product_id(&self) -> u16 {
        0xc548
    }

    async fn write_report(&self, src: &[u8]) -> Result<usize, Box<dyn Error + Sync + Send>> {
        if !self.0.connected.load(Ordering::SeqCst) {
            return Err(Box::new(std::io::Error::from_raw_os_error(19)));
        }
        for r in self.reply(src) {
            let _ = self.0.tx.try_send(r);
        }
        Ok(src.len())
    }

    async fn read_report(&self, buf: &mut [u8]) -> Result<usize, Box<dyn Error + Sync + Send>> {
        match self.0.rx.recv().await {
            Ok(r) => {
                let n = r.len().min(buf.len());
                buf[..n].copy_from_slice(&r[..n]);
                Ok(n)
            }
            // Unplugged: park; the read loop's close signal ends it.
            Err(_) => std::future::pending().await,
        }
    }

    fn is_connected(&self) -> bool {
        self.0.connected.load(Ordering::SeqCst)
    }

    fn supports_short_long_hidpp(&self) -> Option<(bool, bool)> {
        Some((true, true))
    }

    async fn get_report_descriptor(
        &self,
        _buf: &mut [u8],
    ) -> Result<usize, Box<dyn Error + Sync + Send>> {
        Ok(0)
    }
}

/// The unsolicited (`rx`) reports of a capture in `tests/fixtures/hidpp`,
/// in order, with comment lines skipped.
pub fn capture(text: &str) -> Vec<Vec<u8>> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (_ms, dir, _node) = (f.next()?, f.next()?, f.next()?);
            (dir == "rx").then(|| f.map(|b| u8::from_str_radix(b, 16).unwrap()).collect())
        })
        .collect()
}
