//! Raw HID access through `/dev/hidraw*`, for Logitech HID++ (feature
//! `hidpp`): the Linux [`HidBackend`] of `deskhand-hidpp`.
//!
//! Nodes are found through sysfs (`/sys/class/hidraw/<node>/device`, the HID
//! device with its `uevent`, `report_descriptor` and bound `driver`). A node
//! counts as HID++ when its report descriptor declares report 0x10 or 0x11
//! on a vendor page (0xFF00 and up). [`HidrawChannel`] implements `hidpp`'s
//! `RawHidChannel` with non-blocking `read`/`write` driven by tokio's
//! reactor: no C library, no ioctls beyond `FIONBIO`, no polling. (The one
//! `unsafe` is tokio's `AsyncFd::register`, for an fd the channel owns.)
//! [`LinuxHidraw`] adds hotplug through inotify on `/dev`.
//!
//! The nodes are `root 0600` unless a udev rule grants access
//! (`TAG+="uaccess"`, [`UDEV_RULE`], or Solaar's rules), which `open`
//! reports as `PermissionDenied`.

use std::error::Error;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use deskhand_hidpp::{Access, HidBackend, HidBus as Bus, HidNode, Hotplug, RawHidChannel};
use futures_util::StreamExt;
use inotify::{EventMask, Inotify, WatchMask};
use tokio::io::unix::AsyncFd;

/// Logitech's USB vendor id.
pub const LOGITECH_VENDOR: u16 = 0x046d;

/// The udev rule that grants the logged-in user access to Logitech nodes.
pub const UDEV_RULE: &str = include_str!("../../../data/udev/70-deskhand-logitech.rules");
/// Where [`UDEV_RULE`] is installed by hand.
pub const UDEV_RULE_PATH: &str = "/etc/udev/rules.d/70-deskhand-logitech.rules";

/// The commands that install [`UDEV_RULE`] (one-time, as root).
pub const UDEV_INSTALL_HINT: &str = "deskhand udev-rule | sudo tee /etc/udev/rules.d/70-deskhand-logitech.rules >/dev/null && sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=hidraw";

/// Kernel driver that owns a Logitech device and reports its battery itself.
const KERNEL_DEVICE_DRIVER: &str = "logitech-hidpp-device";
/// Kernel driver for Unifying/Lightspeed receivers.
const KERNEL_RECEIVER_DRIVER: &str = "logitech-djreceiver";

/// How long hotplug waits for udev to finish (permissions) and for related
/// nodes, before reporting a change.
const SETTLE: Duration = Duration::from_millis(750);

const SYS_HIDRAW: &str = "/sys/class/hidraw";
const DEV: &str = "/dev";

/// The bus a HID device sits on (the first field of `HID_ID`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidBus {
    Usb,
    Bluetooth,
    Other(u16),
}

impl HidBus {
    fn from_code(code: u16) -> Self {
        match code {
            0x03 => Self::Usb,
            0x05 => Self::Bluetooth,
            other => Self::Other(other),
        }
    }
}

/// One `/dev/hidrawN` node and what sysfs says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidrawNode {
    /// `/dev/hidrawN`.
    pub path: PathBuf,
    /// The HID device directory in sysfs (holds `report_descriptor`).
    pub sys_device: PathBuf,
    pub bus: HidBus,
    pub vendor: u16,
    pub product: u16,
    /// `HID_NAME`, for example "Logitech USB Receiver".
    pub name: String,
    /// `HID_UNIQ` (often empty for receivers).
    pub uniq: String,
    /// The bound HID driver, for example `hid-generic` or `logitech-djreceiver`.
    pub driver: Option<String>,
}

impl HidrawNode {
    pub fn is_logitech(&self) -> bool {
        self.vendor == LOGITECH_VENDOR
    }

    pub fn report_descriptor(&self) -> io::Result<Vec<u8>> {
        std::fs::read(self.sys_device.join("report_descriptor"))
    }

    /// Whether the interface carries HID++ (short or long report on a
    /// vendor page).
    pub fn is_hidpp(&self) -> bool {
        self.report_descriptor().is_ok_and(|d| declares_hidpp(&d))
    }

    /// Opens the node for reading and writing. Needs a tokio runtime.
    pub fn open(&self) -> io::Result<HidrawChannel> {
        let descriptor = self.report_descriptor()?;
        let file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        HidrawChannel::from_file(file, self.vendor, self.product, descriptor)
    }

