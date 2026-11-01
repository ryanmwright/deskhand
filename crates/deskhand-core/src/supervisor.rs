//! The supervisor decides which integrations run.
//!
//! Integrations are grouped by capability in registration (priority) order.
//! For each enabled capability the supervisor probes candidates in order and
//! runs the first available one. Failed integrations restart with exponential
//! backoff; capabilities without a provider are re-probed periodically and
//! after resume (a service may have appeared). Status is kept for `doctor`.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, Enabled, IntegrationConfig};
use crate::hub::{HubHandle, Lifecycle};
use crate::integration::{Integration, IntegrationError, Probe};

/// Timing knobs.
#[derive(Debug, Clone)]
pub struct SupervisorOptions {
    pub restart_min: Duration,
    pub restart_max: Duration,
    /// A run longer than this resets the backoff.
    pub stable_after: Duration,
    pub reprobe_interval: Duration,
    pub probe_timeout: Duration,
    /// How long to wait for integrations to stop at shutdown before aborting them.
    pub shutdown_timeout: Duration,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            restart_min: Duration::from_secs(1),
            restart_max: Duration::from_secs(60),
            stable_after: Duration::from_secs(60),
            reprobe_interval: Duration::from_secs(300),
            probe_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(5),
        }
    }
}

/// What a capability is doing, for `doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityState {
    Disabled,
    /// No candidate is available (see the candidates' probe results).
    Unavailable,
    Running {
        provider: &'static str,
    },
    /// Crashed; restart scheduled.
    Failed {
        provider: &'static str,
        error: String,
        restarts: u32,
    },
    /// Returned `Ok` on its own; not restarted.
    Stopped {
        provider: &'static str,
    },
    ConfigError(String),
}

/// One candidate and its last probe result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateStatus {
    pub id: &'static str,
    pub description: &'static str,
    pub probe: Option<Probe>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityStatus {
    pub capability: &'static str,
    pub enabled: Enabled,
    pub state: CapabilityState,
    pub candidates: Vec<CandidateStatus>,
}

