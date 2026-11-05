//! Shared Linux integrations for Deskhand.
//!
//! Freedesktop interfaces that every Linux desktop backend reuses: logind,
//! ScreenSaver, notifications, MPRIS, UPower, power profiles, BlueZ, systemd,
//! NetworkManager, portal settings, PulseAudio/PipeWire audio, camera,
//! Wayland idle detection, and raw HID for Logitech HID++ (`hidraw`).

pub mod appearance;
#[cfg(feature = "audio-pulse")]
pub mod audio;
pub mod bluetooth;
pub mod camera;
pub mod env;
pub mod gamemode;
#[cfg(feature = "hidpp")]
pub mod hidraw;
pub mod identity;
pub mod idle;
pub mod lock;
pub mod mpris;
pub mod network;
pub mod notifications;
pub mod power;
pub mod rfkill;
pub mod streams;
pub mod systemd;
pub mod upower;
pub mod util;

#[cfg(test)]
mod testutil;

use std::sync::Arc;
use std::time::Duration;

use deskhand_core::Integration;

pub use env::LinuxEnv;
pub use identity::platform_identity;

/// Peripheral battery sources, highest priority first: HID++ wins over
/// UPower while it has a link to the device.
fn peripheral_sources(env: &Arc<LinuxEnv>) -> Vec<Arc<dyn deskhand_common::PeripheralSource>> {
    vec![
        #[cfg(feature = "hidpp")]
        Arc::new(deskhand_hidpp::HidppSource::new(Arc::new(
            hidraw::LinuxHidraw::new(),
        ))),
        Arc::new(upower::UPowerPeripherals::new(env.clone())),
    ]
}

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
        Box::new(deskhand_common::PeripheralBattery::new(peripheral_sources(
            env,
        ))),
        Box::new(upower::PowerProfile::new(env.clone())),
        Box::new(network::Network::new(env.clone())),
        Box::new(bluetooth::Bluetooth::new(env.clone())),
        Box::new(systemd::SystemdUnits::new(env.clone(), systemd_units)),
        Box::new(gamemode::GameMode::new(env.clone())),
        Box::new(mpris::MediaMpris::new(env.clone())),
        Box::new(notifications::Notifications::new(env.clone())),
        #[cfg(feature = "audio-pulse")]
        Box::new(audio::AudioPulse::new(env.streams().clone())),
        Box::new(camera::Camera::new()),
        Box::new(appearance::Appearance::portal(env.clone())),
    ]
}