    fn to_hid_node(&self) -> HidNode {
        HidNode {
            id: self.path.display().to_string(),
            bus: match self.bus {
                HidBus::Usb => Bus::Usb,
                HidBus::Bluetooth => Bus::Bluetooth,
                HidBus::Other(_) => Bus::Other,
            },
            vendor: self.vendor,
            product: self.product,
            name: self.name.clone(),
            driver: self.driver.clone(),
            kernel_owned: self.driver.as_deref() == Some(KERNEL_DEVICE_DRIVER),
            kernel_receiver: self.driver.as_deref() == Some(KERNEL_RECEIVER_DRIVER),
        }
    }
}

/// Scans a HID report descriptor for report id 0x10 or 0x11 declared while
/// a vendor usage page (0xFF00..) is current.
pub fn declares_hidpp(descriptor: &[u8]) -> bool {
    let mut page: u32 = 0;
    let mut stack: Vec<u32> = Vec::new();
    let mut i = 0;
    while i < descriptor.len() {
        let prefix = descriptor[i];
        if prefix == 0xfe {
            // Long item: size in the next byte.
            let size = usize::from(*descriptor.get(i + 1).unwrap_or(&0));
            i += 3 + size;
            continue;
        }
        let size = match prefix & 0x03 {
            3 => 4,
            n => usize::from(n),
        };
        let Some(data) = descriptor.get(i + 1..i + 1 + size) else {
            break;
        };
        let value = data
            .iter()
            .rev()
            .fold(0u32, |acc, b| (acc << 8) | u32::from(*b));
        match prefix & 0xfc {
            0x04 => page = value,                       // Usage Page
            0xa4 => stack.push(page),                   // Push
            0xb4 => page = stack.pop().unwrap_or(page), // Pop
            0x84 if page >= 0xff00 && matches!(value, 0x10 | 0x11) => return true, // Report ID
            _ => {}
        }
        i += 1 + size;
    }
    false
}

/// All hidraw nodes on this machine. A missing `/sys/class/hidraw` (no HID
/// devices, or a container) is an empty list.
pub fn enumerate() -> io::Result<Vec<HidrawNode>> {
    enumerate_in(Path::new(SYS_HIDRAW), Path::new(DEV))
}

/// [`enumerate`] against another sysfs class directory and `/dev` (tests).
pub fn enumerate_in(sys_class: &Path, dev: &Path) -> io::Result<Vec<HidrawNode>> {
    let entries = match std::fs::read_dir(sys_class) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut nodes: Vec<HidrawNode> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_owned();
            node_from(&entry.path().join("device"), &dev.join(&name))
        })
        .collect();
    nodes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(nodes)
}

fn node_from(sys_device: &Path, path: &Path) -> Option<HidrawNode> {
    let uevent = std::fs::read_to_string(sys_device.join("uevent")).ok()?;
    let mut node = HidrawNode {
        path: path.to_owned(),
        sys_device: sys_device
            .canonicalize()
            .unwrap_or_else(|_| sys_device.to_owned()),
        bus: HidBus::Other(0),
        vendor: 0,
        product: 0,
        name: String::new(),
        uniq: String::new(),
        driver: None,
    };
    for line in uevent.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "HID_ID" => {
                let (bus, vendor, product) = parse_hid_id(value)?;
                node.bus = HidBus::from_code(bus);
                node.vendor = vendor;
                node.product = product;
            }
            "HID_NAME" => node.name = value.to_owned(),
            "HID_UNIQ" => node.uniq = value.to_owned(),
            "DRIVER" => node.driver = Some(value.to_owned()),
            _ => {}
        }
    }
    Some(node)
}

/// `HID_ID=0003:0000046D:0000C548` → (bus, vendor, product).
fn parse_hid_id(value: &str) -> Option<(u16, u16, u16)> {
    let mut parts = value.split(':');
    let bus = u32::from_str_radix(parts.next()?, 16).ok()?;
    let vendor = u32::from_str_radix(parts.next()?, 16).ok()?;
    let product = u32::from_str_radix(parts.next()?, 16).ok()?;
    Some((
        u16::try_from(bus).ok()?,
        u16::try_from(vendor).ok()?,
        u16::try_from(product).ok()?,
    ))
}

