//! Platform-neutral core of Deskhand.
//!
//! This crate will hold the entity model, the hub actor, the `Integration` and
//! `Transport` traits, the supervisor, the config framework and device
//! identity. It must not depend on any platform crate, MQTT client or D-Bus
//! library. See `docs/v1-plan.md` §1.

/// Reverse-DNS application id, used as the D-Bus well-known name prefix,
/// the desktop file name and the portal app id. Defined once here.
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
