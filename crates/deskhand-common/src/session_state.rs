//! `session_state` capability: one enum sensor summarising the session,
//! derived from other integrations' entities.
//!
//! | state | when |
//! |---|---|
//! | `off` | Deskhand is shutting down |
//! | `sleeping` | between "about to sleep" and resume |
//! | `locked` | `screen_locked` is on |
//! | `idle` | `user_active` is off |
//! | `active` | otherwise |
//!
//! The entity is always available, so `sleeping` and `off` stay visible in
//! Home Assistant while Deskhand itself is offline.

use deskhand_core::hub::Lifecycle;
use deskhand_core::{Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe, State};

use crate::derived::{Inputs, truthy};

pub const SESSION_STATE: EntityId = EntityId::from_static("session_state");
pub const OPTIONS: &[&str] = &["active", "idle", "locked", "sleeping", "off"];

const SCREEN_LOCKED: EntityId = EntityId::from_static("screen_locked");
const USER_ACTIVE: EntityId = EntityId::from_static("user_active");

/// The state for the given inputs.
pub fn compute(sleeping: bool, screen_locked: &State, user_active: &State) -> &'static str {
    if sleeping {
        "sleeping"
    } else if truthy(screen_locked) {
        "locked"
    } else if matches!(user_active, State::Bool(false)) {
        // Unknown counts as active: no idle provider means no idle information.
        "idle"
    } else {
        "active"
    }
}

/// Derives `session_state` from `screen_locked`, `user_active` and sleep.
#[derive(Debug, Default)]
pub struct SessionState;

impl SessionState {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Integration for SessionState {
    fn id(&self) -> &'static str {
        "session_state"
    }

    fn capability(&self) -> &'static str {
        "session_state"
    }

    fn description(&self) -> &'static str {
        "active / idle / locked / sleeping / off, derived from the lock, idle and power integrations"
    }

    async fn probe(&self) -> Probe {
        Probe::available()
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let sensor = ctx.sensor(
            EntitySpec::sensor(SESSION_STATE, "Session state")
                .enum_options(OPTIONS.iter().map(|s| s.to_string()).collect())
                .icon("mdi:monitor-account")
                .always_available(),
        );
        let mut inputs = Inputs::new(&ctx, &[SCREEN_LOCKED, USER_ACTIVE]);
        let mut lifecycle = ctx.lifecycle();
        let mut sleeping = false;
        let publish = |sleeping: bool, inputs: &Inputs| {
            let s = inputs.current();
            sensor.set(compute(sleeping, &s[0], &s[1]));
        };
        publish(sleeping, &inputs);
        ctx.ready();

        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    // Deskhand is stopping: this is published before the transport goes offline.
                    sensor.set("off");
                    return Ok(());
                }
                changed = inputs.changed() => {
                    if changed.is_none() {
                        return Ok(());
                    }
                }
                ev = lifecycle.recv() => match ev {
                    Ok(Lifecycle::PrepareSleep | Lifecycle::Sleep) => sleeping = true,
                    Ok(Lifecycle::Resume) => sleeping = false,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return Ok(()),
                },
            }
            publish(sleeping, &inputs);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use deskhand_core::testing::FakeTransport;
    use deskhand_core::{Hub, HubEvent, HubOptions, IntegrationConfig};
    use tokio_util::sync::CancellationToken;

    use super::*;

    #[test]
    fn priorities() {
        let on = State::Bool(true);
        let off = State::Bool(false);
        let unknown = State::Unknown;
        assert_eq!(compute(false, &off, &on), "active");
        assert_eq!(compute(false, &unknown, &unknown), "active");
        assert_eq!(compute(false, &off, &off), "idle");
        assert_eq!(compute(false, &on, &off), "locked");
        assert_eq!(compute(true, &on, &on), "sleeping");
    }

    async fn next_session_state(t: &mut FakeTransport) -> State {
        loop {
            match t.next_within(Duration::from_secs(5)).await.expect("event") {
                HubEvent::State { id, state, .. } if id == SESSION_STATE => return state,
                HubEvent::Snapshot(s) => {
                    if let Some(st) = s.states.get(&SESSION_STATE) {
                        return st.clone();
                    }
                }
                _ => {}
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn follows_inputs_sleep_and_shutdown() {
        let (hub, handle, link) = Hub::new(HubOptions::default());
        tokio::spawn(hub.run());
        let mut t = FakeTransport::new(link);
        let others = handle.ctx(IntegrationConfig::new("others"), CancellationToken::new());
        let locked = others.binary_sensor(EntitySpec::binary_sensor(SCREEN_LOCKED, "L", None));
        let active = others.binary_sensor(EntitySpec::binary_sensor(USER_ACTIVE, "A", None));
        locked.set(false);
        active.set(true);

        let cancel = CancellationToken::new();
        let ctx = handle.ctx(IntegrationConfig::new("session_state"), cancel.clone());
        handle.integration_started("session_state");
        let task = tokio::spawn(async move { SessionState::new().run(ctx).await });
        handle.startup_complete();
        t.connect();
        assert_eq!(next_session_state(&mut t).await, "active".into());

        active.set(false);
        assert_eq!(next_session_state(&mut t).await, "idle".into());
        locked.set(true);
        assert_eq!(next_session_state(&mut t).await, "locked".into());

        others.notify_prepare_sleep();
        assert_eq!(next_session_state(&mut t).await, "sleeping".into());
        others.notify_sleep();
        locked.set(false); // unlocking during sleep does not wake the state
        others.notify_resume();
        assert_eq!(next_session_state(&mut t).await, "idle".into());

        cancel.cancel();
        assert_eq!(next_session_state(&mut t).await, "off".into());
        task.await.unwrap().unwrap();
    }
}
