//! Logitech HID++ peripheral batteries for Deskhand.
//!
//! [`HidppSource`] is a `peripheral_battery` source: it finds Logi Bolt and
//! Unifying receivers through a platform [`HidBackend`], tracks every paired
//! device's link, and reports batteries (0x1004, else 0x1000, else 0x1001)
//! from the devices' own events. Sleeping devices are never contacted.
//!
//! Nothing here is platform-specific: Linux provides the backend over
//! `/dev/hidraw*` (`deskhand_linux::hidraw`). The protocol comes from
//! `openlogi-hidpp`, used only in the `adapter` module.

mod adapter;
pub mod backend;
pub mod inspect;
mod receiver;
mod source;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

pub use backend::{Access, HidBackend, HidBus, HidNode, Hotplug, LOGITECH_VENDOR, RawHidChannel};
pub use inspect::inspect;
pub use source::{HidppSource, candidates};
