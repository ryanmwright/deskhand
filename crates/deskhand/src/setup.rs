//! `deskhand setup`: finds the broker, asks for credentials, tests the
//! connection, writes the config (0600, atomically) and installs the
//! systemd user service.
//!
//! Interactive by default. `--yes` runs without prompts from the flags
//! (`--host` required), for scripts and tests.

use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use deskhand_mqtt::{AdminClient, MqttConfig};

/// `deskhand setup` flags.
#[derive(Debug, Default, clap::Args)]
pub struct Options {
    /// Run without prompts, from these flags (needs --host).
    #[arg(long, short)]
    pub yes: bool,
    /// Broker host name or address.
    #[arg(long)]
    pub host: Option<String>,
    /// Broker port [default: 1883, or 8883 with --tls].
    #[arg(long)]
    pub port: Option<u16>,
    /// Broker username.
    #[arg(long)]
    pub username: Option<String>,
    /// Read the broker password from stdin (first line).
    #[arg(long)]
    pub password_stdin: bool,
    /// Connect with TLS.
    #[arg(long)]
    pub tls: bool,
    /// PEM CA bundle for TLS (default: the system roots).
    #[arg(long)]
    pub ca_file: Option<PathBuf>,
    /// Device name in Home Assistant [default: the host name].
    #[arg(long)]
    pub name: Option<String>,
    /// Don't install the systemd user service.
    #[arg(long)]
    pub no_service: bool,
    /// Don't test the broker connection.
    #[arg(long)]
    pub skip_test: bool,
    /// Overwrite an existing config (a copy is kept as config.toml.bak).
    #[arg(long)]
    pub force: bool,
    /// Power buttons Home Assistant may press, comma-separated: suspend,
    /// hibernate, reboot, poweroff, logout [default: none].
    #[arg(long, value_delimiter = ',')]
    pub power_buttons: Vec<String>,
    /// A machine without a desktop session (a server or VM): start at boot
    /// without a login, session and desktop integrations off by default.
    #[arg(long)]
    pub headless: bool,
}

/// Power buttons setup offers: config name and question label.
pub const POWER_BUTTONS: &[(&str, &str)] = &[
    ("suspend", "Suspend"),
    ("hibernate", "Hibernate"),
    ("reboot", "Restart"),
    ("poweroff", "Shut down"),
    ("logout", "Log out (KDE; ends the session without asking)"),
];

/// What the wizard collected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answers {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub ca_file: Option<PathBuf>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub name: Option<String>,
    /// `[integrations.power] buttons`; empty means none.
    pub power_buttons: Vec<String>,
    /// `[general] headless`.
    pub headless: bool,
}

impl Answers {
    fn mqtt_config(&self, password_file: Option<&Path>) -> MqttConfig {
        MqttConfig {
            host: self.host.clone(),
            port: Some(self.port),
            username: self.username.clone(),
            password_file: password_file.map(Path::to_owned),
            tls: self.tls,
            ca_file: self.ca_file.clone(),
            ..MqttConfig::default()
        }
    }
}

fn quoted(s: &str) -> String {
    toml::Value::String(s.to_owned()).to_string()
}

/// The config file text. The password is not in it: it goes to
/// `password_file`.
pub fn render_config(a: &Answers, password_file: Option<&Path>) -> String {
    let mut out = String::from(
        "# Deskhand config, written by `deskhand setup`.\n\
         # Integrations run automatically where supported; see `deskhand doctor`.\n\n",
    );
    if let Some(name) = &a.name {
        out += &format!("[device]\nname = {}\n\n", quoted(name));
    }
    if a.headless {
        out += "# No desktop session: session and desktop integrations are off unless\n\
                # enabled in their [integrations.<name>] table.\n\
                [general]\nheadless = true\n\n";
    }
    out += "[mqtt]\n";
    out += &format!("host = {}\n", quoted(&a.host));
    out += &format!("port = {}\n", a.port);
    if let Some(u) = &a.username {
        out += &format!("username = {}\n", quoted(u));
    }
    if let Some(p) = password_file {
        out += &format!("password_file = {}\n", quoted(&p.display().to_string()));
    }
    if a.tls {
        out += "tls = true\n";
    }
    if let Some(ca) = &a.ca_file {
        out += &format!("ca_file = {}\n", quoted(&ca.display().to_string()));
    }
    if !a.power_buttons.is_empty() {
        let list: Vec<String> = a.power_buttons.iter().map(|b| quoted(b)).collect();
        out += &format!(
            "\n# Power buttons Home Assistant may press (none by default).\n\
             [integrations.power]\n{}buttons = [{}]\n",
            if a.headless { "enabled = true\n" } else { "" },
            list.join(", ")
        );
    }
    out
}

