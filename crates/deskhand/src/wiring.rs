//! What every subcommand needs: the parsed config, the device identity and
//! the compiled-in integrations.

use std::path::Path;

use anyhow::Context;
use deskhand_common::{CommandSensors, Scripts};
use deskhand_core::{Config, DeviceInfo, Integration};
use deskhand_mqtt::MqttConfig;

/// Top-level config sections owned by crates other than the core.
pub const KNOWN_SECTIONS: &[&str] = &["mqtt", "script", "sensor", "shortcut"];

/// Config plus everything derived from it.
#[derive(Debug)]
pub struct Setup {
    pub config: Config,
    pub general: deskhand_core::config::GeneralConfig,
    pub device: DeviceInfo,
    pub mqtt: MqttConfig,
    pub scripts: Scripts,
    pub sensors: CommandSensors,
    /// `[[shortcut]]` entries, registered by the desktop backend.
    pub shortcuts: Vec<deskhand_common::shortcuts::Shortcut>,
    /// Unknown keys and sections, as `config.key` paths.
    pub warnings: Vec<String>,
}

impl Setup {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let config = Config::load(path).with_context(|| {
            format!(
                "could not load the config file (create {} or pass --config)",
                path.display()
            )
        })?;
        let mut warnings = Vec::new();
        let mut unknown = |keys: Vec<String>| {
            warnings.extend(keys.into_iter().map(|k| format!("unknown key {k}")));
        };
        let general = config.general()?;
        unknown(general.unknown_keys);
        let device_cfg = config.device()?;
        unknown(device_cfg.unknown_keys);
        let mqtt = config.section::<MqttConfig>("mqtt")?;
        unknown(mqtt.unknown_keys);
        let scripts = Scripts::load(&config)?;
        unknown(scripts.unknown_keys);
        let sensors = CommandSensors::load(&config)?;
        unknown(sensors.unknown_keys);
        let shortcuts = deskhand_common::shortcuts::load(&config)?;
        unknown(shortcuts.unknown_keys);
        let shortcuts = deskhand_common::shortcuts::check(shortcuts.value)?;
        #[cfg(feature = "kde")]
        deskhand_kde::check_shortcut_keys(&shortcuts).map_err(anyhow::Error::msg)?;
        for section in config.unknown_sections(KNOWN_SECTIONS) {
            warnings.push(format!("unknown section [{section}]"));
        }
        let device = DeviceInfo::resolve(&platform_identity(), &device_cfg.value)?;
        Ok(Self {
            general: general.value,
            device,
            mqtt: mqtt.value,
            scripts: Scripts::new(scripts.value)?,
            sensors: CommandSensors::new(sensors.value)?,
            shortcuts,
            config,
            warnings,
        })
    }

    pub fn log_warnings(&self) {
        for w in &self.warnings {
            tracing::warn!("config: {w} (ignored)");
        }
    }

    /// Every integration compiled in, in priority order per capability.
    pub fn candidates(&self) -> Vec<Box<dyn Integration>> {
        #[allow(unused_mut)]
        let mut list: Vec<Box<dyn Integration>> = vec![
            Box::new(deskhand_common::info::DeskhandInfo::new()),
            Box::new(self.scripts.clone()),
            Box::new(self.sensors.clone()),
            Box::new(deskhand_common::Metrics::new()),
            Box::new(deskhand_common::SessionState::new()),
            Box::new(deskhand_common::InCall::new()),
        ];
        #[cfg(feature = "linux")]
        {
            let env = deskhand_linux::LinuxEnv::from_env();
            let units = self
                .config
                .integration("systemd")
                .ok()
                .and_then(|c| c.options::<deskhand_linux::systemd::SystemdOptions>().ok())
                .map(|p| p.value.units)
                .unwrap_or_default();
            // Desktop backends first: they win where they share a capability
            // with the generic Linux integrations (appearance).
            #[cfg(feature = "kde")]
            list.extend(deskhand_kde::candidates(&env, self.kde_settings()));
            list.extend(deskhand_linux::candidates(
                &env,
                self.general.idle_threshold,
                units,
            ));
        }
        list
    }

    /// Config the KDE integrations need beyond their own section. Option
    /// errors here are reported when the integration reads its section.
    #[cfg(feature = "kde")]
    fn kde_settings(&self) -> deskhand_kde::KdeSettings {
        let options = |capability: &str| self.config.integration(capability).ok();
        deskhand_kde::KdeSettings {
            shortcuts: self.shortcuts.clone(),
            invoke: options("shortcuts")
                .and_then(|c| {
                    c.options::<deskhand_kde::shortcuts::ShortcutsOptions>()
                        .ok()
                })
                .map(|p| p.value.invoke)
                .unwrap_or_default(),
            logout: options("power")
                .and_then(|c| c.options::<deskhand_linux::power::PowerOptions>().ok())
                .is_some_and(|p| p.value.buttons.iter().any(|b| b == "logout")),
            outputs: options("outputs").is_some_and(|c| c.enabled == deskhand_core::Enabled::On),
        }
    }
}

pub(crate) fn platform_identity() -> deskhand_core::PlatformIdentity {
    #[cfg(feature = "linux")]
    {
        deskhand_linux::platform_identity()
    }
    #[cfg(not(feature = "linux"))]
    {
        deskhand_core::PlatformIdentity {
            hostname: std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".into()),
            user: std::env::var("USER").unwrap_or_else(|_| "user".into()),
            model: std::env::consts::OS.into(),
            ..Default::default()
        }
    }
}
