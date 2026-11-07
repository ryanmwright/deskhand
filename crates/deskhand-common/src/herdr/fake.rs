//! A fake herdr server with the real one's rules (checked against herdr
//! 0.9.1): one request per connection, one `events.subscribe` per
//! connection, `pane.agent_status_changed` only per pane, `pane_not_found`
//! for unknown panes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[derive(Clone)]
struct Agent {
    agent: String,
    status: String,
}

struct Sub {
    /// `None`: structural events; `Some`: status events of these panes.
    panes: Option<Vec<String>>,
    types: Vec<String>,
    tx: mpsc::UnboundedSender<String>,
}

#[derive(Default)]
struct State {
    /// Panes (`w1:p1`) and their agent, if any.
    panes: BTreeMap<String, Option<Agent>>,
    subs: Vec<Sub>,
    /// Requests seen, by method.
    requests: Vec<String>,
    /// Answer the next status subscription with `pane_not_found`.
    fail_next_status_subscribe: bool,
}

pub struct FakeHerdr {
    path: PathBuf,
    state: Arc<Mutex<State>>,
    server: Option<JoinHandle<()>>,
}

impl FakeHerdr {
    /// A server on `path` with panes `w1:p1` and `w1:p2` and no agents.
    pub fn start(path: &Path) -> Self {
        let mut state = State::default();
        state.panes.insert("w1:p1".into(), None);
        state.panes.insert("w1:p2".into(), None);
        let mut fake = Self {
            path: path.to_owned(),
            state: Arc::new(Mutex::new(state)),
            server: None,
        };
        fake.restart();
        fake
    }

    /// Stops the server: subscriptions end, the socket goes away.
    pub fn stop(&mut self) {
        if let Some(s) = self.server.take() {
            s.abort();
        }
        let mut st = self.state.lock().unwrap();
        st.subs.clear();
        let _ = std::fs::remove_file(&self.path);
    }

    /// (Re)starts it with the same panes and agents.
    pub fn restart(&mut self) {
        self.stop();
        let listener = UnixListener::bind(&self.path).expect("bind fake herdr");
        let state = self.state.clone();
        self.server = Some(tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve(stream, state.clone()));
            }
        }));
    }

    pub fn add_pane(&self, pane: &str) {
        self.state.lock().unwrap().panes.insert(pane.into(), None);
        self.push(
            None,
            "pane_created",
            json!({"pane_id": pane, "workspace_id": "w1"}),
        );
    }

    /// Like `pane.report_agent`: detects the agent or changes its status.
    pub fn report(&self, pane: &str, agent: &str, status: &str) {
        let detected = {
            let mut st = self.state.lock().unwrap();
            let slot = st.panes.get_mut(pane).expect("known pane");
            let detected = slot.is_none();
            if slot.as_ref().is_some_and(|a| a.status == status) {
                return;
            }
            *slot = Some(Agent {
                agent: agent.into(),
                status: status.into(),
            });
            detected
        };
        if detected {
            self.push(
                None,
                "pane_agent_detected",
                json!({"pane_id": pane, "agent": agent, "workspace_id": "w1"}),
            );
        }
        self.push(
            Some(pane),
            "pane.agent_status_changed",
            json!({"pane_id": pane, "agent": agent, "agent_status": status, "workspace_id": "w1"}),
        );
    }

    /// Like `pane.release_agent`.
    pub fn release(&self, pane: &str) {
        let agent = {
            let mut st = self.state.lock().unwrap();
            st.panes.get_mut(pane).and_then(Option::take)
        };
        let Some(agent) = agent else { return };
        self.push(
            Some(pane),
            "pane.agent_status_changed",
            json!({"pane_id": pane, "agent": agent.agent, "agent_status": "unknown", "workspace_id": "w1"}),
        );
        self.push(
            None,
            "pane_agent_detected",
            json!({"pane_id": pane, "agent": agent.agent, "released": true, "workspace_id": "w1"}),
        );
    }

    pub fn close_pane(&self, pane: &str) {
        self.state.lock().unwrap().panes.remove(pane);
        let data = json!({"pane_id": pane, "workspace_id": "w1"});
        self.push(Some(pane), "pane_closed", data.clone());
        self.push(None, "pane_closed", data);
    }

    pub fn fail_next_status_subscribe(&self) {
        self.state.lock().unwrap().fail_next_status_subscribe = true;
    }

    /// How many requests of `method` arrived.
    pub fn requests(&self, method: &str) -> usize {
        let st = self.state.lock().unwrap();
        st.requests.iter().filter(|m| *m == method).count()
    }

    /// Sends an event to the matching subscribers: structural ones
    /// (`pane` = `None`) or the status subscribers of `pane`.
    fn push(&self, pane: Option<&str>, event: &str, data: Value) {
        let kind = event.replacen('_', ".", 1);
        let line = json!({"event": event, "data": data}).to_string();
        let mut st = self.state.lock().unwrap();
        st.subs.retain(|s| !s.tx.is_closed());
        for s in &st.subs {
            let hit = match (&s.panes, pane) {
                (Some(panes), Some(p)) => panes.iter().any(|x| x == p),
                (None, None) => s.types.contains(&kind),
                _ => false,
            };
            if hit {
                let _ = s.tx.send(line.clone());
            }
        }
    }
}

