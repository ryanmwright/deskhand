//! `deskhand doctor [--json]`: checks the config, the broker connection and
//! which integrations can run here, without starting the daemon.
//!
//! Exit status: 0 when everything needed to run works, 1 otherwise.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use deskhand_core::{Enabled, Probe};
use deskhand_mqtt::AdminClient;
use deskhand_mqtt::topics::{OFFLINE, ONLINE};
use serde::Serialize;

use crate::wiring::Setup;

#[derive(Debug, Serialize)]
pub struct Report {
    pub ok: bool,
    pub version: &'static str,
    pub environment: Environment,
    pub config: ConfigReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mqtt: Option<MqttReport>,
    pub integrations: Vec<CapabilityReport>,
    /// State of the `deskhand.service` user unit (`active`, `inactive`,
    /// `failed`, `not installed`, ...); absent without systemd.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Environment {
    pub os: &'static str,
    pub desktop: Option<String>,
    pub session_type: Option<String>,
    pub features: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct ConfigReport {
    pub path: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DeviceReport {
    pub name: String,
    pub node_id: String,
}

#[derive(Debug, Serialize)]
pub struct MqttReport {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub username: Option<String>,
    pub connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `online` / `offline` from our retained status topic, or `none`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon: Option<String>,
    /// Entities in the retained discovery document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_entities: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct CapabilityReport {
    pub capability: &'static str,
    pub enabled: String,
    /// The provider that would run, if any.
    pub selected: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub candidates: Vec<CandidateReport>,
}

#[derive(Debug, Serialize)]
pub struct CandidateReport {
    pub id: &'static str,
    pub description: &'static str,
    /// `available`, `unavailable` or `not probed` (an earlier candidate won).
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn features() -> Vec<&'static str> {
    let mut f = Vec::new();
    for (on, name) in [
        (cfg!(feature = "linux"), "linux"),
        (cfg!(feature = "kde"), "kde"),
        (cfg!(feature = "tray"), "tray"),
        (cfg!(feature = "setup"), "setup"),
        (cfg!(feature = "audio-pulse"), "audio-pulse"),
    ] {
        if on {
            f.push(name);
        }
    }
    f
}

/// The user unit's active state, or `not installed`.
async fn service_state() -> Option<String> {
    let out = tokio::process::Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--property=LoadState,ActiveState",
            "--value",
            "deskhand.service",
        ])
        .stdin(std::process::Stdio::null())
        .output();
    let out = tokio::time::timeout(Duration::from_secs(3), out)
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let (load, active) = (lines.next()?, lines.next()?);
    Some(if load == "not-found" {
        "not installed".into()
    } else {
        active.to_owned()
    })
}

pub async fn report(config_path: &Path) -> Report {
    let environment = Environment {
        os: std::env::consts::OS,
        desktop: std::env::var("XDG_CURRENT_DESKTOP").ok(),
        session_type: std::env::var("XDG_SESSION_TYPE").ok(),
        features: features(),
    };
    let service = service_state().await;
    let setup = match Setup::load(config_path) {
        Ok(s) => s,
        Err(e) => {
            let mut error = format!("{e:#}");
            if !config_path.exists() {
                error += "\n    run `deskhand setup` to create it";
            }
            return Report {
                ok: false,
                version: deskhand_core::VERSION,
                environment,
                config: ConfigReport {
                    path: config_path.display().to_string(),
                    ok: false,
                    error: Some(error),
                    warnings: Vec::new(),
                },
                device: None,
                mqtt: None,
                integrations: Vec::new(),
                service,
            };
        }
    };
    let (mqtt, integrations) = tokio::join!(check_mqtt(&setup), check_integrations(&setup));
    Report {
        ok: mqtt.connected && integrations.iter().all(|c| c.error.is_none()),
        version: deskhand_core::VERSION,
        environment,
        config: ConfigReport {
            path: config_path.display().to_string(),
            ok: true,
            error: None,
            warnings: setup.warnings.clone(),
        },
        device: Some(DeviceReport {
            name: setup.device.name.clone(),
            node_id: setup.device.node_id.clone(),
        }),
        mqtt: Some(mqtt),
        integrations,
        service,
    }
}

async fn check_mqtt(setup: &Setup) -> MqttReport {
    let cfg = &setup.mqtt;
    let mut report = MqttReport {
        host: cfg.host.clone(),
        port: cfg.effective_port(),
        tls: cfg.tls,
        username: cfg.username.clone(),
        connected: false,
        error: None,
        daemon: None,
        published_entities: None,
    };
    let mut client =
        match AdminClient::connect(cfg, &setup.device.node_id, Duration::from_secs(10)).await {
            Ok(c) => c,
            Err(e) => {
                report.error = Some(e.to_string());
                return report;
            }
        };
    report.connected = true;
    let topics = client.topics().clone();
    let filters = [topics.status(), topics.device_config()];
    match client
        .read_retained(&filters, Duration::from_millis(300), Duration::from_secs(5))
        .await
    {
        Ok(retained) => {
            report.daemon = Some(
                match retained.get(&topics.status()).map(|p| &p[..]) {
                    Some(p) if p == ONLINE.as_bytes() => ONLINE,
                    Some(p) if p == OFFLINE.as_bytes() => OFFLINE,
                    Some(_) => "unknown",
                    None => "none",
                }
                .to_owned(),
            );
            report.published_entities = retained
                .get(&topics.device_config())
                .and_then(|p| serde_json::from_slice::<serde_json::Value>(p).ok())
                .and_then(|doc| {
                    doc.get("components")
                        .or_else(|| doc.get("cmps"))
                        .and_then(|c| c.as_object())
                        .map(|c| c.len())
                });
        }
        Err(e) => {
            report.error = Some(format!(
                "connected, but reading retained topics failed: {e}"
            ))
        }
    }
    client.disconnect().await;
    report
}

