//! MQTT transport for Deskhand.
//!
//! Implements the core [`Transport`](deskhand_core::Transport) trait with
//! `rumqttc`: topic layout ([`topics`]), Home Assistant device-based
//! discovery with stale-entity cleanup ([`discovery`]), state/command
//! encoding ([`payload`]), and the connection lifecycle ([`transport`]).
//! See `docs/v1-plan.md` §1 "MQTT and HA discovery".

pub mod config;
pub mod discovery;
pub mod payload;
pub mod topics;
pub mod transport;

pub use config::{MqttConfig, MqttConfigError};
pub use topics::Topics;
pub use transport::{MqttTimings, MqttTransport};
