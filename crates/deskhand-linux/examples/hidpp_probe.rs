//! M16 spike: Logitech HID++ over `/dev/hidraw*` (Bolt/Unifying receivers and
//! directly connected devices), using `openlogi-hidpp`.
//!
//! ```text
//! hidpp_probe [SECONDS=120] [CAPTURE=hidpp-capture.txt]
//! ```
//! 1. Lists every hidraw node: bus, vendor:product, name, bound driver, and
//!    whether it can be opened read-write (the nodes are root-only without a
//!    udev rule).
//! 2. For each Logitech node that speaks HID++: the receiver (unique id,
//!    notification flags, pairings, codenames, device arrival), then for each
//!    paired or direct device the protocol version, the feature table,
//!    identity (0x0003, 0x0005, 0x0007), battery (0x1004, 0x1000, 0x1001) and
//!    backlight (0x1982). Sleeping devices show how a request to them fails,
//!    with the time it took.
//! 3. Watches for SECONDS, printing receiver connection events and decoded
//!    battery, backlight and wireless-status events. Meanwhile: let the
//!    keyboard sleep and wake it, plug in the USB-C cable and unplug it,
//!    change the backlight with its keys, and unplug and replug the receiver.
//!
//! Every report sent or received is also written, timestamped and in hex, to
//! CAPTURE; these become test fixtures. The probe enables the receiver's
//! wireless and software-present notification flags and restores them on
//! exit; it changes no other setting.

use std::fs::File;
use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use deskhand_linux::hidraw::{self, HidrawChannel, HidrawNode};
use hidpp::channel::{HidppChannel, RawHidChannel, RequestSwId, SwIdPolicy};
use hidpp::device::Device;
use hidpp::feature::EmittingFeature;
use hidpp::feature::backlight::BacklightFeature;
use hidpp::feature::battery_status::BatteryStatusFeature;
use hidpp::feature::battery_voltage::BatteryVoltageFeature;
use hidpp::feature::device_friendly_name::DeviceFriendlyNameFeature;
use hidpp::feature::device_information::DeviceInformationFeature;
use hidpp::feature::device_type_and_name::DeviceTypeAndNameFeature;
use hidpp::feature::registry;
use hidpp::feature::unified_battery::UnifiedBatteryFeature;
use hidpp::feature::wireless_device_status::WirelessDeviceStatusFeature;
use hidpp::nibble::U4;
use hidpp::receiver::{self, NotificationFlags, Receiver};

/// Our HID++ software id: not the kernel's or Solaar's usual 1.
const SW_ID: u8 = 0x0b;
/// The receiver itself, or a directly connected device.
const DIRECT: u8 = 0xff;
const STEP_TIMEOUT: Duration = Duration::from_secs(5);

/// Timestamped output to stdout and the capture file.
struct Log {
    start: Instant,
    file: Mutex<File>,
}

impl Log {
    fn line(&self, text: &str) {
        let ms = self.start.elapsed().as_millis();
        println!("{ms:>8} {text}");
        if let Ok(mut f) = self.file.lock() {
            let _ = writeln!(f, "{ms:>8} {text}");
        }
    }

    fn report(&self, node: &str, dir: &str, bytes: &[u8]) {
        let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
        self.line(&format!("{dir} {node} {}", hex.join(" ")));
    }
}

/// Logs every report in both directions.
struct Tap {
    inner: HidrawChannel,
    node: String,
    log: Arc<Log>,
}

#[async_trait::async_trait]
impl RawHidChannel for Tap {
    fn vendor_id(&self) -> u16 {
        self.inner.vendor_id()
    }

    fn product_id(&self) -> u16 {
        self.inner.product_id()
    }

    async fn write_report(
        &self,
        src: &[u8],
    ) -> Result<usize, Box<dyn std::error::Error + Sync + Send>> {
        self.log.report(&self.node, "tx", src);
        self.inner.write_report(src).await
    }

    async fn read_report(
        &self,
        buf: &mut [u8],
    ) -> Result<usize, Box<dyn std::error::Error + Sync + Send>> {
        let n = self.inner.read_report(buf).await?;
        // Only HID++ reports: a node can also carry input (mouse movement,
        // key presses on DJ device nodes), which is noise and private.
        if matches!(buf.first(), Some(0x10..=0x12)) {
            self.log.report(&self.node, "rx", &buf[..n]);
        }
        Ok(n)
    }

    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }

    fn supports_short_long_hidpp(&self) -> Option<(bool, bool)> {
        self.inner.supports_short_long_hidpp()
    }

    async fn get_report_descriptor(
        &self,
        buf: &mut [u8],
    ) -> Result<usize, Box<dyn std::error::Error + Sync + Send>> {
        self.inner.get_report_descriptor(buf).await
    }
}