/// Writes `contents` to `path` with mode 0600: a temporary file in the same
/// directory, then a rename, so a crash never leaves half a file. The
/// directory is created (0700) if needed.
pub fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !dir.exists() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("deskhand"),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("cannot create {}", tmp.display()))?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

/// Writes the password file (if any) and the config; keeps a backup of an
/// existing config. Returns the backup path.
fn save(config_path: &Path, a: &Answers) -> anyhow::Result<Option<PathBuf>> {
    let password_file = a.password.as_ref().map(|_| {
        config_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("password")
    });
    if let (Some(pf), Some(pw)) = (&password_file, &a.password) {
        write_private(pf, &format!("{pw}\n"))?;
    }
    let backup = if config_path.exists() {
        let b = config_path.with_extension("toml.bak");
        std::fs::copy(config_path, &b)
            .with_context(|| format!("cannot back up {}", config_path.display()))?;
        std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o600))?;
        Some(b)
    } else {
        None
    };
    write_private(config_path, &render_config(a, password_file.as_deref()))?;
    Ok(backup)
}

/// Connects and disconnects; a readable error on failure.
async fn test_connection(a: &Answers, node_id: &str) -> Result<(), String> {
    // The password is not on disk yet: test with it inline.
    let mut cfg = a.mqtt_config(None);
    cfg.password = a.password.clone();
    let client = AdminClient::connect(&cfg, node_id, Duration::from_secs(10))
        .await
        .map_err(|e| e.to_string())?;
    client.disconnect().await;
    Ok(())
}

// ------------------------------------------------------------------ discovery

/// A broker the wizard found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub host: String,
    pub port: u16,
    /// How it was found.
    pub source: &'static str,
    pub reachable: bool,
}

impl std::fmt::Display for Candidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}  ({}", self.host, self.port, self.source)?;
        if !self.reachable {
            f.write_str(", not reachable")?;
        }
        f.write_str(")")
    }
}

/// Candidates in the plan's order (mDNS `_mqtt._tcp`, then Home Assistant
/// hosts on 1883, then `homeassistant.local`, then localhost), without
/// duplicates, reachable ones first (stable otherwise).
pub fn order_candidates(
    mqtt: Vec<(String, u16)>,
    home_assistant: Vec<String>,
    reachable: impl Fn(&str, u16) -> bool,
) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut push = |host: String, port: u16, source: &'static str| {
        if !out.iter().any(|c| c.host == host && c.port == port) {
            out.push(Candidate {
                reachable: reachable(&host, port),
                host,
                port,
                source,
            });
        }
    };
    for (h, p) in mqtt {
        push(h, p, "found on the network (_mqtt._tcp)");
    }
    for h in home_assistant {
        push(h, 1883, "Home Assistant found on the network");
    }
    push(
        "homeassistant.local".into(),
        1883,
        "Home Assistant's default name",
    );
    push("localhost".into(), 1883, "this computer");
    out.sort_by_key(|c| !c.reachable);
    out
}

/// `host.local.` → `host.local`; IPv4 first, for hosts that don't resolve.
fn mdns_host(host: &str, addrs: &[IpAddr]) -> (String, Option<IpAddr>) {
    let name = host.trim_end_matches('.').to_owned();
    let ip = addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.first())
        .copied();
    (name, ip)
}

/// mDNS results: `_mqtt._tcp` brokers as (host name, an address, port) and
/// `_home-assistant._tcp` hosts as (host name, an address).
type MdnsFound = (
    Vec<(String, Option<IpAddr>, u16)>,
    Vec<(String, Option<IpAddr>)>,
);

