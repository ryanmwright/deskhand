//! Hub behaviour tests. All run on paused tokio time, so timeouts are exact
//! and instant.

use std::time::Duration;

use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::config::IntegrationConfig;
use crate::ctx::Ctx;
use crate::entity::{Attributes, CommandValue, EntityId, EntityKind, EntitySpec, State};
use crate::hub::{Hub, HubHandle, HubOptions, Lifecycle};
use crate::testing::FakeTransport;
use crate::transport::{HubEvent, Snapshot, TransportEvent};

fn setup() -> (HubHandle, FakeTransport, tokio::task::JoinHandle<()>) {
    let (hub, handle, link) = Hub::new(HubOptions::default());
    let task = tokio::spawn(hub.run());
    (handle, FakeTransport::new(link), task)
}

fn ctx(hub: &HubHandle, capability: &str) -> Ctx {
    hub.ctx(IntegrationConfig::new(capability), CancellationToken::new())
}

fn id(s: &'static str) -> EntityId {
    EntityId::from_static(s)
}

fn sensor_spec(s: &'static str) -> EntitySpec {
    EntitySpec::sensor(id(s), s)
}

/// Starts one integration, connects, releases the barrier and drains the snapshot.
async fn live(hub: &HubHandle, t: &mut FakeTransport, capability: &str) -> Ctx {
    hub.integration_started(capability);
    let c = ctx(hub, capability);
    c.ready();
    hub.startup_complete();
    t.connect();
    let evs = t.drain().await;
    assert!(
        snapshots(&evs).len() == 1,
        "expected one snapshot, got {evs:?}"
    );
    c
}

fn snapshots(evs: &[HubEvent]) -> Vec<&Snapshot> {
    evs.iter()
        .filter_map(|e| match e {
            HubEvent::Snapshot(s) => Some(s),
            _ => None,
        })
        .collect()
}

fn states(evs: &[HubEvent]) -> Vec<(String, State)> {
    evs.iter()
        .filter_map(|e| match e {
            HubEvent::State { id, state, .. } => Some((id.to_string(), state.clone())),
            _ => None,
        })
        .collect()
}

fn discovery_ids(evs: &[HubEvent]) -> Vec<Vec<String>> {
    evs.iter()
        .filter_map(|e| match e {
            HubEvent::Discovery { entities, .. } => {
                Some(entities.iter().map(|d| d.spec.id.to_string()).collect())
            }
            _ => None,
        })
        .collect()
}

fn entity_ids(s: &Snapshot) -> Vec<String> {
    s.entities.iter().map(|d| d.spec.id.to_string()).collect()
}

#[tokio::test(start_paused = true)]
async fn barrier_waits_for_ready() {
    let (hub, mut t, _) = setup();
    hub.integration_started("a");
    let c = ctx(&hub, "a");
    let s = c.sensor(sensor_spec("a_one"));
    s.set(1);
    hub.startup_complete();
    t.connect();
    assert!(snapshots(&t.drain().await).is_empty());

    c.ready();
    let evs = t.drain().await;
    let snaps = snapshots(&evs);
    assert_eq!(snaps.len(), 1);
    assert_eq!(entity_ids(snaps[0]), vec!["a_one"]);
    assert_eq!(snaps[0].states[&id("a_one")], State::Int(1));
    assert_eq!(snaps[0].availability.get("a"), Some(&true));
}

