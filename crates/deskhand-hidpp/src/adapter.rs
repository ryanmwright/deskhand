//! The one module that uses `hidpp` (`openlogi-hidpp`, pinned exactly in the
//! workspace because its API changes often). Everything else in this crate
//! sees only the plain types defined here.
//!
//! Requests carry a software id leased per node ([`lease_sw_id`]), so other
//! HID++ clients on the same node (Solaar, the kernel driver with id 1) keep
//! their replies. Unsolicited reports are decoded in one listener per channel
//! into [`Notice`]s: link changes (0x41), unpairing (0x40) and HID++ 2.0
//! feature events, which [`DeviceLink::decode`] turns into battery and
//! backlight changes.

use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use deskhand_common::peripherals::{
    Backlight, BacklightCommand, BacklightTimeout, BacklightTimeouts, BatteryLevel, ChargeState,
};
use hidpp::channel::{
    ChannelError, HidppChannel, HidppMessage, MessageListenerGuard, RawHidChannel, RequestSwId,
    SwIdPolicy,
};
use hidpp::device::{Device, DeviceError};
use hidpp::feature::CreatableFeature;
use hidpp::feature::backlight::{
    BacklightConfig, BacklightEvent, BacklightFeature, BacklightMode, BacklightOptions,
    BacklightStatus, SetBacklightConfig,
};
use hidpp::feature::battery_status::{BatteryStatusFeature, LegacyBatteryStatus};
use hidpp::feature::battery_voltage::{
    BatteryVoltageFeature, VoltageBatteryInfo, VoltageChargingStatus,
};
use hidpp::feature::device_information::{DeviceEntityType, DeviceInformationFeature};
use hidpp::feature::device_type_and_name::DeviceTypeAndNameFeature;
use hidpp::feature::unified_battery::{
    self, BatteryEvent, BatteryInfo, BatteryStatus, UnifiedBatteryFeature,
};
use hidpp::feature::wireless_device_status::{
    WirelessDeviceStatus, WirelessDeviceStatusEvent, WirelessDeviceStatusFeature,
};
use hidpp::nibble::U4;
use hidpp::protocol::v10::Hidpp10Error;
use hidpp::protocol::v20::{self, Hidpp20Error};
use hidpp::protocol::{ProtocolVersion, v10};
use hidpp::receiver::{
    self, NotificationFlags, RECEIVER_DEVICE_INDEX, ReceiverError, bolt, unifying,
};
use tokio::sync::mpsc;

/// What went wrong talking to a receiver or device.
#[derive(Debug)]
pub enum AdapterError {
    /// No answer in time (a sleeping device, a busy radio).
    Timeout,
    /// The node is gone (unplugged).
    Gone,
    /// The device is not reachable at that index, or speaks only HID++ 1.0.
    NotFound,
    /// Every software id for this node is taken.
    NoSoftwareId,
    /// The node does not speak HID++.
    NotHidpp,
    Protocol(String),
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("no answer in time"),
            Self::Gone => f.write_str("the device is gone"),
            Self::NotFound => f.write_str("no HID++ 2.0 device at that index"),
            Self::NoSoftwareId => f.write_str("no free HID++ software id for this node"),
            Self::NotHidpp => f.write_str("the node does not speak HID++"),
            Self::Protocol(e) => write!(f, "HID++ error: {e}"),
        }
    }
}

impl StdError for AdapterError {}

impl From<ChannelError> for AdapterError {
    fn from(e: ChannelError) -> Self {
        match e {
            ChannelError::Timeout | ChannelError::NoResponse => Self::Timeout,
            ChannelError::HidppNotSupported => Self::NotHidpp,
            ChannelError::Implementation(inner) => {
                if inner
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.raw_os_error() == Some(19))
                {
                    Self::Gone
                } else {
                    Self::Protocol(inner.to_string())
                }
            }
            other => Self::Protocol(other.to_string()),
        }
    }
}

impl From<Hidpp10Error> for AdapterError {
    fn from(e: Hidpp10Error) -> Self {
        match e {
            Hidpp10Error::Channel(c) => c.into(),
            other => Self::Protocol(format!("{other} ({other:?})")),
        }
    }
}

impl From<Hidpp20Error> for AdapterError {
    fn from(e: Hidpp20Error) -> Self {
        match e {
            Hidpp20Error::Channel(c) => c.into(),
            other => Self::Protocol(format!("{other} ({other:?})")),
        }
    }
}

impl From<ReceiverError> for AdapterError {
    fn from(e: ReceiverError) -> Self {
        match e {
            ReceiverError::Protocol(p) => p.into(),
            other => Self::Protocol(other.to_string()),
        }
    }
}

impl From<DeviceError> for AdapterError {
    fn from(e: DeviceError) -> Self {
        match e {
            DeviceError::Channel(c) => c.into(),
            _ => Self::NotFound,
        }
    }
}

type Result<T> = std::result::Result<T, AdapterError>;

// --- software ids --------------------------------------------------------

/// Ids we lease, in order. 1 is the kernel's; Solaar was seen using 7.
const SW_IDS: [u8; 8] = [0x0b, 0x0c, 0x0d, 0x0e, 0x0a, 0x09, 0x0f, 0x08];

