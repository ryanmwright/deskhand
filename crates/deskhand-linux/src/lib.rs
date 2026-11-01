//! Shared Linux integrations for Deskhand.
//!
//! Freedesktop interfaces that every Linux desktop backend reuses: logind,
//! ScreenSaver, notifications, MPRIS, UPower, power profiles, BlueZ, systemd,
//! NetworkManager, portal settings, PulseAudio/PipeWire audio, camera and
//! Wayland idle detection.

pub mod env;
pub mod identity;
pub mod idle;
pub mod lock;
pub mod power;
mod util;

use std::sync::Arc;
use std::time::Duration;

use deskhand_core::Integration;

pub use env::LinuxEnv;
pub use identity::platform_identity;

/// Every Linux integration, in priority order per capability.
pub fn candidates(env: &Arc<LinuxEnv>, idle_threshold: Duration) -> Vec<Box<dyn Integration>> {
    vec![
        Box::new(lock::LockLogind::new(env.clone())),
        Box::new(lock::LockScreenSaver::new(env.clone())),
        Box::new(power::PowerLogind::new(env.clone())),
        Box::new(power::KeepAwake::new(env.clone())),
        Box::new(idle::IdleWayland::new(idle_threshold)),
        Box::new(idle::IdleScreenSaver::new(env.clone(), idle_threshold)),
        Box::new(idle::IdleLogind::new(env.clone())),
    ]
}