async fn check_integrations(setup: &Setup) -> Vec<CapabilityReport> {
    let mut groups: BTreeMap<&'static str, Vec<Box<dyn deskhand_core::Integration>>> =
        BTreeMap::new();
    let mut order = Vec::new();
    for c in setup.candidates() {
        if !groups.contains_key(c.capability()) {
            order.push(c.capability());
        }
        groups.entry(c.capability()).or_default().push(c);
    }
    let mut out = Vec::new();
    for capability in order {
        let candidates = &groups[capability];
        let (enabled, error) = match setup.config.integration(capability) {
            Ok(cfg) => (cfg.enabled, None),
            Err(e) => (Enabled::Auto, Some(e.to_string())),
        };
        let mut report = CapabilityReport {
            capability,
            enabled: enabled.to_string(),
            selected: None,
            error,
            candidates: Vec::new(),
        };
        for c in candidates {
            let (status, detail) = if enabled == Enabled::Off || report.selected.is_some() {
                ("not probed", None)
            } else {
                match tokio::time::timeout(Duration::from_secs(5), c.probe()).await {
                    Ok(Probe::Available { detail }) => {
                        report.selected = Some(c.id());
                        ("available", detail)
                    }
                    Ok(Probe::Unavailable { reason }) => ("unavailable", Some(reason)),
                    Err(_) => ("unavailable", Some("probe timed out".into())),
                }
            };
            report.candidates.push(CandidateReport {
                id: c.id(),
                description: c.description(),
                status,
                detail,
            });
        }
        if enabled == Enabled::On && report.selected.is_none() && report.error.is_none() {
            report.error = Some("enabled = true, but no provider is available here".into());
        }
        out.push(report);
    }
    out
}

/// Human-readable output.
pub fn print_text(r: &Report) {
    let mark = |ok: bool| if ok { "✓" } else { "✗" };
    println!("Deskhand {}", r.version);
    let env = &r.environment;
    println!(
        "  system:   {} / desktop {} / session {}",
        env.os,
        env.desktop.as_deref().unwrap_or("unknown"),
        env.session_type.as_deref().unwrap_or("unknown")
    );
    println!("  features: {}", env.features.join(", "));
    println!();
    println!("{} Config {}", mark(r.config.ok), r.config.path);
    if let Some(e) = &r.config.error {
        println!("    {e}");
    }
    for w in &r.config.warnings {
        println!("    warning: {w} (ignored)");
    }
    if let Some(d) = &r.device {
        println!("  Device \"{}\" (node id {})", d.name, d.node_id);
    }
    if let Some(state) = &r.service {
        let hint = match state.as_str() {
            "not installed" => " (run `deskhand setup`, or use the Home Manager module)",
            "failed" => " (see `journalctl --user -u deskhand.service`)",
            _ => "",
        };
        println!("  Service deskhand.service: {state}{hint}");
    }
    if let Some(m) = &r.mqtt {
        let user = m
            .username
            .as_deref()
            .map_or(String::new(), |u| format!(" as {u}"));
        let tls = if m.tls { " (TLS)" } else { "" };
        println!(
            "{} MQTT broker {}:{}{tls}{user}",
            mark(m.connected),
            m.host,
            m.port
        );
        if let Some(e) = &m.error {
            println!("    {e}");
        }
        match m.daemon.as_deref() {
            Some(ONLINE) => println!("    a Deskhand daemon for this device is online"),
            Some(OFFLINE) => println!("    the Deskhand daemon for this device is offline"),
            Some("none") => println!("    nothing published for this device yet"),
            _ => {}
        }
        if let Some(n) = m.published_entities {
            println!("    {n} entities in the published discovery document");
        }
    }
    if !r.integrations.is_empty() {
        println!();
        println!("Integrations:");
    }
    for c in &r.integrations {
        let state = match (c.enabled.as_str(), c.selected) {
            ("false", _) => "disabled".to_owned(),
            (_, Some(id)) => format!("runs {id}"),
            (_, None) => "unavailable".to_owned(),
        };
        let ok = c.error.is_none();
        println!("{} {:<16} {state}", mark(ok), c.capability);
        if let Some(e) = &c.error {
            println!("    {e}");
        }
        for cand in &c.candidates {
            if cand.status == "not probed" {
                continue;
            }
            if c.selected == Some(cand.id) && cand.detail.is_none() {
                continue;
            }
            println!(
                "    {} {}: {}",
                cand.id,
                cand.status,
                cand.detail.as_deref().unwrap_or("")
            );
        }
    }
}