/// Leased ids per node, as a bit set.
static LEASES: Mutex<BTreeMap<String, u16>> = Mutex::new(BTreeMap::new());

/// Holds one software id for one node; returns it on drop.
#[derive(Debug)]
struct Lease {
    node: String,
    id: u8,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut leases = LEASES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(bits) = leases.get_mut(&self.node) {
            *bits &= !(1 << self.id);
            if *bits == 0 {
                leases.remove(&self.node);
            }
        }
    }
}

/// A software id no other channel of this process uses on `node`.
fn lease_sw_id(node: &str, reverse: bool) -> Option<(RequestSwId, Lease)> {
    let mut leases = LEASES.lock().unwrap_or_else(|e| e.into_inner());
    let bits = leases.entry(node.to_owned()).or_default();
    let mut order = SW_IDS;
    if reverse {
        order.reverse();
    }
    let id = order.into_iter().find(|id| *bits & (1 << id) == 0)?;
    *bits |= 1 << id;
    let sw = RequestSwId::new(U4::from_lo(id))?;
    Some((
        sw,
        Lease {
            node: node.to_owned(),
            id,
        },
    ))
}

/// `hidpp` wants a sized transport.
struct Boxed(Box<dyn RawHidChannel>);

#[async_trait::async_trait]
impl RawHidChannel for Boxed {
    fn vendor_id(&self) -> u16 {
        self.0.vendor_id()
    }

    fn product_id(&self) -> u16 {
        self.0.product_id()
    }

    async fn write_report(
        &self,
        src: &[u8],
    ) -> std::result::Result<usize, Box<dyn StdError + Sync + Send>> {
        self.0.write_report(src).await
    }

    async fn read_report(
        &self,
        buf: &mut [u8],
    ) -> std::result::Result<usize, Box<dyn StdError + Sync + Send>> {
        self.0.read_report(buf).await
    }

    fn is_connected(&self) -> bool {
        self.0.is_connected()
    }

    fn supports_short_long_hidpp(&self) -> Option<(bool, bool)> {
        self.0.supports_short_long_hidpp()
    }

    async fn get_report_descriptor(
        &self,
        buf: &mut [u8],
    ) -> std::result::Result<usize, Box<dyn StdError + Sync + Send>> {
        self.0.get_report_descriptor(buf).await
    }
}

// --- notices -------------------------------------------------------------

/// Device kinds as receivers report them (shared by Bolt and Unifying).
pub fn kind_name(kind: u8) -> Option<&'static str> {
    Some(match kind {
        0x01 => "keyboard",
        0x02 => "mouse",
        0x03 => "numpad",
        0x04 => "presenter",
        0x07 => "remote",
        0x08 => "trackball",
        0x09 => "touchpad",
        0x0a => "tablet",
        0x0b => "gamepad",
        0x0c => "joystick",
        0x0d => "headset",
        _ => return None,
    })
}

/// An unsolicited report from a receiver or one of its devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A paired device's link changed, or was re-reported on request.
    Connection {
        slot: u8,
        wpid: u16,
        kind: u8,
        online: bool,
        encrypted: bool,
    },
    /// The device in `slot` was unpaired.
    Unpaired { slot: u8 },
    /// A HID++ 2.0 event from the device in `slot`.
    Event {
        slot: u8,
        feature_index: u8,
        function: u8,
        payload: [u8; 16],
    },
}

/// The receiver families `hidpp` drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverKind {
    Bolt,
    /// Unifying, and Lightspeed receivers, which `hidpp` treats the same.
    Unifying,
}

impl ReceiverKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Bolt => "Logi Bolt receiver",
            Self::Unifying => "Unifying receiver",
        }
    }

    /// Prefix of the receiver's device key (`bolt_<unique id>`).
    pub fn key_prefix(self) -> &'static str {
        match self {
            Self::Bolt => "bolt",
            Self::Unifying => "unifying",
        }
    }
}

/// Decodes one report that no request of ours claimed.
pub fn decode_notice(kind: ReceiverKind, msg: HidppMessage) -> Option<Notice> {
    let raw: &[u8] = match &msg {
        HidppMessage::Short(p) => p,
        HidppMessage::Long(p) => p,
    };
    let (device, sub_id, address) = (raw[0], raw[1], raw[2]);
    if device == RECEIVER_DEVICE_INDEX || device == 0 {
        return None;
    }
    match sub_id {
        0x41 => {
            let m = v10::Message::from(msg);
            let (wpid, kind_code, online, encrypted) = match kind {
                ReceiverKind::Bolt => match bolt::decode_notification(&m)? {
                    bolt::Event::DeviceConnection(c) => {
                        (c.wpid, u8::from(c.kind), c.online, c.encrypted)
                    }
                    _ => return None,
                },
                ReceiverKind::Unifying => match unifying::decode_notification(&m)? {
                    unifying::Event::DeviceConnection(c) => {
                        (c.wpid, u8::from(c.kind), c.online, c.encrypted)
                    }
                    #[allow(unreachable_patterns)]
                    _ => return None,
                },
            };
            Some(Notice::Connection {
                slot: device,
                wpid,
                kind: kind_code,
                online,
                encrypted,
            })
        }
        // Device disconnection; address 0x02 means unpaired (as Solaar reads it).
        0x40 if address == 0x02 => Some(Notice::Unpaired { slot: device }),
        // HID++ 2.0 events carry software id 0; anything else is a reply to
        // another client.
        0x01..=0x3f if address & 0x0f == 0 => {
            let m = v20::Message::from(msg);
            Some(Notice::Event {
                slot: device,
                feature_index: sub_id,
                function: address >> 4,
                payload: m.extend_payload(),
            })
        }
        _ => None,
    }
}