#[tokio::test(start_paused = true)]
async fn barrier_times_out_after_startup_complete() {
    let (hub, mut t, _) = setup();
    hub.integration_started("slow");
    hub.startup_complete();
    t.connect();
    sleep(Duration::from_millis(4900)).await;
    assert!(snapshots(&t.drain().await).is_empty());
    sleep(Duration::from_millis(200)).await;
    assert_eq!(snapshots(&t.drain().await).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn barrier_has_an_absolute_upper_bound() {
    // No supervisor ever says startup is complete.
    let (hub, mut t, _) = setup();
    hub.integration_started("x");
    t.connect();
    sleep(Duration::from_secs(14)).await;
    assert!(snapshots(&t.drain().await).is_empty());
    sleep(Duration::from_secs(2)).await;
    assert_eq!(snapshots(&t.drain().await).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn stopped_integration_does_not_block_the_barrier() {
    let (hub, mut t, _) = setup();
    hub.integration_started("a");
    hub.integration_started("b");
    ctx(&hub, "a").ready();
    hub.integration_stopped("b");
    hub.startup_complete();
    t.connect();
    let evs = t.drain().await;
    assert_eq!(snapshots(&evs).len(), 1);
    assert_eq!(snapshots(&evs)[0].availability.get("b"), Some(&false));
}

#[tokio::test(start_paused = true)]
async fn snapshot_on_connect_has_latest_state() {
    let (hub, mut t, _) = setup();
    hub.integration_started("a");
    let c = ctx(&hub, "a");
    let s = c.sensor(sensor_spec("a_one"));
    c.ready();
    hub.startup_complete();
    s.set(1);
    s.set(2);
    let mut attrs = Attributes::new();
    attrs.insert("k".into(), "v".into());
    s.set_attributes(attrs.clone());
    assert!(
        t.drain().await.is_empty(),
        "nothing is sent while disconnected"
    );

    t.connect();
    let evs = t.drain().await;
    let snap = snapshots(&evs)[0];
    assert_eq!(snap.states[&id("a_one")], State::Int(2));
    assert_eq!(snap.attributes[&id("a_one")], attrs);
    assert!(states(&evs).is_empty());
}

#[tokio::test(start_paused = true)]
async fn unchanged_states_and_attributes_are_deduped() {
    let (hub, mut t, _) = setup();
    let c = live(&hub, &mut t, "a").await;
    let s = c.sensor(sensor_spec("a_one"));
    s.set(1);
    s.set(1);
    s.set(2);
    s.set(2);
    let mut attrs = Attributes::new();
    attrs.insert("k".into(), 1.into());
    s.set_attributes(attrs.clone());
    s.set_attributes(attrs);
    let evs = t.drain().await;
    assert_eq!(
        states(&evs),
        vec![
            ("a_one".into(), State::Int(1)),
            ("a_one".into(), State::Int(2))
        ]
    );
    let attr_events = evs
        .iter()
        .filter(|e| matches!(e, HubEvent::Attributes { .. }))
        .count();
    assert_eq!(attr_events, 1);
}

#[tokio::test(start_paused = true)]
async fn discovery_changes_are_batched() {
    let (hub, mut t, _) = setup();
    let c = live(&hub, &mut t, "a").await;
    let _one = c.sensor(sensor_spec("a_one"));
    sleep(Duration::from_millis(500)).await;
    let _two = c.sensor(sensor_spec("a_two"));
    assert!(discovery_ids(&t.drain().await).is_empty());
    sleep(Duration::from_millis(600)).await;
    let evs = t.drain().await;
    assert_eq!(discovery_ids(&evs), vec![vec!["a_one", "a_two"]]);
}

#[tokio::test(start_paused = true)]
async fn dropped_entities_are_removed_after_grace_period() {
    let (hub, mut t, _) = setup();
    let c = live(&hub, &mut t, "a").await;
    let keep = c.sensor(sensor_spec("a_keep"));
    let gone = c.sensor(sensor_spec("a_gone"));
    sleep(Duration::from_secs(2)).await;
    t.drain().await;

    drop(gone);
    sleep(Duration::from_secs(29)).await;
    assert!(discovery_ids(&t.drain().await).is_empty());
    sleep(Duration::from_secs(2)).await;
    assert_eq!(discovery_ids(&t.drain().await), vec![vec!["a_keep"]]);
    drop(keep);
}

#[tokio::test(start_paused = true)]
async fn reregistering_within_grace_period_cancels_removal() {
    let (hub, mut t, _) = setup();
    let c = live(&hub, &mut t, "a").await;
    let s = c.sensor(sensor_spec("a_one"));
    s.set(5);
    sleep(Duration::from_secs(2)).await;
    t.drain().await;

    drop(s); // integration crashed
    sleep(Duration::from_secs(10)).await;
    let s = c.sensor(sensor_spec("a_one")); // restarted
    sleep(Duration::from_secs(60)).await;
    let evs = t.drain().await;
    assert!(discovery_ids(&evs).is_empty(), "no churn: {evs:?}");
    s.set(5);
    assert!(
        states(&t.drain().await).is_empty(),
        "state survived the restart"
    );
}

#[tokio::test(start_paused = true)]
async fn unregister_from_an_older_registration_is_ignored() {
    let (hub, mut t, _) = setup();
    let c = live(&hub, &mut t, "a").await;
    let old = c.sensor(sensor_spec("a_one"));
    let _new = c.sensor(sensor_spec("a_one"));
    sleep(Duration::from_secs(2)).await;
    t.drain().await;
    drop(old);
    sleep(Duration::from_secs(60)).await;
    assert!(discovery_ids(&t.drain().await).is_empty());
}

#[tokio::test(start_paused = true)]
async fn pause_stops_publishing_and_unpause_resyncs() {
    let (hub, mut t, _) = setup();
    let c = live(&hub, &mut t, "a").await;
    let s = c.sensor(sensor_spec("a_one"));
    s.set(1);
    t.drain().await;

    hub.set_paused(true);
    s.set(2);
    let evs = t.drain().await;
    assert_eq!(evs.first(), Some(&HubEvent::Pause(true)));
    assert!(states(&evs).is_empty());

    hub.set_paused(false);
    let evs = t.drain().await;
    assert_eq!(evs[0], HubEvent::Pause(false));
    assert_eq!(snapshots(&evs)[0].states[&id("a_one")], State::Int(2));
}

#[tokio::test(start_paused = true)]
async fn reconnecting_while_paused_reports_pause() {
    let (hub, mut t, _) = setup();
    let _c = live(&hub, &mut t, "a").await;
    hub.set_paused(true);
    t.disconnect();
    t.drain().await;
    t.connect();
    let evs = t.drain().await;
    assert_eq!(evs, vec![HubEvent::Pause(true)]);
}

#[tokio::test(start_paused = true)]
async fn switch_commands_are_typed_and_checked() {
    let (hub, mut t, _) = setup();
    let c = live(&hub, &mut t, "a").await;
    let (_sw, mut cmds) = c.switch(EntitySpec::switch(id("a_sw"), "Sw"));
    t.settle().await;

    t.command("a_sw", CommandValue::Bool(true));
    t.command("a_sw", CommandValue::Text("nope".into()));
    t.settle().await;
    assert_eq!(cmds.try_recv(), Some(true));
    assert_eq!(cmds.try_recv(), None, "wrong-typed command dropped");
}

#[tokio::test(start_paused = true)]
async fn commands_are_ignored_while_paused() {
    let (hub, mut t, _) = setup();
    let c = live(&hub, &mut t, "a").await;
    let mut press = c.button(EntitySpec::button(id("a_btn"), "Btn"));
    hub.set_paused(true);
    t.settle().await;
    t.command("a_btn", CommandValue::Press);
    t.settle().await;
    assert_eq!(press.try_recv(), None);

    hub.set_paused(false);
    t.settle().await;
    t.command("a_btn", CommandValue::Press);
    t.settle().await;
    assert_eq!(press.try_recv(), Some(()));
}

#[tokio::test(start_paused = true)]
async fn button_survives_with_only_its_command_stream() {
    let (hub, mut t, _) = setup();
    let c = live(&hub, &mut t, "a").await;
    let mut press = c.button(EntitySpec::button(id("a_btn"), "Btn"));
    sleep(Duration::from_secs(60)).await;
    t.drain().await;
    t.command("a_btn", CommandValue::Press);
    t.settle().await;
    assert_eq!(press.try_recv(), Some(()));
}

#[tokio::test(start_paused = true)]
async fn select_options_update_keeps_the_command_stream() {
    let (hub, mut t, _) = setup();
    let c = live(&hub, &mut t, "a").await;
    let (sel, mut cmds) = c.select(EntitySpec::select(id("a_sel"), "Sel", vec!["x".into()]));
    sleep(Duration::from_secs(2)).await;
    t.drain().await;

    sel.set_options(vec!["x".into(), "y".into()]);
    sel.set_options(vec!["x".into(), "y".into()]); // unchanged: no-op
    sleep(Duration::from_secs(2)).await;
    let evs = t.drain().await;
    let discoveries: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            HubEvent::Discovery { entities, .. } => Some(entities.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(discoveries.len(), 1);
    assert_eq!(
        discoveries[0][0].spec.kind,
        EntityKind::Select {
            options: vec!["x".into(), "y".into()]
        }
    );

    t.command("a_sel", CommandValue::Text("y".into()));
    t.settle().await;
    assert_eq!(cmds.try_recv().as_deref(), Some("y"));
}

#[tokio::test(start_paused = true)]
async fn json_commands_are_routed_and_listed() {
    let (hub, mut t, _) = setup();
    let c = live(&hub, &mut t, "notifications").await;
    let mut cmds = c.json_command(id("notifications_send"));
    sleep(Duration::from_secs(2)).await;
    let evs = t.drain().await;
    assert!(evs.iter().any(|e| matches!(
        e,
        HubEvent::Discovery { json_commands, .. } if json_commands == &vec![id("notifications_send")]
    )));

    t.command(
        "notifications_send",
        CommandValue::Json(serde_json::json!({"title": "hi"})),
    );
    t.command("notifications_send", CommandValue::Press);
    t.command("nobody_home", CommandValue::Press);
    t.settle().await;
    assert_eq!(cmds.try_recv(), Some(serde_json::json!({"title": "hi"})));
    assert_eq!(cmds.try_recv(), None);
}

#[tokio::test(start_paused = true)]
async fn availability_follows_ready_and_set_available() {
    let (hub, mut t, _) = setup();
    let c = live(&hub, &mut t, "a").await;
    c.set_available(false);
    c.set_available(false);
    c.set_available(true);
    let evs = t.drain().await;
    let avail: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            HubEvent::Availability {
                capability,
                available,
            } => Some((capability.to_string(), *available)),
            _ => None,
        })
        .collect();
    assert_eq!(avail, vec![("a".into(), false), ("a".into(), true)]);
}

#[tokio::test(start_paused = true)]
async fn min_interval_coalesces_updates() {
    let (hub, mut t, _) = setup();
    let c = live(&hub, &mut t, "a").await;
    let s = c.sensor(sensor_spec("a_cpu").min_interval(Duration::from_secs(10)));
    s.set(1);
    t.settle().await;
    s.set(2);
    s.set(3);
    sleep(Duration::from_secs(5)).await;
    assert_eq!(
        states(&t.drain().await),
        vec![("a_cpu".into(), State::Int(1))]
    );
    sleep(Duration::from_secs(6)).await;
    assert_eq!(
        states(&t.drain().await),
        vec![("a_cpu".into(), State::Int(3))]
    );
}

#[tokio::test(start_paused = true)]
async fn observe_sees_other_integrations_states() {
    let (hub, mut t, _) = setup();
    let c = live(&hub, &mut t, "camera").await;
    let derived = ctx(&hub, "derived");
    let mut rx = derived.observe(&id("camera_in_use"));
    assert_eq!(*rx.borrow(), State::Unknown);

    let cam = c.binary_sensor(EntitySpec::binary_sensor(
        id("camera_in_use"),
        "Camera",
        None,
    ));
    cam.set(true);
    rx.changed().await.unwrap();
    assert_eq!(*rx.borrow_and_update(), State::Bool(true));

    // A late observer sees the current value immediately.
    let late = derived.observe(&id("camera_in_use"));
    assert_eq!(*late.borrow(), State::Bool(true));
}

#[tokio::test(start_paused = true)]
async fn events_are_only_sent_while_live() {
    let (hub, mut t, _) = setup();
    let c = live(&hub, &mut t, "a").await;
    let ev = c.event(EntitySpec::event(id("a_ev"), "Ev", vec!["pressed".into()]));
    t.disconnect();
    t.settle().await; // the transport channel is separate from the handle channel
    ev.fire("pressed", Attributes::new());
    assert!(t.drain().await.is_empty());

    t.connect();
    t.drain().await;
    ev.fire("pressed", Attributes::new());
    let evs = t.drain().await;
    assert!(matches!(&evs[..], [HubEvent::Event { event_type, .. }] if event_type == "pressed"));
}

#[tokio::test(start_paused = true)]
async fn ha_restart_triggers_snapshot() {
    let (hub, mut t, _) = setup();
    let _c = live(&hub, &mut t, "a").await;
    t.send(TransportEvent::HaOnline);
    assert_eq!(snapshots(&t.drain().await).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn states_while_disconnected_arrive_in_next_snapshot() {
    let (hub, mut t, _) = setup();
    let c = live(&hub, &mut t, "a").await;
    let s = c.sensor(sensor_spec("a_one"));
    t.disconnect();
    t.settle().await;
    s.set(7);
    assert!(t.drain().await.is_empty());
    t.connect();
    let evs = t.drain().await;
    assert_eq!(snapshots(&evs)[0].states[&id("a_one")], State::Int(7));
}

#[tokio::test(start_paused = true)]
async fn sleep_and_resume_are_forwarded_and_broadcast() {
    let (hub, mut t, _) = setup();
    let c = live(&hub, &mut t, "power").await;
    let mut lifecycle = hub.lifecycle();
    c.notify_sleep();
    c.notify_resume();
    let evs = t.drain().await;
    assert_eq!(evs, vec![HubEvent::Sleep, HubEvent::Resume]);
    assert_eq!(lifecycle.recv().await.unwrap(), Lifecycle::Sleep);
    assert_eq!(lifecycle.recv().await.unwrap(), Lifecycle::Resume);
}

#[tokio::test(start_paused = true)]
async fn shutdown_notifies_transport_and_stops_hub() {
    let (hub, mut t, task) = setup();
    let _c = live(&hub, &mut t, "a").await;
    hub.shutdown();
    assert_eq!(t.drain().await, vec![HubEvent::Shutdown]);
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("hub task ends")
        .unwrap();
}
