//! Platform-neutral core of Deskhand.
//!
//! - [`entity`]: what an integration exposes ([`EntitySpec`], [`State`], ...).
//! - [`integration`]: the [`Integration`] trait backends implement.
//! - [`ctx`]: the [`Ctx`] each integration run gets, and the typed entity handles.
//! - [`hub`]: the actor that owns all state and talks to the transport.
//! - [`supervisor`]: probes, starts and restarts integrations.
//! - [`transport`]: the boundary to MQTT (or any future transport).
//! - [`config`] and [`identity`].
//!
//! This crate must not depend on any platform crate, MQTT client or D-Bus
//! library. See `docs/v1-plan.md` §1.

pub mod config;
pub mod ctx;
pub mod entity;
pub mod hub;
pub mod identity;
pub mod integration;
pub mod supervisor;
pub mod transport;

#[cfg(any(test, feature = "test-util"))]
pub mod testing;

#[cfg(test)]
mod hub_tests;
#[cfg(test)]
mod supervisor_tests;

pub use config::{Config, ConfigError, Enabled, IntegrationConfig};
pub use ctx::{Commands, Ctx};
pub use entity::{Attributes, CommandValue, EntityId, EntityKind, EntitySpec, State, SubDevice};
pub use hub::{Hub, HubHandle, HubOptions, HubStatus, Lifecycle};
pub use identity::{DeviceInfo, PlatformIdentity};
pub use integration::{Integration, IntegrationError, Probe};
pub use supervisor::{StatusHandle, Supervisor, SupervisorOptions};
pub use transport::{HubEvent, Snapshot, Transport, TransportEvent, TransportLink};

/// Reverse-DNS application id, used as the D-Bus well-known name prefix,
/// the desktop file name and the portal app id. Defined once here.
///
/// Do not derive anything persistent from this that users configure (for
/// example the kglobalaccel component name); see `docs/v1-plan.md` M11.
pub const APP_ID: &str = "io.github.ryanmwright.Deskhand";

/// Crate version, reported in HA discovery and `deskhand doctor`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_is_reverse_dns() {
        assert_eq!(APP_ID.split('.').count(), 4);
        assert!(
            APP_ID
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.')
        );
    }
}