// --- receivers -----------------------------------------------------------

/// An open receiver: its channel (and so the node) stays open while this
/// lives.
pub struct ReceiverLink {
    chan: Arc<HidppChannel>,
    kind: ReceiverKind,
    receiver: receiver::Receiver,
    _listener: MessageListenerGuard,
}

impl fmt::Debug for ReceiverLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceiverLink")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// Opens `raw` as a receiver. `Ok(None)`: HID++, but not a receiver `hidpp`
/// knows (a device on a cable, a Lightspeed headset, ...). `inspect` takes
/// software ids from the other end, so `doctor` next to a running daemon
/// does not share the daemon's id.
pub async fn open_receiver(
    raw: Box<dyn RawHidChannel>,
    node: &str,
    inspect: bool,
) -> Result<Option<(ReceiverLink, mpsc::UnboundedReceiver<Notice>)>> {
    let mut chan = HidppChannel::from_raw_channel(Boxed(raw)).await?;
    let (id, lease) = lease_sw_id(node, inspect).ok_or(AdapterError::NoSoftwareId)?;
    chan.set_sw_id_policy(SwIdPolicy::Leased {
        id,
        lease: Box::new(lease),
    });
    let chan = Arc::new(chan);
    let Some(receiver) = receiver::detect(chan.clone()) else {
        return Ok(None);
    };
    let kind = match &receiver {
        receiver::Receiver::Bolt(_) => ReceiverKind::Bolt,
        receiver::Receiver::Unifying(_) => ReceiverKind::Unifying,
        #[allow(unreachable_patterns)]
        _ => return Ok(None),
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let listener = chan.add_msg_listener_guarded(move |msg, matched| {
        if matched {
            return;
        }
        if let Some(n) = decode_notice(kind, msg) {
            let _ = tx.send(n);
        }
    });
    Ok(Some((
        ReceiverLink {
            chan,
            kind,
            receiver,
            _listener: listener,
        },
        rx,
    )))
}

impl ReceiverLink {
    pub fn kind(&self) -> ReceiverKind {
        self.kind
    }

    /// False once the node is gone.
    pub fn is_connected(&self) -> bool {
        self.chan.is_connected()
    }

    pub async fn unique_id(&self) -> Result<String> {
        Ok(self.receiver.get_unique_id().await?)
    }

    /// The receiver firmware (register 0xF1), best effort.
    pub async fn firmware(&self) -> Option<String> {
        let version = self
            .chan
            .read_register(RECEIVER_DEVICE_INDEX, 0xf1, [0x01, 0, 0])
            .await
            .ok()?;
        let build = self
            .chan
            .read_register(RECEIVER_DEVICE_INDEX, 0xf1, [0x02, 0, 0])
            .await
            .ok();
        let mut fw = format!("{:02X}.{:02X}", version[1], version[2]);
        if let Some(b) = build.filter(|b| b[0] == 0x02) {
            fw.push_str(&format!(".B{:02X}{:02X}", b[1], b[2]));
        }
        Some(fw)
    }

    /// Turns on wireless and software-present notifications (`hid-generic`
    /// sets none). Left on afterwards: Solaar relies on them too. Returns
    /// whether they had to be changed.
    pub async fn enable_notifications(&self) -> Result<bool> {
        let flags = receiver::read_notification_flags(&self.chan).await?;
        let want = flags | NotificationFlags::WIRELESS | NotificationFlags::SOFTWARE_PRESENT;
        if want == flags {
            return Ok(false);
        }
        receiver::write_notification_flags(&self.chan, want).await?;
        Ok(true)
    }

    /// Makes the receiver report every paired device's link state (0x41),
    /// without contacting the devices. The notices arrive before this
    /// returns.
    pub async fn trigger_arrival(&self) -> Result<()> {
        match &self.receiver {
            receiver::Receiver::Bolt(b) => b.trigger_device_arrival().await?,
            receiver::Receiver::Unifying(u) => u.trigger_device_arrival().await?,
            #[allow(unreachable_patterns)]
            _ => {}
        }
        Ok(())
    }

    /// The name the receiver stores for the device in `slot`.
    pub async fn codename(&self, slot: u8) -> Option<String> {
        match &self.receiver {
            receiver::Receiver::Bolt(b) => b.get_device_codename(slot).await.ok(),
            // Unifying keeps names at 0x40 + (slot - 1): [sub, length, name...].
            receiver::Receiver::Unifying(_) => {
                let r = self
                    .chan
                    .read_long_sub_register(
                        RECEIVER_DEVICE_INDEX,
                        0xb5,
                        0x40 + slot.checked_sub(1)?,
                        [0, 0],
                    )
                    .await
                    .ok()?;
                let len = usize::from(r[1]).min(14);
                String::from_utf8(r[2..2 + len].to_vec()).ok()
            }
            #[allow(unreachable_patterns)]
            _ => None,
        }
        .map(|s| s.trim_end_matches('\0').trim().to_owned())
        .filter(|s| !s.is_empty())
    }

    /// The unit id the receiver stores for the device in `slot`, formatted
    /// as the Linux kernel reports it as the serial (`16-fc-6e-8e`), so the
    /// kernel's (UPower's) reading of the same device can be joined.
    pub async fn unit_id(&self, slot: u8) -> Option<String> {
        let (sub, at) = match self.kind {
            // Bolt pairing information: [sub, kind, wpid, wpid, unit id...].
            ReceiverKind::Bolt => (0x50 + (slot & 0x0f), 4),
            // Unifying extended pairing information, as the kernel reads it:
            // 0x30 + (slot - 1): [sub, unit id...].
            ReceiverKind::Unifying => (0x30 + slot.checked_sub(1)?, 1),
        };
        let r = self
            .chan
            .read_long_sub_register(RECEIVER_DEVICE_INDEX, 0xb5, sub, [0, 0])
            .await
            .ok()?;
        let id = &r[at..at + 4];
        (id.iter().any(|b| *b != 0)).then(|| {
            id.iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join("-")
        })
    }

    /// Opens the device in `slot`: protocol check, feature lookup, identity
    /// and nothing else; 0x1982 only with `backlight`. Only call this while
    /// the receiver says it's online.
    pub async fn device(&self, slot: u8, backlight: bool) -> Result<DeviceLink> {
        DeviceLink::open(&self.chan, slot, backlight).await
    }
}

// --- devices -------------------------------------------------------------

/// Which battery feature a device has, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryKind {
    /// 0x1004; whether it reports a percentage (else only levels).
    Unified { percentage: bool },
    /// 0x1000.
    Status,
    /// 0x1001 (voltage; the percentage is estimated).
    Voltage,
}

