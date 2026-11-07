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
    /// `[general] headless`.
    pub headless: bool,
    /// Whether systemd starts this user's manager at boot; absent when it
    /// can't be told.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linger: Option<bool>,
    /// Logitech HID++ nodes (receivers and their devices).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub logitech: Vec<LogitechNode>,
    /// The command that installs the udev rule, when a Logitech node can't
    /// be opened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub udev_hint: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct LogitechNode {
    pub node: String,
    pub product: String,
    pub driver: Option<String>,
    /// `ok`, `denied` or an error.
    pub access: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receiver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unique_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<LogitechDevice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LogitechDevice {
    pub slot: u8,
    pub name: Option<String>,
    pub wpid: String,
    pub kind: Option<&'static str>,
    pub online: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Environment {
    pub os: &'static str,
    pub desktop: Option<String>,
    pub session_type: Option<String>,
    /// A display server is reachable (`WAYLAND_DISPLAY` / `DISPLAY`).
    pub graphical: bool,
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
    /// Off only because of `[general] headless`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub headless_default: bool,
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
        (cfg!(feature = "hidpp"), "hidpp"),
        (cfg!(feature = "herdr"), "herdr"),
    ] {
        if on {
            f.push(name);
        }
    }
    f
}

/// Whether this process can reach a display server.
pub fn graphical_session() -> bool {
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    set("WAYLAND_DISPLAY")
        || set("DISPLAY")
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland" || t == "x11")
}

/// Whether `user` lingers (systemd starts their manager at boot); `None`
/// without systemd-logind's linger directory.
pub fn linger_enabled(user: &str) -> Option<bool> {
    let dir = Path::new("/var/lib/systemd/linger");
    (!user.is_empty() && dir.is_dir()).then(|| dir.join(user).exists())
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
        graphical: graphical_session(),
        features: features(),
    };
    let service = service_state().await;
    let linger = linger_enabled(&std::env::var("USER").unwrap_or_default());
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
                headless: false,
                linger,
                logitech: Vec::new(),
                udev_hint: None,
            };
        }
    };
    let (mqtt, integrations, logitech) = tokio::join!(
        check_mqtt(&setup),
        check_integrations(&setup),
        check_logitech()
    );
    let udev_hint = logitech_hint(&logitech);
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
        headless: setup.general.headless,
        linger,
        logitech,
        udev_hint,
    }
}

/// Logitech receivers, their devices and batteries.
#[cfg(feature = "hidpp")]
async fn check_logitech() -> Vec<LogitechNode> {
    use deskhand_hidpp::Access;
    let backend = deskhand_linux::hidraw::LinuxHidraw::new();
    let Ok(report) = deskhand_hidpp::inspect(&backend).await else {
        return Vec::new();
    };
    report
        .into_iter()
        .map(|n| LogitechNode {
            node: n.node.id.clone(),
            product: format!(
                "{:04x}:{:04x} {}",
                n.node.vendor, n.node.product, n.node.name
            ),
            driver: n.node.driver.clone(),
            access: match &n.access {
                Access::Ok => "ok".into(),
                Access::Denied => "denied".into(),
                Access::Error(e) => e.clone(),
            },
            receiver: n.receiver.as_ref().map(|r| r.name.to_owned()),
            unique_id: n.receiver.as_ref().and_then(|r| r.unique_id.clone()),
            firmware: n.receiver.as_ref().and_then(|r| r.firmware.clone()),
            devices: n
                .receiver
                .iter()
                .flat_map(|r| &r.devices)
                .map(|d| LogitechDevice {
                    slot: d.slot,
                    name: d.name.clone(),
                    wpid: format!("{:04X}", d.wpid),
                    kind: d.kind,
                    online: d.online,
                    battery: d.battery.clone(),
                })
                .collect(),
            error: n.error,
        })
        .collect()
}

#[cfg(not(feature = "hidpp"))]
async fn check_logitech() -> Vec<LogitechNode> {
    Vec::new()
}

/// The udev install command, only when a node can't be opened. Solaar's
/// rules grant the same access, so either one is fine.
fn logitech_hint(nodes: &[LogitechNode]) -> Option<&'static str> {
    #[cfg(feature = "hidpp")]
    if nodes.iter().any(|n| n.access == "denied") {
        return Some(deskhand_linux::hidraw::UDEV_INSTALL_HINT);
    }
    let _ = nodes;
    None
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
            headless_default: setup.config.headless_default_off(capability),
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
    if r.headless {
        let linger = match r.linger {
            Some(true) => "lingering on, starts at boot",
            Some(false) => {
                "lingering off: runs only while you are logged in \
                 (`loginctl enable-linger`, or NixOS `users.users.<name>.linger = true`)"
            }
            None => "lingering unknown",
        };
        println!("  Headless ({linger})");
    } else if r.config.ok && !r.environment.graphical {
        println!("  No graphical session here. On a server or VM, set `[general] headless = true`");
        println!("    (or run `deskhand setup --headless`) to start without a login.");
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
            ("false", _) if c.headless_default => "disabled (headless)".to_owned(),
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
    print_logitech(r);
}

fn print_logitech(r: &Report) {
    if r.logitech.is_empty() {
        return;
    }
    println!();
    println!("Logitech receivers:");
    for n in &r.logitech {
        let ok = n.access == "ok" && n.error.is_none();
        let what = n.receiver.as_deref().unwrap_or("not a receiver");
        println!(
            "{} {} {what} ({}, driver {})",
            if ok { "✓" } else { "✗" },
            n.node,
            n.product,
            n.driver.as_deref().unwrap_or("none"),
        );
        match n.access.as_str() {
            "ok" => {}
            "denied" => println!("    permission denied (udev rule missing)"),
            other => println!("    cannot open: {other}"),
        }
        if let Some(e) = &n.error {
            println!("    {e}");
        }
        if let Some(id) = &n.unique_id {
            let fw = n
                .firmware
                .as_deref()
                .map_or(String::new(), |f| format!(", firmware {f}"));
            println!("    unique id {id}{fw}");
        }
        for d in &n.devices {
            let kind = d.kind.map_or(String::new(), |k| format!(" {k}"));
            let state = if d.online { "connected" } else { "asleep" };
            let battery = d
                .battery
                .as_deref()
                .map_or(String::new(), |b| format!(", battery {b}"));
            println!(
                "    slot {}: {} ({}{kind}) {state}{battery}",
                d.slot,
                d.name.as_deref().unwrap_or("?"),
                d.wpid
            );
        }
    }
    if let Some(hint) = r.udev_hint {
        println!("    To allow access (one-time, as root):");
        println!("      {hint}");
    }
}
