//! The agents Deskhand knows about, and the transitions worth an event.
//!
//! herdr's own `done` means "idle and not yet seen by a client", which on a
//! machine nobody is attached to never happens, so Deskhand derives it:
//! an agent that was working or blocked and is now idle is done.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use deskhand_core::Attributes;
use serde_json::{Value, json};

use super::client::{AgentInfo, AgentStatus, Snapshot};

/// The status Deskhand publishes (herdr's `done` counts as idle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Working,
    Blocked,
    Unknown,
}

impl Status {
    pub fn from_herdr(s: AgentStatus) -> Self {
        match s {
            AgentStatus::Idle | AgentStatus::Done => Self::Idle,
            AgentStatus::Working => Self::Working,
            AgentStatus::Blocked => Self::Blocked,
            AgentStatus::Unknown => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Unknown => "unknown",
        }
    }
}

/// The `herdr_agent` event types.
pub const EVENT_TYPES: [&str; 4] = ["working", "blocked", "done", "exited"];

/// The event for an agent going from `last` (its last known status, never
/// `Unknown`) to `new`. `Unknown` fires nothing and is skipped over, so
/// working → unknown → idle is still done.
pub fn transition(last: Option<Status>, new: Status) -> Option<&'static str> {
    if new == Status::Unknown || Some(new) == last {
        return None;
    }
    match new {
        Status::Working => Some("working"),
        Status::Blocked => Some("blocked"),
        Status::Idle if matches!(last, Some(Status::Working | Status::Blocked)) => Some("done"),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    /// The agent kind (`claude`, `codex`, ...).
    pub agent: String,
    /// A name the user gave the agent, if any.
    pub name: Option<String>,
    pub workspace_id: String,
    /// The workspace's label (not unique).
    pub workspace: String,
    pub cwd: Option<String>,
    pub status: Status,
    /// The last status other than `Unknown`.
    pub last_known: Option<Status>,
}

impl Agent {
    fn from_info(info: &AgentInfo, labels: &HashMap<&str, &str>) -> Self {
        let status = Status::from_herdr(info.agent_status);
        Self {
            agent: info
                .agent
                .clone()
                .or_else(|| info.display_agent.clone())
                .unwrap_or_else(|| "agent".into()),
            name: info.name.clone(),
            workspace_id: info.workspace_id.clone(),
            workspace: labels
                .get(info.workspace_id.as_str())
                .map_or_else(|| info.workspace_id.clone(), |l| (*l).to_owned()),
            cwd: info.cwd.clone(),
            status,
            last_known: (status != Status::Unknown).then_some(status),
        }
    }

    /// `claude @ deskhand`, or the agent's name when it has one.
    pub fn label(&self) -> String {
        format!(
            "{} @ {}",
            self.name.as_deref().unwrap_or(&self.agent),
            self.workspace
        )
    }

    pub fn json(&self, pane: &str) -> Value {
        json!({
            "agent": self.agent,
            "name": self.name,
            "workspace": self.workspace,
            "status": self.status.as_str(),
            "cwd": self.cwd,
            "pane": pane,
        })
    }
}

/// An event to fire on `herdr_agent`.
#[derive(Debug, Clone, PartialEq)]
pub struct Fired {
    pub event_type: &'static str,
    pub attributes: Attributes,
}

impl Fired {
    fn new(event_type: &'static str, pane: &str, agent: &Agent, previous: Option<Status>) -> Self {
        let mut a = Attributes::new();
        a.insert("agent".into(), json!(agent.agent));
        a.insert("name".into(), json!(agent.name));
        a.insert("label".into(), json!(agent.label()));
        a.insert("workspace".into(), json!(agent.workspace));
        a.insert("cwd".into(), json!(agent.cwd));
        a.insert("status".into(), json!(agent.status.as_str()));
        a.insert("pane".into(), json!(pane));
        a.insert("previous".into(), json!(previous.map(Status::as_str)));
        Self {
            event_type,
            attributes: a,
        }
    }
}

/// The agents of one herdr server, by pane id.
#[derive(Debug, Default)]
pub struct Tracker {
    pub agents: BTreeMap<String, Agent>,
}

impl Tracker {
    /// Replaces everything with `snapshot`, firing nothing (a fresh start or
    /// reconnect: there is no earlier state to compare with).
    pub fn seed(&mut self, snapshot: &Snapshot) {
        let labels = labels(snapshot);
        self.agents = snapshot
            .agents
            .iter()
            .map(|a| (a.pane_id.clone(), Agent::from_info(a, &labels)))
            .collect();
    }

    /// Applies a fresh snapshot: events for changed statuses, for new agents
    /// that are already busy, and `exited` for agents that are gone.
    pub fn apply_snapshot(&mut self, snapshot: &Snapshot) -> Vec<Fired> {
        let labels = labels(snapshot);
        let mut fired = Vec::new();
        let mut next = BTreeMap::new();
        for info in &snapshot.agents {
            let mut fresh = Agent::from_info(info, &labels);
            match self.agents.remove(&info.pane_id) {
                Some(old) => {
                    if let Some(t) = transition(old.last_known, fresh.status) {
                        fired.push(Fired::new(t, &info.pane_id, &fresh, old.last_known));
                    }
                    if fresh.status == Status::Unknown {
                        fresh.last_known = old.last_known;
                    }
                }
                None => {
                    if let Some(t) = transition(None, fresh.status) {
                        fired.push(Fired::new(t, &info.pane_id, &fresh, None));
                    }
                }
            }
            next.insert(info.pane_id.clone(), fresh);
        }
        for (pane, gone) in std::mem::replace(&mut self.agents, next) {
            fired.push(Fired::new("exited", &pane, &gone, gone.last_known));
        }
        fired
    }