/// One battery reading in Deskhand's terms.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Battery {
    pub percentage: Option<f64>,
    pub estimated: bool,
    pub level: Option<BatteryLevel>,
    pub state: ChargeState,
    pub low: Option<bool>,
}

/// What a device says about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    /// 0x0005 name ("MX Keys S").
    pub name: Option<String>,
    /// 0x0003 serial.
    pub serial: Option<String>,
    /// The active application firmware ("RBK 81.03.B0018").
    pub firmware: Option<String>,
}

/// What a device event means.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeviceEvent {
    Battery(Battery),
    /// 0x1982: the backlight changed on the device (keys, light sensor,
    /// battery).
    Backlight(BacklightChange),
    /// 0x1D4B: the device reconnected (woke, or was switched on).
    Reconnected,
}

/// A 0x1982 info event. It carries no timeouts, and the mode only in part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BacklightChange {
    pub levels: u8,
    pub level: u8,
    pub enabled: Option<bool>,
    /// `None` for a level set with the keys (temporary manual), which
    /// leaves the configured mode as it was.
    pub automatic: Option<bool>,
}

impl BacklightChange {
    fn from_status(levels: u8, level: u8, status: BacklightStatus) -> Self {
        let (enabled, automatic) = match status {
            BacklightStatus::DisabledBySoftware => (Some(false), None),
            BacklightStatus::AlsAutomatic | BacklightStatus::AlsSaturated => {
                (Some(true), Some(true))
            }
            BacklightStatus::PermanentManual => (Some(true), Some(false)),
            BacklightStatus::TemporaryManual => (Some(true), None),
            // Critical battery, or a status this crate doesn't know.
            _ => (None, None),
        };
        Self {
            levels,
            level,
            enabled,
            automatic,
        }
    }

    pub fn apply(&self, b: &mut Backlight) {
        b.levels = self.levels;
        b.level = self.level;
        if let Some(enabled) = self.enabled {
            b.enabled = enabled;
        }
        if let (Some(auto), Some(a)) = (self.automatic, b.automatic.as_mut()) {
            *a = auto;
        }
    }
}

/// 0x1982 counts its timeouts in 5 s steps, from 1 to 0x05A0 (2 hours).
const TIMEOUT_UNIT: u64 = 5;
const TIMEOUT_UNITS_MAX: u64 = 0x05a0;

fn timeout_units(d: Duration) -> u16 {
    d.as_secs()
        .div_ceil(TIMEOUT_UNIT)
        .clamp(1, TIMEOUT_UNITS_MAX) as u16
}

fn timeout(units: u16) -> Duration {
    Duration::from_secs(u64::from(units) * TIMEOUT_UNIT)
}

