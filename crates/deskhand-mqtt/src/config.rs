//! `[mqtt]` config section and connection setup.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

/// The `[mqtt]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct MqttConfig {
    pub host: String,
    /// Defaults to 1883, or 8883 with TLS.
    pub port: Option<u16>,
    pub username: Option<String>,
    /// Plain-text password. Prefer `password_file` or `password_command`.
    pub password: Option<String>,
    /// File whose first line is the password (`~/` is expanded).
    pub password_file: Option<PathBuf>,
    /// Command whose stdout is the password, as argv (no shell).
    pub password_command: Option<Vec<String>>,
    pub tls: bool,
    /// PEM CA bundle replacing the system roots.
    pub ca_file: Option<PathBuf>,
    /// PEM client certificate and key for mutual TLS.
    pub client_cert: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
    pub discovery_prefix: String,
    pub topic_prefix: String,
    #[serde(with = "humantime_serde")]
    pub keepalive: Duration,
    /// Defaults to `deskhand-<node_id>`.
    pub client_id: Option<String>,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            host: "homeassistant.local".to_owned(),
            port: None,
            username: None,
            password: None,
            password_file: None,
            password_command: None,
            tls: false,
            ca_file: None,
            client_cert: None,
            client_key: None,
            discovery_prefix: "homeassistant".to_owned(),
            topic_prefix: "deskhand".to_owned(),
            keepalive: Duration::from_secs(30),
            client_id: None,
        }
    }
}

/// Errors preparing the MQTT connection.
#[derive(Debug, thiserror::Error)]
pub enum MqttConfigError {
    #[error("set only one of password, password_file and password_command")]
    AmbiguousPassword,
    #[error("cannot read password_file {path}: {source}")]
    PasswordFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("password_command {command:?} failed: {reason}")]
    PasswordCommand {
        command: Vec<String>,
        reason: String,
    },
    #[error("TLS setup failed: {0}")]
    Tls(String),
    #[error("invalid {field}: {reason}")]
    Invalid { field: &'static str, reason: String },
}

