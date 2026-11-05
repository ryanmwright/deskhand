//! The HID++ source against [`MockReceiver`]s, through the real
//! `peripheral_battery` integration and hub.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use deskhand_common::PeripheralBattery;
use deskhand_common::peripherals::{CAPABILITY, PeripheralSource};
use deskhand_core::testing::{TestRig, rig_with, wait_attr, wait_entity, wait_spec, wait_state};
use deskhand_core::{Config, EntityId, State};
use serde_json::json;
use tokio::sync::mpsc;

use crate::backend::{Access, HidBackend, HidBus, HidNode, Hotplug, RawHidChannel};
use crate::mock::{self, MockBattery, MockDevice, MockReceiver};
use crate::receiver::Timing;
use crate::source::HidppSource;

/// Nodes and their mock receivers; tests add and remove them and announce
/// it through hotplug.
#[derive(Default)]
struct Backend {
    nodes: Mutex<BTreeMap<String, (HidNode, MockReceiver)>>,
    denied: Mutex<Vec<String>>,
    hotplug: Mutex<Vec<mpsc::UnboundedSender<()>>>,
}

fn node(id: &str) -> HidNode {
    HidNode {
        id: id.into(),
        bus: HidBus::Usb,
        vendor: 0x046d,
        product: 0xc548,
        name: "Logitech USB Receiver".into(),
        driver: Some("hid-generic".into()),
        kernel_owned: false,
        kernel_receiver: false,
    }
}

impl Backend {
    fn plug(&self, id: &str, rx: &MockReceiver) {
        self.nodes
            .lock()
            .unwrap()
            .insert(id.into(), (node(id), rx.clone()));
        self.changed();
    }

    fn unplug(&self, id: &str) {
        if let Some((_, rx)) = self.nodes.lock().unwrap().remove(id) {
            rx.unplug();
        }
        self.changed();
    }

    fn changed(&self) {
        for tx in self.hotplug.lock().unwrap().iter() {
            let _ = tx.send(());
        }
    }
}

struct ChangeStream(mpsc::UnboundedReceiver<()>);