/// The configuration and info replies in Deskhand's terms.
fn backlight(config: &BacklightConfig, levels: u8, level: u8, timeouts: bool) -> Backlight {
    Backlight {
        enabled: config.enabled,
        level,
        levels,
        automatic: config
            .options
            .contains(BacklightOptions::AUTO_MODE_SUPPORTED)
            .then_some(config.mode == BacklightMode::Automatic),
        timeouts: timeouts.then(|| BacklightTimeouts {
            hands_out: timeout(config.duration_hands_out),
            hands_in: timeout(config.duration_hands_in),
            powered: timeout(config.duration_powered),
        }),
    }
}

/// The write for `cmd`, built from the configuration as read, so the
/// option bits and the fourth timeout go back unchanged. Like Solaar, the
/// level is only sent in permanent manual mode, the one where it holds.
fn backlight_write(config: &BacklightConfig, cmd: BacklightCommand) -> SetBacklightConfig {
    let mut set = SetBacklightConfig {
        enabled: config.enabled,
        options: config.options,
        mode: config.mode,
        effect: None,
        current_level: config.current_level,
        duration_hands_out: config.duration_hands_out,
        duration_hands_in: config.duration_hands_in,
        duration_powered: config.duration_powered,
        duration_not_powered: config.duration_not_powered,
    };
    match cmd {
        BacklightCommand::Enabled(on) => set.enabled = on,
        BacklightCommand::Level(level) => {
            set.enabled = true;
            set.mode = BacklightMode::PermanentManual;
            set.current_level = level;
        }
        BacklightCommand::Automatic(auto) => {
            set.enabled = true;
            set.mode = if auto {
                BacklightMode::Automatic
            } else {
                BacklightMode::PermanentManual
            };
        }
        BacklightCommand::Timeout(which, d) => {
            let units = timeout_units(d);
            match which {
                BacklightTimeout::HandsOut => set.duration_hands_out = units,
                BacklightTimeout::HandsIn => set.duration_hands_in = units,
                BacklightTimeout::Powered => set.duration_powered = units,
            }
        }
    }
    if set.mode != BacklightMode::PermanentManual {
        set.current_level = 0;
    }
    set
}

/// A device behind a receiver, with the features Deskhand uses.
pub struct DeviceLink {
    pub identity: Identity,
    pub battery: Option<BatteryKind>,
    battery_index: u8,
    wireless_index: Option<u8>,
    /// 0x1982: its index, the feature, and whether it has timeouts (v3).
    backlight: Option<(u8, Arc<BacklightFeature>, bool)>,
    unified: Option<Arc<UnifiedBatteryFeature>>,
    status: Option<Arc<BatteryStatusFeature>>,
    voltage: Option<Arc<BatteryVoltageFeature>>,
}

impl fmt::Debug for DeviceLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceLink")
            .field("identity", &self.identity)
            .field("battery", &self.battery)
            .field("backlight", &self.backlight.is_some())
            .finish_non_exhaustive()
    }
}

impl DeviceLink {
    /// `backlight`: also look up 0x1982 (only when the control is enabled).
    async fn open(chan: &Arc<HidppChannel>, slot: u8, backlight: bool) -> Result<Self> {
        let mut dev = Device::new(chan.clone(), slot).await?;
        if !matches!(dev.protocol_version, ProtocolVersion::V20 { .. }) {
            return Err(AdapterError::NotFound);
        }
        let root = dev.root();
        let find = async |id: u16| -> Result<Option<(u8, u8)>> {
            Ok(root.get_feature(id).await?.map(|f| (f.index, f.version)))
        };
        let info = find(DeviceInformationFeature::ID).await?;
        let name = find(DeviceTypeAndNameFeature::ID).await?;
        let unified = find(UnifiedBatteryFeature::ID).await?;
        let (status, voltage) = if unified.is_some() {
            (None, None)
        } else {
            let status = find(BatteryStatusFeature::ID).await?;
            let voltage = if status.is_none() {
                find(BatteryVoltageFeature::ID).await?
            } else {
                None
            };
            (status, voltage)
        };
        let wireless_index = find(WirelessDeviceStatusFeature::ID).await?.map(|(i, _)| i);
        let backlight = if backlight {
            find(BacklightFeature::ID).await?
        } else {
            None
        };

        let mut identity = Identity::default();
        if let Some((index, _)) = name {
            let f = dev.add_feature::<DeviceTypeAndNameFeature>(index);
            identity.name = f
                .get_whole_device_name()
                .await
                .ok()
                .map(|s| s.trim_end_matches('\0').trim().to_owned())
                .filter(|s| !s.is_empty());
        }
        if let Some((index, _)) = info {
            let f = dev.add_feature::<DeviceInformationFeature>(index);
            if let Ok(i) = f.get_device_info().await {
                if i.capabilities.serial_number {
                    identity.serial = f
                        .get_serial_number()
                        .await
                        .ok()
                        .map(|s| s.trim_end_matches('\0').trim().to_owned())
                        .filter(|s| !s.is_empty());
                }
                let mut firmware = None;
                for e in 0..i.entity_count {
                    let Ok(fw) = f.get_fw_info(e).await else {
                        continue;
                    };
                    if fw.entity_type == DeviceEntityType::MainApplication {
                        let text = format!(
                            "{} {:02}.{:02}.B{:04}",
                            fw.firmware_prefix.trim_end_matches('\0'),
                            fw.firmware_number,
                            fw.revision,
                            fw.build
                        );
                        let active = fw.active;
                        if active || firmware.is_none() {
                            firmware = Some(text.trim().to_owned());
                        }
                        if active {
                            break;
                        }
                    }
                }
                identity.firmware = firmware;
            }
        }

        let mut link = Self {
            identity,
            battery: None,
            battery_index: 0,
            wireless_index,
            backlight: backlight.map(|(index, version)| {
                (
                    index,
                    dev.add_feature::<BacklightFeature>(index),
                    version >= 3,
                )
            }),
            unified: None,
            status: None,
            voltage: None,
        };
        if let Some((index, _)) = unified {
            let f = dev.add_feature::<UnifiedBatteryFeature>(index);
            let percentage = f
                .get_battery_capabilities()
                .await
                .map(|c| c.percentage)
                .unwrap_or(true);
            link.battery = Some(BatteryKind::Unified { percentage });
            link.battery_index = index;
            link.unified = Some(f);
        } else if let Some((index, _)) = status {
            link.battery = Some(BatteryKind::Status);
            link.battery_index = index;
            link.status = Some(dev.add_feature::<BatteryStatusFeature>(index));
        } else if let Some((index, _)) = voltage {
            link.battery = Some(BatteryKind::Voltage);
            link.battery_index = index;
            link.voltage = Some(dev.add_feature::<BatteryVoltageFeature>(index));
        }
        Ok(link)
    }

