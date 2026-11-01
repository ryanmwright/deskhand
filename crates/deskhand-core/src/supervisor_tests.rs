//! Supervisor tests with scripted fake integrations, on paused tokio time.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::ctx::Ctx;
use crate::hub::{Hub, HubHandle, HubOptions};
use crate::integration::{Integration, IntegrationError, Probe};
use crate::supervisor::{CapabilityState, StatusHandle, Supervisor, SupervisorOptions};

#[derive(Clone, Copy)]
enum Behavior {
    /// Ready, then run until cancelled.
    Forever,
    /// Return `Ok` immediately.
    ReturnOk,
    /// Fail the first `n` runs, then behave like `Forever`.
    FailFirst(u32),
    Panic,
}

struct Fake {
    id: &'static str,
    capability: &'static str,
    probe: Arc<Mutex<Probe>>,
    behavior: Behavior,
    runs: Arc<AtomicU32>,
}

#[derive(Clone)]
struct FakeControl {
    probe: Arc<Mutex<Probe>>,
    runs: Arc<AtomicU32>,
}

impl FakeControl {
    fn runs(&self) -> u32 {
        self.runs.load(Ordering::SeqCst)
    }

    fn set_probe(&self, probe: Probe) {
        *self.probe.lock().unwrap() = probe;
    }
}

fn fake(
    id: &'static str,
    capability: &'static str,
    available: bool,
    behavior: Behavior,
) -> (Box<dyn Integration>, FakeControl) {
    let probe = Arc::new(Mutex::new(if available {
        Probe::available()
    } else {
        Probe::unavailable("not here")
    }));
    let runs = Arc::new(AtomicU32::new(0));
    let control = FakeControl {
        probe: probe.clone(),
        runs: runs.clone(),
    };
    let integration = Fake {
        id,
        capability,
        probe,
        behavior,
        runs,
    };
    (Box::new(integration), control)
}

#[async_trait::async_trait]
impl Integration for Fake {
    fn id(&self) -> &'static str {
        self.id
    }

    fn capability(&self) -> &'static str {
        self.capability
    }

    fn description(&self) -> &'static str {
        "fake"
    }

    async fn probe(&self) -> Probe {
        self.probe.lock().unwrap().clone()
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let run = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
        match self.behavior {
            Behavior::ReturnOk => return Ok(()),
            Behavior::Panic => panic!("fake integration panicked"),
            Behavior::FailFirst(n) if run <= n => return Err(IntegrationError::msg("boom")),
            Behavior::FailFirst(_) | Behavior::Forever => {}
        }
        ctx.ready();
        ctx.cancelled().cancelled().await;
        Ok(())
    }
}

struct Running {
    status: StatusHandle,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    hub: HubHandle,
}

fn start(config: &str, candidates: Vec<Box<dyn Integration>>) -> Running {
    let (hub, handle, link) = Hub::new(HubOptions::default());
    tokio::spawn(hub.run());
    // Keep the transport side alive for the whole test.
    std::mem::forget(link);
    let config = Config::from_toml_str(config, None).unwrap();
    let cancel = CancellationToken::new();
    let sup = Supervisor::new(
        handle.clone(),
        &config,
        candidates,
        SupervisorOptions::default(),
        cancel.clone(),
    );
    let status = sup.status();
    let task = tokio::spawn(sup.run());
    Running {
        status,
        cancel,
        task,
        hub: handle,
    }
}

fn state(r: &Running, capability: &str) -> CapabilityState {
    r.status.get(capability).unwrap().state
}

async fn tick() {
    sleep(Duration::from_millis(10)).await;
}

#[tokio::test(start_paused = true)]
async fn first_available_candidate_wins() {
    let (a, ca) = fake("idle.a", "idle", false, Behavior::Forever);
    let (b, cb) = fake("idle.b", "idle", true, Behavior::Forever);
    let (c, cc) = fake("idle.c", "idle", true, Behavior::Forever);
    let r = start("", vec![a, b, c]);
    tick().await;

    assert_eq!((ca.runs(), cb.runs(), cc.runs()), (0, 1, 0));
    assert_eq!(
        state(&r, "idle"),
        CapabilityState::Running { provider: "idle.b" }
    );
    let status = r.status.get("idle").unwrap();
    assert_eq!(
        status.candidates[0].probe,
        Some(Probe::unavailable("not here"))
    );
    assert_eq!(status.candidates[1].probe, Some(Probe::available()));
    assert_eq!(
        status.candidates[2].probe, None,
        "later candidates are not probed"
    );
}

#[tokio::test(start_paused = true)]
async fn disabled_capability_is_never_probed() {
    let (a, ca) = fake("cam.a", "camera", true, Behavior::Forever);
    let r = start("[integrations.camera]\nenabled = false", vec![a]);
    sleep(Duration::from_secs(600)).await;
    assert_eq!(ca.runs(), 0);
    assert_eq!(state(&r, "camera"), CapabilityState::Disabled);
    assert_eq!(r.status.get("camera").unwrap().candidates[0].probe, None);
}

