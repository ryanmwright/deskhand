//! Raw HID access through `/dev/hidraw*`, for Logitech HID++ (feature `hidpp`).
//!
//! Nodes are found through sysfs (`/sys/class/hidraw/<node>/device`, the HID
//! device with its `uevent`, `report_descriptor` and bound `driver`).
//! [`HidrawChannel`] implements `hidpp`'s [`RawHidChannel`] with plain
//! `read`/`write` on the node: no C library and no ioctls.
//!
//! `hidpp` runs its read loop on its own thread under a simple executor and
//! races each read against a close signal, so a read waits in `poll(2)` for
//! at most [`READ_SLICE`] and then yields instead of blocking forever.
//!
//! The nodes are `root 0600` unless a udev rule grants access
//! (`TAG+="uaccess"`), which `open` reports as `PermissionDenied`.

use std::error::Error;
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use hidpp::channel::RawHidChannel;
use rustix::event::{PollFd, PollFlags, Timespec, poll};

/// Logitech's USB vendor id.
pub const LOGITECH_VENDOR: u16 = 0x046d;

/// How long one read waits in `poll(2)` before yielding to the read loop.
const READ_SLICE: Duration = Duration::from_millis(100);

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

    /// Opens the node for reading and writing.
    pub fn open(&self) -> io::Result<HidrawChannel> {
        let descriptor = self.report_descriptor()?;
        let file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        Ok(HidrawChannel::from_file(
            file,
            self.vendor,
            self.product,
            descriptor,
        ))
    }
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
    file: File,
    vendor: u16,
    product: u16,
    descriptor: Vec<u8>,
    connected: AtomicBool,
}

impl HidrawChannel {
    /// A channel over an already open node (or a socket standing in for one).
    pub fn from_file(file: File, vendor: u16, product: u16, descriptor: Vec<u8>) -> Self {
        Self {
            file,
            vendor,
            product,
            descriptor,
            connected: AtomicBool::new(true),
        }
    }

    /// Waits up to [`READ_SLICE`] for a report. `Ok(false)` on timeout.
    fn wait_readable(&self) -> io::Result<bool> {
        let mut fds = [PollFd::new(&self.file, PollFlags::IN)];
        let timeout = Timespec {
            tv_sec: READ_SLICE.as_secs() as _,
            tv_nsec: READ_SLICE.subsec_nanos() as _,
        };
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => Ok(false),
            Ok(_) => Ok(true),
            Err(rustix::io::Errno::INTR) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn gone(&self, e: &io::Error) -> bool {
        // ENODEV once the device is unplugged; EIO/ESHUTDOWN on some drivers.
        matches!(
            e.raw_os_error(),
            Some(code) if code == rustix::io::Errno::NODEV.raw_os_error()
                || code == rustix::io::Errno::IO.raw_os_error()
                || code == rustix::io::Errno::SHUTDOWN.raw_os_error()
        )
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
        match (&self.file).write(src) {
            Ok(n) => Ok(n),
            Err(e) => {
                if self.gone(&e) {
                    self.connected.store(false, Ordering::Relaxed);
                }
                Err(Box::new(e))
            }
        }
    }

    async fn read_report(&self, buf: &mut [u8]) -> Result<usize, Box<dyn Error + Sync + Send>> {
        loop {
            if !self.connected.load(Ordering::Relaxed) {
                // Permanent: park; the read loop's close signal ends it.
                std::future::pending::<()>().await;
            }
            match self.wait_readable() {
                Ok(true) => match (&self.file).read(buf) {
                    Ok(n) => return Ok(n),
                    Err(e) if self.gone(&e) => self.connected.store(false, Ordering::Relaxed),
                    Err(e) => return Err(Box::new(e)),
                },
                Ok(false) => YieldNow::default().await,
                Err(e) if self.gone(&e) => self.connected.store(false, Ordering::Relaxed),
                Err(e) => return Err(Box::new(e)),
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

/// Returns `Pending` once, so the caller's `select!` can see other branches.
#[derive(Default)]
struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_node(root: &Path, node: &str, uevent: &str) {
        let dev = root.join("devices").join(node);
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("uevent"), uevent).unwrap();
        std::fs::write(dev.join("report_descriptor"), [0x06, 0x00, 0xff]).unwrap();
        let class = root.join("class").join(node);
        std::fs::create_dir_all(&class).unwrap();
        std::os::unix::fs::symlink(&dev, class.join("device")).unwrap();
    }

    #[test]
    fn enumerates_sysfs_nodes() {
        let tmp = tempfile::tempdir().unwrap();
        fake_node(
            tmp.path(),
            "hidraw2",
            "DRIVER=hid-generic\nHID_ID=0003:0000046D:0000C548\nHID_NAME=Logitech USB Receiver\nHID_PHYS=usb-0000:00:14.0-2/input2\nHID_UNIQ=\nMODALIAS=hid:b0003g0001v0000046Dp0000C548\n",
        );
        fake_node(
            tmp.path(),
            "hidraw0",
            "HID_ID=0005:0000046D:0000B378\nHID_NAME=MX Keys S\nHID_UNIQ=aa:bb:cc:dd:ee:ff\n",
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
    fn missing_class_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let nodes = enumerate_in(&tmp.path().join("nope"), Path::new("/dev")).unwrap();
        assert!(nodes.is_empty());
    }

    /// The HID++ interface descriptor of Logitech receivers: vendor page
    /// 0xFF00, short report 0x10 (usage 1) and long report 0x11 (usage 2).
    const HIDPP_DESCRIPTOR: [u8; 54] = [
        0x06, 0x00, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x85, 0x10, 0x75, 0x08, 0x95, 0x06, 0x15, 0x00,
        0x26, 0xff, 0x00, 0x09, 0x01, 0x81, 0x00, 0x09, 0x01, 0x91, 0x00, 0xc0, 0x06, 0x00, 0xff,
        0x09, 0x02, 0xa1, 0x01, 0x85, 0x11, 0x75, 0x08, 0x95, 0x13, 0x15, 0x00, 0x26, 0xff, 0x00,
        0x09, 0x02, 0x81, 0x00, 0x09, 0x02, 0x91, 0x00, 0xc0,
    ];

    /// A channel over one end of a socketpair; the other end plays the device.
    fn socket_channel() -> (HidrawChannel, std::os::unix::net::UnixStream) {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let file = File::from(std::os::fd::OwnedFd::from(ours));
        (
            HidrawChannel::from_file(file, LOGITECH_VENDOR, 0xc548, HIDPP_DESCRIPTOR.to_vec()),
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

    #[test]
    fn hidpp_accepts_the_channel_and_drops_promptly() {
        // hidpp joins its read thread on drop; a read that never yields
        // would hang here.
        let (chan, _peer) = socket_channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let hidpp = block_on(hidpp::channel::HidppChannel::from_raw_channel(chan))
                .expect("HID++ descriptor accepted");
            drop(hidpp);
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("HidppChannel dropped within 5 s");
    }

    fn block_on<F: Future>(fut: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(fut)
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
}
