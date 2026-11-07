//! The integration against [`FakeHerdr`], in real time (Unix sockets).

use std::time::Duration;

use deskhand_core::State;
use deskhand_core::testing::{TestRig, rig, wait_event, wait_state};
use serde_json::json;

use super::fake::FakeHerdr;
use super::*;

fn timing() -> Timing {
    Timing {
        retry_min: Duration::from_millis(50),
        retry_max: Duration::from_millis(200),
        quiet: Duration::from_millis(20),
        max_settle: Duration::from_millis(100),
        resync: Duration::from_secs(60),
    }
}

fn start(dir: &tempfile::TempDir) -> (TestRig, tokio_util::sync::CancellationToken) {
    let socket = dir.path().join("herdr.sock");
    rig(Herdr::with_socket(socket, timing()), "herdr")
}

async fn counts(r: &mut TestRig, agents: u64, working: u64, blocked: u64) {
    wait_state(r, &HERDR_AGENTS, State::from(agents)).await;
    wait_state(r, &HERDR_WORKING, State::from(working)).await;
    wait_state(r, &HERDR_BLOCKED, State::from(blocked)).await;
}

/// Lets in-flight events arrive, then returns the types fired since.
async fn fired(r: &mut TestRig) -> Vec<String> {
    r.settle(Duration::from_millis(300)).await;
    r.take_events().into_iter().map(|(_, t, _)| t).collect()
}

#[tokio::test]
async fn seed_fires_nothing_then_transitions_fire() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeHerdr::start(&dir.path().join("herdr.sock"));
    fake.report("w1:p1", "claude", "working");
    let (mut r, _cancel) = start(&dir);

    wait_state(&mut r, &HERDR_RUNNING, State::Bool(true)).await;
    counts(&mut r, 1, 1, 0).await;
    assert!(fired(&mut r).await.is_empty(), "the seed fires nothing");
    assert_eq!(
        r.attributes(&HERDR_WORKING).unwrap()["agents"],
        json!(["claude @ deskhand"])
    );

    fake.report("w1:p1", "claude", "blocked");
    let a = wait_event(&mut r, &HERDR_AGENT, "blocked").await;
    assert_eq!(a["previous"], json!("working"));
    assert_eq!(a["workspace"], json!("deskhand"));
    assert_eq!(a["pane"], json!("w1:p1"));
    counts(&mut r, 1, 0, 1).await;

    fake.report("w1:p1", "claude", "working");
    wait_event(&mut r, &HERDR_AGENT, "working").await;
    // herdr's own `done` is idle: one done, not two.
    fake.report("w1:p1", "claude", "done");
    wait_event(&mut r, &HERDR_AGENT, "done").await;
    fake.report("w1:p1", "claude", "idle");
    counts(&mut r, 1, 0, 0).await;
    assert!(fired(&mut r).await.is_empty());
}

#[tokio::test]
async fn new_agents_are_followed() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeHerdr::start(&dir.path().join("herdr.sock"));
    let (mut r, _cancel) = start(&dir);
    wait_state(&mut r, &HERDR_RUNNING, State::Bool(true)).await;
    counts(&mut r, 0, 0, 0).await;

    // Detected through the watch and the snapshot...
    fake.report("w1:p2", "codex", "working");
    let a = wait_event(&mut r, &HERDR_AGENT, "working").await;
    assert_eq!(a["agent"], json!("codex"));
    counts(&mut r, 1, 1, 0).await;
    // ...then followed through a status subscription for its pane.
    fake.report("w1:p2", "codex", "blocked");
    wait_event(&mut r, &HERDR_AGENT, "blocked").await;

    // A pane created later works the same way.
    fake.add_pane("w1:p3");
    fake.report("w1:p3", "claude", "blocked");
    wait_event(&mut r, &HERDR_AGENT, "blocked").await;
    counts(&mut r, 2, 0, 2).await;
    fake.report("w1:p3", "claude", "idle");
    wait_event(&mut r, &HERDR_AGENT, "done").await;
}

