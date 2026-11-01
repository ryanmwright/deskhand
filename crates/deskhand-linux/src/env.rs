//! Shared access to the D-Bus buses and the user's graphical logind session.
//!
//! One [`LinuxEnv`] is created by the binary and shared (`Arc`) by every Linux
//! integration. Connections are opened lazily and reused. Tests point it at a
//! `TestBus` with [`LinuxEnv::with_addresses`].

use std::sync::Arc;

use tokio::sync::OnceCell;

use crate::streams::Streams;
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, Proxy};

pub const LOGIN1: &str = "org.freedesktop.login1";
pub const LOGIN1_PATH: &str = "/org/freedesktop/login1";
pub const LOGIN1_MANAGER: &str = "org.freedesktop.login1.Manager";
pub const LOGIN1_SESSION: &str = "org.freedesktop.login1.Session";
pub const LOGIN1_USER: &str = "org.freedesktop.login1.User";

#[derive(Debug, Clone)]
enum Address {
    Default,
    Explicit(String),
}

/// Buses, desktop and session of the running user.
#[derive(Debug)]
pub struct LinuxEnv {
    session_addr: Address,
    system_addr: Address,
    desktops: Vec<String>,
    session: OnceCell<Connection>,
    system: OnceCell<Connection>,
    streams: Arc<Streams>,
}

impl LinuxEnv {
    /// The real session and system buses; desktop from `XDG_CURRENT_DESKTOP`.
    pub fn from_env() -> Arc<Self> {
        let desktops = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .split(':')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        Arc::new(Self {
            session_addr: Address::Default,
            system_addr: Address::Default,
            desktops,
            session: OnceCell::new(),
            system: OnceCell::new(),
            streams: Arc::default(),
        })
    }

    /// Explicit bus addresses (tests).
    pub fn with_addresses(session: &str, system: &str, desktops: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            session_addr: Address::Explicit(session.to_owned()),
            system_addr: Address::Explicit(system.to_owned()),
            desktops: desktops.iter().map(|s| s.to_string()).collect(),
            session: OnceCell::new(),
            system: OnceCell::new(),
            streams: Arc::default(),
        })
    }

    /// Playback streams, shared by the `audio` and `media` integrations.
    pub fn streams(&self) -> &Arc<Streams> {
        &self.streams
    }

    /// Whether `XDG_CURRENT_DESKTOP` names `desktop` (case-insensitive), e.g. `KDE`.
    pub fn desktop_is(&self, desktop: &str) -> bool {
        self.desktops
            .iter()
            .any(|d| d.eq_ignore_ascii_case(desktop))
    }

    pub async fn session_bus(&self) -> zbus::Result<Connection> {
        let addr = self.session_addr.clone();
        self.session
            .get_or_try_init(|| async move {
                match addr {
                    Address::Default => Connection::session().await,
                    Address::Explicit(a) => {
                        zbus::connection::Builder::address(a.as_str())?
                            .build()
                            .await
                    }
                }
            })
            .await
            .cloned()
    }

    pub async fn system_bus(&self) -> zbus::Result<Connection> {
        let addr = self.system_addr.clone();
        self.system
            .get_or_try_init(|| async move {
                match addr {
                    Address::Default => Connection::system().await,
                    Address::Explicit(a) => {
                        zbus::connection::Builder::address(a.as_str())?
                            .build()
                            .await
                    }
                }
            })
            .await
            .cloned()
    }

    /// Whether `name` currently has an owner on `conn`.
    pub async fn has_owner(conn: &Connection, name: &str) -> bool {
        let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await else {
            return false;
        };
        let Ok(name) = name.try_into() else {
            return false;
        };
        dbus.name_has_owner(name).await.unwrap_or(false)
    }

    pub async fn login1_manager(&self) -> zbus::Result<Proxy<'static>> {
        Proxy::new(
            &self.system_bus().await?,
            LOGIN1,
            LOGIN1_PATH,
            LOGIN1_MANAGER,
        )
        .await
    }

    /// The user's graphical session: `User.Display`, falling back to
    /// `GetSession("auto")`. A user service has no session of its own, and
    /// "auto" from an ssh login is the ssh session (docs/dev/spikes.md §8).
    pub async fn graphical_session(&self) -> zbus::Result<OwnedObjectPath> {
        let manager = self.login1_manager().await?;
        let conn = self.system_bus().await?;
        let user: zbus::Result<OwnedObjectPath> =
            manager.call("GetUserByPID", &(std::process::id(),)).await;
        if let Ok(user) = user {
            let user = Proxy::new(&conn, LOGIN1, user, LOGIN1_USER).await?;
            if let Ok((id, path)) = user
                .get_property::<(String, OwnedObjectPath)>("Display")
                .await
                && !id.is_empty()
            {
                return Ok(path);
            }
        }
        manager.call("GetSession", &("auto",)).await
    }

    /// A proxy for the graphical session object.
    pub async fn session_proxy(&self) -> zbus::Result<Proxy<'static>> {
        let path = self.graphical_session().await?;
        Proxy::new(&self.system_bus().await?, LOGIN1, path, LOGIN1_SESSION).await
    }
}

#[cfg(test)]
mod tests {
    use deskhand_testkit::mocks::{MockLogind, logind};
    use deskhand_testkit::{TestBus, require_dbus};

    use super::*;

    #[tokio::test]
    async fn resolves_the_display_session() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let _logind = MockLogind::start(&bus).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &["KDE"]);
        assert!(env.desktop_is("kde"));
        assert!(!env.desktop_is("GNOME"));
        let path = env.graphical_session().await.unwrap();
        assert_eq!(path.as_str(), logind::SESSION_PATH);
        let conn = env.system_bus().await.unwrap();
        assert!(LinuxEnv::has_owner(&conn, logind::BUS_NAME).await);
        assert!(!LinuxEnv::has_owner(&conn, "org.example.Nobody").await);
    }
}
