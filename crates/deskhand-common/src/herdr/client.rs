//! herdr's socket API, the part Deskhand uses: newline-delimited JSON over a
//! Unix socket. A plain request gets one response, then the server closes
//! the connection; a subscription connection takes one `events.subscribe`
//! and then streams events (one per line) until either side closes it.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// How long a request or a subscription's acknowledgement may take.
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("herdr did not answer within {TIMEOUT:?}")]
    Timeout,
    #[error("herdr closed the connection")]
    Closed,
    #[error("herdr: {code}: {message}")]
    Api { code: String, message: String },
    #[error("unexpected answer from herdr: {0}")]
    Protocol(String),
}

impl ClientError {
    /// The API error code (`pane_not_found`, `invalid_request`, ...).
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } => Some(code),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct Response {
    result: Option<Value>,
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct ApiError {
    code: String,
    message: String,
}

/// Connects, sends `method` and returns the first response line's `result`.
/// The connection stays open for a subscription's events.
async fn call(
    socket: &Path,
    method: &str,
    params: Value,
) -> Result<(Value, Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf), ClientError> {
    let exchange = async {
        let stream = UnixStream::connect(socket).await?;
        let (read, mut write) = stream.into_split();
        let mut line = serde_json::to_vec(&json!({
            "id": format!("deskhand:{method}"),
            "method": method,
            "params": params,
        }))
        .expect("request serializes");
        line.push(b'\n');
        write.write_all(&line).await?;
        let mut lines = BufReader::new(read).lines();
        let answer = lines.next_line().await?.ok_or(ClientError::Closed)?;
        Ok::<_, ClientError>((answer, lines, write))
    };
    let (answer, lines, write) = tokio::time::timeout(TIMEOUT, exchange)
        .await
        .map_err(|_| ClientError::Timeout)??;
    let response: Response =
        serde_json::from_str(&answer).map_err(|e| ClientError::Protocol(e.to_string()))?;
    match (response.result, response.error) {
        (_, Some(e)) => Err(ClientError::Api {
            code: e.code,
            message: e.message,
        }),
        (Some(result), None) => Ok((result, lines, write)),
        (None, None) => Err(ClientError::Protocol(answer)),
    }
}

/// One request on its own connection.
pub async fn request(socket: &Path, method: &str, params: Value) -> Result<Value, ClientError> {
    call(socket, method, params)
        .await
        .map(|(result, ..)| result)
}

/// A live subscription: event lines until herdr closes it. Dropping it
/// unsubscribes.
pub struct Subscription {
    lines: Lines<BufReader<OwnedReadHalf>>,
    /// Kept open: herdr may treat a closed write side as a hang-up.
    _write: OwnedWriteHalf,
}

impl Subscription {
    /// The next event, or `None` once herdr closed the stream (herdr went
    /// away, or dropped a slow subscriber).
    pub async fn next(&mut self) -> Option<Event> {
        loop {
            match self.lines.next_line().await {
                Ok(Some(line)) => match serde_json::from_str::<Event>(&line) {
                    Ok(ev) => return Some(ev),
                    Err(e) => tracing::debug!(error = %e, %line, "unreadable herdr event"),
                },
                Ok(None) | Err(_) => return None,
            }
        }
    }
}

/// Subscribes to `subscriptions` (each `{"type": ..., ...}`).
pub async fn subscribe(
    socket: &Path,
    subscriptions: Vec<Value>,
) -> Result<Subscription, ClientError> {
    let (result, lines, write) = call(
        socket,
        "events.subscribe",
        json!({ "subscriptions": subscriptions }),
    )
    .await?;
    if result.get("type").and_then(Value::as_str) != Some("subscription_started") {
        return Err(ClientError::Protocol(result.to_string()));
    }
    Ok(Subscription {
        lines,
        _write: write,
    })
}

/// A pushed event: `{"event": "pane_closed", "data": {...}}`.
#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    pub event: String,
    #[serde(default)]
    pub data: Value,
}

/// An agent's status as herdr reports it. `done` means idle and not yet
/// seen by an attached client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    #[serde(other)]
    Unknown,
}

/// The parts of `session.snapshot` Deskhand uses.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub agents: Vec<AgentInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentInfo {
    pub pane_id: String,
    #[serde(default)]
    pub workspace_id: String,
    pub agent: Option<String>,
    pub display_agent: Option<String>,
    pub name: Option<String>,
    pub agent_status: AgentStatus,
    pub cwd: Option<String>,
}

/// `ping`: the server's version.
pub async fn ping(socket: &Path) -> Result<String, ClientError> {
    let result = request(socket, "ping", json!({})).await?;
    Ok(result
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned())
}

/// `session.snapshot`.
pub async fn snapshot(socket: &Path) -> Result<Snapshot, ClientError> {
    let result = request(socket, "session.snapshot", json!({})).await?;
    let snapshot = result
        .get("snapshot")
        .cloned()
        .ok_or_else(|| ClientError::Protocol(result.to_string()))?;
    serde_json::from_value(snapshot).map_err(|e| ClientError::Protocol(e.to_string()))
}
