//! The private buses and mocks behave like the real services as seen by a
//! plain zbus client (the way Deskhand's Linux integrations will use them).

use std::time::Duration;

use deskhand_testkit::mocks::{MockLogind, MockScreenSaver, logind, screensaver};
use deskhand_testkit::{TestBus, require_dbus};
use futures_util::StreamExt;
use zbus::Proxy;
use zbus::zvariant::{OwnedFd, OwnedObjectPath};

async fn within<T>(fut: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), fut)
        .await
        .expect("timed out")
}

#[tokio::test]
async fn buses_are_private_and_separate() {
    require_dbus!();
    let bus = TestBus::start().unwrap();
    assert_ne!(bus.session_address(), bus.system_address());
    let _logind = MockLogind::start(&bus).await.unwrap();
    let system = bus.system().await.unwrap();
    let session = bus.session().await.unwrap();
    let has_logind = |conn: zbus::Connection| async move {
        zbus::fdo::DBusProxy::new(&conn)
            .await
            .unwrap()
            .name_has_owner(logind::BUS_NAME.try_into().unwrap())
            .await
            .unwrap()
    };
    assert!(has_logind(system).await);
    assert!(!has_logind(session).await);
}

#[tokio::test]
async fn logind_sessions_power_and_lock_state() {
    require_dbus!();
    let bus = TestBus::start().unwrap();
    let mock = MockLogind::start(&bus).await.unwrap();
    let conn = bus.system().await.unwrap();
    let manager = Proxy::new(
        &conn,
        logind::BUS_NAME,
        logind::MANAGER_PATH,
        "org.freedesktop.login1.Manager",
    )
    .await
    .unwrap();

    let path: OwnedObjectPath = manager.call("GetSession", &("auto",)).await.unwrap();
    assert_eq!(path.as_str(), logind::SESSION_PATH);
    assert!(
        manager
            .call::<_, _, OwnedObjectPath>("GetSession", &("7",))
            .await
            .is_err()
    );

    mock.set_can("Hibernate", "no");
    let can: String = manager.call("CanHibernate", &()).await.unwrap();
    assert_eq!(can, "no");
    let () = manager.call("Suspend", &(false,)).await.unwrap();
    assert!(mock.calls().contains(&"Suspend(false)".to_owned()));

    // LockedHint changes arrive as PropertiesChanged on the `auto` path too.
    let session = Proxy::new(
        &conn,
        logind::BUS_NAME,
        logind::SESSION_AUTO_PATH,
        "org.freedesktop.login1.Session",
    )
    .await
    .unwrap();
    assert!(!session.get_property::<bool>("LockedHint").await.unwrap());
    let mut changes = session.receive_property_changed::<bool>("LockedHint").await;
    mock.set_locked(true).await.unwrap();
    loop {
        let change = within(changes.next()).await.unwrap();
        if change.get().await.unwrap() {
            break;
        }
    }
    assert!(session.get_property::<bool>("LockedHint").await.unwrap());

    // Session.Lock emits the Lock signal.
    let mut lock_signals = session.receive_signal("Lock").await.unwrap();
    let () = session.call("Lock", &()).await.unwrap();
    within(lock_signals.next()).await.unwrap();
    assert!(mock.calls().contains(&"Session.Lock".to_owned()));
}

#[tokio::test]
async fn logind_prepare_for_sleep_and_inhibitors() {
    require_dbus!();
    let bus = TestBus::start().unwrap();
    let mock = MockLogind::start(&bus).await.unwrap();
    let conn = bus.system().await.unwrap();
    let manager = Proxy::new(
        &conn,
        logind::BUS_NAME,
        logind::MANAGER_PATH,
        "org.freedesktop.login1.Manager",
    )
    .await
    .unwrap();

    let fd: OwnedFd = manager
        .call(
            "Inhibit",
            &("sleep", "deskhand", "publish sleeping", "delay"),
        )
        .await
        .unwrap();
    assert_eq!(mock.inhibitors().len(), 1);
    assert!(mock.inhibitors()[0].active);
    let delayed: String = manager.get_property("DelayInhibited").await.unwrap();
    assert_eq!(delayed, "sleep");

    let mut sleeps = manager.receive_signal("PrepareForSleep").await.unwrap();
    mock.prepare_for_sleep(true).await.unwrap();
    let msg = within(sleeps.next()).await.unwrap();
    let (start,): (bool,) = msg.body().deserialize().unwrap();
    assert!(start);

    // Closing the fd releases the lock.
    drop(fd);
    within(async {
        while mock.inhibitors()[0].active {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
}

#[tokio::test]
async fn screensaver_idle_inhibit_and_active_changed() {
    require_dbus!();
    let bus = TestBus::start().unwrap();
    let mock = MockScreenSaver::start(&bus).await.unwrap();
    let conn = bus.session().await.unwrap();
    let proxy = Proxy::new(
        &conn,
        screensaver::BUS_NAME,
        "/ScreenSaver",
        "org.freedesktop.ScreenSaver",
    )
    .await
    .unwrap();

    mock.set_idle_time(42);
    let idle: u32 = proxy.call("GetSessionIdleTime", &()).await.unwrap();
    assert_eq!(idle, 42);

    let cookie: u32 = proxy
        .call("Inhibit", &("deskhand", "keep awake"))
        .await
        .unwrap();
    assert_eq!(mock.inhibitions().len(), 1);
    let () = proxy.call("UnInhibit", &(cookie,)).await.unwrap();
    assert!(mock.inhibitions().is_empty());

    let mut changed = proxy.receive_signal("ActiveChanged").await.unwrap();
    mock.set_active(true).await.unwrap();
    let msg = within(changed.next()).await.unwrap();
    let (active,): (bool,) = msg.body().deserialize().unwrap();
    assert!(active);
    assert!(proxy.call::<_, _, bool>("GetActive", &()).await.unwrap());
}