impl MqttConfig {
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(if self.tls { 8883 } else { 1883 })
    }

    /// Checks prefixes for characters that would break topic filters.
    pub fn validate(&self) -> Result<(), MqttConfigError> {
        for (field, value) in [
            ("discovery_prefix", &self.discovery_prefix),
            ("topic_prefix", &self.topic_prefix),
        ] {
            if value.is_empty() || value.contains(['+', '#']) || value.ends_with('/') {
                return Err(MqttConfigError::Invalid {
                    field,
                    reason: format!(
                        "{value:?} must be non-empty, without + or #, and not end in /"
                    ),
                });
            }
        }
        if self.host.trim().is_empty() {
            return Err(MqttConfigError::Invalid {
                field: "host",
                reason: "must not be empty".into(),
            });
        }
        Ok(())
    }

    /// Resolves the password from whichever source is configured.
    pub fn resolve_password(&self) -> Result<Option<String>, MqttConfigError> {
        let sources = [
            self.password.is_some(),
            self.password_file.is_some(),
            self.password_command.is_some(),
        ];
        if sources.iter().filter(|s| **s).count() > 1 {
            return Err(MqttConfigError::AmbiguousPassword);
        }
        if let Some(p) = &self.password {
            return Ok(Some(p.clone()));
        }
        if let Some(path) = &self.password_file {
            let path = expand_home(path);
            let text = std::fs::read_to_string(&path)
                .map_err(|source| MqttConfigError::PasswordFile { path, source })?;
            return Ok(Some(text.lines().next().unwrap_or("").to_owned()));
        }
        if let Some(cmd) = &self.password_command {
            let err = |reason: String| MqttConfigError::PasswordCommand {
                command: cmd.clone(),
                reason,
            };
            let (program, args) = cmd
                .split_first()
                .ok_or_else(|| err("empty command".into()))?;
            let out = std::process::Command::new(program)
                .args(args)
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|e| err(e.to_string()))?;
            if !out.status.success() {
                return Err(err(format!("exited with {}", out.status)));
            }
            let text = String::from_utf8(out.stdout).map_err(|e| err(e.to_string()))?;
            return Ok(Some(text.lines().next().unwrap_or("").to_owned()));
        }
        Ok(None)
    }

    /// Builds the rustls client config (ring provider, system or custom roots).
    pub fn tls_config(&self) -> Result<Arc<rustls::ClientConfig>, MqttConfigError> {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};

        let tls_err = |e: &dyn std::fmt::Display| MqttConfigError::Tls(e.to_string());
        let mut roots = rustls::RootCertStore::empty();
        match &self.ca_file {
            Some(path) => {
                for cert in CertificateDer::pem_file_iter(expand_home(path))
                    .map_err(|e| tls_err(&format!("ca_file: {e}")))?
                {
                    let cert = cert.map_err(|e| tls_err(&format!("ca_file: {e}")))?;
                    roots.add(cert).map_err(|e| tls_err(&e))?;
                }
            }
            None => {
                let loaded = rustls_native_certs::load_native_certs();
                for e in &loaded.errors {
                    tracing::debug!(error = %e, "skipping unreadable system certificate");
                }
                let (added, _) = roots.add_parsable_certificates(loaded.certs);
                if added == 0 {
                    return Err(MqttConfigError::Tls(
                        "no system CA certificates found; set ca_file".into(),
                    ));
                }
            }
        }
        let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| tls_err(&e))?
        .with_root_certificates(roots);
        let config = match (&self.client_cert, &self.client_key) {
            (Some(cert), Some(key)) => {
                let certs = CertificateDer::pem_file_iter(expand_home(cert))
                    .map_err(|e| tls_err(&format!("client_cert: {e}")))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| tls_err(&format!("client_cert: {e}")))?;
                let key = PrivateKeyDer::from_pem_file(expand_home(key))
                    .map_err(|e| tls_err(&format!("client_key: {e}")))?;
                builder
                    .with_client_auth_cert(certs, key)
                    .map_err(|e| tls_err(&e))?
            }
            (None, None) => builder.with_no_client_auth(),
            _ => {
                return Err(MqttConfigError::Tls(
                    "client_cert and client_key must be set together".into(),
                ));
            }
        };
        Ok(Arc::new(config))
    }
}

/// Expands a leading `~/` to `$HOME`.
pub fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_ports() {
        let c = MqttConfig::default();
        assert_eq!(c.effective_port(), 1883);
        let tls = MqttConfig {
            tls: true,
            ..MqttConfig::default()
        };
        assert_eq!(tls.effective_port(), 8883);
        assert!(c.validate().is_ok());
        let bad = MqttConfig {
            topic_prefix: "desk/#".into(),
            ..MqttConfig::default()
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn password_sources() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pw");
        std::fs::write(&file, "s3cret\nignored\n").unwrap();

        let c = MqttConfig {
            password_file: Some(file.clone()),
            ..MqttConfig::default()
        };
        assert_eq!(c.resolve_password().unwrap().as_deref(), Some("s3cret"));

        let c = MqttConfig {
            password_command: Some(vec!["echo".into(), "from-cmd".into()]),
            ..MqttConfig::default()
        };
        assert_eq!(c.resolve_password().unwrap().as_deref(), Some("from-cmd"));

        let c = MqttConfig {
            password_command: Some(vec!["false".into()]),
            ..MqttConfig::default()
        };
        assert!(c.resolve_password().is_err());

        let c = MqttConfig {
            password: Some("a".into()),
            password_file: Some(file),
            ..MqttConfig::default()
        };
        assert!(matches!(
            c.resolve_password(),
            Err(MqttConfigError::AmbiguousPassword)
        ));
        assert_eq!(MqttConfig::default().resolve_password().unwrap(), None);
    }

    #[test]
    fn system_tls_config_builds() {
        // The Nix sandbox may have no CA bundle; only assert when one exists.
        match MqttConfig::default().tls_config() {
            Ok(_) => {}
            Err(MqttConfigError::Tls(msg)) => assert!(msg.contains("no system CA"), "{msg}"),
            Err(e) => panic!("{e}"),
        }
    }
}