/// An open hidraw node, as `hidpp`'s transport.
#[derive(Debug)]
pub struct HidrawChannel {
    fd: AsyncFd<File>,
    vendor: u16,
    product: u16,
    descriptor: Vec<u8>,
    connected: AtomicBool,
}

impl HidrawChannel {
    /// A channel over an already open node (or a socket standing in for
    /// one). Switches it to non-blocking; needs a tokio runtime with I/O.
    pub fn from_file(
        file: File,
        vendor: u16,
        product: u16,
        descriptor: Vec<u8>,
    ) -> io::Result<Self> {
        rustix::io::ioctl_fionbio(&file, true)?;
        // SAFETY: `register` needs the fd to stay open and refer to the same
        // file for the AsyncFd's lifetime. The AsyncFd owns the `File`, which
        // closes its fd only on drop and never swaps it.
        #[allow(unsafe_code)]
        let fd = unsafe { AsyncFd::register(file) }.map_err(|e| e.into_parts().1)?;
        Ok(Self {
            fd,
            vendor,
            product,
            descriptor,
            connected: AtomicBool::new(true),
        })
    }

    fn gone(&self, e: &io::Error) -> bool {
        // ENODEV once the device is unplugged; EIO/ESHUTDOWN on some drivers.
        let gone = matches!(
            e.raw_os_error(),
            Some(code) if code == rustix::io::Errno::NODEV.raw_os_error()
                || code == rustix::io::Errno::IO.raw_os_error()
                || code == rustix::io::Errno::SHUTDOWN.raw_os_error()
        );
        if gone {
            self.connected.store(false, Ordering::Relaxed);
        }
        gone
    }
}

#[async_trait::async_trait]
impl RawHidChannel for HidrawChannel {
    fn vendor_id(&self) -> u16 {
        self.vendor
    }

    fn product_id(&self) -> u16 {
        self.product
    }

    async fn write_report(&self, src: &[u8]) -> Result<usize, Box<dyn Error + Sync + Send>> {
        loop {
            match self.fd.get_ref().write(src) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let mut guard = self.fd.writable().await?;
                    guard.clear_ready();
                }
                Err(e) => {
                    self.gone(&e);
                    return Err(Box::new(e));
                }
            }
        }
    }

    async fn read_report(&self, buf: &mut [u8]) -> Result<usize, Box<dyn Error + Sync + Send>> {
        loop {
            if !self.connected.load(Ordering::Relaxed) {
                // Permanent: park; the read loop's close signal ends it.
                std::future::pending::<()>().await;
            }
            let mut guard = match self.fd.readable().await {
                Ok(g) => g,
                Err(_) => {
                    // The reactor is gone (runtime shutting down): nothing
                    // more will be read.
                    self.connected.store(false, Ordering::Relaxed);
                    continue;
                }
            };
            match guard.try_io(|fd| fd.get_ref().read(buf)) {
                // End of file: the other end is gone (only sockets do this).
                Ok(Ok(0)) => self.connected.store(false, Ordering::Relaxed),
                Ok(Ok(n)) => return Ok(n),
                Ok(Err(e)) if self.gone(&e) => {}
                Ok(Err(e)) => return Err(Box::new(e)),
                Err(_would_block) => {}
            }
        }
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn supports_short_long_hidpp(&self) -> Option<(bool, bool)> {
        // Let hidpp parse the report descriptor.
        None
    }

    async fn get_report_descriptor(
        &self,
        buf: &mut [u8],
    ) -> Result<usize, Box<dyn Error + Sync + Send>> {
        let n = self.descriptor.len().min(buf.len());
        buf[..n].copy_from_slice(&self.descriptor[..n]);
        Ok(n)
    }
}

/// The hidraw [`HidBackend`]: Logitech HID++ nodes from sysfs, hotplug from
/// inotify on `/dev`.
#[derive(Debug, Clone)]
pub struct LinuxHidraw {
    sys_class: PathBuf,
    dev: PathBuf,
}

impl Default for LinuxHidraw {
    fn default() -> Self {
        Self {
            sys_class: SYS_HIDRAW.into(),
            dev: DEV.into(),
        }
    }
}

impl LinuxHidraw {
    pub fn new() -> Self {
        Self::default()
    }

