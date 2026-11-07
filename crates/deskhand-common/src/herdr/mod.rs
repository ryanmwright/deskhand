//! `herdr` capability: coding agents running in
//! [herdr](https://github.com/herdrdev/herdr) on this machine.
//!
//! ```toml
//! [integrations.herdr]
//! session = "work"              # default: herdr's default session
//! # socket = "/path/herdr.sock" # instead of `session`
//! ```
//! Entities: `herdr_running`, the counts `herdr_agents`, `herdr_working` and
//! `herdr_blocked` (attribute `agents`), and the `herdr_agent` event
//! (`working`, `blocked`, `done`, `exited`). No entity per agent: pane ids
//! only live as long as the herdr server.
//!
//! herdr only reports status changes per pane, so Deskhand keeps two
//! subscriptions: a *watch* on structural events (panes, agents detected,
//! workspaces) that triggers a re-read of the snapshot, and a *status* one
//! for the panes with an agent, rebuilt when that set changes. The server
//! often starts after Deskhand (on the first attach), so the integration
//! stays up and reconnects by itself.

mod client;
mod tracker;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use deskhand_core::ctx::{BinarySensor, Event, Sensor};
use deskhand_core::entity::{BinarySensorClass, StateClass};
use deskhand_core::{Attributes, Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::Instant;

use client::{AgentStatus, ClientError, Subscription};
use tracker::{Fired, Status, StatusUpdate, Tracker};

pub const HERDR_RUNNING: EntityId = EntityId::from_static("herdr_running");
pub const HERDR_AGENTS: EntityId = EntityId::from_static("herdr_agents");
pub const HERDR_WORKING: EntityId = EntityId::from_static("herdr_working");
pub const HERDR_BLOCKED: EntityId = EntityId::from_static("herdr_blocked");
pub const HERDR_AGENT: EntityId = EntityId::from_static("herdr_agent");

/// `[integrations.herdr]` options.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct HerdrOptions {
    /// A named herdr session (`herdr --session <name>`).
    pub session: Option<String>,
    /// The server socket; overrides `session`.
    pub socket: Option<PathBuf>,
}

/// herdr's config directory: `$XDG_CONFIG_HOME/herdr`, else `~/.config/herdr`.
fn herdr_dir(env: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env("HOME").map(|h| Path::new(&h).join(".config")))
        .map(|c| c.join("herdr"))
}

/// The socket herdr itself would use: the options, then `HERDR_SOCKET_PATH`,
/// `HERDR_SESSION`, then the default session's.
pub fn socket_path(opts: &HerdrOptions, env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(s) = &opts.socket {
        return Some(s.clone());
    }
    let session = match &opts.session {
        Some(s) => Some(s.clone()),
        None => {
            if let Some(p) = env("HERDR_SOCKET_PATH").filter(|p| !p.is_empty()) {
                return Some(p.into());
            }
            env("HERDR_SESSION").filter(|s| !s.is_empty())
        }
    };
    let dir = herdr_dir(&env)?;
    Some(match session.as_deref() {
        None | Some("default") => dir.join("herdr.sock"),
        Some(name) => dir.join("sessions").join(name).join("herdr.sock"),
    })
}

/// Reconnect backoff, debounce and resync periods (shorter in tests).
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub retry_min: Duration,
    pub retry_max: Duration,
    /// A burst of structural events ends after this much quiet...
    pub quiet: Duration,
    /// ...or this long after its first event.
    pub max_settle: Duration,
    /// Re-read the snapshot this often regardless (missed events).
    pub resync: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            retry_min: Duration::from_secs(1),
            retry_max: Duration::from_secs(10),
            quiet: Duration::from_millis(100),
            max_settle: Duration::from_millis(500),
            resync: Duration::from_secs(60),
        }
    }
}

#[derive(Debug)]
pub struct Herdr {
    socket: Option<PathBuf>,
    dir: Option<PathBuf>,
    timing: Timing,
}

impl Herdr {
    /// Resolves the socket from `options` and this process's environment.
    pub fn new(options: &HerdrOptions) -> Self {
        let env = |k: &str| std::env::var(k).ok();
        Self {
            socket: socket_path(options, env),
            dir: herdr_dir(&env),
            timing: Timing::default(),
        }
    }

    /// For tests: a fixed socket and timing.
    pub fn with_socket(socket: PathBuf, timing: Timing) -> Self {
        Self {
            socket: Some(socket),
            dir: None,
            timing,
        }
    }
}