/// Runs one step with a timeout and logs its result and duration.
async fn step<T: std::fmt::Debug, E: std::fmt::Display>(
    log: &Log,
    what: &str,
    fut: impl Future<Output = Result<T, E>>,
) -> Option<T> {
    let t = Instant::now();
    match tokio::time::timeout(STEP_TIMEOUT, fut).await {
        Ok(Ok(v)) => {
            log.line(&format!("  {what}: {v:?} ({} ms)", t.elapsed().as_millis()));
            Some(v)
        }
        Ok(Err(e)) => {
            log.line(&format!(
                "  {what}: ERROR {e} ({} ms)",
                t.elapsed().as_millis()
            ));
            None
        }
        Err(_) => {
            log.line(&format!(
                "  {what}: TIMEOUT after {} ms",
                t.elapsed().as_millis()
            ));
            None
        }
    }
}

/// Forwards a feature's decoded events to the log until the probe ends.
fn watch<T: std::fmt::Debug + Send + 'static>(
    log: &Arc<Log>,
    label: String,
    rx: async_channel::Receiver<T>,
) {
    let log = log.clone();
    tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            log.line(&format!("EVENT {label}: {ev:?}"));
        }
    });
}

fn list_nodes(log: &Log, nodes: &[HidrawNode]) {
    log.line(&format!("{} hidraw node(s)", nodes.len()));
    for n in nodes {
        let access = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&n.path)
        {
            Ok(_) => "rw ok".to_owned(),
            Err(e) => format!("rw FAILED: {e}"),
        };
        log.line(&format!(
            "  {} bus={:?} {:04x}:{:04x} name={:?} uniq={:?} driver={} [{access}]",
            n.path.display(),
            n.bus,
            n.vendor,
            n.product,
            n.name,
            n.uniq,
            n.driver.as_deref().unwrap_or("-"),
        ));
    }
}

/// Prints pairing details; returns the paired slots and the notification
/// flags to restore on exit (if the probe changed them).
async fn probe_receiver(
    log: &Arc<Log>,
    chan: &HidppChannel,
    node: &str,
    rcv: &Receiver,
) -> (Vec<u8>, Option<NotificationFlags>) {
    log.line(&format!("{node}: receiver {}", rcv.name()));
    step(log, "unique id", rcv.get_unique_id()).await;
    let mut slots = Vec::new();
    let mut restore = None;
    match rcv {
        Receiver::Bolt(bolt) => {
            if let Some(flags) =
                step(log, "notification flags", bolt.get_notification_flags()).await
            {
                let want =
                    flags | NotificationFlags::WIRELESS | NotificationFlags::SOFTWARE_PRESENT;
                if want != flags {
                    step(
                        log,
                        "enable wireless + software-present",
                        bolt.set_notification_flags(want),
                    )
                    .await;
                    restore = Some(flags);
                }
            }
            watch(log, format!("{node} bolt"), bolt.listen());
            step(log, "pairing count", bolt.count_pairings()).await;
            if let Some(devs) = step(
                log,
                "paired devices (device arrival)",
                bolt.collect_paired_devices(),
            )
            .await
            {
                slots.extend(devs.iter().map(|d| d.index));
            }
            for slot in 1..=6u8 {
                step(
                    log,
                    &format!("slot {slot} pairing info"),
                    bolt.get_device_pairing_information(slot),
                )
                .await;
                step(
                    log,
                    &format!("slot {slot} codename"),
                    bolt.get_device_codename(slot),
                )
                .await;
            }
        }
        Receiver::Unifying(uni) => {
            step(log, "receiver info", uni.get_receiver_info()).await;
            if let Some(flags) = step(
                log,
                "notification flags",
                receiver::read_notification_flags(chan),
            )
            .await
            {
                restore = Some(flags);
            }
            step(
                log,
                "enable wireless notifications",
                uni.set_wireless_notifications(true),
            )
            .await;
            watch(log, format!("{node} unifying"), uni.listen());
            step(log, "pairing count", uni.count_pairings()).await;
            for slot in 1..=6u8 {
                if step(
                    log,
                    &format!("slot {slot} pairing info"),
                    uni.get_device_pairing_information(slot),
                )
                .await
                .is_some()
                {
                    slots.push(slot);
                }
            }
            step(log, "trigger device arrival", uni.trigger_device_arrival()).await;
        }
        _ => log.line("  (receiver type not handled by the probe)"),
    }
    (slots, restore)
}