/// Browses mDNS for `wait`.
fn browse_mdns(wait: Duration) -> MdnsFound {
    let mut mqtt = Vec::new();
    let mut ha = Vec::new();
    let Ok(daemon) = mdns_sd::ServiceDaemon::new() else {
        return (mqtt, ha);
    };
    let browse = |ty: &str| daemon.browse(ty).ok();
    let (rx_mqtt, rx_ha) = (
        browse("_mqtt._tcp.local."),
        browse("_home-assistant._tcp.local."),
    );
    let deadline = std::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let step = left.min(Duration::from_millis(100));
        for (rx, is_mqtt) in [(&rx_mqtt, true), (&rx_ha, false)] {
            let Some(rx) = rx else { continue };
            while let Ok(ev) = rx.recv_timeout(step / 2) {
                if let mdns_sd::ServiceEvent::ServiceResolved(info) = ev {
                    let addrs: Vec<IpAddr> =
                        info.addresses.iter().map(|a| a.to_ip_addr()).collect();
                    let (host, ip) = mdns_host(&info.host, &addrs);
                    if is_mqtt {
                        mqtt.push((host, ip, info.port));
                    } else {
                        ha.push((host, ip));
                    }
                }
            }
        }
    }
    let _ = daemon.shutdown();
    (mqtt, ha)
}

async fn reachable(host: &str, port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(1),
            tokio::net::TcpStream::connect((host, port))
        )
        .await,
        Ok(Ok(_))
    )
}

/// Finds brokers: mDNS for 3 s, then the fallbacks; checks which accept TCP.
async fn discover() -> Vec<Candidate> {
    let (mqtt, ha) = tokio::task::spawn_blocking(|| browse_mdns(Duration::from_secs(3)))
        .await
        .unwrap_or_default();
    // Prefer the mDNS host name when it resolves here, else its address.
    let mut pick = Vec::new();
    for (host, ip, port) in mqtt {
        let h = match ip {
            Some(ip) if !reachable(&host, port).await => ip.to_string(),
            _ => host,
        };
        pick.push((h, port));
    }
    let mut ha_hosts = Vec::new();
    for (host, ip) in ha {
        let h = match ip {
            Some(ip) if !reachable(&host, 1883).await => ip.to_string(),
            _ => host,
        };
        ha_hosts.push(h);
    }
    let mut all: Vec<(String, u16)> = pick.clone();
    all.extend(ha_hosts.iter().map(|h| (h.clone(), 1883)));
    all.push(("homeassistant.local".into(), 1883));
    all.push(("localhost".into(), 1883));
    let mut ok = Vec::new();
    for (h, p) in &all {
        if reachable(h, *p).await {
            ok.push((h.clone(), *p));
        }
    }
    order_candidates(pick, ha_hosts, |h, p| {
        ok.iter().any(|(oh, op)| oh == h && *op == p)
    })
}

// -------------------------------------------------------------------- service

pub const UNIT_NAME: &str = "deskhand.service";

/// The systemd user unit for `exe` (and a non-default config path). It
/// follows the graphical session, or with `headless` the user manager
/// (which lingering starts at boot).
pub fn render_unit(exe: &Path, config: Option<&Path>, headless: bool) -> String {
    let config_arg = config.map_or(String::new(), |c| format!(" --config {}", c.display()));
    let (unit, target) = if headless {
        ("", "default.target")
    } else {
        (
            "PartOf=graphical-session.target\nAfter=graphical-session.target\n",
            "graphical-session.target",
        )
    };
    format!(
        "[Unit]\n\
         Description=Deskhand: this desktop in Home Assistant\n\
         {unit}\
         \n\
         [Service]\n\
         Type=notify\n\
         ExecStart={}{config_arg} run\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy={target}\n",
        exe.display()
    )
}

fn user_unit_dir() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    config.join("systemd/user")
}

