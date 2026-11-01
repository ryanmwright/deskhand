//! Test utilities for Deskhand (dev-only, never published).
//!
//! - `FakeTransport` (re-exported from `deskhand-core`) plays the transport
//!   side of the hub.
//! - [`TestBus`] runs private session and system dbus-daemons.
//! - [`mocks`] are `#[zbus::interface]` stand-ins for desktop services, with
//!   handles that change their state and record calls.

pub mod bus;
pub mod mocks;

pub use bus::{TestBus, TestBusError};
pub use deskhand_core::testing::FakeTransport;
