//! Mock `org.freedesktop.login1` (systemd-logind) on a [`TestBus`] system bus.
//!
//! Serves the Manager at `/org/freedesktop/login1` and one session (id `1`)
//! at both `/org/freedesktop/login1/session/_31` and `.../session/auto`,
//! which is how a user service finds "its" graphical session. The
//! [`MockLogind`] handle changes state (emitting `PropertiesChanged`), emits
//! signals, and records method calls.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::{Arc, Mutex};

use zbus::fdo;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedFd, OwnedObjectPath};

use crate::TestBus;

pub const BUS_NAME: &str = "org.freedesktop.login1";
pub const MANAGER_PATH: &str = "/org/freedesktop/login1";
pub const SESSION_ID: &str = "1";
pub const SESSION_PATH: &str = "/org/freedesktop/login1/session/_31";
pub const SESSION_AUTO_PATH: &str = "/org/freedesktop/login1/session/auto";
pub const USER_PATH: &str = "/org/freedesktop/login1/user/_1000";
pub const USER_SELF_PATH: &str = "/org/freedesktop/login1/user/self";

/// An inhibitor lock taken through `Inhibit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inhibitor {
    pub what: String,
    pub who: String,
    pub why: String,
    pub mode: String,
    /// False once the client closed its file descriptor.
    pub active: bool,
}

#[derive(Debug)]
struct State {
    idle_hint: bool,
    idle_since_usec: u64,
    lid_closed: bool,
    docked: bool,
    locked_hint: bool,
    session_active: bool,
    can: BTreeMap<String, String>,
    calls: Vec<String>,
    inhibitors: Vec<Inhibitor>,
}

impl Default for State {
    fn default() -> Self {
        let can = [
            "Suspend",
            "Hibernate",
            "Reboot",
            "PowerOff",
            "HybridSleep",
            "SuspendThenHibernate",
        ]
        .into_iter()
        .map(|a| (a.to_owned(), "yes".to_owned()))
        .collect();
        Self {
            idle_hint: false,
            idle_since_usec: 0,
            lid_closed: false,
            docked: false,
            locked_hint: false,
            session_active: true,
            can,
            calls: Vec::new(),
            inhibitors: Vec::new(),
        }
    }
}

type Shared = Arc<Mutex<State>>;

fn lock(state: &Shared) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

struct Manager {
    state: Shared,
}

impl Manager {
    fn record(&self, call: String) {
        lock(&self.state).calls.push(call);
    }

    fn can(&self, action: &str) -> String {
        lock(&self.state)
            .can
            .get(action)
            .cloned()
            .unwrap_or_else(|| "na".into())
    }
}