    /// Applies a pushed status change.
    pub fn apply_status(&mut self, pane: &str, status: AgentStatus) -> StatusUpdate {
        let Some(agent) = self.agents.get_mut(pane) else {
            return StatusUpdate::UnknownPane;
        };
        let new = Status::from_herdr(status);
        let previous = agent.last_known;
        agent.status = new;
        if new != Status::Unknown {
            agent.last_known = Some(new);
        }
        StatusUpdate::Known(transition(previous, new).map(|t| Fired::new(t, pane, agent, previous)))
    }

    /// Panes with an agent: the ones to watch for status changes.
    pub fn panes(&self) -> BTreeSet<String> {
        self.agents.keys().cloned().collect()
    }

    pub fn with_status(&self, status: Status) -> impl Iterator<Item = (&String, &Agent)> {
        self.agents.iter().filter(move |(_, a)| a.status == status)
    }
}

#[derive(Debug, PartialEq)]
pub enum StatusUpdate {
    /// Not an agent Deskhand knows: re-read the snapshot.
    UnknownPane,
    /// Updated; maybe with an event.
    Known(Option<Fired>),
}

fn labels(snapshot: &Snapshot) -> HashMap<&str, &str> {
    snapshot
        .workspaces
        .iter()
        .map(|w| (w.workspace_id.as_str(), w.label.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::client::Workspace;

    fn info(pane: &str, status: AgentStatus) -> AgentInfo {
        AgentInfo {
            pane_id: pane.into(),
            workspace_id: "w1".into(),
            agent: Some("claude".into()),
            display_agent: None,
            name: None,
            agent_status: status,
            cwd: Some("/src/deskhand".into()),
        }
    }

    fn snap(agents: Vec<AgentInfo>) -> Snapshot {
        Snapshot {
            workspaces: vec![Workspace {
                workspace_id: "w1".into(),
                label: "deskhand".into(),
            }],
            agents,
        }
    }

    fn types(f: &[Fired]) -> Vec<&'static str> {
        f.iter().map(|f| f.event_type).collect()
    }

    #[test]
    fn transitions() {
        use Status::*;
        assert_eq!(transition(None, Working), Some("working"));
        assert_eq!(transition(Some(Idle), Working), Some("working"));
        assert_eq!(transition(Some(Working), Blocked), Some("blocked"));
        assert_eq!(transition(Some(Blocked), Working), Some("working"));
        assert_eq!(transition(Some(Working), Idle), Some("done"));
        assert_eq!(transition(Some(Blocked), Idle), Some("done"));
        assert_eq!(transition(None, Idle), None);
        assert_eq!(transition(Some(Idle), Idle), None);
        assert_eq!(transition(Some(Working), Working), None);
        assert_eq!(transition(Some(Working), Unknown), None);
    }

    #[test]
    fn seed_fires_nothing_and_snapshots_diff() {
        let mut t = Tracker::default();
        t.seed(&snap(vec![info("w1:p1", AgentStatus::Working)]));
        assert_eq!(t.agents["w1:p1"].label(), "claude @ deskhand");

        // Same state: nothing. herdr's done is idle: done once.
        assert!(
            t.apply_snapshot(&snap(vec![info("w1:p1", AgentStatus::Working)]))
                .is_empty()
        );
        let f = t.apply_snapshot(&snap(vec![info("w1:p1", AgentStatus::Done)]));
        assert_eq!(types(&f), ["done"]);
        assert_eq!(f[0].attributes["previous"], json!("working"));
        assert_eq!(f[0].attributes["workspace"], json!("deskhand"));
        assert!(
            t.apply_snapshot(&snap(vec![info("w1:p1", AgentStatus::Idle)]))
                .is_empty()
        );

        // A new busy agent fires; a vanished one exits.
        let f = t.apply_snapshot(&snap(vec![info("w1:p2", AgentStatus::Blocked)]));
        assert_eq!(types(&f), ["blocked", "exited"]);
        assert_eq!(f[1].attributes["pane"], json!("w1:p1"));
    }

    #[test]
    fn unknown_in_between_keeps_the_last_status() {
        let mut t = Tracker::default();
        t.seed(&snap(vec![info("w1:p1", AgentStatus::Working)]));
        assert_eq!(
            t.apply_status("w1:p1", AgentStatus::Unknown),
            StatusUpdate::Known(None)
        );
        assert_eq!(t.agents["w1:p1"].status, Status::Unknown);
        let StatusUpdate::Known(Some(f)) = t.apply_status("w1:p1", AgentStatus::Idle) else {
            panic!("expected done");
        };
        assert_eq!(f.event_type, "done");
        assert_eq!(
            t.apply_status("w1:p9", AgentStatus::Working),
            StatusUpdate::UnknownPane
        );
    }
}
