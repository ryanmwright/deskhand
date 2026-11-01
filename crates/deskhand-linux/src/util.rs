//! Small helpers shared by the Linux integrations (and desktop backends
//! such as `deskhand-kde`).

use deskhand_core::{Commands, IntegrationError};

/// D-Bus errors become integration failures (the supervisor restarts us).
pub fn dbus_err(e: zbus::Error) -> IntegrationError {
    IntegrationError::other(e)
}

/// Receives from an optional command stream; never resolves for `None`.
pub async fn recv_opt<T>(commands: Option<&mut Commands<T>>) -> Option<T> {
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
pub struct ServiceSignals {
    signals: zbus::MessageStream,
    owner_changes: zbus::fdo::NameOwnerChangedStream,
}

pub async fn signals_from(conn: &zbus::Connection, service: &str) -> zbus::Result<ServiceSignals> {
    signals_from_path(conn, service, None).await
}

/// Like [`signals_from`], limited to signals on one object path.
pub async fn signals_from_path(
    conn: &zbus::Connection,
    service: &str,
    path: Option<&str>,
) -> zbus::Result<ServiceSignals> {
    let dbus = zbus::fdo::DBusProxy::new(conn).await?;
    let name = zbus::names::BusName::try_from(service.to_owned())?;
    let owner_changes = dbus
        .receive_name_owner_changed_with_args(&[(0, service)])
        .await?;
    let owner = dbus.get_name_owner(name).await?;
    let mut rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(owner.to_string())?;
    if let Some(path) = path {
        rule = rule.path(path.to_owned())?;
    }
    let rule = rule.build();
    let signals = zbus::MessageStream::for_match_rule(rule, conn, Some(1024)).await?;
    Ok(ServiceSignals {
        signals,
        owner_changes,
    })
}

impl std::fmt::Debug for ServiceSignals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceSignals").finish_non_exhaustive()
    }
}

impl ServiceSignals {
    /// The next signal. `None` when the service went away or restarted, or
    /// the connection closed.
    pub async fn next(&mut self) -> Option<zbus::Message> {
        use futures_util::StreamExt;
        tokio::select! {
            m = self.signals.next() => m?.ok(),
            _ = self.owner_changes.next() => None,
        }
    }
}

/// Waits for a signal, then until none arrived for `quiet`, but at most
/// [`MAX_SETTLE_FACTOR`] × `quiet` (at least 1 s) after the first one: busy
/// services (NetworkManager while scanning, BlueZ with nearby devices) send
/// signals continuously and would otherwise never settle. `None` when the
/// service went away or restarted, or the connection closed.
pub async fn changed(s: &mut ServiceSignals, quiet: std::time::Duration) -> Option<()> {
    use futures_util::StreamExt;
    tokio::select! {
        m = s.signals.next() => { let _ = m?; }
        _ = s.owner_changes.next() => return None,
    }
    let deadline = tokio::time::Instant::now()
        + (quiet * MAX_SETTLE_FACTOR).max(std::time::Duration::from_secs(1));
    loop {
        tokio::select! {
            () = tokio::time::sleep(quiet) => return Some(()),
            () = tokio::time::sleep_until(deadline) => return Some(()),
            m = s.signals.next() => { let _ = m?; }
            _ = s.owner_changes.next() => return None,
        }
    }
}

/// Upper bound of [`changed`]'s settling, in multiples of `quiet`.
pub const MAX_SETTLE_FACTOR: u32 = 4;

/// All properties of `interface` on `path`.
pub async fn get_all(
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

pub type Props = std::collections::HashMap<String, zbus::zvariant::OwnedValue>;

/// A typed property from a [`Props`] map.
pub fn prop<T>(props: &Props, key: &str) -> Option<T>
where
    T: TryFrom<zbus::zvariant::OwnedValue>,
{
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| T::try_from(v).ok())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use deskhand_testkit::{TestBus, require_dbus};

    use super::*;

    struct Chatty;

    #[zbus::interface(name = "org.example.Chatty")]
    impl Chatty {
        #[zbus(signal)]
        async fn tick(e: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
    }

    #[tokio::test]
    async fn settles_even_when_signals_never_stop() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let service = zbus::connection::Builder::address(bus.session_address())
            .unwrap()
            .serve_at("/chatty", Chatty)
            .unwrap()
            .name("org.example.Chatty")
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = zbus::connection::Builder::address(bus.session_address())
            .unwrap()
            .build()
            .await
            .unwrap();
        let mut signals = signals_from(&client, "org.example.Chatty").await.unwrap();
        // A signal every 50 ms, forever (like NetworkManager while scanning).
        let emitter = tokio::spawn(async move {
            let e = zbus::object_server::SignalEmitter::new(&service, "/chatty").unwrap();
            loop {
                let _ = Chatty::tick(&e).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        let quiet = Duration::from_millis(300);
        let start = Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(5), changed(&mut signals, quiet)).await;
        emitter.abort();
        assert_eq!(r.expect("never settled"), Some(()));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
    }
}
