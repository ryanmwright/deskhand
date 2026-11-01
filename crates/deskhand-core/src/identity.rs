//! Device identity: which Home Assistant device this machine is.
//!
//! The node id must survive hostname changes and reinstalls of Deskhand, and
//! two users on one machine must not collide. It is derived from the platform
//! machine id and the user name, hashed so the machine id itself is not
//! published.

use sha2::{Digest, Sha256};

use crate::config::DeviceConfig;

/// Facts the platform layer provides about this machine and user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlatformIdentity {
    /// `/etc/machine-id` on Linux; IOPlatformUUID or MachineGuid elsewhere.
    pub machine_id: String,
    pub user: String,
    pub hostname: String,
    /// Human-friendly hostname (systemd-hostnamed `PrettyHostname`), if set.
    pub pretty_hostname: Option<String>,
    /// For example "KDE Plasma 6.5 on NixOS 26.05".
    pub model: String,
    /// For example the kernel version.
    pub hw_version: Option<String>,
    /// Primary physical NIC, formatted `aa:bb:cc:dd:ee:ff`.
    pub mac: Option<String>,
}

/// The device every entity belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Topic segment and HA identifier: `[a-z0-9_-]`, 1-32 characters.
    pub node_id: String,
    pub name: String,
    pub manufacturer: String,
    pub model: String,
    pub sw_version: String,
    pub hw_version: Option<String>,
    /// `(type, value)` pairs, for example `("mac", "aa:bb:...")`.
    pub connections: Vec<(String, String)>,
}

/// Error for a `[device] node_id` override that is not a valid topic segment.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid node_id {0:?}: must be 1-32 characters of [a-z0-9_-]")]
pub struct InvalidNodeId(pub String);

/// Derives the node id: the first 12 hex characters of
/// `sha256("deskhand\0" + machine_id + "\0" + user)`.
pub fn derive_node_id(machine_id: &str, user: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"deskhand\0");
    hasher.update(machine_id.trim().as_bytes());
    hasher.update(b"\0");
    hasher.update(user.as_bytes());
    hasher
        .finalize()
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Validates a user-supplied node id.
pub fn validate_node_id(id: &str) -> Result<(), InvalidNodeId> {
    let ok = !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-');
    if ok {
        Ok(())
    } else {
        Err(InvalidNodeId(id.to_owned()))
    }
}

impl DeviceInfo {
    /// Combines platform facts with the `[device]` config.
    pub fn resolve(
        platform: &PlatformIdentity,
        config: &DeviceConfig,
    ) -> Result<Self, InvalidNodeId> {
        let node_id = match &config.node_id {
            Some(id) => {
                validate_node_id(id)?;
                id.clone()
            }
            None => derive_node_id(&platform.machine_id, &platform.user),
        };
        let name = config
            .name
            .clone()
            .or_else(|| platform.pretty_hostname.clone())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| platform.hostname.clone());
        let connections = match (&platform.mac, config.publish_mac) {
            (Some(mac), true) => vec![("mac".to_owned(), mac.to_ascii_lowercase())],
            _ => Vec::new(),
        };
        Ok(Self {
            node_id,
            name,
            manufacturer: "Deskhand".to_owned(),
            model: platform.model.clone(),
            sw_version: crate::VERSION.to_owned(),
            hw_version: platform.hw_version.clone(),
            connections,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform() -> PlatformIdentity {
        PlatformIdentity {
            machine_id: "0123456789abcdef0123456789abcdef\n".into(),
            user: "ryan".into(),
            hostname: "desk".into(),
            pretty_hostname: Some("Ryan's Desk".into()),
            model: "KDE Plasma 6 on NixOS".into(),
            hw_version: None,
            mac: Some("AA:BB:CC:DD:EE:FF".into()),
        }
    }

    #[test]
    fn node_id_is_stable_short_hex_and_per_user() {
        let a = derive_node_id("0123456789abcdef0123456789abcdef", "ryan");
        assert_eq!(a.len(), 12);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            a,
            derive_node_id("0123456789abcdef0123456789abcdef\n", "ryan")
        );
        assert_ne!(
            a,
            derive_node_id("0123456789abcdef0123456789abcdef", "alex")
        );
        assert_ne!(
            a,
            derive_node_id("ffff456789abcdef0123456789abcdef", "ryan")
        );
    }

    #[test]
    fn resolve_prefers_config_then_pretty_hostname() {
        let p = platform();
        let d = DeviceInfo::resolve(&p, &DeviceConfig::default()).unwrap();
        assert_eq!(d.name, "Ryan's Desk");
        assert_eq!(d.node_id, derive_node_id(&p.machine_id, "ryan"));
        assert_eq!(
            d.connections,
            vec![("mac".into(), "aa:bb:cc:dd:ee:ff".into())]
        );

        let cfg = DeviceConfig {
            name: Some("Office".into()),
            node_id: Some("office-pc".into()),
            publish_mac: false,
        };
        let d = DeviceInfo::resolve(&p, &cfg).unwrap();
        assert_eq!(
            (d.name.as_str(), d.node_id.as_str()),
            ("Office", "office-pc")
        );
        assert!(d.connections.is_empty());

        let no_pretty = PlatformIdentity {
            pretty_hostname: Some(" ".into()),
            ..p
        };
        let d = DeviceInfo::resolve(&no_pretty, &DeviceConfig::default()).unwrap();
        assert_eq!(d.name, "desk");
    }

    #[test]
    fn node_id_override_is_validated() {
        let cfg = DeviceConfig {
            node_id: Some("Office PC".into()),
            ..DeviceConfig::default()
        };
        assert!(DeviceInfo::resolve(&platform(), &cfg).is_err());
        assert!(validate_node_id(&"a".repeat(33)).is_err());
        assert!(validate_node_id("pc-1_a").is_ok());
    }
}