#[tokio::test(start_paused = true)]
async fn invalid_config_is_reported_and_not_started() {
    let (a, ca) = fake("cam.a", "camera", true, Behavior::Forever);
    let r = start("[integrations.camera]\nenabled = \"sometimes\"", vec![a]);
    tick().await;
    assert_eq!(ca.runs(), 0);
    assert!(matches!(
        state(&r, "camera"),
        CapabilityState::ConfigError(_)
    ));
}

#[tokio::test(start_paused = true)]
async fn failures_restart_with_exponential_backoff() {
    let (a, ca) = fake("x.a", "x", true, Behavior::FailFirst(3));
    let r = start("", vec![a]);
    tick().await;
    assert_eq!(ca.runs(), 1);
    assert!(matches!(
        state(&r, "x"),
        CapabilityState::Failed { restarts: 1, .. }
    ));

    sleep(Duration::from_millis(1000)).await; // +1s
    assert_eq!(ca.runs(), 2);
    sleep(Duration::from_millis(1500)).await; // backoff now 2s: not yet
    assert_eq!(ca.runs(), 2);
    sleep(Duration::from_millis(600)).await;
    assert_eq!(ca.runs(), 3);
    sleep(Duration::from_secs(4)).await; // backoff 4s
    assert_eq!(ca.runs(), 4);
    assert_eq!(state(&r, "x"), CapabilityState::Running { provider: "x.a" });
}

#[tokio::test(start_paused = true)]
async fn returning_ok_is_not_restarted() {
    let (a, ca) = fake("x.a", "x", true, Behavior::ReturnOk);
    let r = start("", vec![a]);
    sleep(Duration::from_secs(900)).await;
    assert_eq!(ca.runs(), 1);
    assert_eq!(state(&r, "x"), CapabilityState::Stopped { provider: "x.a" });
}

#[tokio::test(start_paused = true)]
async fn panics_count_as_failures() {
    let (a, ca) = fake("x.a", "x", true, Behavior::Panic);
    let r = start("", vec![a]);
    tick().await;
    match state(&r, "x") {
        CapabilityState::Failed { error, .. } => assert_eq!(error, "panicked"),
        other => panic!("unexpected state {other:?}"),
    }
    sleep(Duration::from_secs(2)).await;
    assert!(ca.runs() >= 2, "restarted after panic");
}

#[tokio::test(start_paused = true)]
async fn unavailable_capabilities_are_reprobed_periodically() {
    let (a, ca) = fake("x.a", "x", false, Behavior::Forever);
    let r = start("", vec![a]);
    tick().await;
    assert_eq!(state(&r, "x"), CapabilityState::Unavailable);

    ca.set_probe(Probe::available());
    sleep(Duration::from_secs(290)).await;
    assert_eq!(ca.runs(), 0);
    sleep(Duration::from_secs(20)).await;
    assert_eq!(ca.runs(), 1);
    assert_eq!(state(&r, "x"), CapabilityState::Running { provider: "x.a" });
}

#[tokio::test(start_paused = true)]
async fn resume_triggers_reprobe() {
    let (a, ca) = fake("x.a", "x", false, Behavior::Forever);
    let r = start("", vec![a]);
    tick().await;
    ca.set_probe(Probe::available());
    r.hub.resume();
    tick().await;
    assert_eq!(ca.runs(), 1);
}

#[tokio::test(start_paused = true)]
async fn cancellation_stops_integrations_and_returns() {
    let (a, ca) = fake("x.a", "x", true, Behavior::Forever);
    let (b, cb) = fake("y.a", "y", true, Behavior::Forever);
    let r = start("", vec![a, b]);
    tick().await;
    assert_eq!((ca.runs(), cb.runs()), (1, 1));
    r.cancel.cancel();
    let Running { status, task, .. } = r;
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("supervisor returns promptly")
        .unwrap();
    // Integrations stopped because of shutdown are not marked Stopped/Failed.
    assert_eq!(
        status.get("x").unwrap().state,
        CapabilityState::Running { provider: "x.a" }
    );
}

/// Ignores cancellation, to exercise the shutdown timeout.
struct Stubborn {
    dropped: Arc<AtomicU32>,
}

struct DropFlag(Arc<AtomicU32>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Integration for Stubborn {
    fn id(&self) -> &'static str {
        "stubborn"
    }

    fn capability(&self) -> &'static str {
        "stubborn"
    }

    fn description(&self) -> &'static str {
        "never stops"
    }

    async fn probe(&self) -> Probe {
        Probe::available()
    }

    async fn run(&self, _ctx: Ctx) -> Result<(), IntegrationError> {
        let _flag = DropFlag(self.dropped.clone());
        std::future::pending::<()>().await;
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn integrations_ignoring_cancellation_are_aborted_at_shutdown() {
    let dropped = Arc::new(AtomicU32::new(0));
    let r = start(
        "",
        vec![Box::new(Stubborn {
            dropped: dropped.clone(),
        })],
    );
    tick().await;
    r.cancel.cancel();
    tokio::time::timeout(Duration::from_secs(10), r.task)
        .await
        .expect("supervisor gives up after shutdown_timeout")
        .unwrap();
    tick().await;
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        1,
        "the run future was aborted"
    );
}