    fn nodes(&self) -> io::Result<Vec<HidrawNode>> {
        Ok(enumerate_in(&self.sys_class, &self.dev)?
            .into_iter()
            .filter(|n| n.is_logitech() && n.is_hidpp())
            .collect())
    }
}

#[async_trait::async_trait]
impl HidBackend for LinuxHidraw {
    fn enumerate(&self) -> io::Result<Vec<HidNode>> {
        Ok(self.nodes()?.iter().map(HidrawNode::to_hid_node).collect())
    }

    fn access(&self, node: &HidNode) -> Access {
        match OpenOptions::new().read(true).write(true).open(&node.id) {
            Ok(_) => Access::Ok,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Access::Denied,
            Err(e) => Access::Error(e.to_string()),
        }
    }

    fn open(&self, node: &HidNode) -> io::Result<Box<dyn RawHidChannel>> {
        let found = self
            .nodes()?
            .into_iter()
            .find(|n| n.path.as_os_str() == node.id.as_str())
            .ok_or(io::ErrorKind::NotFound)?;
        Ok(Box::new(found.open()?))
    }

    fn hotplug(&self) -> io::Result<Box<dyn Hotplug>> {
        let inotify = Inotify::init()?;
        // Nodes appear and disappear (create/delete); udev grants access
        // afterwards (attrib: mode and ACL changes).
        inotify.watches().add(
            &self.dev,
            WatchMask::CREATE | WatchMask::DELETE | WatchMask::ATTRIB,
        )?;
        Ok(Box::new(DevWatch {
            events: inotify.into_event_stream([0u8; 4096])?,
        }))
    }

    fn access_hint(&self) -> Option<&'static str> {
        Some(UDEV_INSTALL_HINT)
    }
}

struct DevWatch {
    events: inotify::EventStream<[u8; 4096]>,
}

impl DevWatch {
    async fn next_hidraw(&mut self) -> io::Result<()> {
        loop {
            let ev = self
                .events
                .next()
                .await
                .ok_or_else(|| io::Error::other("inotify stream ended"))??;
            let hidraw = ev
                .name
                .as_ref()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("hidraw"));
            if hidraw || ev.mask.contains(EventMask::Q_OVERFLOW) {
                return Ok(());
            }
        }
    }
}