async fn probe_device(log: &Arc<Log>, chan: &Arc<HidppChannel>, node: &str, index: u8) {
    log.line(&format!("{node}: device index {index:#04x}"));
    let t = Instant::now();
    let mut dev = match tokio::time::timeout(STEP_TIMEOUT, Device::new(chan.clone(), index)).await {
        Ok(Ok(d)) => {
            log.line(&format!(
                "  protocol {:?} ({} ms)",
                d.protocol_version,
                t.elapsed().as_millis()
            ));
            d
        }
        Ok(Err(e)) => {
            log.line(&format!(
                "  not reachable: {e} ({e:?}) after {} ms (asleep or offline?)",
                t.elapsed().as_millis()
            ));
            return;
        }
        Err(_) => {
            log.line(&format!(
                "  not reachable: TIMEOUT after {} ms (asleep or offline?)",
                t.elapsed().as_millis()
            ));
            return;
        }
    };
    // Feature table, with names from the registry.
    match tokio::time::timeout(STEP_TIMEOUT, dev.enumerate_features()).await {
        Ok(Ok(Some(features))) => {
            for f in &features {
                let name = registry::lookup(f.id).map_or("?", |k| k.name);
                log.line(&format!(
                    "  feature {:#06x} v{} {:?} {name}",
                    f.id, f.version, f.typ
                ));
            }
        }
        Ok(Ok(None)) => log.line("  no FeatureSet (0x0001)"),
        Ok(Err(e)) => log.line(&format!("  feature table: ERROR {e}")),
        Err(_) => log.line("  feature table: TIMEOUT"),
    }

    if let Some(f) = dev.get_feature::<DeviceInformationFeature>()
        && let Some(info) = step(log, "0x0003 device info", f.get_device_info()).await
    {
        for e in 0..info.entity_count {
            step(
                log,
                &format!("0x0003 firmware entity {e}"),
                f.get_fw_info(e),
            )
            .await;
        }
        if info.capabilities.serial_number {
            step(log, "0x0003 serial", f.get_serial_number()).await;
        }
    }
    if let Some(f) = dev.get_feature::<DeviceTypeAndNameFeature>() {
        step(log, "0x0005 name", f.get_whole_device_name()).await;
        step(log, "0x0005 type", f.get_device_type()).await;
    }
    if let Some(f) = dev.get_feature::<DeviceFriendlyNameFeature>() {
        step(log, "0x0007 friendly name", f.get_whole_friendly_name()).await;
    }
    if let Some(f) = dev.get_feature::<UnifiedBatteryFeature>() {
        step(log, "0x1004 capabilities", f.get_battery_capabilities()).await;
        step(log, "0x1004 status", f.get_battery_info()).await;
        watch(log, format!("{node}/{index} 0x1004"), f.listen());
    }
    if let Some(f) = dev.get_feature::<BatteryStatusFeature>() {
        step(log, "0x1000 capability", f.get_battery_capability()).await;
        step(log, "0x1000 status", f.get_battery_level_status()).await;
    }
    if let Some(f) = dev.get_feature::<BatteryVoltageFeature>() {
        step(log, "0x1001 status", f.get_battery_info()).await;
    }
    if let Some(f) = dev.get_feature::<BacklightFeature>() {
        step(log, "0x1982 info", f.get_backlight_info()).await;
        step(log, "0x1982 config", f.get_backlight_config()).await;
        watch(log, format!("{node}/{index} 0x1982"), f.listen());
    }
    if let Some(f) = dev.get_feature::<WirelessDeviceStatusFeature>() {
        watch(log, format!("{node}/{index} 0x1d4b"), f.listen());
    }
    // Keep the feature handles (and so their event listeners) alive.
    std::mem::forget(dev);
}

