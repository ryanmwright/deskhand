//! Mock notification server (`org.freedesktop.Notifications` on the session bus).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;

use crate::TestBus;

pub const BUS_NAME: &str = "org.freedesktop.Notifications";
pub const PATH: &str = "/org/freedesktop/Notifications";

/// One `Notify` call as received.
#[derive(Debug, Clone, PartialEq)]
pub struct Received {
    pub id: u32,
    pub app_name: String,
    pub replaces_id: u32,
    pub icon: String,
    pub summary: String,
    pub body: String,
    pub actions: Vec<String>,
    pub hints: HashMap<String, OwnedValue>,
    pub timeout: i32,
}

#[derive(Debug, Default)]
struct State {
    next_id: u32,
    received: Vec<Received>,
    closed: Vec<u32>,
}

type Shared = Arc<Mutex<State>>;

struct Server {
    state: Shared,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let mut s = self.state.lock().unwrap();
        let id = if replaces_id != 0 {
            replaces_id
        } else {
            s.next_id += 1;
            s.next_id
        };
        s.received.push(Received {
            id,
            app_name,
            replaces_id,
            icon: app_icon,
            summary,
            body,
            actions,
            hints,
            timeout: expire_timeout,
        });
        id
    }

    async fn close_notification(
        &self,
        id: u32,
        #[zbus(signal_emitter)] e: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        self.state.lock().unwrap().closed.push(id);
        Self::notification_closed(&e, id, 3).await?;
        Ok(())
    }

    async fn get_capabilities(&self) -> Vec<String> {
        ["body", "body-markup", "actions", "inline-reply"]
            .map(String::from)
            .to_vec()
    }

    async fn get_server_information(&self) -> (String, String, String, String) {
        ("Mock".into(), "Deskhand".into(), "1.0".into(), "1.2".into())
    }

    #[zbus(signal)]
    async fn notification_closed(e: &SignalEmitter<'_>, id: u32, reason: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(e: &SignalEmitter<'_>, id: u32, action_key: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn notification_replied(e: &SignalEmitter<'_>, id: u32, text: &str) -> zbus::Result<()>;
}

/// Mock notification server.
#[derive(Debug, Clone)]
pub struct MockNotifications {
    conn: zbus::Connection,
    state: Shared,
}

impl MockNotifications {
    pub async fn start(bus: &TestBus) -> zbus::Result<Self> {
        let state: Shared = Arc::default();
        let conn = zbus::connection::Builder::address(bus.session_address())?
            .serve_at(
                PATH,
                Server {
                    state: state.clone(),
                },
            )?
            .name(BUS_NAME)?
            .build()
            .await?;
        Ok(Self { conn, state })
    }

    /// Every `Notify` call so far.
    pub fn received(&self) -> Vec<Received> {
        self.state.lock().unwrap().received.clone()
    }

    /// Ids passed to `CloseNotification`.
    pub fn closed(&self) -> Vec<u32> {
        self.state.lock().unwrap().closed.clone()
    }

    async fn emitter(&self) -> zbus::Result<SignalEmitter<'static>> {
        SignalEmitter::new(&self.conn, PATH)
    }

    /// The user clicked an action button.
    pub async fn invoke_action(&self, id: u32, key: &str) -> zbus::Result<()> {
        Server::action_invoked(&self.emitter().await?, id, key).await
    }

    /// The user sent an inline reply.
    pub async fn reply(&self, id: u32, text: &str) -> zbus::Result<()> {
        Server::notification_replied(&self.emitter().await?, id, text).await
    }

    /// The notification closed (1 expired, 2 dismissed, 3 closed by call).
    pub async fn close(&self, id: u32, reason: u32) -> zbus::Result<()> {
        Server::notification_closed(&self.emitter().await?, id, reason).await
    }
}
