//! Platform-neutral integrations for Deskhand.
//!
//! Scripts, command sensors, system metrics, derived sensors such as
//! `session_state` and `in_call`, and the domain kits (media, audio, power)
//! that keep entity ids identical across operating systems.

pub mod command_sensors;
pub mod derived;
pub mod info;
pub mod metrics;
pub mod process;
pub mod scripts;
pub mod time;

pub use command_sensors::CommandSensors;
pub use metrics::Metrics;
pub use scripts::Scripts;