/// Logs channels that disconnect and hidraw nodes that appear or disappear
/// (receiver unplugged and replugged). The probe does not reopen nodes.
fn watch_nodes(
    log: &Arc<Log>,
    keep: &[(Arc<HidppChannel>, Option<Receiver>)],
    nodes: Vec<HidrawNode>,
) {
    let log = log.clone();
    let chans: Vec<Arc<HidppChannel>> = keep.iter().map(|(c, _)| c.clone()).collect();
    tokio::spawn(async move {
        let mut known: Vec<_> = nodes.into_iter().map(|n| n.path).collect();
        let mut connected = vec![true; chans.len()];
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            for (i, c) in chans.iter().enumerate() {
                if connected[i] && !c.is_connected() {
                    connected[i] = false;
                    log.line(&format!(
                        "CHANNEL {:04x}:{:04x} disconnected",
                        c.vendor_id, c.product_id
                    ));
                }
            }
            let Ok(now) = hidraw::enumerate() else {
                continue;
            };
            let now: Vec<_> = now.into_iter().map(|n| n.path).collect();
            for p in now.iter().filter(|p| !known.contains(p)) {
                log.line(&format!("NODE added {}", p.display()));
            }
            for p in known.iter().filter(|p| !now.contains(p)) {
                log.line(&format!("NODE removed {}", p.display()));
            }
            known = now;
        }
    });
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let seconds: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(120);
    let capture = args.next().unwrap_or_else(|| "hidpp-capture.txt".into());
    let log = Arc::new(Log {
        start: Instant::now(),
        file: Mutex::new(File::create(&capture)?),
    });
    log.line(&format!(
        "hidpp_probe {} (openlogi-hidpp 0.9.0), kernel {}",
        env!("CARGO_PKG_VERSION"),
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .unwrap_or_default()
            .trim()
    ));

    let nodes = hidraw::enumerate()?;
    list_nodes(&log, &nodes);

    // Channels and receivers stay alive (and so keep listening) until exit.
    let mut keep: Vec<(Arc<HidppChannel>, Option<Receiver>)> = Vec::new();
    let mut restore: Vec<(Arc<HidppChannel>, NotificationFlags)> = Vec::new();
    // (channel, device index) of every paired device, for the closing ping.
    let mut paired: Vec<(Arc<HidppChannel>, String, u8)> = Vec::new();
    for n in nodes.iter().filter(|n| n.is_logitech()) {
        let label = n.path.display().to_string();
        if n.driver.as_deref() == Some("logitech-hidpp-device") {
            // A device node the kernel's HID++ driver owns (its battery is in
            // UPower already); its device index is the slot, not 0xff.
            log.line(&format!(
                "{label}: kernel HID++ driver owns this device; skipped"
            ));
            continue;
        }
        let inner = match n.open() {
            Ok(c) => c,
            Err(e) => {
                log.line(&format!("{label}: cannot open: {e}"));
                continue;
            }
        };
        let tap = Tap {
            inner,
            node: label.clone(),
            log: log.clone(),
        };
        let mut chan = match HidppChannel::from_raw_channel(tap).await {
            Ok(c) => c,
            Err(e) => {
                log.line(&format!("{label}: no HID++ on this interface ({e})"));
                continue;
            }
        };
        if let Some(id) = RequestSwId::new(U4::from_lo(SW_ID)) {
            chan.set_sw_id_policy(SwIdPolicy::Fixed(id));
        }
        let chan = Arc::new(chan);
        let rcv = receiver::detect(chan.clone());
        let indices = match &rcv {
            Some(rcv) => {
                let (slots, flags) = probe_receiver(&log, &chan, &label, rcv).await;
                if let Some(flags) = flags {
                    restore.push((chan.clone(), flags));
                }
                paired.extend(slots.iter().map(|s| (chan.clone(), label.clone(), *s)));
                slots
            }
            None => {
                log.line(&format!(
                    "{label}: not a known receiver; trying it as a direct device"
                ));
                vec![DIRECT]
            }
        };
        for index in indices {
            probe_device(&log, &chan, &label, index).await;
        }
        keep.push((chan, rcv));
    }

    log.line(&format!(
        "watching for {seconds} s: let the keyboard sleep and wake it, plug and unplug USB-C, \
         change the backlight, unplug and replug the receiver (Ctrl-C ends early)"
    ));
    watch_nodes(&log, &keep, nodes);
    tokio::select! {
        () = tokio::time::sleep(Duration::from_secs(seconds)) => {}
        _ = tokio::signal::ctrl_c() => log.line("interrupted"),
    }

    // How long a paired device takes to answer now (a sleeping one is slow,
    // or does not answer at all).
    for (chan, label, index) in &paired {
        if chan.is_connected() {
            step(
                &log,
                &format!("{label} closing ping of device {index:#04x}"),
                hidpp::protocol::determine_version(chan, *index),
            )
            .await;
        }
    }

    for (chan, flags) in restore.into_iter().filter(|(c, _)| c.is_connected()) {
        step(
            &log,
            "restore notification flags",
            receiver::write_notification_flags(&chan, flags),
        )
        .await;
    }
    log.line(&format!("capture written to {capture}"));
    // hidpp joins its read threads on drop; exit directly instead.
    drop(keep);
    std::process::exit(0);
}