#[async_trait::async_trait]
impl Hotplug for ChangeStream {
    async fn changed(&mut self) -> io::Result<()> {
        self.0.recv().await.ok_or(io::ErrorKind::BrokenPipe)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl HidBackend for Backend {
    fn enumerate(&self) -> io::Result<Vec<HidNode>> {
        Ok(self
            .nodes
            .lock()
            .unwrap()
            .values()
            .map(|(n, _)| n.clone())
            .collect())
    }

    fn access(&self, node: &HidNode) -> Access {
        if self.denied.lock().unwrap().contains(&node.id) {
            Access::Denied
        } else {
            Access::Ok
        }
    }

    fn open(&self, node: &HidNode) -> io::Result<Box<dyn RawHidChannel>> {
        if self.denied.lock().unwrap().contains(&node.id) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let nodes = self.nodes.lock().unwrap();
        let (_, rx) = nodes.get(&node.id).ok_or(io::ErrorKind::NotFound)?;
        Ok(Box::new(rx.clone()))
    }

    fn hotplug(&self) -> io::Result<Box<dyn Hotplug>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.hotplug.lock().unwrap().push(tx);
        Ok(Box::new(ChangeStream(rx)))
    }
}

const TIMING: Timing = Timing {
    wake_read: Duration::from_millis(300),
    retry: Duration::from_millis(500),
    retry_max: Duration::from_secs(2),
    job_timeout: Duration::from_secs(3),
    refresh: None,
};

fn start(backend: &Arc<Backend>, toml: &str) -> TestRig {
    start_with(backend, toml, TIMING)
}

fn start_with(backend: &Arc<Backend>, toml: &str, timing: Timing) -> TestRig {
    let source = HidppSource::new(backend.clone()).with_timing(timing);
    let config = Config::from_toml_str(toml, None)
        .unwrap()
        .integration(CAPABILITY)
        .unwrap();
    rig_with(PeripheralBattery::new(vec![Arc::new(source)]), config).0
}

fn one_receiver(
    devices: impl IntoIterator<Item = (u8, MockDevice)>,
) -> (Arc<Backend>, MockReceiver) {
    let backend = Arc::new(Backend::default());
    let rx = MockReceiver::new(devices);
    backend.plug("/dev/hidraw10", &rx);
    (backend, rx)
}

fn id(s: &str) -> EntityId {
    EntityId::slugify(s)
}

const BATTERY: &str = "battery_mx_keys_s";
const RECEIVER: &str = "bolt_aaaabbbbcccc0001";

#[tokio::test]
async fn awake_keyboard_is_a_device_on_the_receiver() {
    let (backend, rx) = one_receiver([(1, MockDevice::mx_keys_s())]);
    let mut t = start(&backend, "");
    wait_state(&mut t, &id(BATTERY), State::Float(100.0)).await;
    wait_state(&mut t, &id("battery_mx_keys_s_state"), "discharging".into()).await;
    wait_state(
        &mut t,
        &id("battery_mx_keys_s_connected"),
        State::Bool(true),
    )
    .await;
    wait_state(&mut t, &id("battery_mx_keys_s_low"), State::Bool(false)).await;
    wait_attr(&mut t, &id(BATTERY), "source", json!("hidpp")).await;
    wait_attr(&mut t, &id(BATTERY), "slot", json!(1)).await;
    wait_attr(&mut t, &id(BATTERY), "encrypted", json!(true)).await;
    wait_attr(&mut t, &id(BATTERY), "level", json!("full")).await;
    wait_attr(&mut t, &id(BATTERY), "kind", json!("keyboard")).await;
    wait_spec(&mut t, &id(BATTERY), "device fields", |s| {
        s.device.as_ref().is_some_and(|d| {
            d.name == "MX Keys S"
                && d.model_id.as_deref() == Some("B378")
                && d.serial_number.as_deref() == Some("1234AB567CD8")
                && d.sw_version.as_deref() == Some("RBK 81.03.B0018")
                && d.manufacturer.as_deref() == Some("Logitech")
                && d.via.as_ref() == Some(&id(RECEIVER))
        })
    })
    .await;
    let updated = t.state(&id("battery_mx_keys_s_updated")).cloned();
    assert!(matches!(updated, Some(State::Text(_))), "{updated:?}");
    let paired = id("bolt_aaaabbbbcccc0001_paired");
    wait_state(&mut t, &paired, State::Int(1)).await;
    wait_spec(&mut t, &paired, "receiver device", |s| {
        s.device.as_ref().is_some_and(|d| {
            d.serial_number.as_deref() == Some(mock::UNIQUE_ID)
                && d.sw_version.as_deref() == Some("05.02.B0018")
        })
    })
    .await;
    let slots = t.attributes(&paired).unwrap()["slots"].clone();
    assert_eq!(
        slots,
        json!([{"slot": 1, "name": "MX Keys S", "model_id": "B378", "online": true}])
    );
    // hid-generic sets no notification flags; Deskhand turns on wireless +
    // software present and leaves them on.
    assert_eq!(rx.flags(), [0x00, 0x09, 0x00]);
    // One discovery read, nothing else.
    assert_eq!(rx.battery_reads(1), 1);
}

#[tokio::test]
async fn sleeping_keyboard_is_not_contacted_and_one_read_on_wake() {
    let (backend, rx) = one_receiver([(
        1,
        MockDevice {
            online: false,
            ..MockDevice::mx_keys_s()
        },
    )]);
    let mut t = start(&backend, "");
    // Known from the receiver alone: codename, link state; no battery yet.
    wait_state(
        &mut t,
        &id("battery_mx_keys_s_connected"),
        State::Bool(false),
    )
    .await;
    wait_state(&mut t, &id("battery_mx_keys_s_state"), "unknown".into()).await;
    t.settle(Duration::from_millis(500)).await;
    assert!(rx.requests_to(1).is_empty(), "asked a sleeping device");

    // Key press: link up; discovery and exactly one battery read.
    rx.set_online(1, true);
    wait_state(&mut t, &id(BATTERY), State::Float(100.0)).await;
    wait_state(
        &mut t,
        &id("battery_mx_keys_s_connected"),
        State::Bool(true),
    )
    .await;
    t.settle(Duration::from_millis(500)).await;
    assert_eq!(rx.battery_reads(1), 1);

    // Sleep keeps the value; nothing is sent.
    let before = rx.requests_to(1).len();
    rx.set_online(1, false);
    wait_state(
        &mut t,
        &id("battery_mx_keys_s_connected"),
        State::Bool(false),
    )
    .await;
    t.settle(Duration::from_millis(500)).await;
    assert_eq!(t.state(&id(BATTERY)), Some(&State::Float(100.0)));
    assert_eq!(rx.requests_to(1).len(), before);

    // Wake with the report the keyboard sends by itself: no read at all.
    rx.set_battery(
        1,
        MockBattery::Unified {
            percent: 99,
            level: 0x08,
            status: 0,
        },
    );
    rx.set_online(1, true);
    rx.battery_event(1);
    wait_state(&mut t, &id(BATTERY), State::Float(99.0)).await;
    t.settle(Duration::from_millis(600)).await;
    assert_eq!(rx.battery_reads(1), 1);

    // Wake without it: exactly one read, after the wait.
    rx.set_online(1, false);
    wait_state(
        &mut t,
        &id("battery_mx_keys_s_connected"),
        State::Bool(false),
    )
    .await;
    rx.set_battery(
        1,
        MockBattery::Unified {
            percent: 98,
            level: 0x08,
            status: 0,
        },
    );
    rx.set_online(1, true);
    wait_state(&mut t, &id(BATTERY), State::Float(98.0)).await;
    t.settle(Duration::from_millis(600)).await;
    assert_eq!(rx.battery_reads(1), 2);
}

#[tokio::test]
async fn battery_changes_arrive_as_events_without_requests() {
    let (backend, rx) = one_receiver([(1, MockDevice::mx_keys_s())]);
    let mut t = start(&backend, "");
    wait_state(&mut t, &id(BATTERY), State::Float(100.0)).await;
    let before = rx.requests_to(1).len();
    // The M16 capture: cable in, cable out, backlight keys, sleep, wake.
    let events = mock::capture(include_str!(
        "../../deskhand-linux/tests/fixtures/hidpp/mx-keys-s-bolt-events.txt"
    ));
    let (charging, rest) = events.split_at(1);
    rx.inject(&charging[0]);
    wait_state(&mut t, &id("battery_mx_keys_s_state"), "charging".into()).await;
    for r in rest {
        rx.inject(r);
    }
    // It ends awake after the wake sequence, discharging at 100 %.
    wait_state(&mut t, &id("battery_mx_keys_s_state"), "discharging".into()).await;
    wait_state(
        &mut t,
        &id("battery_mx_keys_s_connected"),
        State::Bool(true),
    )
    .await;
    t.settle(Duration::from_millis(600)).await;
    assert_eq!(t.state(&id(BATTERY)), Some(&State::Float(100.0)));
    // Replies to another client (Solaar's software id 7) were not taken as
    // ours, and the wake's own battery report spared a read.
    assert_eq!(
        rx.requests_to(1).len(),
        before,
        "polled: {:x?}",
        rx.requests_to(1)
    );
}

#[tokio::test]
async fn receiver_loss_and_return_keeps_one_device() {
    let (backend, rx) = one_receiver([(1, MockDevice::mx_keys_s())]);
    let mut t = start(&backend, "");
    wait_state(&mut t, &id(BATTERY), State::Float(100.0)).await;
    backend.unplug("/dev/hidraw10");
    wait_state(
        &mut t,
        &id("battery_mx_keys_s_connected"),
        State::Bool(false),
    )
    .await;
    // The values stay while the receiver is away.
    assert_eq!(t.state(&id(BATTERY)), Some(&State::Float(100.0)));
    drop(rx);

    // Back on another node, within the hub's removal grace and after it.
    let rx2 = MockReceiver::new([(
        1,
        MockDevice {
            battery: MockBattery::Unified {
                percent: 95,
                level: 0x04,
                status: 0,
            },
            ..MockDevice::mx_keys_s()
        },
    )]);
    backend.plug("/dev/hidraw11", &rx2);
    wait_state(&mut t, &id(BATTERY), State::Float(95.0)).await;
    wait_state(
        &mut t,
        &id("battery_mx_keys_s_connected"),
        State::Bool(true),
    )
    .await;
    t.settle(Duration::from_millis(800)).await;
    let batteries: Vec<&EntityId> = t
        .entities
        .iter()
        .filter(|e| e.as_str().starts_with("battery_") && !e.as_str().contains("_state"))
        .filter(|e| {
            !["_low", "_connected", "_updated"]
                .iter()
                .any(|s| e.as_str().ends_with(s))
        })
        .collect();
    assert_eq!(batteries, [&id(BATTERY)]);
}

#[tokio::test]
async fn same_node_name_after_a_replug_is_reopened() {
    let (backend, _rx) = one_receiver([(1, MockDevice::mx_keys_s())]);
    let mut t = start(&backend, "");
    wait_state(&mut t, &id(BATTERY), State::Float(100.0)).await;
    // Removed and re-added within one debounce: same path, new receiver.
    let rx2 = MockReceiver::new([(
        1,
        MockDevice {
            battery: MockBattery::Unified {
                percent: 77,
                level: 0x04,
                status: 0,
            },
            ..MockDevice::mx_keys_s()
        },
    )]);
    {
        let mut nodes = backend.nodes.lock().unwrap();
        let (_, old) = nodes.remove("/dev/hidraw10").unwrap();
        old.unplug();
        nodes.insert("/dev/hidraw10".into(), (node("/dev/hidraw10"), rx2.clone()));
    }
    backend.changed();
    wait_state(&mut t, &id(BATTERY), State::Float(77.0)).await;
}

#[tokio::test]
async fn unpairing_removes_the_device() {
    let (backend, rx) = one_receiver([(1, MockDevice::mx_keys_s())]);
    let mut t = start(&backend, "");
    wait_entity(&mut t, &id(BATTERY), true).await;
    rx.unpair(1);
    wait_entity(&mut t, &id(BATTERY), false).await;
    wait_entity(&mut t, &id("battery_mx_keys_s_connected"), false).await;
    wait_state(&mut t, &id("bolt_aaaabbbbcccc0001_paired"), State::Int(0)).await;
}

#[tokio::test]
async fn devices_with_only_0x1000_or_0x1001() {
    let mouse = MockDevice {
        wpid: 0x4082,
        kind: 0x02,
        codename: "M720 Triathlon".into(),
        name: "M720 Triathlon Multi-Device Mouse".into(),
        serial: "AABBCCDD0011".into(),
        online: true,
        battery: MockBattery::Status {
            level: 50,
            status: 0,
        },
    };
    let trackball = MockDevice {
        wpid: 0x4093,
        kind: 0x08,
        codename: "MX Ergo".into(),
        name: "MX Ergo".into(),
        serial: "AABBCCDD0022".into(),
        online: true,
        battery: MockBattery::Voltage { mv: 3811, flags: 0 },
    };
    let (backend, rx) = one_receiver([(2, mouse), (3, trackball)]);
    let mut t = start(&backend, "");
    wait_state(&mut t, &id("battery_m720_triathlon"), State::Float(50.0)).await;
    wait_state(&mut t, &id("battery_mx_ergo"), State::Float(50.0)).await;
    wait_attr(&mut t, &id("battery_mx_ergo"), "estimated", json!(true)).await;
    wait_attr(
        &mut t,
        &id("battery_m720_triathlon"),
        "kind",
        json!("mouse"),
    )
    .await;
    // 0x1000 broadcast: recharging at 60 %.
    rx.set_battery(
        2,
        MockBattery::Status {
            level: 60,
            status: 1,
        },
    );
    rx.battery_event(2);
    wait_state(&mut t, &id("battery_m720_triathlon"), State::Float(60.0)).await;
    wait_state(
        &mut t,
        &id("battery_m720_triathlon_state"),
        "charging".into(),
    )
    .await;
    // 0x1001 broadcast: critical.
    rx.set_battery(
        3,
        MockBattery::Voltage {
            mv: 3600,
            flags: 0x20,
        },
    );
    rx.battery_event(3);
    wait_state(&mut t, &id("battery_mx_ergo_low"), State::Bool(true)).await;
}

#[tokio::test]
async fn battery_refresh_reads_only_connected_devices() {
    let (backend, rx) = one_receiver([
        (1, MockDevice::mx_keys_s()),
        (
            2,
            MockDevice {
                wpid: 0xb023,
                kind: 0x02,
                codename: "MX Master 3S".into(),
                name: "MX Master 3S".into(),
                online: false,
                ..MockDevice::mx_keys_s()
            },
        ),
    ]);
    // The option's minimum is a minute; the test shortens it.
    let timing = Timing {
        refresh: Some(Duration::from_millis(400)),
        ..TIMING
    };
    let mut t = start_with(&backend, "", timing);
    wait_state(&mut t, &id(BATTERY), State::Float(100.0)).await;
    wait_state(
        &mut t,
        &id("battery_mx_master_3s_connected"),
        State::Bool(false),
    )
    .await;
    t.settle(Duration::from_millis(1000)).await;
    assert!(rx.battery_reads(1) >= 2, "{} reads", rx.battery_reads(1));
    assert!(rx.requests_to(2).is_empty());
}

#[tokio::test]
async fn probe_reports_access_and_absence() {
    let backend = Arc::new(Backend::default());
    let source = HidppSource::new(backend.clone());
    let p = source.probe().await;
    assert!(p.is_available(), "{p:?}");
    backend.plug("/dev/hidraw10", &MockReceiver::new([]));
    backend.denied.lock().unwrap().push("/dev/hidraw10".into());
    let p = source.probe().await;
    assert!(!p.is_available());
    assert!(format!("{p:?}").contains("udev"), "{p:?}");
}

#[tokio::test]
async fn inspect_lists_receivers_and_devices() {
    let (backend, _rx) = one_receiver([
        (1, MockDevice::mx_keys_s()),
        (
            2,
            MockDevice {
                wpid: 0xb023,
                codename: "MX Master 3S".into(),
                online: false,
                ..MockDevice::mx_keys_s()
            },
        ),
    ]);
    let report = crate::inspect(backend.as_ref()).await.unwrap();
    assert_eq!(report.len(), 1);
    let r = report[0].receiver.as_ref().expect("a receiver");
    assert_eq!(r.unique_id.as_deref(), Some(mock::UNIQUE_ID));
    assert_eq!(r.devices.len(), 2);
    assert_eq!(r.devices[0].name.as_deref(), Some("MX Keys S"));
    assert_eq!(r.devices[0].battery.as_deref(), Some("100 % discharging"));
    assert_eq!(r.devices[1].name.as_deref(), Some("MX Master 3S"));
    assert_eq!(r.devices[1].battery, None);
}