impl Drop for FakeHerdr {
    fn drop(&mut self) {
        self.stop();
    }
}

fn snapshot(st: &State) -> Value {
    let agents: Vec<Value> = st
        .panes
        .iter()
        .filter_map(|(pane, a)| {
            a.as_ref().map(|a| {
                json!({
                    "pane_id": pane, "workspace_id": "w1", "tab_id": "w1:t1",
                    "terminal_id": "t", "focused": false, "revision": 0,
                    "agent": a.agent, "agent_status": a.status, "cwd": "/src/deskhand",
                })
            })
        })
        .collect();
    json!({
        "type": "session_snapshot",
        "snapshot": {
            "version": "0.9.1", "protocol": 22,
            "workspaces": [{"workspace_id": "w1", "label": "deskhand", "number": 1,
                "focused": true, "pane_count": st.panes.len(), "tab_count": 1,
                "active_tab_id": "w1:t1", "agent_status": "unknown"}],
            "tabs": [], "panes": [], "layouts": [], "agents": agents,
        }
    })
}

async fn serve(stream: UnixStream, state: Arc<Mutex<State>>) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Ok(Some(line)) = lines.next_line().await else {
        return;
    };
    let req: Value = serde_json::from_str(&line).unwrap_or_default();
    let id = req["id"].clone();
    let method = req["method"].as_str().unwrap_or_default().to_owned();
    let answer = |result: Result<Value, (&str, String)>| {
        let v = match result {
            Ok(r) => json!({"id": id, "result": r}),
            Err((code, message)) => json!({"id": id, "error": {"code": code, "message": message}}),
        };
        format!("{v}\n")
    };
    let mut rx = None;
    let reply = {
        let mut st = state.lock().unwrap();
        st.requests.push(method.clone());
        match method.as_str() {
            "ping" => answer(Ok(
                json!({"type": "pong", "version": "0.9.1", "protocol": 22}),
            )),
            "session.snapshot" => answer(Ok(snapshot(&st))),
            "events.subscribe" => match subscribe(&mut st, &req["params"]["subscriptions"]) {
                Ok(r) => {
                    rx = Some(r);
                    answer(Ok(json!({"type": "subscription_started"})))
                }
                Err(e) => answer(Err(e)),
            },
            other => answer(Err((
                "invalid_request",
                format!("unknown variant `{other}`"),
            ))),
        }
    };
    if write.write_all(reply.as_bytes()).await.is_err() {
        return;
    }
    let Some(mut rx) = rx else { return };
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(line) => {
                    if write.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                        return;
                    }
                }
                None => return,
            },
            // A second request (or EOF) ends the subscription, as in herdr.
            _ = lines.next_line() => return,
        }
    }
}

fn subscribe(
    st: &mut State,
    subs: &Value,
) -> Result<mpsc::UnboundedReceiver<String>, (&'static str, String)> {
    let subs = subs.as_array().cloned().unwrap_or_default();
    let mut panes = Vec::new();
    let mut types = Vec::new();
    for s in &subs {
        let ty = s["type"].as_str().unwrap_or_default().to_owned();
        if ty == "pane.agent_status_changed" {
            let Some(p) = s["pane_id"].as_str() else {
                return Err(("invalid_request", "missing field `pane_id`".into()));
            };
            if !st.panes.contains_key(p) {
                return Err(("pane_not_found", format!("pane {p} not found")));
            }
            panes.push(p.to_owned());
        } else {
            types.push(ty);
        }
    }
    if !panes.is_empty() && std::mem::take(&mut st.fail_next_status_subscribe) {
        return Err(("pane_not_found", "pane gone".into()));
    }
    let (tx, rx) = mpsc::unbounded_channel();
    if panes.is_empty() {
        st.subs.push(Sub {
            panes: None,
            types,
            tx,
        });
    } else {
        // Status subscriptions also get their panes' pane_closed.
        st.subs.push(Sub {
            panes: Some(panes),
            types: Vec::new(),
            tx,
        });
    }
    Ok(rx)
}
