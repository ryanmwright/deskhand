//! Configuration framework.
//!
//! The config file is TOML. The core only interprets `[device]`, `[general]`
//! and the `enabled` key of each `[integrations.<capability>]` table. Every
//! other section belongs to the crate that uses it (`[mqtt]` to the MQTT
//! transport, `[[script]]` to the scripts integration, ...), which reads it
//! with [`Config::section`]. Unknown keys are reported, not rejected, so an
//! older binary still starts with a newer config file.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

/// Errors loading or interpreting the config.
/// The underlying error is part of the message (not a `source()`), so
/// `{:#}` chains don't repeat it.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {path}: {error}")]
    Io {
        path: PathBuf,
        error: std::io::Error,
    },
    #[error("invalid TOML in {}: {error}", path.as_ref().map_or("config".into(), |p| p.display().to_string()))]
    Parse {
        path: Option<PathBuf>,
        error: toml::de::Error,
    },
    #[error("invalid [{section}] config: {error}")]
    Section {
        section: String,
        error: toml::de::Error,
    },
}

/// The `enabled` setting of an integration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Enabled {
    /// Start it if the platform supports it (the default).
    #[default]
    Auto,
    /// Start it, and complain loudly if the platform does not support it.
    On,
    /// Never start it.
    Off,
}

impl fmt::Display for Enabled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::On => "true",
            Self::Off => "false",
        })
    }
}

impl<'de> Deserialize<'de> for Enabled {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bool(bool),
            Str(String),
        }
        match Raw::deserialize(d)? {
            Raw::Bool(true) => Ok(Self::On),
            Raw::Bool(false) => Ok(Self::Off),
            Raw::Str(s) if s == "auto" => Ok(Self::Auto),
            Raw::Str(s) => Err(serde::de::Error::custom(format!(
                "expected true, false or \"auto\", got {s:?}"
            ))),
        }
    }
}

/// A deserialized section plus the dotted paths of keys nobody understood.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed<T> {
    pub value: T,
    pub unknown_keys: Vec<String>,
}

/// The whole config file.
#[derive(Debug, Clone, Default)]
pub struct Config {
    raw: toml::Table,
    source: Option<PathBuf>,
}

impl Config {
    /// Top-level sections the core interprets itself.
    pub const CORE_SECTIONS: &'static [&'static str] = &["device", "general", "integrations"];

    /// An empty config: every setting at its default.
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_toml_str(text: &str, source: Option<PathBuf>) -> Result<Self, ConfigError> {
        let raw = text
            .parse::<toml::Table>()
            .map_err(|source_err| ConfigError::Parse {
                path: source.clone(),
                error: source_err,
            })?;
        Ok(Self { raw, source })
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|error| ConfigError::Io {
            path: path.to_owned(),
            error,
        })?;
        Self::from_toml_str(&text, Some(path.to_owned()))
    }

    /// The file this config was loaded from, if any.
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// Deserializes a top-level section (a table or an array of tables).
    /// A missing section yields `T::default()`.
    pub fn section<T: DeserializeOwned + Default>(
        &self,
        key: &str,
    ) -> Result<Parsed<T>, ConfigError> {
        match self.raw.get(key) {
            None => Ok(Parsed {
                value: T::default(),
                unknown_keys: Vec::new(),
            }),
            Some(value) => deserialize_value(value.clone(), key),
        }
    }

    pub fn device(&self) -> Result<Parsed<DeviceConfig>, ConfigError> {
        self.section("device")
    }

    pub fn general(&self) -> Result<Parsed<GeneralConfig>, ConfigError> {
        self.section("general")
    }

    /// The `[integrations.<capability>]` table, split into `enabled` and the
    /// integration's own options.
    pub fn integration(&self, capability: &str) -> Result<IntegrationConfig, ConfigError> {
        let section = format!("integrations.{capability}");
        let mut options = self
            .raw
            .get("integrations")
            .and_then(|v| v.as_table())
            .and_then(|t| t.get(capability))
            .map(|v| {
                v.as_table().cloned().ok_or_else(|| ConfigError::Section {
                    section: section.clone(),
                    error: serde::de::Error::custom("expected a table"),
                })
            })
            .transpose()?
            .unwrap_or_default();
        let enabled = match options.remove("enabled") {
            None => Enabled::Auto,
            Some(v) => Enabled::deserialize(v).map_err(|error| ConfigError::Section {
                section: section.clone(),
                error,
            })?,
        };
        Ok(IntegrationConfig {
            capability: Arc::from(capability),
            enabled,
            options,
        })
    }

    /// Top-level sections that neither the core nor any name in `known` claims.
    pub fn unknown_sections(&self, known: &[&str]) -> Vec<String> {
        self.raw
            .keys()
            .filter(|k| !Self::CORE_SECTIONS.contains(&k.as_str()) && !known.contains(&k.as_str()))
            .cloned()
            .collect()
    }

    /// Capabilities that have an `[integrations.<name>]` table.
    pub fn configured_integrations(&self) -> Vec<String> {
        self.raw
            .get("integrations")
            .and_then(|v| v.as_table())
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default()
    }
}