    /// Reads the battery once. `Ok(None)`: the device has no battery feature.
    pub async fn read_battery(&self) -> Result<Option<Battery>> {
        Ok(match self.battery {
            Some(BatteryKind::Unified { percentage }) => {
                let Some(f) = &self.unified else {
                    return Ok(None);
                };
                Some(unified(&f.get_battery_info().await?, percentage))
            }
            Some(BatteryKind::Status) => {
                let Some(f) = &self.status else {
                    return Ok(None);
                };
                let s = f.get_battery_level_status().await?;
                Some(legacy(s.discharge_level, s.status))
            }
            Some(BatteryKind::Voltage) => {
                let Some(f) = &self.voltage else {
                    return Ok(None);
                };
                Some(voltage(&f.get_battery_info().await?))
            }
            None => None,
        })
    }

    /// Reads the backlight. `Ok(None)`: no 0x1982 (or not looked up).
    pub async fn read_backlight(&self) -> Result<Option<Backlight>> {
        let Some((_, f, timeouts)) = &self.backlight else {
            return Ok(None);
        };
        let config = f.get_backlight_config().await?;
        let info = f.get_backlight_info().await?;
        Ok(Some(backlight(
            &config,
            info.nb_levels,
            info.current_level,
            *timeouts,
        )))
    }

    /// Changes the backlight (read, modify, write; the device keeps it
    /// across power cycles) and reads it back.
    pub async fn set_backlight(&self, cmd: BacklightCommand) -> Result<Option<Backlight>> {
        let Some((_, f, _)) = &self.backlight else {
            return Ok(None);
        };
        let config = f.get_backlight_config().await?;
        f.set_backlight_config(backlight_write(&config, cmd))
            .await?;
        self.read_backlight().await
    }

    /// Decodes an event from this device, if it's one Deskhand uses.
    pub fn decode(
        &self,
        feature_index: u8,
        function: u8,
        payload: &[u8; 16],
    ) -> Option<DeviceEvent> {
        if Some(feature_index) == self.wireless_index {
            return match WirelessDeviceStatusEvent::decode(function, payload)? {
                WirelessDeviceStatusEvent::StatusBroadcast(b)
                    if b.status == WirelessDeviceStatus::Reconnection =>
                {
                    Some(DeviceEvent::Reconnected)
                }
                _ => None,
            };
        }
        if let Some((index, ..)) = &self.backlight
            && feature_index == *index
        {
            return match BacklightEvent::decode(function, payload)? {
                BacklightEvent::InfoChanged(i) => Some(DeviceEvent::Backlight(
                    BacklightChange::from_status(i.nb_levels, i.current_level, i.status),
                )),
                #[allow(unreachable_patterns)]
                _ => None,
            };
        }
        if feature_index != self.battery_index {
            return None;
        }
        decode_battery(self.battery?, function, payload).map(DeviceEvent::Battery)
    }
}

/// Decodes a battery event (function 0, the status broadcast) of `kind`.
pub fn decode_battery(kind: BatteryKind, function: u8, payload: &[u8; 16]) -> Option<Battery> {
    if function != 0 {
        return None;
    }
    match kind {
        BatteryKind::Unified { percentage } => match BatteryEvent::decode(function, payload)? {
            BatteryEvent::InfoUpdate(info) => Some(unified(&info, percentage)),
            #[allow(unreachable_patterns)]
            _ => None,
        },
        // 0x1000 broadcasts the getBatteryLevelStatus layout: level, next, status.
        BatteryKind::Status => {
            let status = LegacyBatteryStatus::try_from(payload[2]).ok()?;
            Some(legacy(payload[0], status))
        }
        BatteryKind::Voltage => Some(voltage(&VoltageBatteryInfo::from_wire(payload))),
    }
}