#[tokio::test]
async fn released_and_closed_agents_exit() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeHerdr::start(&dir.path().join("herdr.sock"));
    fake.report("w1:p1", "claude", "working");
    fake.report("w1:p2", "codex", "idle");
    let (mut r, _cancel) = start(&dir);
    counts(&mut r, 2, 1, 0).await;

    fake.release("w1:p1");
    let a = wait_event(&mut r, &HERDR_AGENT, "exited").await;
    assert_eq!(a["pane"], json!("w1:p1"));
    assert_eq!(a["previous"], json!("working"));
    counts(&mut r, 1, 0, 0).await;

    fake.close_pane("w1:p2");
    let a = wait_event(&mut r, &HERDR_AGENT, "exited").await;
    assert_eq!(a["agent"], json!("codex"));
    counts(&mut r, 0, 0, 0).await;
    assert!(fired(&mut r).await.is_empty());
}

#[tokio::test]
async fn waits_for_herdr_and_reconnects() {
    let dir = tempfile::tempdir().unwrap();
    // Not running yet: Deskhand is up and says so.
    let (mut r, _cancel) = start(&dir);
    wait_state(&mut r, &HERDR_RUNNING, State::Bool(false)).await;
    counts(&mut r, 0, 0, 0).await;

    let mut fake = FakeHerdr::start(&dir.path().join("herdr.sock"));
    fake.report("w1:p1", "claude", "working");
    wait_state(&mut r, &HERDR_RUNNING, State::Bool(true)).await;
    counts(&mut r, 1, 1, 0).await;
    assert_eq!(
        r.attributes(&HERDR_RUNNING).unwrap()["version"],
        json!("0.9.1")
    );

    fake.stop();
    wait_state(&mut r, &HERDR_RUNNING, State::Bool(false)).await;
    counts(&mut r, 0, 0, 0).await;
    fake.restart();
    wait_state(&mut r, &HERDR_RUNNING, State::Bool(true)).await;
    counts(&mut r, 1, 1, 0).await;
    // Neither going away nor coming back is an agent event.
    assert!(fired(&mut r).await.is_empty());

    fake.report("w1:p1", "claude", "blocked");
    wait_event(&mut r, &HERDR_AGENT, "blocked").await;
}

#[tokio::test]
async fn a_pane_vanishing_before_the_subscription_is_retried() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeHerdr::start(&dir.path().join("herdr.sock"));
    fake.report("w1:p1", "claude", "working");
    fake.fail_next_status_subscribe();
    let (mut r, _cancel) = start(&dir);
    counts(&mut r, 1, 1, 0).await;
    fake.report("w1:p1", "claude", "blocked");
    wait_event(&mut r, &HERDR_AGENT, "blocked").await;
    assert!(
        fake.requests("events.subscribe") >= 3,
        "watch, failed and retried status"
    );
}

#[test]
fn socket_resolution() {
    let env = |vars: &'static [(&'static str, &'static str)]| {
        move |k: &str| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    };
    let home = env(&[("HOME", "/home/u")]);
    let none = HerdrOptions::default();
    assert_eq!(
        socket_path(&none, home),
        Some("/home/u/.config/herdr/herdr.sock".into())
    );
    let session = HerdrOptions {
        session: Some("work".into()),
        socket: None,
    };
    assert_eq!(
        socket_path(
            &session,
            env(&[("XDG_CONFIG_HOME", "/cfg"), ("HOME", "/home/u")])
        ),
        Some("/cfg/herdr/sessions/work/herdr.sock".into())
    );
    assert_eq!(
        socket_path(&none, env(&[("HOME", "/h"), ("HERDR_SESSION", "x")])),
        Some("/h/.config/herdr/sessions/x/herdr.sock".into())
    );
    assert_eq!(
        socket_path(
            &none,
            env(&[("HOME", "/h"), ("HERDR_SOCKET_PATH", "/run/h.sock")])
        ),
        Some("/run/h.sock".into())
    );
    // The config wins over the environment.
    assert_eq!(
        socket_path(
            &session,
            env(&[("HOME", "/h"), ("HERDR_SOCKET_PATH", "/run/h.sock")])
        ),
        Some("/h/.config/herdr/sessions/work/herdr.sock".into())
    );
    assert_eq!(socket_path(&none, env(&[])), None);
}