/// Shared, read-only view of the supervisor's status.
#[derive(Debug, Clone, Default)]
pub struct StatusHandle(Arc<RwLock<BTreeMap<&'static str, CapabilityStatus>>>);

impl StatusHandle {
    pub fn snapshot(&self) -> Vec<CapabilityStatus> {
        self.0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    pub fn get(&self, capability: &str) -> Option<CapabilityStatus> {
        self.0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(capability)
            .cloned()
    }

    fn update(&self, capability: &'static str, f: impl FnOnce(&mut CapabilityStatus)) {
        let mut map = self.0.write().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = map.get_mut(capability) {
            f(s);
        }
    }
}

struct Group {
    capability: &'static str,
    candidates: Vec<Arc<dyn Integration>>,
    config: Option<IntegrationConfig>,
    running: Option<Running>,
    restarts: u32,
    restart_at: Option<Instant>,
    /// The last provider returned `Ok` by itself; leave it alone.
    finished: bool,
}

struct Running {
    provider: Arc<dyn Integration>,
    started_at: Instant,
    cancel: CancellationToken,
}

type Outcome = (
    usize,
    Result<Result<(), IntegrationError>, tokio::task::JoinError>,
);

/// Runs integrations. Create, then `supervisor.run().await` until `cancel` fires.
pub struct Supervisor {
    hub: HubHandle,
    opts: SupervisorOptions,
    groups: Vec<Group>,
    status: StatusHandle,
    cancel: CancellationToken,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("groups", &self.groups.len())
            .finish_non_exhaustive()
    }
}

impl Supervisor {
    /// `candidates` are in priority order; the first available one per capability wins.
    pub fn new(
        hub: HubHandle,
        config: &Config,
        candidates: Vec<Box<dyn Integration>>,
        opts: SupervisorOptions,
        cancel: CancellationToken,
    ) -> Self {
        let mut groups: Vec<Group> = Vec::new();
        for c in candidates {
            let c: Arc<dyn Integration> = Arc::from(c);
            match groups.iter_mut().find(|g| g.capability == c.capability()) {
                Some(g) => g.candidates.push(c),
                None => groups.push(Group {
                    capability: c.capability(),
                    candidates: vec![c],
                    config: None,
                    running: None,
                    restarts: 0,
                    restart_at: None,
                    finished: false,
                }),
            }
        }

        let status = StatusHandle::default();
        {
            let mut map = status.0.write().unwrap_or_else(|e| e.into_inner());
            for g in &mut groups {
                let candidates = g
                    .candidates
                    .iter()
                    .map(|c| CandidateStatus {
                        id: c.id(),
                        description: c.description(),
                        probe: None,
                    })
                    .collect();
                let (enabled, state) = match config.integration(g.capability) {
                    Ok(cfg) => {
                        let enabled = cfg.enabled;
                        g.config = Some(cfg);
                        let state = if enabled == Enabled::Off {
                            CapabilityState::Disabled
                        } else {
                            CapabilityState::Unavailable
                        };
                        (enabled, state)
                    }
                    Err(e) => {
                        tracing::error!(capability = g.capability, error = %e, "invalid integration config; not starting it");
                        (Enabled::Auto, CapabilityState::ConfigError(e.to_string()))
                    }
                };
                map.insert(
                    g.capability,
                    CapabilityStatus {
                        capability: g.capability,
                        enabled,
                        state,
                        candidates,
                    },
                );
            }
        }

        Self {
            hub,
            opts,
            groups,
            status,
            cancel,
        }
    }

    pub fn status(&self) -> StatusHandle {
        self.status.clone()
    }

    fn startable(g: &Group) -> bool {
        g.config.as_ref().is_some_and(|c| c.enabled != Enabled::Off)
            && g.running.is_none()
            && g.restart_at.is_none()
            && !g.finished
    }

    /// Probes candidates of group `idx` and starts the first available one.
    async fn probe_and_start(&mut self, idx: usize, tasks: &mut JoinSet<Outcome>) {
        let capability = self.groups[idx].capability;
        let candidates = self.groups[idx].candidates.clone();
        let mut winner = None;
        for (i, c) in candidates.iter().enumerate() {
            let probe = match tokio::time::timeout(self.opts.probe_timeout, c.probe()).await {
                Ok(p) => p,
                Err(_) => Probe::unavailable("probe timed out"),
            };
            tracing::debug!(capability, provider = c.id(), ?probe, "probed");
            let available = probe.is_available();
            self.status
                .update(capability, |s| s.candidates[i].probe = Some(probe));
            if available {
                winner = Some(c.clone());
                break;
            }
        }
        match winner {
            Some(provider) => self.start(idx, provider, tasks),
            None => {
                let enabled = self.groups[idx].config.as_ref().map(|c| c.enabled);
                if enabled == Some(Enabled::On) {
                    tracing::warn!(
                        capability,
                        "enabled in config but no provider is available here"
                    );
                } else {
                    tracing::debug!(capability, "no provider available");
                }
                self.status
                    .update(capability, |s| s.state = CapabilityState::Unavailable);
            }
        }
    }

    fn start(&mut self, idx: usize, provider: Arc<dyn Integration>, tasks: &mut JoinSet<Outcome>) {
        let g = &mut self.groups[idx];
        let cancel = self.cancel.child_token();
        let config = g.config.clone().expect("startable groups have a config");
        let ctx = self.hub.ctx(config, cancel.clone());
        self.hub.integration_started(g.capability);
        tracing::info!(
            capability = g.capability,
            provider = provider.id(),
            "starting integration"
        );
        let p = provider.clone();
        tasks.spawn(async move {
            // Run in a nested task so a panic becomes a JoinError we can report.
            // The guard aborts the nested task if this wrapper is aborted.
            let inner = tokio::spawn(async move { p.run(ctx).await });
            let guard = AbortOnDrop(inner.abort_handle());
            let res = inner.await;
            drop(guard);
            (idx, res)
        });
        let provider_id = provider.id();
        g.running = Some(Running {
            provider,
            started_at: Instant::now(),
            cancel,
        });
        self.status.update(g.capability, |s| {
            s.state = CapabilityState::Running {
                provider: provider_id,
            }
        });
    }

    fn on_outcome(&mut self, outcome: Outcome) {
        let (idx, res) = outcome;
        let opts = self.opts.clone();
        let cancelled = self.cancel.is_cancelled();
        let g = &mut self.groups[idx];
        let Some(running) = g.running.take() else {
            return;
        };
        self.hub.integration_stopped(g.capability);
        let provider = running.provider.id();
        let error = match res {
            Ok(Ok(())) => {
                if !cancelled {
                    tracing::info!(capability = g.capability, provider, "integration finished");
                    g.finished = true;
                    self.status.update(g.capability, |s| {
                        s.state = CapabilityState::Stopped { provider }
                    });
                }
                return;
            }
            Ok(Err(e)) => e.to_string(),
            Err(join) if join.is_panic() => "panicked".to_owned(),
            Err(join) => join.to_string(),
        };
        if cancelled {
            return;
        }
        if running.started_at.elapsed() >= opts.stable_after {
            g.restarts = 0;
        }
        g.restarts += 1;
        let delay = backoff(g.restarts, opts.restart_min, opts.restart_max);
        g.restart_at = Some(Instant::now() + delay);
        tracing::warn!(capability = g.capability, provider, %error, restart_in = ?delay, "integration failed");
        let restarts = g.restarts;
        self.status.update(g.capability, |s| {
            s.state = CapabilityState::Failed {
                provider,
                error,
                restarts,
            }
        });
    }

    /// Runs until the cancellation token fires, then stops all integrations.
    pub async fn run(mut self) {
        let mut tasks: JoinSet<Outcome> = JoinSet::new();
        let mut lifecycle = self.hub.lifecycle();

        for idx in 0..self.groups.len() {
            if Self::startable(&self.groups[idx]) {
                self.probe_and_start(idx, &mut tasks).await;
            }
        }
        self.hub.startup_complete();

        let mut next_reprobe = Instant::now() + self.opts.reprobe_interval;
        loop {
            let next_restart = self.groups.iter().filter_map(|g| g.restart_at).min();
            let wake = next_restart.map_or(next_reprobe, |r| r.min(next_reprobe));
            tokio::select! {
                () = self.cancel.cancelled() => break,
                Some(outcome) = tasks.join_next() => {
                    match outcome {
                        Ok(outcome) => self.on_outcome(outcome),
                        Err(e) => tracing::error!(error = %e, "supervisor task failed"),
                    }
                }
                ev = lifecycle.recv() => {
                    if let Ok(Lifecycle::Resume) = ev {
                        tracing::debug!("resumed; re-probing unavailable capabilities");
                        self.reprobe(&mut tasks).await;
                    }
                }
                () = tokio::time::sleep_until(wake) => {
                    let now = Instant::now();
                    for idx in 0..self.groups.len() {
                        if self.groups[idx].restart_at.is_some_and(|t| t <= now) {
                            self.groups[idx].restart_at = None;
                            self.probe_and_start(idx, &mut tasks).await;
                        }
                    }
                    if now >= next_reprobe {
                        next_reprobe = now + self.opts.reprobe_interval;
                        self.reprobe(&mut tasks).await;
                    }
                }
            }
        }

        for g in &self.groups {
            if let Some(r) = &g.running {
                r.cancel.cancel();
            }
        }
        let drain = async { while tasks.join_next().await.is_some() {} };
        if tokio::time::timeout(self.opts.shutdown_timeout, drain)
            .await
            .is_err()
        {
            tracing::warn!("integrations did not stop in time; aborting them");
            tasks.abort_all();
        }
    }

    async fn reprobe(&mut self, tasks: &mut JoinSet<Outcome>) {
        for idx in 0..self.groups.len() {
            if Self::startable(&self.groups[idx]) {
                self.probe_and_start(idx, tasks).await;
            }
        }
    }
}

/// Aborts a task when dropped.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `min * 2^(n-1)`, capped at `max`.
fn backoff(attempt: u32, min: Duration, max: Duration) -> Duration {
    let factor = 2u32.saturating_pow(attempt.saturating_sub(1));
    min.saturating_mul(factor).min(max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let (min, max) = (Duration::from_secs(1), Duration::from_secs(60));
        let got: Vec<u64> = (1..=8).map(|n| backoff(n, min, max).as_secs()).collect();
        assert_eq!(got, vec![1, 2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(backoff(100, min, max), max);
    }
}
