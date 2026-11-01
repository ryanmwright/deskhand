//! `in_call` capability: a derived binary sensor that is on while the
//! microphone has been in use by a real app for at least 10 s, or a camera
//! is in use. Attributes `microphone` and `camera` say which.
//!
//! The 10 s delay skips short recordings (a voice message, a level check);
//! the 5 s release keeps it on through a brief mute or reconnect.

use std::time::Duration;

use deskhand_core::{Attributes, Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use serde_json::json;
use tokio::time::Instant;

use crate::derived::{Debounced, Inputs, sleep_until_opt, truthy};

pub const IN_CALL: EntityId = EntityId::from_static("in_call");
const MIC_IN_USE: EntityId = EntityId::from_static("mic_in_use");
const CAMERA_IN_USE: EntityId = EntityId::from_static("camera_in_use");

pub const MIC_ON_AFTER: Duration = Duration::from_secs(10);
pub const MIC_OFF_AFTER: Duration = Duration::from_secs(5);

/// Derives `in_call` from `mic_in_use` and `camera_in_use`.
#[derive(Debug, Default)]
pub struct InCall;

impl InCall {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Integration for InCall {
    fn id(&self) -> &'static str {
        "in_call"
    }

    fn capability(&self) -> &'static str {
        "in_call"
    }

    fn description(&self) -> &'static str {
        "In a call: microphone in use for 10 s or camera in use (derived from audio and camera)"
    }

    async fn probe(&self) -> Probe {
        Probe::available()
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let sensor = ctx.binary_sensor(
            EntitySpec::binary_sensor(IN_CALL, "In call", None).icon("mdi:phone-in-talk"),
        );
        let mut inputs = Inputs::new(&ctx, &[MIC_IN_USE, CAMERA_IN_USE]);
        let mut mic = Debounced::new(MIC_ON_AFTER, MIC_OFF_AFTER);
        let mut last: Option<(bool, bool)> = None;
        let cancel = ctx.cancelled();
        let mut ready = false;
        loop {
            let now = Instant::now();
            let s = inputs.current();
            let mic_on = mic.update(truthy(&s[0]), now);
            let camera_on = truthy(&s[1]);
            if last != Some((mic_on, camera_on)) {
                last = Some((mic_on, camera_on));
                let mut attrs = Attributes::new();
                attrs.insert("microphone".into(), json!(mic_on));
                attrs.insert("camera".into(), json!(camera_on));
                sensor.set_attributes(attrs);
                sensor.set(mic_on || camera_on);
            }
            if !ready {
                ctx.ready();
                ready = true;
            }
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                changed = inputs.changed() => {
                    if changed.is_none() {
                        return Ok(());
                    }
                }
                () = sleep_until_opt(mic.deadline()) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::testing::FakeTransport;
    use deskhand_core::{Hub, HubEvent, HubOptions, IntegrationConfig, State};
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn in_call_states(events: &[HubEvent]) -> Vec<State> {
        events
            .iter()
            .filter_map(|e| match e {
                HubEvent::State { id, state, .. } if *id == IN_CALL => Some(state.clone()),
                HubEvent::Snapshot(s) => s.states.get(&IN_CALL).cloned(),
                _ => None,
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn mic_needs_ten_seconds_camera_is_immediate() {
        let (hub, handle, link) = Hub::new(HubOptions::default());
        tokio::spawn(hub.run());
        let mut t = FakeTransport::new(link);
        let source = handle.ctx(IntegrationConfig::new("source"), CancellationToken::new());
        let mic = source.binary_sensor(EntitySpec::binary_sensor(MIC_IN_USE, "Mic", None));
        let cam = source.binary_sensor(EntitySpec::binary_sensor(CAMERA_IN_USE, "Cam", None));
        mic.set(false);
        cam.set(false);
        source.ready();
        let ctx = handle.ctx(IntegrationConfig::new("in_call"), CancellationToken::new());
        tokio::spawn(async move { InCall::new().run(ctx).await });
        handle.startup_complete();
        t.connect();
        let s = Duration::from_secs;
        tokio::time::sleep(s(1)).await;
        assert_eq!(
            in_call_states(&t.drain().await).last(),
            Some(&State::Bool(false))
        );

        // A 5 s voice message does not count.
        mic.set(true);
        tokio::time::sleep(s(5)).await;
        mic.set(false);
        tokio::time::sleep(s(20)).await;
        assert!(in_call_states(&t.drain().await).is_empty());

        // A call: on after 10 s, survives a 3 s mute, off 5 s after it ends.
        mic.set(true);
        tokio::time::sleep(s(9)).await;
        assert!(in_call_states(&t.drain().await).is_empty());
        tokio::time::sleep(s(2)).await;
        assert_eq!(in_call_states(&t.drain().await), vec![State::Bool(true)]);
        mic.set(false);
        tokio::time::sleep(s(3)).await;
        mic.set(true);
        tokio::time::sleep(s(10)).await;
        assert!(in_call_states(&t.drain().await).is_empty());
        mic.set(false);
        tokio::time::sleep(s(6)).await;
        assert_eq!(in_call_states(&t.drain().await), vec![State::Bool(false)]);

        // The camera counts right away.
        cam.set(true);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(in_call_states(&t.drain().await), vec![State::Bool(true)]);
    }
}