fn deserialize_value<T: DeserializeOwned>(
    value: toml::Value,
    section: &str,
) -> Result<Parsed<T>, ConfigError> {
    let mut unknown_keys = Vec::new();
    let value = serde_ignored::deserialize(value, |path| {
        unknown_keys.push(format!("{section}.{path}"));
    })
    .map_err(|error| ConfigError::Section {
        section: section.to_owned(),
        error,
    })?;
    Ok(Parsed {
        value,
        unknown_keys,
    })
}

/// One integration's settings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IntegrationConfig {
    pub capability: Arc<str>,
    pub enabled: Enabled,
    options: toml::Table,
}

impl IntegrationConfig {
    /// Defaults for a capability with no config table.
    pub fn new(capability: &str) -> Self {
        Self {
            capability: Arc::from(capability),
            ..Self::default()
        }
    }

    /// Deserializes the integration's options (everything except `enabled`).
    pub fn options<T: DeserializeOwned + Default>(&self) -> Result<Parsed<T>, ConfigError> {
        if self.options.is_empty() {
            return Ok(Parsed {
                value: T::default(),
                unknown_keys: Vec::new(),
            });
        }
        deserialize_value(
            toml::Value::Table(self.options.clone()),
            &format!("integrations.{}", self.capability),
        )
    }
}

/// `[device]`: how this machine appears in Home Assistant.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct DeviceConfig {
    /// Display name. Defaults to the pretty hostname, then the hostname.
    pub name: Option<String>,
    /// Overrides the derived node id (topic segment and HA identifier).
    pub node_id: Option<String>,
    /// Publish the primary MAC address so HA can wake the machine (Wake-on-LAN).
    pub publish_mac: bool,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            name: None,
            node_id: None,
            publish_mac: true,
        }
    }
}

/// `[general]`: daemon-wide settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub tray: bool,
    /// How long without input before the user counts as idle.
    #[serde(with = "humantime_serde")]
    pub idle_threshold: Duration,
    /// Default log filter (overridden by `RUST_LOG`).
    pub log: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            tray: true,
            idle_threshold: Duration::from_secs(300),
            log: "info".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [device]
        name = "Office PC"

        [general]
        idle_threshold = "2m 30s"
        colour = "blue"

        [mqtt]
        host = "broker"

        [integrations.lock]
        allow_remote_unlock = true

        [integrations.camera]
        enabled = false

        [integrations.audio]
        enabled = true
        mic_ignore_apps = ["pavucontrol"]
        typo_key = 1

        [integrations.metrics]
        enabled = "auto"

        [[script]]
        id = "backup"

        [mystery]
        x = 1
    "#;

    fn sample() -> Config {
        Config::from_toml_str(SAMPLE, None).unwrap()
    }

    #[test]
    fn enabled_values() {
        let c = sample();
        assert_eq!(c.integration("camera").unwrap().enabled, Enabled::Off);
        assert_eq!(c.integration("audio").unwrap().enabled, Enabled::On);
        assert_eq!(c.integration("metrics").unwrap().enabled, Enabled::Auto);
        assert_eq!(c.integration("lock").unwrap().enabled, Enabled::Auto);
        assert_eq!(c.integration("absent").unwrap().enabled, Enabled::Auto);

        let bad = Config::from_toml_str("[integrations.x]\nenabled = \"yes\"", None).unwrap();
        let err = bad.integration("x").unwrap_err().to_string();
        assert!(err.contains("integrations.x"), "{err}");
    }

    #[derive(Debug, Default, Deserialize, PartialEq)]
    #[serde(default)]
    struct AudioOptions {
        mic_ignore_apps: Vec<String>,
    }

    #[test]
    fn integration_options_and_unknown_keys() {
        let c = sample();
        let audio = c.integration("audio").unwrap();
        let parsed = audio.options::<AudioOptions>().unwrap();
        assert_eq!(parsed.value.mic_ignore_apps, vec!["pavucontrol"]);
        assert_eq!(parsed.unknown_keys, vec!["integrations.audio.typo_key"]);

        // `enabled` is consumed by the core and never reported as unknown.
        let cam = c.integration("camera").unwrap();
        assert!(
            cam.options::<AudioOptions>()
                .unwrap()
                .unknown_keys
                .is_empty()
        );
    }

    #[test]
    fn core_sections() {
        let c = sample();
        let device = c.device().unwrap().value;
        assert_eq!(device.name.as_deref(), Some("Office PC"));
        assert!(device.publish_mac, "defaults fill in missing keys");

        let general = c.general().unwrap();
        assert_eq!(general.value.idle_threshold, Duration::from_secs(150));
        assert!(general.value.tray);
        assert_eq!(general.unknown_keys, vec!["general.colour"]);

        let empty = Config::empty();
        assert_eq!(empty.general().unwrap().value, GeneralConfig::default());
    }

    #[test]
    fn unknown_sections_and_arrays() {
        let c = sample();
        assert_eq!(c.unknown_sections(&["mqtt", "script"]), vec!["mystery"]);

        #[derive(Debug, Default, Deserialize)]
        struct Script {
            id: String,
        }
        let scripts = c.section::<Vec<Script>>("script").unwrap().value;
        assert_eq!(scripts[0].id, "backup");
        assert!(c.section::<Vec<Script>>("sensor").unwrap().value.is_empty());
    }

    #[test]
    fn parse_errors_name_the_file() {
        let err = Config::from_toml_str("[broken", Some("/tmp/x.toml".into())).unwrap_err();
        assert!(err.to_string().contains("/tmp/x.toml"), "{err}");
    }
}
