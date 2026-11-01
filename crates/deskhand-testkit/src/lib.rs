//! Test utilities for Deskhand (dev-only, never published).
//!
//! Provides `FakeTransport` (re-exported from `deskhand-core`) and, from M4,
//! `TestBus` (private session and system dbus-daemons), mock D-Bus services
//! and a mosquitto container helper.

pub use deskhand_core::testing::FakeTransport;