#[async_trait::async_trait]
impl Hotplug for DevWatch {
    async fn changed(&mut self) -> io::Result<()> {
        self.next_hidraw().await?;
        // Settle, taking in whatever else arrives meanwhile.
        let settle = tokio::time::sleep(SETTLE);
        tokio::pin!(settle);
        loop {
            tokio::select! {
                () = &mut settle => return Ok(()),
                r = self.next_hidraw() => r?,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The HID++ interface descriptor of Logitech receivers: vendor page
    /// 0xFF00, short report 0x10 (usage 1) and long report 0x11 (usage 2).
    const HIDPP_DESCRIPTOR: [u8; 54] = [
        0x06, 0x00, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x85, 0x10, 0x75, 0x08, 0x95, 0x06, 0x15, 0x00,
        0x26, 0xff, 0x00, 0x09, 0x01, 0x81, 0x00, 0x09, 0x01, 0x91, 0x00, 0xc0, 0x06, 0x00, 0xff,
        0x09, 0x02, 0xa1, 0x01, 0x85, 0x11, 0x75, 0x08, 0x95, 0x13, 0x15, 0x00, 0x26, 0xff, 0x00,
        0x09, 0x02, 0x81, 0x00, 0x09, 0x02, 0x91, 0x00, 0xc0,
    ];

    /// A boot keyboard: generic desktop page, no report ids.
    const KEYBOARD_DESCRIPTOR: [u8; 12] = [
        0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7,
    ];

    /// A mouse with report id 0x10 on the generic desktop page: not HID++.
    const MOUSE_WITH_ID_16: [u8; 10] = [0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x10, 0xc0, 0x00];

    fn fake_node(root: &Path, node: &str, uevent: &str, descriptor: &[u8]) {
        let dev = root.join("devices").join(node);
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("uevent"), uevent).unwrap();
        std::fs::write(dev.join("report_descriptor"), descriptor).unwrap();
        let class = root.join("class").join(node);
        std::fs::create_dir_all(&class).unwrap();
        std::os::unix::fs::symlink(&dev, class.join("device")).unwrap();
    }

    /// The KDE host of M16: the Bolt receiver's four interfaces on
    /// hid-generic (HID++ on hidraw10), the Lightspeed receiver on the DJ
    /// driver with its mouse on the kernel's HID++ driver, and a
    /// non-Logitech keyboard.
    fn host_tree(root: &Path) {
        let bolt = "DRIVER=hid-generic\nHID_ID=0003:0000046D:0000C548\nHID_NAME=Logitech USB Receiver\nHID_UNIQ=\n";
        fake_node(root, "hidraw8", bolt, &KEYBOARD_DESCRIPTOR);
        fake_node(root, "hidraw9", bolt, &MOUSE_WITH_ID_16);
        fake_node(root, "hidraw10", bolt, &HIDPP_DESCRIPTOR);
        fake_node(
            root,
            "hidraw11",
            "DRIVER=hid-multitouch\nHID_ID=0003:0000046D:0000C548\nHID_NAME=Logitech USB Receiver\n",
            &KEYBOARD_DESCRIPTOR,
        );
        fake_node(
            root,
            "hidraw3",
            "DRIVER=logitech-djreceiver\nHID_ID=0003:0000046D:0000C53F\nHID_NAME=Logitech USB Receiver\n",
            &HIDPP_DESCRIPTOR,
        );
        fake_node(
            root,
            "hidraw4",
            "DRIVER=logitech-hidpp-device\nHID_ID=0003:0000046D:00004074\nHID_NAME=Logitech G305\nHID_UNIQ=4074-aa-bb-cc-dd\n",
            &HIDPP_DESCRIPTOR,
        );
        fake_node(
            root,
            "hidraw0",
            "DRIVER=hid-generic\nHID_ID=0003:000004D9:0000A0F8\nHID_NAME=USB Keyboard\n",
            &KEYBOARD_DESCRIPTOR,
        );
    }

    #[test]
    fn enumerates_sysfs_nodes() {
        let tmp = tempfile::tempdir().unwrap();
        fake_node(
            tmp.path(),
            "hidraw2",
            "DRIVER=hid-generic\nHID_ID=0003:0000046D:0000C548\nHID_NAME=Logitech USB Receiver\nHID_PHYS=usb-0000:00:14.0-2/input2\nHID_UNIQ=\nMODALIAS=hid:b0003g0001v0000046Dp0000C548\n",
            &[0x06, 0x00, 0xff],
        );
        fake_node(
            tmp.path(),
            "hidraw0",
            "HID_ID=0005:0000046D:0000B378\nHID_NAME=MX Keys S\nHID_UNIQ=aa:bb:cc:dd:ee:ff\n",
            &[0x06, 0x00, 0xff],
        );
        let nodes = enumerate_in(&tmp.path().join("class"), Path::new("/dev")).unwrap();
        assert_eq!(nodes.len(), 2);
        let keys = &nodes[0];
        assert_eq!(keys.path, Path::new("/dev/hidraw0"));
        assert_eq!(keys.bus, HidBus::Bluetooth);
        assert_eq!((keys.vendor, keys.product), (0x046d, 0xb378));
        assert_eq!(keys.uniq, "aa:bb:cc:dd:ee:ff");
        assert_eq!(keys.driver, None);
        let bolt = &nodes[1];
        assert_eq!(bolt.bus, HidBus::Usb);
        assert_eq!(bolt.product, 0xc548);
        assert_eq!(bolt.name, "Logitech USB Receiver");
        assert_eq!(bolt.driver.as_deref(), Some("hid-generic"));
        assert!(bolt.is_logitech());
        assert_eq!(bolt.report_descriptor().unwrap(), [0x06, 0x00, 0xff]);
    }

    #[test]
    fn backend_lists_logitech_hidpp_interfaces_only() {
        let tmp = tempfile::tempdir().unwrap();
        host_tree(tmp.path());
        let backend = LinuxHidraw {
            sys_class: tmp.path().join("class"),
            dev: "/dev".into(),
        };
        let nodes = backend.enumerate().unwrap();
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["/dev/hidraw10", "/dev/hidraw3", "/dev/hidraw4"]);
        let bolt = &nodes[0];
        assert!(!bolt.kernel_owned && !bolt.kernel_receiver);
        assert!(nodes[1].kernel_receiver);
        assert!(nodes[2].kernel_owned);
        // The source skips the kernel's device node.
        let used: Vec<&str> = deskhand_hidpp::candidates(&nodes)
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(used, ["/dev/hidraw10", "/dev/hidraw3"]);
    }

    #[test]
    fn hidpp_descriptor_detection() {
        assert!(declares_hidpp(&HIDPP_DESCRIPTOR));
        assert!(!declares_hidpp(&KEYBOARD_DESCRIPTOR));
        assert!(!declares_hidpp(&MOUSE_WITH_ID_16));
        // Truncated items end the scan.
        assert!(!declares_hidpp(&[0x06, 0x00]));
        // Push/pop restore the page.
        assert!(!declares_hidpp(&[
            0x06, 0x00, 0xff, 0xa4, 0x05, 0x01, 0xb4, 0x05, 0x01, 0x85, 0x10
        ]));
        assert!(declares_hidpp(&[
            0x06, 0x00, 0xff, 0xa4, 0x05, 0x01, 0xb4, 0x85, 0x11
        ]));
    }

    #[test]
    fn missing_class_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let nodes = enumerate_in(&tmp.path().join("nope"), Path::new("/dev")).unwrap();
        assert!(nodes.is_empty());
    }