fn unified(info: &BatteryInfo, has_percentage: bool) -> Battery {
    let level = match info.level {
        unified_battery::BatteryLevel::Critical => BatteryLevel::Critical,
        unified_battery::BatteryLevel::Low => BatteryLevel::Low,
        unified_battery::BatteryLevel::Good => BatteryLevel::Normal,
        unified_battery::BatteryLevel::Full => BatteryLevel::Full,
        #[allow(unreachable_patterns)]
        _ => BatteryLevel::Normal,
    };
    let (percentage, estimated) = if has_percentage {
        (Some(f64::from(info.charging_percentage)), false)
    } else {
        // Levels only: Solaar's approximations.
        let p = match level {
            BatteryLevel::Critical => 5.0,
            BatteryLevel::Low => 20.0,
            BatteryLevel::Normal => 50.0,
            BatteryLevel::Full => 90.0,
        };
        (Some(p), true)
    };
    let state = match info.status {
        BatteryStatus::Discharging => ChargeState::Discharging,
        BatteryStatus::Charging
        | BatteryStatus::ChargingNearlyFull
        | BatteryStatus::ChargingSlow => ChargeState::Charging,
        BatteryStatus::Full => ChargeState::Full,
        _ => ChargeState::Error,
    };
    Battery {
        percentage,
        estimated,
        level: Some(level),
        state,
        low: None,
    }
}

fn legacy(level: u8, status: LegacyBatteryStatus) -> Battery {
    let state = match status {
        LegacyBatteryStatus::Discharging => ChargeState::Discharging,
        LegacyBatteryStatus::Recharging
        | LegacyBatteryStatus::AlmostFull
        | LegacyBatteryStatus::SlowRecharge => ChargeState::Charging,
        LegacyBatteryStatus::Full => ChargeState::Full,
        _ => ChargeState::Error,
    };
    Battery {
        // 0 means "unknown" on devices that only report a status.
        percentage: (level > 0).then(|| f64::from(level.min(100))),
        estimated: false,
        level: None,
        state,
        low: None,
    }
}

/// Voltage → percentage for a Li-ion cell (Solaar's table), interpolated.
const VOLTAGE_CURVE: [(u16, f64); 13] = [
    (4186, 100.0),
    (4067, 90.0),
    (3989, 80.0),
    (3922, 70.0),
    (3859, 60.0),
    (3811, 50.0),
    (3778, 40.0),
    (3751, 30.0),
    (3717, 20.0),
    (3671, 10.0),
    (3646, 5.0),
    (3579, 2.0),
    (3500, 0.0),
];

fn voltage_percent(mv: u16) -> f64 {
    if mv >= VOLTAGE_CURVE[0].0 {
        return 100.0;
    }
    for pair in VOLTAGE_CURVE.windows(2) {
        let ((hi_mv, hi_p), (lo_mv, lo_p)) = (pair[0], pair[1]);
        if mv >= lo_mv {
            let t = f64::from(mv - lo_mv) / f64::from(hi_mv - lo_mv);
            return lo_p + t * (hi_p - lo_p);
        }
    }
    0.0
}

