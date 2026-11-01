//! `[[shortcut]]` entries: custom global shortcuts. Each becomes a
//! `shortcut_<id>` event entity that fires `pressed`.
//!
//! This module only reads and checks the config, so entity ids are the same
//! on every desktop. The desktop backend registers the shortcuts (KDE:
//! kglobalaccel) and parses `default_keys` (`"Meta+Shift+F12"`, chords
//! separated by `", "`). The user can rebind them in the desktop's settings.

use std::collections::BTreeSet;

use deskhand_core::config::Parsed;
use deskhand_core::{Config, ConfigError, EntityId};
use serde::Deserialize;

use crate::scripts::{EntryError, entry_id};

/// One `[[shortcut]]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ShortcutConfig {
    /// Entity id suffix: the event is `shortcut_<id>`.
    pub id: String,
    pub name: Option<String>,
    /// Keys bound until the user changes them. None: unbound until the
    /// user binds it in the desktop's settings.
    #[serde(default)]
    pub default_keys: KeyList,
}

/// One key sequence or several alternatives.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum KeyList {
    #[default]
    None,
    One(String),
    Many(Vec<String>),
}

impl KeyList {
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            Self::None => Vec::new(),
            Self::One(s) => vec![s.clone()],
            Self::Many(v) => v.clone(),
        }
    }
}

/// A checked `[[shortcut]]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortcut {
    /// The config id, also the backend's action name.
    pub id: String,
    pub entity: EntityId,
    pub name: String,
    pub default_keys: Vec<String>,
}

/// Reads the `[[shortcut]]` array (unknown keys are reported, not rejected).
pub fn load(config: &Config) -> Result<Parsed<Vec<ShortcutConfig>>, ConfigError> {
    config.section("shortcut")
}

/// Checks ids (valid, unique) and fills in default names.
pub fn check(configs: Vec<ShortcutConfig>) -> Result<Vec<Shortcut>, EntryError> {
    let mut seen = BTreeSet::new();
    configs
        .into_iter()
        .map(|c| {
            let entity = entry_id("shortcut", "shortcut_", &c.id, &mut seen)?;
            Ok(Shortcut {
                name: c.name.unwrap_or_else(|| c.id.clone()),
                default_keys: c.default_keys.to_vec(),
                id: c.id,
                entity,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Result<Vec<Shortcut>, String> {
        let config = Config::from_toml_str(toml, None).map_err(|e| e.to_string())?;
        let parsed = load(&config).map_err(|e| e.to_string())?;
        check(parsed.value).map_err(|e| e.to_string())
    }

    #[test]
    fn reads_shortcuts() {
        let s = parse(
            r#"
            [[shortcut]]
            id = "lights"
            name = "Toggle lights"
            default_keys = "Meta+Shift+L"
            [[shortcut]]
            id = "scene"
            default_keys = ["Meta+F9", "Ctrl+Alt+S"]
            [[shortcut]]
            id = "unbound"
            "#,
        )
        .unwrap();
        assert_eq!(s[0].entity.as_str(), "shortcut_lights");
        assert_eq!(s[0].name, "Toggle lights");
        assert_eq!(s[0].default_keys, vec!["Meta+Shift+L"]);
        assert_eq!(s[1].name, "scene");
        assert_eq!(s[1].default_keys, vec!["Meta+F9", "Ctrl+Alt+S"]);
        assert!(s[2].default_keys.is_empty());
    }

    #[test]
    fn rejects_bad_and_duplicate_ids() {
        assert!(parse("[[shortcut]]\nid='Bad Id'").is_err());
        assert!(parse("[[shortcut]]\nid='a'\n[[shortcut]]\nid='a'").is_err());
    }
}