#[async_trait::async_trait]
impl Integration for Herdr {
    fn id(&self) -> &'static str {
        "herdr.socket"
    }

    fn capability(&self) -> &'static str {
        "herdr"
    }

    fn description(&self) -> &'static str {
        "Coding agents in herdr: how many work or wait for you, and events when that changes"
    }

    async fn probe(&self) -> Probe {
        let Some(socket) = &self.socket else {
            return Probe::unavailable("no herdr socket path (HOME is not set)");
        };
        // The server need not run yet: it usually starts on the first
        // attach, and the integration waits for it.
        let installed = socket.exists()
            || socket.parent().is_some_and(Path::exists)
            || self.dir.as_deref().is_some_and(Path::exists);
        if !installed {
            return Probe::unavailable("herdr not found (no ~/.config/herdr)");
        }
        let state = match client::ping(socket).await {
            Ok(v) => format!("herdr {v} running"),
            Err(_) => "herdr not running yet".into(),
        };
        Probe::available_with(format!("{state}, {}", socket.display()))
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let _: HerdrOptions = ctx.config()?;
        let socket = self
            .socket
            .clone()
            .ok_or_else(|| IntegrationError::msg("no herdr socket path (HOME is not set)"))?;
        let entities = Entities::new(&ctx);
        let cancel = ctx.cancelled();
        let mut ready = false;
        let mut delay = self.timing.retry_min;
        loop {
            let mut session = Session {
                socket: &socket,
                timing: self.timing,
                entities: &entities,
                tracker: Tracker::default(),
                status: None,
                watched: BTreeSet::new(),
            };
            let outcome = tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                r = session.run(&ctx, &mut ready) => r,
            };
            match outcome {
                Ok(()) => {
                    tracing::info!("herdr went away; waiting for it");
                    delay = self.timing.retry_min;
                }
                Err(e) => tracing::debug!(error = %e, "herdr not reachable"),
            }
            entities.disconnected();
            if !ready {
                ctx.ready();
                ready = true;
            }
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                () = tokio::time::sleep(delay) => {}
            }
            delay = (delay * 2).min(self.timing.retry_max);
        }
    }
}

/// The published entities.
struct Entities {
    running: BinarySensor,
    agents: Sensor,
    working: Sensor,
    blocked: Sensor,
    events: Event,
}

impl Entities {
    fn new(ctx: &Ctx) -> Self {
        let count = |id, name, icon| {
            ctx.sensor(
                EntitySpec::sensor(id, name)
                    .state_class(StateClass::Measurement)
                    .icon(icon),
            )
        };
        Self {
            running: ctx.binary_sensor(
                EntitySpec::binary_sensor(
                    HERDR_RUNNING,
                    "herdr running",
                    Some(BinarySensorClass::Running),
                )
                .icon("mdi:console"),
            ),
            agents: count(HERDR_AGENTS, "Coding agents", "mdi:robot"),
            working: count(HERDR_WORKING, "Coding agents working", "mdi:robot-happy"),
            blocked: count(HERDR_BLOCKED, "Coding agents waiting", "mdi:robot-confused"),
            events: ctx.event(
                EntitySpec::event(
                    HERDR_AGENT,
                    "Coding agent",
                    tracker::EVENT_TYPES.iter().map(|t| (*t).into()).collect(),
                )
                .icon("mdi:robot-outline"),
            ),
        }
    }

    fn publish(&self, tracker: &Tracker, version: &str, socket: &Path) {
        let mut attrs = Attributes::new();
        attrs.insert("version".into(), json!(version));
        attrs.insert("socket".into(), json!(socket.display().to_string()));
        self.running.set_attributes(attrs);
        self.running.set(true);

        let all: Vec<Value> = tracker.agents.iter().map(|(p, a)| a.json(p)).collect();
        self.agents.set(all.len() as u64);
        self.agents.set_attributes(list_attr(json!(all)));
        for (sensor, status) in [
            (&self.working, Status::Working),
            (&self.blocked, Status::Blocked),
        ] {
            let labels: Vec<String> = tracker
                .with_status(status)
                .map(|(_, a)| a.label())
                .collect();
            sensor.set(labels.len() as u64);
            sensor.set_attributes(list_attr(json!(labels)));
        }
    }

    fn disconnected(&self) {
        self.running.set(false);
        self.running.set_attributes(Attributes::new());
        for s in [&self.agents, &self.working, &self.blocked] {
            s.set(0u64);
            s.set_attributes(list_attr(json!([])));
        }
    }

    fn fire(&self, fired: Fired) {
        tracing::debug!(event = fired.event_type, attributes = ?fired.attributes, "herdr agent");
        self.events.fire(fired.event_type, fired.attributes);
    }
}

fn list_attr(agents: Value) -> Attributes {
    let mut a = Attributes::new();
    a.insert("agents".into(), agents);
    a
}

/// Structural events that can change the set of agents or their labels.
const WATCH: [&str; 6] = [
    "pane.created",
    "pane.closed",
    "pane.exited",
    "pane.agent_detected",
    "workspace.renamed",
    "workspace.closed",
];

/// One connection to a running herdr server.
struct Session<'a> {
    socket: &'a Path,
    timing: Timing,
    entities: &'a Entities,
    tracker: Tracker,
    /// The status subscription and the panes it covers.
    status: Option<Subscription>,
    watched: BTreeSet<String>,
}