fn voltage(info: &VoltageBatteryInfo) -> Battery {
    let state = match info.status {
        VoltageChargingStatus::Discharging => ChargeState::Discharging,
        VoltageChargingStatus::Charging
        | VoltageChargingStatus::ChargingFast
        | VoltageChargingStatus::ChargingSlow => ChargeState::Charging,
        VoltageChargingStatus::Full => ChargeState::Full,
        VoltageChargingStatus::NotCharging => ChargeState::PendingCharge,
        _ => ChargeState::Unknown,
    };
    Battery {
        percentage: (info.voltage_mv > 0).then(|| voltage_percent(info.voltage_mv).round()),
        estimated: true,
        level: None,
        state,
        low: info.critical.then_some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short(b: [u8; 6]) -> HidppMessage {
        HidppMessage::Short(b)
    }

    fn long(head: &[u8]) -> HidppMessage {
        let mut b = [0u8; 19];
        b[..head.len()].copy_from_slice(head);
        HidppMessage::Long(b)
    }

    #[test]
    fn link_notices_from_the_capture() {
        // M16: `10 01 41 10 41 78 b3` (down) and `10 01 41 10 01 78 b3` (up).
        let down = decode_notice(
            ReceiverKind::Bolt,
            short([0x01, 0x41, 0x10, 0x41, 0x78, 0xb3]),
        );
        assert_eq!(
            down,
            Some(Notice::Connection {
                slot: 1,
                wpid: 0xb378,
                kind: 1,
                online: false,
                encrypted: true,
            })
        );
        let up = decode_notice(
            ReceiverKind::Bolt,
            short([0x01, 0x41, 0x10, 0x01, 0x78, 0xb3]),
        );
        assert!(matches!(up, Some(Notice::Connection { online: true, .. })));
    }

    #[test]
    fn events_replies_and_unpairing() {
        // 0x1004 event (software id 0) vs. Solaar's reply (software id 7).
        let event = decode_notice(
            ReceiverKind::Bolt,
            long(&[0x01, 0x08, 0x00, 0x64, 0x08, 0x01, 0x01]),
        );
        let Some(Notice::Event {
            slot: 1,
            feature_index: 0x08,
            function: 0,
            payload,
        }) = event
        else {
            panic!("not an event: {event:?}");
        };
        assert_eq!(payload[..4], [0x64, 0x08, 0x01, 0x01]);
        assert_eq!(
            decode_notice(ReceiverKind::Bolt, long(&[0x01, 0x08, 0x17, 0x64])),
            None
        );
        assert_eq!(
            decode_notice(ReceiverKind::Unifying, short([0x02, 0x40, 0x02, 0, 0, 0])),
            Some(Notice::Unpaired { slot: 2 })
        );
        // Receiver register replies are not notices.
        assert_eq!(
            decode_notice(ReceiverKind::Bolt, short([0xff, 0x80, 0x02, 0, 0, 0])),
            None
        );
    }

    #[test]
    fn battery_decoding() {
        let mut p = [0u8; 16];
        p[..4].copy_from_slice(&[0x64, 0x08, 0x01, 0x01]);
        let b = decode_battery(BatteryKind::Unified { percentage: true }, 0, &p).unwrap();
        assert_eq!(b.percentage, Some(100.0));
        assert_eq!(b.state, ChargeState::Charging);
        assert_eq!(b.level, Some(BatteryLevel::Full));
        assert!(!b.estimated);
        let b = decode_battery(BatteryKind::Unified { percentage: false }, 0, &p).unwrap();
        assert_eq!((b.percentage, b.estimated), (Some(90.0), true));
        assert_eq!(decode_battery(BatteryKind::Status, 1, &p), None);

        let mut p = [0u8; 16];
        p[..3].copy_from_slice(&[50, 20, 0]);
        let b = decode_battery(BatteryKind::Status, 0, &p).unwrap();
        assert_eq!(
            (b.percentage, b.state),
            (Some(50.0), ChargeState::Discharging)
        );

        // 3.811 V discharging; 3.6 V critical.
        let mut p = [0u8; 16];
        p[..3].copy_from_slice(&[0x0e, 0xe3, 0x00]);
        let b = decode_battery(BatteryKind::Voltage, 0, &p).unwrap();
        assert_eq!((b.percentage, b.estimated), (Some(50.0), true));
        p[..3].copy_from_slice(&[0x0e, 0x10, 0x20]);
        let b = decode_battery(BatteryKind::Voltage, 0, &p).unwrap();
        assert_eq!(b.low, Some(true));
        assert!(b.percentage.unwrap() < 5.0);
    }

    #[test]
    fn backlight_events_and_timeouts() {
        let mut b = Backlight {
            enabled: true,
            level: 2,
            levels: 8,
            automatic: Some(true),
            timeouts: None,
        };
        let change = |bytes: [u8; 4]| {
            let mut p = [0u8; 16];
            p[..4].copy_from_slice(&bytes);
            let Some(BacklightEvent::InfoChanged(i)) = BacklightEvent::decode(0, &p) else {
                panic!("not decoded: {bytes:x?}");
            };
            BacklightChange::from_status(i.nb_levels, i.current_level, i.status)
        };
        // A key press from the capture (temporary manual): the mode stays.
        change([0x08, 0x01, 0x04, 0x00]).apply(&mut b);
        assert_eq!((b.level, b.automatic, b.enabled), (1, Some(true), true));
        // Permanent manual, then disabled by software.
        change([0x08, 0x05, 0x05, 0x00]).apply(&mut b);
        assert_eq!((b.level, b.automatic), (5, Some(false)));
        change([0x08, 0x00, 0x00, 0x00]).apply(&mut b);
        assert!(!b.enabled);
        // No light sensor: the mode stays unknown.
        b.automatic = None;
        change([0x08, 0x02, 0x02, 0x00]).apply(&mut b);
        assert_eq!((b.automatic, b.enabled), (None, true));

        assert_eq!(timeout_units(Duration::ZERO), 1);
        assert_eq!(timeout_units(Duration::from_secs(16)), 4);
        assert_eq!(timeout_units(Duration::from_secs(9000)), 0x05a0);
        assert_eq!(timeout(3), Duration::from_secs(15));
    }

    #[test]
    fn software_ids_are_leased_per_node() {
        let (a, la) = lease_sw_id("/dev/test-lease", false).unwrap();
        let (b, _lb) = lease_sw_id("/dev/test-lease", false).unwrap();
        let (c, _lc) = lease_sw_id("/dev/test-lease", true).unwrap();
        assert_eq!(a.get().to_lo(), 0x0b);
        assert_eq!(b.get().to_lo(), 0x0c);
        assert_eq!(c.get().to_lo(), 0x08);
        drop(la);
        let (d, _ld) = lease_sw_id("/dev/test-lease", false).unwrap();
        assert_eq!(d.get().to_lo(), 0x0b);
        let (e, _le) = lease_sw_id("/dev/other-lease", false).unwrap();
        assert_eq!(e.get().to_lo(), 0x0b);
    }
}
