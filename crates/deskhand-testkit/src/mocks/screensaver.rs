//! Mock `org.freedesktop.ScreenSaver` on a [`TestBus`] session bus, served
//! at `/org/freedesktop/ScreenSaver` and `/ScreenSaver` like KDE does.

use std::sync::{Arc, Mutex};

use zbus::object_server::SignalEmitter;

use crate::TestBus;

pub const BUS_NAME: &str = "org.freedesktop.ScreenSaver";
pub const PATHS: [&str; 2] = ["/org/freedesktop/ScreenSaver", "/ScreenSaver"];

/// An `Inhibit` call that has not been undone with `UnInhibit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inhibition {
    pub cookie: u32,
    pub application: String,
    pub reason: String,
}

#[derive(Debug, Default)]
struct State {
    active: bool,
    active_since_secs: u32,
    idle_secs: u32,
    next_cookie: u32,
    inhibitions: Vec<Inhibition>,
    calls: Vec<String>,
}

type Shared = Arc<Mutex<State>>;

fn lock(state: &Shared) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

struct ScreenSaver {
    state: Shared,
}

#[zbus::interface(name = "org.freedesktop.ScreenSaver")]
impl ScreenSaver {
    async fn get_active(&self) -> bool {
        lock(&self.state).active
    }

    async fn set_active(
        &self,
        active: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<bool> {
        {
            let mut s = lock(&self.state);
            s.calls.push(format!("SetActive({active})"));
            s.active = active;
        }
        Self::active_changed(&emitter, active).await?;
        Ok(true)
    }

    async fn get_active_time(&self) -> u32 {
        lock(&self.state).active_since_secs
    }

    async fn get_session_idle_time(&self) -> u32 {
        lock(&self.state).idle_secs
    }

    async fn inhibit(&self, application_name: &str, reason_for_inhibit: &str) -> u32 {
        let mut s = lock(&self.state);
        s.next_cookie += 1;
        let cookie = s.next_cookie;
        s.calls.push(format!("Inhibit({application_name})"));
        s.inhibitions.push(Inhibition {
            cookie,
            application: application_name.into(),
            reason: reason_for_inhibit.into(),
        });
        cookie
    }

    async fn un_inhibit(&self, cookie: u32) {
        let mut s = lock(&self.state);
        s.calls.push(format!("UnInhibit({cookie})"));
        s.inhibitions.retain(|i| i.cookie != cookie);
    }

    async fn lock(&self) {
        lock(&self.state).calls.push("Lock".into());
    }

    #[zbus(signal)]
    async fn active_changed(emitter: &SignalEmitter<'_>, active: bool) -> zbus::Result<()>;
}

/// Control handle for the mock.
#[derive(Debug, Clone)]
pub struct MockScreenSaver {
    conn: zbus::Connection,
    state: Shared,
}

impl MockScreenSaver {
    /// Claims `org.freedesktop.ScreenSaver` on the test session bus.
    pub async fn start(bus: &TestBus) -> zbus::Result<Self> {
        let state: Shared = Arc::default();
        let mut builder = zbus::connection::Builder::address(bus.session_address())?;
        for path in PATHS {
            builder = builder.serve_at(
                path,
                ScreenSaver {
                    state: state.clone(),
                },
            )?;
        }
        let conn = builder.name(BUS_NAME)?.build().await?;
        Ok(Self { conn, state })
    }

    pub fn calls(&self) -> Vec<String> {
        lock(&self.state).calls.clone()
    }

    pub fn inhibitions(&self) -> Vec<Inhibition> {
        lock(&self.state).inhibitions.clone()
    }

    /// What `GetSessionIdleTime` returns, in seconds.
    pub fn set_idle_time(&self, secs: u32) {
        lock(&self.state).idle_secs = secs;
    }

    /// Changes the active (locked/blanked) state and emits `ActiveChanged` on both paths.
    pub async fn set_active(&self, active: bool) -> zbus::Result<()> {
        lock(&self.state).active = active;
        for path in PATHS {
            let iface = self
                .conn
                .object_server()
                .interface::<_, ScreenSaver>(path)
                .await?;
            ScreenSaver::active_changed(iface.signal_emitter(), active).await?;
        }
        Ok(())
    }
}
