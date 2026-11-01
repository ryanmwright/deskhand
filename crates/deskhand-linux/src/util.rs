//! Small helpers shared by the Linux integrations.

use deskhand_core::{Commands, IntegrationError};

/// D-Bus errors become integration failures (the supervisor restarts us).
pub(crate) fn dbus_err(e: zbus::Error) -> IntegrationError {
    IntegrationError::other(e)
}

/// Receives from an optional command stream; never resolves for `None`.
pub(crate) async fn recv_opt<T>(commands: Option<&mut Commands<T>>) -> Option<T> {
    match commands {
        Some(c) => c.recv().await,
        None => std::future::pending().await,
    }
}

/// Every signal a service emits. Integrations that mirror a whole service
/// (UPower, BlueZ, NetworkManager, systemd) treat any signal as "something
/// changed" and re-read what they need, which also covers objects appearing
/// and disappearing.
///
/// The match uses the service's current unique name (zbus compares senders
/// by unique name), so a restart of the service ends the stream: the
/// integration fails and the supervisor restarts it against the new owner.
pub(crate) struct ServiceSignals {
    signals: zbus::MessageStream,
    owner_changes: zbus::fdo::NameOwnerChangedStream,
}

pub(crate) async fn signals_from(
    conn: &zbus::Connection,
    service: &str,
) -> zbus::Result<ServiceSignals> {
    let dbus = zbus::fdo::DBusProxy::new(conn).await?;
    let name = zbus::names::BusName::try_from(service.to_owned())?;
    let owner_changes = dbus
        .receive_name_owner_changed_with_args(&[(0, service)])
        .await?;
    let owner = dbus.get_name_owner(name).await?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(owner.to_string())?
        .build();
    let signals = zbus::MessageStream::for_match_rule(rule, conn, Some(1024)).await?;
    Ok(ServiceSignals {
        signals,
        owner_changes,
    })
}

impl ServiceSignals {
    /// The next signal. `None` when the service went away or restarted, or
    /// the connection closed.
    pub(crate) async fn next(&mut self) -> Option<zbus::Message> {
        use futures_util::StreamExt;
        tokio::select! {
            m = self.signals.next() => m?.ok(),
            _ = self.owner_changes.next() => None,
        }
    }
}

/// Waits for a signal, then until none arrived for `quiet`. `None` when the
/// service went away or restarted, or the connection closed.
pub(crate) async fn changed(s: &mut ServiceSignals, quiet: std::time::Duration) -> Option<()> {
    use futures_util::StreamExt;
    tokio::select! {
        m = s.signals.next() => { let _ = m?; }
        _ = s.owner_changes.next() => return None,
    }
    loop {
        tokio::select! {
            () = tokio::time::sleep(quiet) => return Some(()),
            m = s.signals.next() => { let _ = m?; }
            _ = s.owner_changes.next() => return None,
        }
    }
}

/// All properties of `interface` on `path`.
pub(crate) async fn get_all(
    conn: &zbus::Connection,
    destination: &str,
    path: &str,
    interface: &str,
) -> zbus::Result<Props> {
    let p = zbus::fdo::PropertiesProxy::builder(conn)
        .destination(destination.to_owned())?
        .path(path.to_owned())?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    let iface = zbus::names::InterfaceName::try_from(interface.to_owned())?;
    Ok(p.get_all(iface).await?)
}

pub(crate) type Props = std::collections::HashMap<String, zbus::zvariant::OwnedValue>;

/// A typed property from a [`Props`] map.
pub(crate) fn prop<T>(props: &Props, key: &str) -> Option<T>
where
    T: TryFrom<zbus::zvariant::OwnedValue>,
{
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| T::try_from(v).ok())
}
