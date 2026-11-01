//! Topic layout.
//!
//! ```text
//! <prefix>/<node>/status                  global availability (LWT), retained
//! <prefix>/<node>/status/<capability>     per-integration availability, retained
//! <prefix>/<node>/<entity>/state          retained (except events)
//! <prefix>/<node>/<entity>/attributes     retained
//! <prefix>/<node>/<entity>/set            commands, never retained
//! <discovery>/device/deskhand_<node>/config
//! ```

use deskhand_core::EntityId;

pub const ONLINE: &str = "online";
pub const OFFLINE: &str = "offline";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topics {
    base: String,
    discovery_prefix: String,
    node_id: String,
}

impl Topics {
    pub fn new(topic_prefix: &str, discovery_prefix: &str, node_id: &str) -> Self {
        Self {
            base: format!("{topic_prefix}/{node_id}"),
            discovery_prefix: discovery_prefix.to_owned(),
            node_id: node_id.to_owned(),
        }
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// HA device identifier and discovery object id: `deskhand_<node>`.
    pub fn device_id(&self) -> String {
        format!("deskhand_{}", self.node_id)
    }

    pub fn status(&self) -> String {
        format!("{}/status", self.base)
    }

    pub fn capability_status(&self, capability: &str) -> String {
        format!("{}/status/{capability}", self.base)
    }

    /// The capability named by a capability status topic, if it is one of ours.
    pub fn capability_from_status(&self, topic: &str) -> Option<String> {
        topic
            .strip_prefix(&self.base)?
            .strip_prefix("/status/")
            .filter(|c| !c.is_empty() && !c.contains('/'))
            .map(str::to_owned)
    }

    pub fn state(&self, id: &EntityId) -> String {
        format!("{}/{id}/state", self.base)
    }

    pub fn attributes(&self, id: &EntityId) -> String {
        format!("{}/{id}/attributes", self.base)
    }

    pub fn command(&self, id: &EntityId) -> String {
        format!("{}/{id}/set", self.base)
    }

    /// Subscription filter for all command topics.
    pub fn command_filter(&self) -> String {
        format!("{}/+/set", self.base)
    }

    /// The entity id a command topic addresses.
    pub fn id_from_command(&self, topic: &str) -> Option<EntityId> {
        let rest = topic.strip_prefix(&self.base)?.strip_prefix('/')?;
        let id = rest.strip_suffix("/set")?;
        EntityId::new(id).ok()
    }

    pub fn device_config(&self) -> String {
        format!(
            "{}/device/{}/config",
            self.discovery_prefix,
            self.device_id()
        )
    }

    /// HA's birth/will topic.
    pub fn ha_status(&self) -> String {
        format!("{}/status", self.discovery_prefix)
    }

    /// Every retained topic of ours lives under this filter (for uninstall).
    pub fn base_filter(&self) -> String {
        format!("{}/#", self.base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let t = Topics::new("deskhand", "homeassistant", "abc123");
        let id = EntityId::from_static("cpu_percent");
        assert_eq!(t.status(), "deskhand/abc123/status");
        assert_eq!(t.capability_status("audio"), "deskhand/abc123/status/audio");
        assert_eq!(t.state(&id), "deskhand/abc123/cpu_percent/state");
        assert_eq!(t.command(&id), "deskhand/abc123/cpu_percent/set");
        assert_eq!(t.command_filter(), "deskhand/abc123/+/set");
        assert_eq!(
            t.device_config(),
            "homeassistant/device/deskhand_abc123/config"
        );
        assert_eq!(t.ha_status(), "homeassistant/status");
    }

    #[test]
    fn parsing() {
        let t = Topics::new("deskhand", "homeassistant", "abc123");
        assert_eq!(
            t.id_from_command("deskhand/abc123/media_volume/set"),
            Some(EntityId::from_static("media_volume"))
        );
        assert_eq!(t.id_from_command("deskhand/other/media_volume/set"), None);
        assert_eq!(t.id_from_command("deskhand/abc123/Bad-Id/set"), None);
        assert_eq!(
            t.capability_from_status("deskhand/abc123/status/audio")
                .as_deref(),
            Some("audio")
        );
        assert_eq!(t.capability_from_status("deskhand/abc123/status"), None);
        assert_eq!(t.capability_from_status("deskhand/abc123/status/a/b"), None);
    }
}
