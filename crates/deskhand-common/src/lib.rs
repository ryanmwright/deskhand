//! Platform-neutral integrations for Deskhand.
//!
//! Scripts, `[[shortcut]]` config, command sensors, system metrics, derived sensors such as
//! `session_state` and `in_call`, and the domain kits (media, audio, power)
//! that keep entity ids identical across operating systems.

pub mod audio;
pub mod command_sensors;
pub mod derived;
pub mod in_call;
pub mod info;
pub mod media;
pub mod metrics;
pub mod process;
pub mod scripts;
pub mod session_state;
pub mod shortcuts;
pub mod time;

pub use audio::{AudioKit, AudioOptions, AudioSource};
pub use command_sensors::CommandSensors;
pub use in_call::InCall;
pub use media::{MediaKit, MediaOptions, MediaSource};
pub use metrics::Metrics;
pub use scripts::Scripts;
pub use session_state::SessionState;
