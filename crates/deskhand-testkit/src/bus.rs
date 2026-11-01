//! Private D-Bus daemons for tests.
//!
//! [`TestBus`] starts two `dbus-daemon`s (one standing in for the session
//! bus, one for the system bus) with a permissive policy, in a temp dir. Mocks
//! claim well-known names on them; code under test connects with
//! [`TestBus::session`] / [`TestBus::system`]. Dropping the `TestBus` kills
//! both daemons.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};

/// Policy that lets anyone own any name and send anything (tests only).
const CONFIG: &str = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir=@DIR@</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#;

/// Errors starting a test bus.
#[derive(Debug, thiserror::Error)]
pub enum TestBusError {
    #[error("cannot run dbus-daemon (is the dbus package installed? `nix develop` has it): {0}")]
    Spawn(std::io::Error),
    #[error("dbus-daemon did not print its address: {0}")]
    Address(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug)]
struct Daemon {
    child: Child,
    address: String,
}

impl Daemon {
    fn start(dir: &Path, name: &str) -> Result<Self, TestBusError> {
        let socket_dir = dir.join(name);
        std::fs::create_dir_all(&socket_dir)?;
        let config = dir.join(format!("{name}.conf"));
        std::fs::write(
            &config,
            CONFIG.replace("@DIR@", &socket_dir.to_string_lossy()),
        )?;
        let mut child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--nopidfile", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(TestBusError::Spawn)?;
        let stdout = child.stdout.take().expect("piped stdout");
        let mut line = String::new();
        let read = BufReader::new(stdout).read_line(&mut line);
        let address = line.trim().to_owned();
        if read.is_err() || address.is_empty() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(TestBusError::Address(format!("{read:?}")));
        }
        Ok(Self { child, address })
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A private session bus and system bus.
#[derive(Debug)]
pub struct TestBus {
    session: Daemon,
    system: Daemon,
    _dir: tempfile::TempDir,
}

impl TestBus {
    /// Whether `dbus-daemon` can be run here.
    pub fn available() -> bool {
        Command::new("dbus-daemon")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    pub fn start() -> Result<Self, TestBusError> {
        let dir = tempfile::Builder::new().prefix("deskhand-bus-").tempdir()?;
        Ok(Self {
            session: Daemon::start(dir.path(), "session")?,
            system: Daemon::start(dir.path(), "system")?,
            _dir: dir,
        })
    }

    pub fn session_address(&self) -> &str {
        &self.session.address
    }

    pub fn system_address(&self) -> &str {
        &self.system.address
    }

    /// A new client connection to the session bus.
    pub async fn session(&self) -> zbus::Result<zbus::Connection> {
        connect(self.session_address()).await
    }

    /// A new client connection to the system bus.
    pub async fn system(&self) -> zbus::Result<zbus::Connection> {
        connect(self.system_address()).await
    }
}

pub(crate) async fn connect(address: &str) -> zbus::Result<zbus::Connection> {
    zbus::connection::Builder::address(address)?.build().await
}

/// Skips the current test (returns early) when `dbus-daemon` is missing,
/// except in CI, where a missing daemon is a failure.
#[macro_export]
macro_rules! require_dbus {
    () => {
        if !$crate::TestBus::available() {
            if std::env::var_os("CI").is_some() {
                panic!("dbus-daemon is required in CI");
            }
            eprintln!("skipped: dbus-daemon not found (run inside `nix develop`)");
            return;
        }
    };
}