fn session_path() -> OwnedObjectPath {
    OwnedObjectPath::try_from(SESSION_PATH).expect("valid path")
}

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl Manager {
    async fn get_session(&self, id: &str) -> fdo::Result<OwnedObjectPath> {
        match id {
            SESSION_ID | "auto" => Ok(session_path()),
            other => Err(fdo::Error::Failed(format!("No session '{other}' known"))),
        }
    }

    async fn get_session_by_pid(&self, _pid: u32) -> OwnedObjectPath {
        session_path()
    }

    async fn get_user_by_pid(&self, _pid: u32) -> OwnedObjectPath {
        OwnedObjectPath::try_from(USER_PATH).expect("valid path")
    }

    async fn get_user(&self, _uid: u32) -> OwnedObjectPath {
        OwnedObjectPath::try_from(USER_PATH).expect("valid path")
    }

    /// `(what, who, why, mode, uid, pid)` of active inhibitors.
    async fn list_inhibitors(&self) -> Vec<(String, String, String, String, u32, u32)> {
        lock(&self.state)
            .inhibitors
            .iter()
            .filter(|i| i.active)
            .map(|i| {
                (
                    i.what.clone(),
                    i.who.clone(),
                    i.why.clone(),
                    i.mode.clone(),
                    1000,
                    4242,
                )
            })
            .collect()
    }

    async fn list_sessions(&self) -> Vec<(String, u32, String, String, OwnedObjectPath)> {
        vec![(
            SESSION_ID.into(),
            1000,
            "dev".into(),
            "seat0".into(),
            session_path(),
        )]
    }

    async fn can_suspend(&self) -> String {
        self.can("Suspend")
    }
    async fn can_hibernate(&self) -> String {
        self.can("Hibernate")
    }
    async fn can_reboot(&self) -> String {
        self.can("Reboot")
    }
    async fn can_power_off(&self) -> String {
        self.can("PowerOff")
    }
    async fn can_hybrid_sleep(&self) -> String {
        self.can("HybridSleep")
    }
    async fn can_suspend_then_hibernate(&self) -> String {
        self.can("SuspendThenHibernate")
    }

    async fn suspend(&self, interactive: bool) {
        self.record(format!("Suspend({interactive})"));
    }
    async fn hibernate(&self, interactive: bool) {
        self.record(format!("Hibernate({interactive})"));
    }
    async fn reboot(&self, interactive: bool) {
        self.record(format!("Reboot({interactive})"));
    }
    async fn power_off(&self, interactive: bool) {
        self.record(format!("PowerOff({interactive})"));
    }
    async fn lock_session(&self, id: &str) {
        self.record(format!("LockSession({id})"));
    }
    async fn unlock_session(&self, id: &str) {
        self.record(format!("UnlockSession({id})"));
    }

    /// Returns one end of a pipe; the lock is held until the client closes it.
    async fn inhibit(
        &self,
        what: &str,
        who: &str,
        why: &str,
        mode: &str,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> fdo::Result<OwnedFd> {
        let (mut reader, writer) =
            std::io::pipe().map_err(|e| fdo::Error::Failed(e.to_string()))?;
        let index = {
            let mut s = lock(&self.state);
            s.calls.push(format!("Inhibit({what},{who},{mode})"));
            s.inhibitors.push(Inhibitor {
                what: what.into(),
                who: who.into(),
                why: why.into(),
                mode: mode.into(),
                active: true,
            });
            s.inhibitors.len() - 1
        };
        emit_inhibited_changed(conn.clone()).await;
        // EOF on the read end means every copy of the client's fd is closed.
        let state = self.state.clone();
        let conn = conn.clone();
        let runtime = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            let mut buf = [0u8; 16];
            while matches!(reader.read(&mut buf), Ok(n) if n > 0) {}
            lock(&state).inhibitors[index].active = false;
            runtime.spawn(emit_inhibited_changed(conn));
        });
        Ok(OwnedFd::from(std::os::fd::OwnedFd::from(writer)))
    }

    #[zbus(property)]
    async fn idle_hint(&self) -> bool {
        lock(&self.state).idle_hint
    }

    #[zbus(property)]
    async fn idle_since_hint(&self) -> u64 {
        lock(&self.state).idle_since_usec
    }

    #[zbus(property)]
    async fn lid_closed(&self) -> bool {
        lock(&self.state).lid_closed
    }

    #[zbus(property)]
    async fn docked(&self) -> bool {
        lock(&self.state).docked
    }

    #[zbus(property)]
    async fn block_inhibited(&self) -> String {
        self.inhibited("block")
    }

    #[zbus(property)]
    async fn delay_inhibited(&self) -> String {
        self.inhibited("delay")
    }

    #[zbus(signal)]
    async fn prepare_for_sleep(emitter: &SignalEmitter<'_>, start: bool) -> zbus::Result<()>;
}

impl Manager {
    /// Colon-separated `what` of active inhibitors in `mode`, like logind.
    fn inhibited(&self, mode: &str) -> String {
        let s = lock(&self.state);
        let mut whats: Vec<&str> = s
            .inhibitors
            .iter()
            .filter(|i| i.active && i.mode == mode)
            .flat_map(|i| i.what.split(':'))
            .collect();
        whats.sort_unstable();
        whats.dedup();
        whats.join(":")
    }
}

/// `PropertiesChanged` for `BlockInhibited` and `DelayInhibited`.
async fn emit_inhibited_changed(conn: zbus::Connection) {
    if let Ok(m) = conn
        .object_server()
        .interface::<_, Manager>(MANAGER_PATH)
        .await
    {
        let manager = m.get().await;
        let _ = manager.block_inhibited_changed(m.signal_emitter()).await;
        let _ = manager.delay_inhibited_changed(m.signal_emitter()).await;
    }
}

struct User;

#[zbus::interface(name = "org.freedesktop.login1.User")]
impl User {
    /// The user's graphical session: what a user service should follow.
    #[zbus(property)]
    async fn display(&self) -> (String, OwnedObjectPath) {
        (SESSION_ID.into(), session_path())
    }

    #[zbus(property, name = "UID")]
    async fn uid(&self) -> u32 {
        1000
    }
}

struct Session {
    state: Shared,
}

#[zbus::interface(name = "org.freedesktop.login1.Session")]
impl Session {
    /// Asks the screen locker to lock (the real logind emits `Lock`; the
    /// locker then sets `LockedHint`). The mock records the call and emits `Lock`.
    #[zbus(name = "Lock")]
    async fn lock_method(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        lock(&self.state).calls.push("Session.Lock".into());
        Self::lock_signal(&emitter).await?;
        Ok(())
    }

    #[zbus(name = "Unlock")]
    async fn unlock_method(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        lock(&self.state).calls.push("Session.Unlock".into());
        Self::unlock_signal(&emitter).await?;
        Ok(())
    }

    async fn set_locked_hint(&self, locked: bool) {
        let mut s = lock(&self.state);
        s.calls.push(format!("SetLockedHint({locked})"));
        s.locked_hint = locked;
    }

    async fn set_idle_hint(&self, idle: bool) {
        let mut s = lock(&self.state);
        s.calls.push(format!("SetIdleHint({idle})"));
        s.idle_hint = idle;
    }

