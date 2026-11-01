//! Shared Linux integrations for Deskhand.
//!
//! Freedesktop interfaces that every Linux desktop backend reuses: logind,
//! ScreenSaver, notifications, MPRIS, UPower, power profiles, BlueZ, systemd,
//! NetworkManager, portal settings, PulseAudio/PipeWire audio, camera and
//! Wayland idle detection.

pub mod bluetooth;
pub mod env;
pub mod gamemode;
pub mod identity;
pub mod idle;
pub mod lock;
pub mod network;
pub mod power;
pub mod systemd;
pub mod upower;
mod util;

#[cfg(test)]
mod testutil;

use std::sync::Arc;
use std::time::Duration;

use deskhand_core::Integration;

pub use env::LinuxEnv;
pub use identity::platform_identity;

/// Every Linux integration, in priority order per capability.
/// `systemd_units` is the `[integrations.systemd] units` allowlist.
pub fn candidates(
    env: &Arc<LinuxEnv>,
    idle_threshold: Duration,
    systemd_units: Vec<String>,
) -> Vec<Box<dyn Integration>> {
    vec![
        Box::new(lock::LockLogind::new(env.clone())),
        Box::new(lock::LockScreenSaver::new(env.clone())),
        Box::new(power::PowerLogind::new(env.clone())),
        Box::new(power::KeepAwake::new(env.clone())),
        Box::new(idle::IdleWayland::new(idle_threshold)),
        Box::new(idle::IdleScreenSaver::new(env.clone(), idle_threshold)),
        Box::new(idle::IdleLogind::new(env.clone())),
        Box::new(upower::Battery::new(env.clone())),
        Box::new(upower::PowerProfile::new(env.clone())),
        Box::new(network::Network::new(env.clone())),
        Box::new(bluetooth::Bluetooth::new(env.clone())),
        Box::new(systemd::SystemdUnits::new(env.clone(), systemd_units)),
        Box::new(gamemode::GameMode::new(env.clone())),
    ]
}