/// Profile links that point into the Nix store and survive upgrades.
fn nix_profile_links() -> Vec<PathBuf> {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    [
        format!("{}/.nix-profile/bin/deskhand", var("HOME")),
        format!("{}/.local/state/nix/profile/bin/deskhand", var("HOME")),
        format!("/etc/profiles/per-user/{}/bin/deskhand", var("USER")),
        "/run/current-system/sw/bin/deskhand".to_owned(),
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

/// The path to put in the unit: for a binary in the Nix store, a profile
/// link that resolves to it (`nix profile install`), so upgrades don't
/// break the unit; otherwise the binary itself.
fn stable_exe(exe: &Path, profile_links: &[PathBuf]) -> PathBuf {
    if !exe.starts_with("/nix/store") {
        return exe.to_owned();
    }
    profile_links
        .iter()
        .find(|l| l.canonicalize().is_ok_and(|t| t == exe))
        .cloned()
        .unwrap_or_else(|| exe.to_owned())
}

/// Why the unit should not be installed by us, if so.
fn service_blocker(exe: &Path, unit_path: &Path) -> Option<String> {
    if exe.starts_with("/nix/store") {
        return Some(
            "this deskhand is only in the Nix store (not in a profile), so the service would \
             break on the next update. Install it with `nix profile install`, or use the Home \
             Manager module (`services.deskhand.enable = true;`)"
                .into(),
        );
    }
    if std::fs::read_link(unit_path).is_ok_and(|t| t.starts_with("/nix/store")) {
        return Some(format!(
            "{} is managed by Home Manager; not replacing it",
            unit_path.display()
        ));
    }
    None
}

fn systemctl(args: &[&str]) -> anyhow::Result<()> {
    let out = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("cannot run systemctl")?;
    if !out.status.success() {
        bail!(
            "systemctl --user {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Writes, enables and (re)starts the user unit. `reenable` moves the
/// install symlink when the target changed (desktop ↔ headless).
fn install_service(exe: &Path, config: Option<&Path>, headless: bool) -> anyhow::Result<PathBuf> {
    let unit_path = user_unit_dir().join(UNIT_NAME);
    std::fs::create_dir_all(unit_path.parent().expect("unit dir"))?;
    std::fs::write(&unit_path, render_unit(exe, config, headless))
        .with_context(|| format!("cannot write {}", unit_path.display()))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["reenable", UNIT_NAME])?;
    systemctl(&["restart", UNIT_NAME])?;
    Ok(unit_path)
}

// ------------------------------------------------------------------------ run

fn device_default_name() -> String {
    deskhand_core::DeviceInfo::resolve(
        &crate::wiring::platform_identity(),
        &deskhand_core::config::DeviceConfig::default(),
    )
    .map(|d| d.name)
    .unwrap_or_else(|_| "Deskhand".into())
}

fn node_id() -> String {
    deskhand_core::DeviceInfo::resolve(
        &crate::wiring::platform_identity(),
        &deskhand_core::config::DeviceConfig::default(),
    )
    .map(|d| d.node_id)
    .unwrap_or_else(|_| "setup".into())
}

fn answers_from_flags(o: &Options) -> anyhow::Result<Answers> {
    let host = o
        .host
        .clone()
        .context("--yes needs --host (the broker's host name or address)")?;
    let password = if o.password_stdin {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Some(line.trim_end_matches(['\r', '\n']).to_owned()).filter(|p| !p.is_empty())
    } else {
        None
    };
    for b in &o.power_buttons {
        if !POWER_BUTTONS.iter().any(|(name, _)| name == b) {
            bail!(
                "--power-buttons: unknown button {b:?} (suspend, hibernate, reboot, poweroff, logout)"
            );
        }
    }
    Ok(Answers {
        host,
        port: o.port.unwrap_or(if o.tls { 8883 } else { 1883 }),
        tls: o.tls,
        ca_file: o.ca_file.clone(),
        username: o.username.clone(),
        password,
        name: o.name.clone(),
        power_buttons: o.power_buttons.clone(),
        headless: o.headless,
    })
}

/// Prompts for everything. `defaults` pre-fills the answers (after a failed
/// connection test); `headless` is `--headless` (not asked again).
async fn ask(defaults: Option<&Answers>, headless: bool) -> anyhow::Result<Answers> {
    use inquire::{Confirm, MultiSelect, Password, PasswordDisplayMode, Select, Text};

    let (host, port) = match defaults {
        Some(d) => {
            let host = Text::new("Broker host:").with_default(&d.host).prompt()?;
            let port = inquire::CustomType::<u16>::new("Port:")
                .with_default(d.port)
                .prompt()?;
            (host, port)
        }
        None => {
            println!("Looking for MQTT brokers on the network…");
            let found = discover().await;
            let mut options: Vec<String> = found.iter().map(ToString::to_string).collect();
            options.push("Enter a host manually".into());
            let choice = Select::new("Which broker should Deskhand use?", options)
                .raw_prompt()?
                .index;
            match found.get(choice) {
                Some(c) => (c.host.clone(), c.port),
                None => {
                    let host = Text::new("Broker host:")
                        .with_help_message("Host name or address, e.g. homeassistant.local")
                        .prompt()?;
                    let port = inquire::CustomType::<u16>::new("Port:")
                        .with_default(1883)
                        .prompt()?;
                    (host, port)
                }
            }
        }
    };
    let tls = Confirm::new("Use TLS?")
        .with_default(defaults.map_or(port == 8883, |d| d.tls))
        .prompt()?;
    let ca_file = if tls {
        let ca = Text::new("CA file (PEM):")
            .with_help_message("Leave empty to use the system's certificate store")
            .with_default(
                &defaults
                    .and_then(|d| d.ca_file.as_ref())
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            )
            .prompt()?;
        Some(PathBuf::from(ca)).filter(|p| !p.as_os_str().is_empty())
    } else {
        None
    };
    let username = Text::new("Username:")
        .with_help_message(
            "With Home Assistant's Mosquitto add-on: a Home Assistant user. Empty: anonymous",
        )
        .with_default(
            defaults
                .and_then(|d| d.username.as_deref())
                .unwrap_or_default(),
        )
        .prompt()?;
    let username = Some(username).filter(|u| !u.is_empty());
    let password = match &username {
        Some(_) => {
            let p = Password::new("Password:")
                .without_confirmation()
                .with_display_mode(PasswordDisplayMode::Masked)
                .with_help_message("Stored in a separate file readable only by you")
                .prompt()?;
            Some(p).filter(|p| !p.is_empty())
        }
        None => None,
    };
    let default_name = defaults
        .and_then(|d| d.name.clone())
        .unwrap_or_else(device_default_name);
    let name = Text::new("Device name in Home Assistant:")
        .with_default(&default_name)
        .prompt()?;
    let headless = headless
        || Confirm::new("Is this a headless machine (a server or VM nobody sits at)?")
            .with_default(defaults.map_or(!crate::doctor::graphical_session(), |d| d.headless))
            .with_help_message(
                "Starts at boot without a login; session and desktop integrations are off",
            )
            .prompt()?;
    let labels: Vec<&str> = POWER_BUTTONS.iter().map(|(_, l)| *l).collect();
    let preselected: Vec<usize> = POWER_BUTTONS
        .iter()
        .enumerate()
        .filter(|(_, (b, _))| defaults.is_some_and(|d| d.power_buttons.iter().any(|x| x == b)))
        .map(|(i, _)| i)
        .collect();
    let chosen = MultiSelect::new(
        "Which power buttons may Home Assistant press? (none is fine)",
        labels,
    )
    .with_default(&preselected)
    .with_help_message("Space selects, Enter confirms. Lock screen is always available")
    .raw_prompt()?;
    let power_buttons = chosen
        .iter()
        .map(|o| POWER_BUTTONS[o.index].0.to_owned())
        .collect();
    Ok(Answers {
        host,
        port,
        tls,
        ca_file,
        username,
        password,
        name: Some(name).filter(|n| !n.is_empty() && *n != device_default_name()),
        power_buttons,
        headless,
    })
}

pub async fn setup(config_path: &Path, explicit_config: bool, o: Options) -> anyhow::Result<()> {
    let interactive = !o.yes;
    if !interactive && o.host.is_none() {
        bail!("--yes needs --host (the broker's host name or address)");
    }
    if config_path.exists() && !o.force {
        if !interactive {
            bail!(
                "{} exists; pass --force to replace it (a copy is kept as .bak)",
                config_path.display()
            );
        }
        let replace = inquire::Confirm::new(&format!(
            "{} exists. Replace it? (a copy is kept as config.toml.bak)",
            config_path.display()
        ))
        .with_default(false)
        .prompt()?;
        if !replace {
            println!("Nothing changed.");
            return Ok(());
        }
    }

    let mut answers = if interactive {
        ask(None, o.headless).await?
    } else {
        answers_from_flags(&o)?
    };
    if !o.skip_test {
        let node = node_id();
        loop {
            println!("Connecting to {}:{}…", answers.host, answers.port);
            match test_connection(&answers, &node).await {
                Ok(()) => {
                    println!("✓ Connected to the broker");
                    break;
                }
                Err(e) if !interactive => bail!("cannot connect to the broker: {e}"),
                Err(e) => {
                    println!("✗ {e}");
                    let next = inquire::Select::new(
                        "What now?",
                        vec!["Change the settings", "Save anyway", "Quit"],
                    )
                    .prompt()?;
                    match next {
                        "Change the settings" => {
                            answers = ask(Some(&answers), o.headless).await?;
                        }
                        "Save anyway" => break,
                        _ => return Ok(()),
                    }
                }
            }
        }
    }

    let backup = save(config_path, &answers)?;
    println!("✓ Wrote {}", config_path.display());
    if let Some(b) = backup {
        println!("  (the old config is in {})", b.display());
    }

    #[cfg(feature = "hidpp")]
    offer_udev_rule(interactive)?;

    if o.no_service {
        return Ok(());
    }
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("cannot find the deskhand binary")?;
    let exe = stable_exe(&exe, &nix_profile_links());
    let unit_path = user_unit_dir().join(UNIT_NAME);
    if let Some(why) = service_blocker(&exe, &unit_path) {
        println!("Not installing the service: {why}");
        return Ok(());
    }
    let starts = if answers.headless {
        "starts at boot, without a login"
    } else {
        "starts with your desktop session"
    };
    let install = !interactive
        || inquire::Confirm::new(&format!(
            "Install and start the systemd user service ({starts})?"
        ))
        .with_default(true)
        .prompt()?;
    if install {
        let config_arg = explicit_config.then_some(config_path);
        let path = install_service(&exe, config_arg, answers.headless)?;
        println!("✓ Installed {} and started it", path.display());
        if answers.headless {
            offer_linger(interactive)?;
        }
        println!("  Logs: journalctl --user -u {UNIT_NAME} -f    Status: deskhand doctor");
    } else {
        println!("Start it yourself with: {} run", exe.display());
    }
    println!(
        "The device should appear in Home Assistant under Settings → Devices & services → MQTT."
    );
    Ok(())
}

/// Headless: the user manager (and so the service) only runs at boot with
/// lingering. Turns it on (`loginctl` asks polkit; NixOS declares it).
fn offer_linger(interactive: bool) -> anyhow::Result<()> {
    let user = std::env::var("USER").unwrap_or_default();
    if crate::doctor::linger_enabled(&user) != Some(false) {
        return Ok(());
    }
    println!();
    println!("Lingering is off for {user}, so the service only runs while you are logged in.");
    if Path::new("/etc/NIXOS").exists() {
        println!("On NixOS, set it in your system configuration:");
        println!("  users.users.{user}.linger = true;");
        return Ok(());
    }
    let enable = !interactive
        || inquire::Confirm::new("Enable lingering now (loginctl enable-linger)?")
            .with_default(true)
            .prompt()?;
    if !enable {
        println!("Later: loginctl enable-linger {user}");
        return Ok(());
    }
    let ok = std::process::Command::new("loginctl")
        .args(["--no-ask-password", "enable-linger", &user])
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        println!("✓ Lingering enabled: Deskhand now starts at boot");
    } else {
        println!("✗ loginctl was not allowed to; run: sudo loginctl enable-linger {user}");
    }
    Ok(())
}

/// Offers to install the udev rule when a Logitech receiver is present
/// but can't be opened (peripheral batteries over HID++). Solaar's rules
/// grant the same access, so nothing is offered when they are installed.
#[cfg(feature = "hidpp")]
fn offer_udev_rule(interactive: bool) -> anyhow::Result<()> {
    use deskhand_hidpp::{Access, HidBackend};
    use deskhand_linux::hidraw::{LinuxHidraw, UDEV_INSTALL_HINT, UDEV_RULE, UDEV_RULE_PATH};

    let backend = LinuxHidraw::new();
    let Ok(nodes) = backend.enumerate() else {
        return Ok(());
    };
    let denied = deskhand_hidpp::candidates(&nodes).any(|n| backend.access(n) == Access::Denied);
    if !denied {
        return Ok(());
    }
    println!();
    println!("A Logitech receiver is connected, but Deskhand can't open it, so its keyboard and");
    println!(
        "mouse batteries won't show up. A udev rule (like Solaar's) fixes that, once per machine."
    );
    if Path::new("/etc/NIXOS").exists() {
        println!("On NixOS, add the package's rule to your system configuration:");
        println!("  services.udev.packages = [ deskhand ];  # the package that provides deskhand");
        return Ok(());
    }
    let commands = [
        format!("sudo tee {UDEV_RULE_PATH} >/dev/null   (the rule from `deskhand udev-rule`)"),
        "sudo udevadm control --reload".to_owned(),
        "sudo udevadm trigger --subsystem-match=hidraw".to_owned(),
    ];
    if !interactive {
        println!("Install it with:\n  {UDEV_INSTALL_HINT}");
        return Ok(());
    }
    println!("This runs:");
    for c in &commands {
        println!("  {c}");
    }
    let install = inquire::Confirm::new("Install the udev rule now (asks for your sudo password)?")
        .with_default(true)
        .prompt()?;
    if !install {
        println!("Later: {UDEV_INSTALL_HINT}");
        return Ok(());
    }
    let mut tee = std::process::Command::new("sudo")
        .args(["tee", UDEV_RULE_PATH])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .context("cannot run sudo")?;
    tee.stdin
        .take()
        .context("no stdin for sudo tee")?
        .write_all(UDEV_RULE.as_bytes())?;
    if !tee.wait()?.success() {
        bail!("installing {UDEV_RULE_PATH} failed");
    }
    for args in [
        &["udevadm", "control", "--reload"][..],
        &["udevadm", "trigger", "--subsystem-match=hidraw"][..],
    ] {
        let status = std::process::Command::new("sudo").args(args).status()?;
        if !status.success() {
            bail!("sudo {} failed", args.join(" "));
        }
    }
    println!("✓ Installed {UDEV_RULE_PATH}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers() -> Answers {
        Answers {
            host: "homeassistant.local".into(),
            port: 1883,
            tls: false,
            ca_file: None,
            username: Some("desk\"hand".into()),
            password: Some("pw".into()),
            name: Some("Office PC".into()),
            power_buttons: vec!["suspend".into(), "reboot".into()],
            headless: false,
        }
    }

    #[test]
    fn config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deskhand/config.toml");
        save(&path, &answers()).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let pw = path.parent().unwrap().join("password");
        assert_eq!(mode(&pw), 0o600);
        assert_eq!(std::fs::read_to_string(&pw).unwrap(), "pw\n");

        let config = deskhand_core::Config::load(&path).unwrap();
        let mqtt = config.section::<MqttConfig>("mqtt").unwrap();
        assert!(mqtt.unknown_keys.is_empty(), "{:?}", mqtt.unknown_keys);
        let mqtt = mqtt.value;
        assert_eq!(mqtt.host, "homeassistant.local");
        assert_eq!(mqtt.username.as_deref(), Some("desk\"hand"));
        assert_eq!(mqtt.password_file.as_deref(), Some(pw.as_path()));
        assert_eq!(mqtt.resolve_password().unwrap().as_deref(), Some("pw"));
        assert_eq!(
            config.device().unwrap().value.name.as_deref(),
            Some("Office PC")
        );
        #[derive(Debug, Default, serde::Deserialize)]
        struct Power {
            buttons: Vec<String>,
        }
        let power = config
            .integration("power")
            .unwrap()
            .options::<Power>()
            .unwrap();
        assert!(power.unknown_keys.is_empty());
        assert_eq!(power.value.buttons, vec!["suspend", "reboot"]);

        // Saving again keeps a backup.
        let backup = save(
            &path,
            &Answers {
                port: 1884,
                ..answers()
            },
        )
        .unwrap()
        .unwrap();
        assert!(
            std::fs::read_to_string(backup)
                .unwrap()
                .contains("port = 1883")
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("port = 1884")
        );
    }

    #[test]
    fn headless_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let a = Answers {
            headless: true,
            ..answers()
        };
        save(&path, &a).unwrap();
        let config = deskhand_core::Config::load(&path).unwrap();
        assert!(config.general().unwrap().value.headless);
        // Power buttons stay on despite the headless default.
        let power = config.integration("power").unwrap();
        assert_eq!(power.enabled, deskhand_core::Enabled::On);
        assert_eq!(
            config.integration("media").unwrap().enabled,
            deskhand_core::Enabled::Off
        );
        assert!(!render_config(&answers(), None).contains("[general]"));
    }

    #[test]
    fn anonymous_tls_config() {
        let a = Answers {
            tls: true,
            port: 8883,
            ca_file: Some("/etc/ca.pem".into()),
            username: None,
            password: None,
            name: None,
            power_buttons: Vec::new(),
            ..answers()
        };
        let text = render_config(&a, None);
        assert!(text.contains("tls = true\nca_file = \"/etc/ca.pem\"\n"));
        assert!(
            !text.contains("username")
                && !text.contains("password")
                && !text.contains("[device]")
                && !text.contains("[integrations.power]")
        );
    }

    #[test]
    fn candidate_order() {
        let c = order_candidates(
            vec![
                ("broker.local".into(), 1883),
                ("homeassistant.local".into(), 1883),
            ],
            vec!["ha.local".into(), "broker.local".into()],
            |h, _| h != "broker.local",
        );
        let hosts: Vec<_> = c.iter().map(|c| (c.host.as_str(), c.reachable)).collect();
        assert_eq!(
            hosts,
            vec![
                ("homeassistant.local", true),
                ("ha.local", true),
                ("localhost", true),
                ("broker.local", false),
            ]
        );
        assert_eq!(c[0].source, "found on the network (_mqtt._tcp)");
        assert_eq!(
            mdns_host(
                "ha.local.",
                &["fe80::1".parse().unwrap(), "10.0.0.2".parse().unwrap()]
            ),
            ("ha.local".into(), Some("10.0.0.2".parse().unwrap()))
        );
    }

    #[test]
    fn unit_file() {
        let unit = render_unit(Path::new("/opt/deskhand"), None, false);
        assert!(unit.contains("ExecStart=/opt/deskhand run\n"));
        assert!(unit.contains("Type=notify\n"));
        assert!(unit.contains("PartOf=graphical-session.target\n"));
        assert!(unit.contains("WantedBy=graphical-session.target\n"));
        let unit = render_unit(Path::new("/opt/deskhand"), None, true);
        assert!(!unit.contains("graphical-session"), "{unit}");
        assert!(unit.contains("WantedBy=default.target\n"));
        let unit = render_unit(
            Path::new("/opt/deskhand"),
            Some(Path::new("/etc/d.toml")),
            false,
        );
        assert!(unit.contains("ExecStart=/opt/deskhand --config /etc/d.toml run\n"));
        assert!(
            service_blocker(
                Path::new("/nix/store/abc-deskhand/bin/deskhand"),
                Path::new("/nonexistent")
            )
            .is_some()
        );
        assert!(service_blocker(Path::new("/opt/deskhand"), Path::new("/nonexistent")).is_none());
    }

    #[test]
    fn nix_profile_link_is_preferred() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("deskhand");
        // Any absolute target works for the test; only /nix/store paths are
        // replaced, so use one when it exists (always on NixOS).
        let Some(target) = std::fs::read_dir("/nix/store")
            .ok()
            .and_then(|mut d| d.next())
            .and_then(|e| e.ok())
            .map(|e| e.path())
        else {
            return;
        };
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let canonical = target.canonicalize().unwrap();
        assert_eq!(
            stable_exe(&canonical, &[PathBuf::from("/nonexistent"), link.clone()]),
            link
        );
        assert_eq!(stable_exe(&canonical, &[]), canonical);
        assert_eq!(
            stable_exe(Path::new("/opt/deskhand"), &[link]),
            Path::new("/opt/deskhand")
        );
    }
}