    #[zbus(property)]
    async fn id(&self) -> String {
        SESSION_ID.into()
    }

    #[zbus(property)]
    async fn name(&self) -> String {
        "dev".into()
    }

    #[zbus(property, name = "Type")]
    async fn session_type(&self) -> String {
        "wayland".into()
    }

    #[zbus(property)]
    async fn class(&self) -> String {
        "user".into()
    }

    #[zbus(property)]
    async fn active(&self) -> bool {
        lock(&self.state).session_active
    }

    #[zbus(property)]
    async fn locked_hint(&self) -> bool {
        lock(&self.state).locked_hint
    }

    #[zbus(property)]
    async fn idle_hint(&self) -> bool {
        lock(&self.state).idle_hint
    }

    #[zbus(signal, name = "Lock")]
    async fn lock_signal(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal, name = "Unlock")]
    async fn unlock_signal(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Control handle for the mock. Keep it alive for as long as the service should run.
#[derive(Debug, Clone)]
pub struct MockLogind {
    conn: zbus::Connection,
    state: Shared,
}

impl MockLogind {
    /// Claims `org.freedesktop.login1` on the test system bus.
    pub async fn start(bus: &TestBus) -> zbus::Result<Self> {
        let state: Shared = Arc::default();
        let conn = zbus::connection::Builder::address(bus.system_address())?
            .serve_at(
                MANAGER_PATH,
                Manager {
                    state: state.clone(),
                },
            )?
            .serve_at(USER_PATH, User)?
            .serve_at(USER_SELF_PATH, User)?
            .serve_at(
                SESSION_PATH,
                Session {
                    state: state.clone(),
                },
            )?
            .serve_at(
                SESSION_AUTO_PATH,
                Session {
                    state: state.clone(),
                },
            )?
            .name(BUS_NAME)?
            .build()
            .await?;
        Ok(Self { conn, state })
    }

    /// Method calls received so far, like `Suspend(false)` or `Session.Lock`.
    pub fn calls(&self) -> Vec<String> {
        lock(&self.state).calls.clone()
    }

    pub fn inhibitors(&self) -> Vec<Inhibitor> {
        lock(&self.state).inhibitors.clone()
    }

    /// Sets what `Can<action>` returns (`yes`, `no`, `challenge`, `na`).
    pub fn set_can(&self, action: &str, value: &str) {
        lock(&self.state).can.insert(action.into(), value.into());
    }

    pub async fn set_locked(&self, locked: bool) -> zbus::Result<()> {
        lock(&self.state).locked_hint = locked;
        for path in [SESSION_PATH, SESSION_AUTO_PATH] {
            let iface = self
                .conn
                .object_server()
                .interface::<_, Session>(path)
                .await?;
            iface
                .get()
                .await
                .locked_hint_changed(iface.signal_emitter())
                .await?;
        }
        Ok(())
    }

    /// Sets the idle hint on the manager and the session, with `IdleSinceHint`.
    pub async fn set_idle(&self, idle: bool, since_usec: u64) -> zbus::Result<()> {
        {
            let mut s = lock(&self.state);
            s.idle_hint = idle;
            s.idle_since_usec = since_usec;
        }
        let m = self.manager().await?;
        m.get().await.idle_hint_changed(m.signal_emitter()).await?;
        m.get()
            .await
            .idle_since_hint_changed(m.signal_emitter())
            .await?;
        for path in [SESSION_PATH, SESSION_AUTO_PATH] {
            let iface = self
                .conn
                .object_server()
                .interface::<_, Session>(path)
                .await?;
            iface
                .get()
                .await
                .idle_hint_changed(iface.signal_emitter())
                .await?;
        }
        Ok(())
    }

    pub async fn set_lid_closed(&self, closed: bool) -> zbus::Result<()> {
        lock(&self.state).lid_closed = closed;
        let m = self.manager().await?;
        m.get().await.lid_closed_changed(m.signal_emitter()).await
    }

    pub async fn set_docked(&self, docked: bool) -> zbus::Result<()> {
        lock(&self.state).docked = docked;
        let m = self.manager().await?;
        m.get().await.docked_changed(m.signal_emitter()).await
    }

    /// Emits `PrepareForSleep(start)` (true before suspend, false after resume).
    pub async fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()> {
        let m = self.manager().await?;
        Manager::prepare_for_sleep(m.signal_emitter(), start).await
    }

    /// Emits the session's `Lock` signal, as `loginctl lock-session` does.
    pub async fn emit_lock(&self) -> zbus::Result<()> {
        let iface = self
            .conn
            .object_server()
            .interface::<_, Session>(SESSION_PATH)
            .await?;
        Session::lock_signal(iface.signal_emitter()).await
    }

    async fn manager(&self) -> zbus::Result<zbus::object_server::InterfaceRef<Manager>> {
        self.conn
            .object_server()
            .interface::<_, Manager>(MANAGER_PATH)
            .await
    }
}
