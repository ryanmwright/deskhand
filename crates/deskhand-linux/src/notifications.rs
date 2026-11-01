//! `notifications` capability: desktop notifications from Home Assistant
//! through `org.freedesktop.Notifications`.
//!
//! - `notify` (HA notify entity): each message becomes a notification.
//! - `notification` (JSON command endpoint, `deskhand/<node>/notification/set`):
//!   ```json
//!   {"title": "Doorbell", "message": "Someone is at the door", "icon": "dialog-information",
//!    "urgency": "critical", "timeout": 30, "tag": "door",
//!    "actions": [{"id": "unlock", "label": "Unlock"}, "Ignore"],
//!    "reply": {"placeholder": "Answer…", "button": "Send"}}
//!   ```
//!   A notification with the same `tag` replaces the previous one;
//!   `{"tag": "door", "dismiss": true}` closes it. `timeout` is in seconds
//!   (0 = until dismissed). An action given as a plain string uses the
//!   string as both id and label. The action id `default` is invoked by
//!   clicking the notification itself.
//! - `notification_event` (HA event entity): `action` (attribute `action`),
//!   `reply` (attribute `reply`) and `closed` (attribute `reason`), each
//!   with the notification's `tag` and `title`.
//!
//! Actions only come back as events; nothing is executed.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use deskhand_core::ctx::Event;
use deskhand_core::{Attributes, Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use serde::Deserialize;
use serde_json::json;
use zbus::Proxy;
use zbus::zvariant::Value;

use crate::env::LinuxEnv;
use crate::util::{dbus_err, signals_from};

const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";

pub const NOTIFY: EntityId = EntityId::from_static("notify");
/// The rich JSON command endpoint.
pub const NOTIFICATION: EntityId = EntityId::from_static("notification");
pub const NOTIFICATION_EVENT: EntityId = EntityId::from_static("notification_event");

const APP_NAME: &str = "Home Assistant";
const REPLY_ACTION: &str = "inline-reply";
/// Notifications remembered for their events; the oldest are forgotten first.
const MAX_TRACKED: usize = 256;

/// A rich notification request.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Rich {
    pub title: Option<String>,
    pub message: Option<String>,
    pub icon: Option<String>,
    pub urgency: Option<Urgency>,
    /// Seconds; 0 keeps it until dismissed.
    pub timeout: Option<f64>,
    pub tag: Option<String>,
    pub actions: Vec<ActionSpec>,
    pub reply: Option<ReplySpec>,
    pub dismiss: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ActionSpec {
    Label(String),
    Full { id: String, label: String },
}

impl ActionSpec {
    fn id_label(&self) -> (&str, &str) {
        match self {
            Self::Label(l) => (l, l),
            Self::Full { id, label } => (id, label),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ReplySpec {
    Enabled(bool),
    Custom {
        #[serde(default)]
        placeholder: Option<String>,
        #[serde(default)]
        button: Option<String>,
    },
}

/// What the notification server supports (`GetCapabilities`).
#[derive(Debug, Clone, Copy, Default)]
struct Caps {
    actions: bool,
    body_markup: bool,
    inline_reply: bool,
}

/// Escapes text for servers that render the body as markup.
fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Arguments of one `Notify` call.
#[derive(Debug, Clone, PartialEq)]
struct NotifyCall {
    summary: String,
    body: String,
    icon: String,
    actions: Vec<String>,
    urgency: Option<u8>,
    reply: Option<(String, String)>,
    timeout_ms: i32,
}

fn build(rich: &Rich, caps: Caps) -> NotifyCall {
    let body = rich.message.clone().unwrap_or_default();
    let mut actions = Vec::new();
    if caps.actions {
        for a in &rich.actions {
            let (id, label) = a.id_label();
            actions.extend([id.to_owned(), label.to_owned()]);
        }
    } else if !rich.actions.is_empty() {
        tracing::warn!("notification server does not support actions; dropping them");
    }
    let reply = match &rich.reply {
        None | Some(ReplySpec::Enabled(false)) => None,
        Some(_) if !caps.inline_reply => {
            tracing::warn!("notification server does not support inline replies");
            None
        }
        Some(ReplySpec::Enabled(true)) => Some((None, None)),
        Some(ReplySpec::Custom {
            placeholder,
            button,
        }) => Some((placeholder.clone(), button.clone())),
    }
    .map(|(placeholder, button)| {
        let button = button.unwrap_or_else(|| "Reply".into());
        actions.extend([REPLY_ACTION.to_owned(), button.clone()]);
        (placeholder.unwrap_or_default(), button)
    });
    NotifyCall {
        summary: rich.title.clone().unwrap_or_else(|| APP_NAME.into()),
        body: if caps.body_markup {
            escape_markup(&body)
        } else {
            body
        },
        icon: rich.icon.clone().unwrap_or_default(),
        actions,
        urgency: rich.urgency.map(|u| match u {
            Urgency::Low => 0,
            Urgency::Normal => 1,
            Urgency::Critical => 2,
        }),
        reply,
        timeout_ms: match rich.timeout {
            None => -1,
            Some(t) if t <= 0.0 => 0,
            Some(t) => (t * 1000.0).min(f64::from(i32::MAX)) as i32,
        },
    }
}

/// A notification we sent and still report events for.
#[derive(Debug, Clone)]
struct Sent {
    tag: Option<String>,
    title: Option<String>,
}

/// Desktop notifications.
#[derive(Debug)]
pub struct Notifications {
    env: Arc<LinuxEnv>,
}

impl Notifications {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }
}

struct Session {
    proxy: Proxy<'static>,
    caps: Caps,
    events: Event,
    /// Notification id → what we know about it (ids only grow).
    sent: BTreeMap<u32, Sent>,
    tags: HashMap<String, u32>,
}

impl Session {
    async fn send(&mut self, rich: Rich) {
        let replaces = rich
            .tag
            .as_ref()
            .and_then(|t| self.tags.get(t))
            .copied()
            .unwrap_or(0);
        if rich.dismiss {
            if replaces != 0
                && let Err(e) = self
                    .proxy
                    .call_method("CloseNotification", &(replaces,))
                    .await
            {
                tracing::debug!(error = %e, "closing notification failed");
            }
            return;
        }
        let call = build(&rich, self.caps);
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("desktop-entry", Value::from(deskhand_core::APP_ID));
        if let Some(u) = call.urgency {
            hints.insert("urgency", Value::U8(u));
        }
        if let Some((placeholder, button)) = &call.reply {
            if !placeholder.is_empty() {
                hints.insert(
                    "x-kde-reply-placeholder-text",
                    Value::from(placeholder.as_str()),
                );
            }
            hints.insert(
                "x-kde-reply-submit-button-text",
                Value::from(button.as_str()),
            );
        }
        let result: zbus::Result<u32> = self
            .proxy
            .call(
                "Notify",
                &(
                    APP_NAME,
                    replaces,
                    call.icon.as_str(),
                    call.summary.as_str(),
                    call.body.as_str(),
                    &call.actions,
                    hints,
                    call.timeout_ms,
                ),
            )
            .await;
        let id = match result {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "sending notification failed");
                return;
            }
        };
        if let Some(tag) = &rich.tag {
            self.tags.insert(tag.clone(), id);
        }
        self.sent.insert(
            id,
            Sent {
                tag: rich.tag,
                title: rich.title,
            },
        );
        while self.sent.len() > MAX_TRACKED {
            if let Some((old, s)) = self.sent.pop_first() {
                self.forget_tag(old, &s);
            }
        }
    }

    fn forget_tag(&mut self, id: u32, sent: &Sent) {
        if let Some(t) = &sent.tag
            && self.tags.get(t) == Some(&id)
        {
            self.tags.remove(t);
        }
    }

    fn fire(&self, sent: &Sent, event_type: &str, extra: (&str, serde_json::Value)) {
        let mut attrs = Attributes::new();
        attrs.insert("tag".into(), json!(sent.tag));
        attrs.insert("title".into(), json!(sent.title));
        attrs.insert(extra.0.into(), extra.1);
        self.events.fire(event_type, attrs);
    }

    fn on_signal(&mut self, msg: &zbus::Message) {
        let header = msg.header();
        let (Some(iface), Some(member)) = (header.interface(), header.member()) else {
            return;
        };
        if iface.as_str() != NOTIFICATIONS {
            return;
        }
        match member.as_str() {
            "ActionInvoked" => {
                let Ok((id, key)) = msg.body().deserialize::<(u32, String)>() else {
                    return;
                };
                if key == REPLY_ACTION {
                    return; // The reply itself arrives as NotificationReplied.
                }
                if let Some(s) = self.sent.get(&id) {
                    self.fire(s, "action", ("action", json!(key)));
                }
            }
            "NotificationReplied" => {
                let Ok((id, text)) = msg.body().deserialize::<(u32, String)>() else {
                    return;
                };
                if let Some(s) = self.sent.get(&id) {
                    self.fire(s, "reply", ("reply", json!(text)));
                }
            }
            "NotificationClosed" => {
                let Ok((id, reason)) = msg.body().deserialize::<(u32, u32)>() else {
                    return;
                };
                if let Some(s) = self.sent.remove(&id) {
                    self.forget_tag(id, &s);
                    let reason = match reason {
                        1 => "expired",
                        2 => "dismissed",
                        3 => "closed",
                        _ => "undefined",
                    };
                    self.fire(&s, "closed", ("reason", json!(reason)));
                }
            }
            _ => {}
        }
    }
}

#[async_trait::async_trait]
impl Integration for Notifications {
    fn id(&self) -> &'static str {
        "notifications.freedesktop"
    }

    fn capability(&self) -> &'static str {
        "notifications"
    }

    fn description(&self) -> &'static str {
        "Desktop notifications from Home Assistant, with actions and replies as events"
    }

    async fn probe(&self) -> Probe {
        let conn = match self.env.session_bus().await {
            Ok(c) => c,
            Err(e) => return Probe::unavailable(format!("session bus: {e}")),
        };
        if LinuxEnv::has_owner(&conn, NOTIFICATIONS).await {
            return Probe::available();
        }
        let activatable = match zbus::fdo::DBusProxy::new(&conn).await {
            Ok(d) => d
                .list_activatable_names()
                .await
                .is_ok_and(|n| n.iter().any(|n| n.as_str() == NOTIFICATIONS)),
            Err(_) => false,
        };
        if activatable {
            Probe::available_with("activatable")
        } else {
            Probe::unavailable("no notification server on the session bus")
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let conn = self.env.session_bus().await.map_err(dbus_err)?;
        let proxy = Proxy::new(&conn, NOTIFICATIONS, NOTIFICATIONS_PATH, NOTIFICATIONS)
            .await
            .map_err(dbus_err)?;
        let mut plain = ctx.notify(EntitySpec::notify(NOTIFY, "Notify").icon("mdi:message-badge"));
        let mut rich = ctx.json_command(NOTIFICATION);
        let events = ctx.event(
            EntitySpec::event(
                NOTIFICATION_EVENT,
                "Notification",
                vec!["action".into(), "reply".into(), "closed".into()],
            )
            .icon("mdi:message-reply-text"),
        );
        // Ready before talking to the server: at login it may still be
        // starting (D-Bus activation waits for plasmashell). Commands queue
        // in the meantime.
        ctx.ready();
        // Also starts an activatable server, so the signal match below has an owner.
        let caps: Vec<String> = proxy.call("GetCapabilities", &()).await.map_err(dbus_err)?;
        let has = |c: &str| caps.iter().any(|x| x == c);
        let caps = Caps {
            actions: has("actions"),
            body_markup: has("body-markup"),
            inline_reply: has("inline-reply"),
        };
        let mut signals = signals_from(&conn, NOTIFICATIONS).await.map_err(dbus_err)?;
        let mut s = Session {
            proxy,
            caps,
            events,
            sent: BTreeMap::new(),
            tags: HashMap::new(),
        };

        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                Some(text) = plain.recv() => {
                    s.send(Rich { message: Some(text), ..Rich::default() }).await;
                }
                Some(v) = rich.recv() => match serde_json::from_value::<Rich>(v) {
                    Ok(r) => s.send(r).await,
                    Err(e) => tracing::warn!(error = %e, "invalid notification JSON"),
                },
                msg = signals.next() => match msg {
                    Some(m) => s.on_signal(&m),
                    None => return Err(IntegrationError::msg("notification server restarted or went away")),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use deskhand_core::CommandValue;
    use deskhand_core::testing::wait_event;
    use deskhand_testkit::mocks::MockNotifications;
    use deskhand_testkit::mocks::notifications::Received;
    use deskhand_testkit::{TestBus, require_dbus};

    use super::*;
    use crate::testutil::rig;

    const ALL: Caps = Caps {
        actions: true,
        body_markup: true,
        inline_reply: true,
    };

    #[test]
    fn builds_notify_calls() {
        let rich: Rich = serde_json::from_value(json!({
            "title": "Doorbell",
            "message": "Someone <b>is</b> at the door & waiting",
            "urgency": "critical",
            "timeout": 2.5,
            "actions": [{"id": "unlock", "label": "Unlock"}, "Ignore"],
            "reply": {"placeholder": "Answer…"},
            "unknown_key": 1,
        }))
        .unwrap();
        let call = build(&rich, ALL);
        assert_eq!(call.summary, "Doorbell");
        assert_eq!(
            call.body,
            "Someone &lt;b&gt;is&lt;/b&gt; at the door &amp; waiting"
        );
        assert_eq!(
            call.actions,
            [
                "unlock",
                "Unlock",
                "Ignore",
                "Ignore",
                REPLY_ACTION,
                "Reply"
            ]
        );
        assert_eq!(call.urgency, Some(2));
        assert_eq!(call.timeout_ms, 2500);
        assert_eq!(call.reply, Some(("Answer…".into(), "Reply".into())));

        let plain = build(
            &Rich {
                message: Some("a < b".into()),
                timeout: Some(0.0),
                actions: vec![ActionSpec::Label("x".into())],
                reply: Some(ReplySpec::Enabled(true)),
                ..Rich::default()
            },
            Caps::default(),
        );
        assert_eq!(plain.summary, APP_NAME);
        assert_eq!(plain.body, "a < b", "no escaping without body-markup");
        assert!(plain.actions.is_empty() && plain.reply.is_none());
        assert_eq!(plain.timeout_ms, 0);
    }

    async fn wait_received(server: &MockNotifications, n: usize) -> Received {
        for _ in 0..200 {
            if let Some(r) = server.received().get(n - 1) {
                return r.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("expected {n} notifications, got {:?}", server.received());
    }

    #[tokio::test]
    async fn sends_notifications_and_reports_events() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let server = MockNotifications::start(&bus).await.unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &[]);
        let integration = Notifications::new(env);
        assert!(integration.probe().await.is_available());
        let (mut t, _cancel) = rig(integration, "notifications");
        crate::testutil::wait_entity(&mut t, &NOTIFICATION_EVENT, true).await;

        // The notify entity: plain text.
        t.command(NOTIFY.as_str(), CommandValue::Text("Hello".into()));
        let first = wait_received(&server, 1).await;
        assert_eq!(first.app_name, APP_NAME);
        assert_eq!(first.summary, APP_NAME);
        assert_eq!(first.body, "Hello");
        assert_eq!(first.timeout, -1);

        // Rich: actions, reply, tag.
        let door = json!({
            "title": "Doorbell", "message": "Ring", "tag": "door",
            "actions": [{"id": "unlock", "label": "Unlock"}], "reply": true,
        });
        t.command(NOTIFICATION.as_str(), CommandValue::Json(door.clone()));
        let n = wait_received(&server, 2).await;
        assert_eq!(n.actions, ["unlock", "Unlock", REPLY_ACTION, "Reply"]);
        assert!(n.hints.contains_key("x-kde-reply-submit-button-text"));
        assert_eq!(n.replaces_id, 0);

        server.invoke_action(n.id, "unlock").await.unwrap();
        let ev = wait_event(&mut t, &NOTIFICATION_EVENT, "action").await;
        assert_eq!(ev["action"], json!("unlock"));
        assert_eq!(ev["tag"], json!("door"));
        assert_eq!(ev["title"], json!("Doorbell"));
        server.reply(n.id, "On my way").await.unwrap();
        let ev = wait_event(&mut t, &NOTIFICATION_EVENT, "reply").await;
        assert_eq!(ev["reply"], json!("On my way"));

        // Same tag: replaces. Then dismiss by tag.
        t.command(NOTIFICATION.as_str(), CommandValue::Json(door));
        let again = wait_received(&server, 3).await;
        assert_eq!(again.replaces_id, n.id);
        t.command(
            NOTIFICATION.as_str(),
            CommandValue::Json(json!({"tag": "door", "dismiss": true})),
        );
        let ev = wait_event(&mut t, &NOTIFICATION_EVENT, "closed").await;
        assert_eq!(ev["reason"], json!("closed"));
        assert_eq!(server.closed(), vec![n.id]);

        // Events for notifications from other apps are not reported.
        server.invoke_action(999, "default").await.unwrap();
        server.close(first.id, 2).await.unwrap();
        let ev = wait_event(&mut t, &NOTIFICATION_EVENT, "closed").await;
        assert_eq!(ev["reason"], json!("dismissed"));
        assert_eq!(ev["tag"], serde_json::Value::Null);
        t.settle(Duration::from_millis(200)).await;
        assert_eq!(t.take_events(), vec![], "no event for the foreign action");
    }
}