impl Session<'_> {
    /// Follows herdr until it goes away (`Ok`) or can't be reached (`Err`).
    async fn run(&mut self, ctx: &Ctx, ready: &mut bool) -> Result<(), ClientError> {
        let version = client::ping(self.socket).await?;
        let mut watch = client::subscribe(
            self.socket,
            WATCH.iter().map(|t| json!({ "type": t })).collect(),
        )
        .await?;
        let snapshot = client::snapshot(self.socket).await?;
        self.tracker.seed(&snapshot);
        self.resubscribe().await?;
        self.entities.publish(&self.tracker, &version, self.socket);
        tracing::info!(%version, agents = self.tracker.agents.len(), "connected to herdr");
        if !*ready {
            ctx.ready();
            *ready = true;
        }

        // When structural events started and last arrived.
        let mut burst: Option<(Instant, Instant)> = None;
        let mut resync =
            tokio::time::interval_at(Instant::now() + self.timing.resync, self.timing.resync);
        loop {
            let settle = burst.map(|(first, last)| {
                (last + self.timing.quiet).min(first + self.timing.max_settle)
            });
            tokio::select! {
                ev = watch.next() => {
                    if ev.is_none() {
                        return Ok(());
                    }
                    let now = Instant::now();
                    burst = Some(burst.map_or((now, now), |(first, _)| (first, now)));
                }
                ev = next_status(&mut self.status) => match ev {
                    Some(ev) => {
                        if self.on_status(&ev) {
                            let now = Instant::now();
                            burst = Some(burst.map_or((now, now), |(first, _)| (first, now)));
                        } else {
                            self.entities.publish(&self.tracker, &version, self.socket);
                        }
                    }
                    None => {
                        // Lost (e.g. dropped as a slow subscriber): rebuild.
                        self.status = None;
                        self.watched.clear();
                        burst = Some((Instant::now(), Instant::now()));
                    }
                },
                () = sleep_until(settle) => {
                    burst = None;
                    self.reconcile().await?;
                    self.entities.publish(&self.tracker, &version, self.socket);
                }
                _ = resync.tick() => {
                    self.reconcile().await?;
                    self.entities.publish(&self.tracker, &version, self.socket);
                }
            }
        }
    }

    /// A status subscription event. Returns whether the snapshot must be
    /// re-read (an unknown pane, or a pane closed).
    fn on_status(&mut self, ev: &client::Event) -> bool {
        if ev.event != "pane.agent_status_changed" {
            return true;
        }
        let pane = ev.data.get("pane_id").and_then(Value::as_str);
        let status = ev
            .data
            .get("agent_status")
            .cloned()
            .and_then(|s| serde_json::from_value::<AgentStatus>(s).ok());
        let (Some(pane), Some(status)) = (pane, status) else {
            return true;
        };
        match self.tracker.apply_status(pane, status) {
            StatusUpdate::UnknownPane => true,
            StatusUpdate::Known(fired) => {
                if let Some(f) = fired {
                    self.entities.fire(f);
                }
                false
            }
        }
    }

    /// Re-reads the snapshot, fires what changed and follows new panes.
    async fn reconcile(&mut self) -> Result<(), ClientError> {
        let snapshot = client::snapshot(self.socket).await?;
        for f in self.tracker.apply_snapshot(&snapshot) {
            self.entities.fire(f);
        }
        self.resubscribe().await
    }

    /// Subscribes to the status of every agent pane, when that set changed.
    /// The new subscription is open before the old one is dropped. A pane
    /// that closed in between (`pane_not_found`) means re-reading first.
    async fn resubscribe(&mut self) -> Result<(), ClientError> {
        for _ in 0..3 {
            let panes = self.tracker.panes();
            if panes == self.watched && (self.status.is_some() || panes.is_empty()) {
                return Ok(());
            }
            if panes.is_empty() {
                self.status = None;
                self.watched.clear();
                return Ok(());
            }
            let subs = panes
                .iter()
                .map(|p| json!({ "type": "pane.agent_status_changed", "pane_id": p }))
                .collect();
            match client::subscribe(self.socket, subs).await {
                Ok(s) => {
                    self.status = Some(s);
                    self.watched = panes;
                    return Ok(());
                }
                Err(e) if e.code() == Some("pane_not_found") => {
                    let snapshot = client::snapshot(self.socket).await?;
                    for f in self.tracker.apply_snapshot(&snapshot) {
                        self.entities.fire(f);
                    }
                }
                Err(e) => return Err(e),
            }
        }
        // Panes keep vanishing; the next event or resync tries again.
        self.status = None;
        self.watched.clear();
        Ok(())
    }
}

async fn next_status(s: &mut Option<Subscription>) -> Option<client::Event> {
    match s {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;
