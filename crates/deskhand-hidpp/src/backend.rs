//! How a platform reaches HID devices: [`HidBackend`] lists the nodes that
//! speak HID++, opens them as `hidpp` transports and reports hotplug.
//!
//! Linux implements it over `/dev/hidraw*` (`deskhand_linux::hidraw`);
//! macOS (IOHID) and Windows (HID API) can follow without touching the rest
//! of this crate.

use std::io;

pub use hidpp::channel::RawHidChannel;

/// Logitech's USB vendor id.
pub const LOGITECH_VENDOR: u16 = 0x046d;

/// The bus a HID node sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidBus {
    Usb,
    Bluetooth,
    Other,
}

/// One HID interface that carries HID++ (vendor page 0xFF00 with the short
/// or long HID++ report).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidNode {
    /// Platform path or id, unique while the node exists (`/dev/hidraw10`).
    pub id: String,
    pub bus: HidBus,
    pub vendor: u16,
    pub product: u16,
    /// What the platform calls it ("Logitech USB Receiver").
    pub name: String,
    /// The kernel driver bound to it, where the platform has such a thing.
    pub driver: Option<String>,
    /// A kernel driver owns the device behind this node and already reports
    /// its battery (Linux: `logitech-hidpp-device`). Deskhand leaves it alone.
    pub kernel_owned: bool,
    /// A kernel driver handles the receiver (Linux: `logitech-djreceiver`)
    /// and keeps its notification flags set itself.
    pub kernel_receiver: bool,
}

/// Whether a node can be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    Ok,
    /// Permission denied: the udev rule (or Solaar's) is missing.
    Denied,
    Error(String),
}

/// A platform's HID access.
#[async_trait::async_trait]
pub trait HidBackend: Send + Sync + 'static {
    /// Every node that speaks HID++ (any vendor; callers filter).
    fn enumerate(&self) -> io::Result<Vec<HidNode>>;

    /// Whether [`HidBackend::open`] would succeed, without keeping it open.
    fn access(&self, node: &HidNode) -> Access;

    /// Opens a node for reading and writing reports.
    fn open(&self, node: &HidNode) -> io::Result<Box<dyn RawHidChannel>>;

    /// Starts watching for nodes appearing, disappearing or changing access.
    fn hotplug(&self) -> io::Result<Box<dyn Hotplug>>;

    /// The command that grants the user access to Logitech nodes, shown when
    /// a node can't be opened.
    fn access_hint(&self) -> Option<&'static str> {
        None
    }
}

/// Node changes, from [`HidBackend::hotplug`].
#[async_trait::async_trait]
pub trait Hotplug: Send {
    /// Resolves after something changed (debounced by the backend). An error
    /// ends the watch.
    async fn changed(&mut self) -> io::Result<()>;
}
