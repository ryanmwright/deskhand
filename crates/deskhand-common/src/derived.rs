//! Building blocks for derived sensors: entities computed from other
//! entities' states (for example `session_state` from idle, lock and sleep,
//! or `in_call` from microphone and camera use).
//!
//! - [`Inputs`] watches a set of entities (from any integration) and wakes up
//!   whenever one of them changes.
//! - [`Debounced`] is a boolean that turns on or off only after its input has
//!   held steady for a while ("mic in use for at least 10 s").
//!
//! A derived integration typically loops over `inputs.changed()` and the
//! debouncer's `deadline()`, recomputes, and sets its own handle. The hub
//! dedupes unchanged states, so recomputing more often than needed is fine.

use std::task::Poll;
use std::time::Duration;

use deskhand_core::{Ctx, EntityId, State};
use tokio::sync::watch;
use tokio::time::Instant;

/// Latest states of a fixed list of entities.
#[derive(Debug)]
pub struct Inputs {
    ids: Vec<EntityId>,
    rx: Vec<watch::Receiver<State>>,
}

impl Inputs {
    pub fn new(ctx: &Ctx, ids: &[EntityId]) -> Self {
        Self {
            ids: ids.to_vec(),
            rx: ids.iter().map(|id| ctx.observe(id)).collect(),
        }
    }

    pub fn ids(&self) -> &[EntityId] {
        &self.ids
    }

    /// Current states, in the order the ids were given.
    pub fn current(&self) -> Vec<State> {
        self.rx.iter().map(|r| r.borrow().clone()).collect()
    }

    /// The current state of one input.
    pub fn get(&self, id: &EntityId) -> Option<State> {
        let idx = self.ids.iter().position(|i| i == id)?;
        Some(self.rx[idx].borrow().clone())
    }

    /// Waits until any input changes, then returns all current states.
    /// Returns `None` once the hub is gone. With no inputs it never returns.
    pub async fn changed(&mut self) -> Option<Vec<State>> {
        if self.rx.is_empty() {
            std::future::pending::<()>().await;
        }
        let mut waiting: Vec<_> = self.rx.iter_mut().map(|r| Box::pin(r.changed())).collect();
        let res = std::future::poll_fn(|cx| {
            for f in &mut waiting {
                if let Poll::Ready(res) = f.as_mut().poll(cx) {
                    return Poll::Ready(res);
                }
            }
            Poll::Pending
        })
        .await;
        drop(waiting);
        res.ok()?;
        for r in &mut self.rx {
            r.mark_unchanged();
        }
        Some(self.current())
    }
}

/// Treats `Bool(true)`, `"on"` and non-zero numbers as true; everything else
/// (including unknown) as false.
pub fn truthy(state: &State) -> bool {
    match state {
        State::Bool(b) => *b,
        State::Int(i) => *i != 0,
        State::Float(f) => *f != 0.0,
        State::Text(t) => matches!(t.as_str(), "on" | "true" | "yes"),
        _ => false,
    }
}

/// A boolean that follows its input only after the input has been stable
/// for `on_after` (rising) or `off_after` (falling).
#[derive(Debug, Clone)]
pub struct Debounced {
    on_after: Duration,
    off_after: Duration,
    output: bool,
    /// Input value differing from `output`, and when it started.
    pending: Option<(bool, Instant)>,
}

impl Debounced {
    pub fn new(on_after: Duration, off_after: Duration) -> Self {
        Self {
            on_after,
            off_after,
            output: false,
            pending: None,
        }
    }

    pub fn output(&self) -> bool {
        self.output
    }

    /// Feeds the current input. Returns the (possibly updated) output.
    pub fn update(&mut self, input: bool, now: Instant) -> bool {
        if input == self.output {
            self.pending = None;
        } else if self.pending.is_none_or(|(v, _)| v != input) {
            self.pending = Some((input, now));
        }
        self.poll(now)
    }

    /// When the pending change takes effect, if any. Sleep until then and
    /// call [`Debounced::poll`].
    pub fn deadline(&self) -> Option<Instant> {
        self.pending
            .map(|(v, since)| since + if v { self.on_after } else { self.off_after })
    }

    /// Applies the pending change if its delay has passed. Returns the output.
    pub fn poll(&mut self, now: Instant) -> bool {
        if let (Some((v, _)), Some(deadline)) = (self.pending, self.deadline())
            && now >= deadline
        {
            self.output = v;
            self.pending = None;
        }
        self.output
    }
}

/// Sleeps until `deadline`, or forever if there is none (for `select!`).
pub async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::{EntitySpec, Hub, HubOptions, IntegrationConfig};
    use tokio_util::sync::CancellationToken;

    use super::*;

    #[test]
    fn debounce_delays_rising_and_falling_edges() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut d = Debounced::new(s(10), s(2));
        assert!(!d.update(true, t0));
        assert_eq!(d.deadline(), Some(t0 + s(10)));
        assert!(!d.poll(t0 + s(9)));
        // A blip resets the timer.
        assert!(!d.update(false, t0 + s(9)));
        assert_eq!(d.deadline(), None);
        assert!(!d.update(true, t0 + s(10)));
        assert!(d.poll(t0 + s(20)));
        // Falling edge after 2 s.
        assert!(d.update(false, t0 + s(21)));
        assert!(
            d.update(false, t0 + s(22)),
            "repeated input keeps the timer"
        );
        assert!(!d.poll(t0 + s(23)));
    }

    #[test]
    fn truthiness() {
        assert!(truthy(&State::Bool(true)));
        assert!(truthy(&"on".into()));
        assert!(truthy(&State::Int(2)));
        assert!(!truthy(&State::Unknown));
        assert!(!truthy(&"off".into()));
    }

    #[tokio::test(start_paused = true)]
    async fn inputs_follow_other_integrations() {
        let (hub, handle, _link) = Hub::new(HubOptions::default());
        tokio::spawn(hub.run());
        let source = handle.ctx(IntegrationConfig::new("source"), CancellationToken::new());
        let derived = handle.ctx(IntegrationConfig::new("derived"), CancellationToken::new());
        let a = EntityId::from_static("a");
        let b = EntityId::from_static("b");
        let mut inputs = Inputs::new(&derived, &[a.clone(), b.clone()]);
        assert_eq!(inputs.current(), vec![State::Unknown, State::Unknown]);

        let sa = source.binary_sensor(EntitySpec::binary_sensor(a.clone(), "A", None));
        let sb = source.sensor(EntitySpec::sensor(b.clone(), "B"));
        sa.set(true);
        let got = inputs.changed().await.unwrap();
        assert_eq!(got[0], State::Bool(true));
        sb.set("x");
        let got = inputs.changed().await.unwrap();
        assert_eq!(got, vec![State::Bool(true), "x".into()]);
        assert_eq!(inputs.get(&b), Some("x".into()));
    }
}