    /// A channel over one end of a socketpair; the other end plays the device.
    fn socket_channel() -> (HidrawChannel, std::os::unix::net::UnixStream) {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let file = File::from(std::os::fd::OwnedFd::from(ours));
        (
            HidrawChannel::from_file(file, LOGITECH_VENDOR, 0xc548, HIDPP_DESCRIPTOR.to_vec())
                .unwrap(),
            theirs,
        )
    }

    #[tokio::test]
    async fn reads_and_writes_reports() {
        let (chan, mut peer) = socket_channel();
        let report = [0x10, 0xff, 0x81, 0x00, 0x00, 0x00, 0x00];
        peer.write_all(&report).unwrap();
        let mut buf = [0u8; 64];
        let n = chan.read_report(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], report);
        chan.write_report(&[0x10, 0x01, 0x00, 0x1b, 0, 0, 0])
            .await
            .unwrap();
        let mut got = [0u8; 7];
        peer.read_exact(&mut got).unwrap();
        assert_eq!(got, [0x10, 0x01, 0x00, 0x1b, 0, 0, 0]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn hidpp_reads_through_the_reactor_and_drops_promptly() {
        // hidpp reads on its own thread; the reactor wakes it. Dropping the
        // channel joins that thread, so a read that never yields would hang.
        let (chan, mut peer) = socket_channel();
        let hidpp = hidpp::channel::HidppChannel::from_raw_channel(chan)
            .await
            .expect("HID++ descriptor accepted");
        let (tx, rx) = std::sync::mpsc::channel();
        let _guard = hidpp.add_msg_listener_guarded(move |msg, _| {
            let _ = tx.send(msg);
        });
        peer.write_all(&[0x10, 0x01, 0x41, 0x10, 0x01, 0x78, 0xb3])
            .unwrap();
        let msg = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(5)))
            .await
            .unwrap()
            .expect("report delivered");
        assert!(matches!(msg, hidpp::channel::HidppMessage::Short(b) if b[1] == 0x41));
        let dropped = tokio::task::spawn_blocking(move || drop(hidpp));
        tokio::time::timeout(Duration::from_secs(5), dropped)
            .await
            .expect("HidppChannel dropped within 5 s")
            .unwrap();
    }

    #[tokio::test]
    async fn a_closed_peer_parks_the_read_and_disconnects() {
        let (chan, peer) = socket_channel();
        drop(peer);
        let mut buf = [0u8; 64];
        let read = tokio::time::timeout(Duration::from_millis(200), chan.read_report(&mut buf));
        assert!(read.await.is_err(), "read must park, not spin or return");
        assert!(!chan.is_connected());
    }

    #[test]
    fn bad_hid_id_skips_the_node() {
        assert_eq!(
            parse_hid_id("0003:0000046D:0000C548"),
            Some((3, 0x046d, 0xc548))
        );
        assert_eq!(parse_hid_id("0003:0000046D"), None);
        assert_eq!(parse_hid_id("zz:1:2"), None);
    }

    #[test]
    fn udev_rule_is_embedded() {
        assert!(UDEV_RULE.contains("TAG+=\"uaccess\""));
        assert!(UDEV_INSTALL_HINT.contains(UDEV_RULE_PATH));
    }
}
