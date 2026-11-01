//! Integration tests against a real mosquitto broker in a container.
//!
//! Run with `DESKHAND_IT=1 cargo test -p deskhand-mqtt --test broker`
//! (`just it`). Needs Docker or podman (the devShell points testcontainers at
//! the podman socket). Without `DESKHAND_IT` every test returns immediately.

use std::collections::BTreeSet;
use std::net::TcpListener;
use std::time::Duration;

use deskhand_core::entity::ImageSource;
use deskhand_core::{
    Ctx, DeviceInfo, EntityId, EntitySpec, Hub, HubHandle, HubOptions, IntegrationConfig, Transport,
};
use deskhand_mqtt::{AdminClient, MqttConfig, MqttTimings, MqttTransport, Topics};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde_json::Value;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const NODE: &str = "itnode";

fn enabled() -> bool {
    std::env::var_os("DESKHAND_IT").is_some()
}

macro_rules! require_it {
    () => {
        if !enabled() {
            eprintln!("skipped: set DESKHAND_IT=1 to run broker integration tests");
            return;
        }
    };
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Mosquitto with anonymous access on a fixed host port (so it survives restarts).
async fn broker() -> (ContainerAsync<GenericImage>, u16) {
    let port = free_port();
    let container = GenericImage::new("docker.io/library/eclipse-mosquitto", "2")
        .with_wait_for(WaitFor::message_on_either_std("running"))
        .with_mapped_port(port, 1883.tcp())
        .with_cmd(["mosquitto", "-c", "/mosquitto-no-auth.conf"])
        .start()
        .await
        .expect("start mosquitto container (is Docker/podman available?)");
    (container, port)
}

fn device() -> DeviceInfo {
    DeviceInfo {
        node_id: NODE.into(),
        name: "IT Desk".into(),
        manufacturer: "Deskhand".into(),
        model: "test".into(),
        sw_version: "0.0.0".into(),
        hw_version: None,
        connections: vec![],
    }
}

fn topics() -> Topics {
    Topics::new("deskhand", "homeassistant", NODE)
}

fn fast_timings() -> MqttTimings {
    MqttTimings {
        retained_read_timeout: Duration::from_millis(500),
        stub_delay: Duration::from_millis(300),
        birth_jitter: Duration::from_millis(100),
        reconnect_min: Duration::from_millis(200),
        reconnect_max: Duration::from_secs(1),
    }
}

/// A running hub + MQTT transport with one "test" capability.
struct Daemon {
    hub: HubHandle,
    ctx: Ctx,
    transport: JoinHandle<()>,
    _hub_task: JoinHandle<()>,
}

impl Daemon {
    fn start(port: u16, disabled: &[&str]) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "warn".into()),
            )
            .with_test_writer()
            .try_init();
        let config = MqttConfig {
            host: "127.0.0.1".into(),
            port: Some(port),
            ..MqttConfig::default()
        };
        let transport = MqttTransport::new(
            config,
            device(),
            disabled
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<_>>(),
        )
        .unwrap()
        .with_timings(fast_timings());
        let (hub, handle, link) = Hub::new(HubOptions::default());
        let hub_task = tokio::spawn(hub.run());
        let transport = tokio::spawn(async move {
            Box::new(transport).run(link).await.unwrap();
        });
        handle.integration_started("test");
        let ctx = handle.ctx(IntegrationConfig::new("test"), CancellationToken::new());
        Self {
            hub: handle,
            ctx,
            transport,
            _hub_task: hub_task,
        }
    }

    /// Marks the test integration ready, which releases the hub's startup barrier.
    fn ready(&self) {
        self.ctx.ready();
        self.hub.startup_complete();
    }

    async fn shutdown(self) {
        self.hub.shutdown();
        tokio::time::timeout(Duration::from_secs(5), self.transport)
            .await
            .expect("transport stops")
            .unwrap();
    }

    /// Simulates `kill -9`: the socket closes without a DISCONNECT.
    fn crash(self) {
        self.transport.abort();
        self._hub_task.abort();
    }
}

#[derive(Debug, Clone)]
struct Msg {
    topic: String,
    payload: Vec<u8>,
    retain: bool,
}

impl Msg {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.payload).into_owned()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.payload).expect("JSON payload")
    }
}

/// A plain MQTT client that records everything it sees and can publish.
struct Observer {
    client: AsyncClient,
    rx: mpsc::UnboundedReceiver<Msg>,
}

impl Observer {
    async fn connect(port: u16, filter: &str) -> Self {
        let mut opts = MqttOptions::new(format!("observer-{}", free_port()), "127.0.0.1", port);
        opts.set_max_packet_size(8 << 20, 8 << 20);
        let (client, mut eventloop) = AsyncClient::new(opts, 100);
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::Publish(p))) => {
                        let _ = tx.send(Msg {
                            topic: p.topic,
                            payload: p.payload.to_vec(),
                            retain: p.retain,
                        });
                    }
                    Ok(_) => {}
                    Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                }
            }
        });
        client.subscribe(filter, QoS::AtLeastOnce).await.unwrap();
        Self { client, rx }
    }

    /// Waits for a message on `topic` matching `pred`.
    async fn wait_for(&mut self, topic: &str, pred: impl Fn(&Msg) -> bool) -> Msg {
        let deadline = Duration::from_secs(10);
        tokio::time::timeout(deadline, async {
            loop {
                let m = self.rx.recv().await.expect("observer stream ended");
                if m.topic == topic && pred(&m) {
                    return m;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for a matching message on {topic}"))
    }

    /// Everything received within `window`.
    async fn collect(&mut self, window: Duration) -> Vec<Msg> {
        let mut out = Vec::new();
        let _ = tokio::time::timeout(window, async {
            while let Some(m) = self.rx.recv().await {
                out.push(m);
            }
        })
        .await;
        out
    }

    async fn publish(&self, topic: &str, payload: &str, retain: bool) {
        self.client
            .publish(topic, QoS::AtLeastOnce, retain, payload)
            .await
            .unwrap();
    }
}

fn components(doc: &Value) -> BTreeSet<String> {
    doc["components"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

fn id(s: &'static str) -> EntityId {
    EntityId::from_static(s)
}

#[tokio::test]
async fn publishes_discovery_states_and_availability() {
    require_it!();
    let (_broker, port) = broker().await;
    let t = topics();
    let mut obs = Observer::connect(port, "#").await;

    let d = Daemon::start(port, &[]);
    let cpu = d
        .ctx
        .sensor(EntitySpec::sensor(id("cpu_percent"), "CPU").unit("%"));
    cpu.set(12.5);
    d.ready();

    let msgs = obs.collect(Duration::from_secs(3)).await;
    let last = |topic: String| {
        msgs.iter()
            .rev()
            .find(|m| m.topic == topic)
            .cloned()
            .unwrap_or_else(|| panic!("nothing on {topic}"))
    };
    let doc = last(t.device_config()).json();
    assert_eq!(
        components(&doc),
        BTreeSet::from(["cpu_percent".into(), "deskhand_connected".into()])
    );
    assert_eq!(doc["device"]["identifiers"][0], "deskhand_itnode");
    assert_eq!(last(t.status()).text(), "online");
    assert_eq!(last(t.capability_status("test")).text(), "online");
    assert_eq!(last(t.state(&id("cpu_percent"))).text(), "12.5");
    // Live deliveries have the retain flag cleared; a late subscriber proves retention.
    let mut late = Observer::connect(port, &t.device_config()).await;
    assert!(late.wait_for(&t.device_config(), |_| true).await.retain);

    // Later updates flow through, and retained values are visible to late subscribers.
    cpu.set(50);
    obs.wait_for(&t.state(&id("cpu_percent")), |m| m.text() == "50")
        .await;
    let mut late = Observer::connect(port, &t.state(&id("cpu_percent"))).await;
    let m = late.wait_for(&t.state(&id("cpu_percent")), |_| true).await;
    assert!(m.retain && m.text() == "50");

    d.shutdown().await;
    obs.wait_for(&t.status(), |m| m.text() == "offline").await;
}

#[tokio::test]
async fn commands_are_routed_and_retained_commands_ignored() {
    require_it!();
    let (_broker, port) = broker().await;
    let t = topics();
    let obs = Observer::connect(port, "unused/#").await;
    // A stale retained command left on the broker must never fire.
    obs.publish(&t.command(&id("keep_awake")), "ON", true).await;

    let d = Daemon::start(port, &[]);
    let (_sw, mut cmds) = d
        .ctx
        .switch(EntitySpec::switch(id("keep_awake"), "Keep awake"));
    d.ready();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(cmds.try_recv(), None, "retained command ignored");

    obs.publish(&t.command(&id("keep_awake")), "OFF", false)
        .await;
    let got = tokio::time::timeout(Duration::from_secs(5), cmds.recv())
        .await
        .unwrap();
    assert_eq!(got, Some(false));

    obs.publish(&t.command(&id("keep_awake")), "garbage", false)
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(cmds.try_recv(), None, "invalid payload dropped");
    d.shutdown().await;
}

#[tokio::test]
async fn crash_publishes_last_will() {
    require_it!();
    let (_broker, port) = broker().await;
    let t = topics();
    let mut obs = Observer::connect(port, &t.status()).await;
    let d = Daemon::start(port, &[]);
    d.ready();
    obs.wait_for(&t.status(), |m| m.text() == "online").await;

    let start = std::time::Instant::now();
    d.crash();
    obs.wait_for(&t.status(), |m| m.text() == "offline").await;
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "LWT was immediate"
    );
}

#[tokio::test]
async fn stale_entities_are_removed_across_restarts() {
    require_it!();
    let (_broker, port) = broker().await;
    let t = topics();

    // First run: two entities.
    let d = Daemon::start(port, &[]);
    let _a = d.ctx.sensor(EntitySpec::sensor(id("test_keep"), "Keep"));
    let _b = d.ctx.sensor(EntitySpec::sensor(id("test_old_name"), "Old"));
    d.ready();
    let mut obs = Observer::connect(port, &t.device_config()).await;
    obs.wait_for(&t.device_config(), |m| {
        components(&m.json()).contains("test_old_name")
    })
    .await;
    d.shutdown().await;

    // Second run: one entity was renamed.
    let d = Daemon::start(port, &[]);
    let _a = d.ctx.sensor(EntitySpec::sensor(id("test_keep"), "Keep"));
    let _c = d.ctx.sensor(EntitySpec::sensor(id("test_new_name"), "New"));
    d.ready();
    let stub = obs
        .wait_for(&t.device_config(), |m| {
            m.json()["components"]["test_old_name"] == serde_json::json!({"platform": "sensor"})
        })
        .await;
    assert!(components(&stub.json()).contains("test_new_name"));
    let final_doc = obs
        .wait_for(&t.device_config(), |m| {
            !components(&m.json()).contains("test_old_name")
        })
        .await;
    assert_eq!(
        components(&final_doc.json()),
        BTreeSet::from([
            "deskhand_connected".into(),
            "test_keep".into(),
            "test_new_name".into()
        ])
    );
    d.shutdown().await;
}

#[tokio::test]
async fn disabled_capabilities_are_removed_and_missing_ones_kept() {
    require_it!();
    let (_broker, port) = broker().await;
    let t = topics();

    // First run publishes entities for "test" (stands in for any capability).
    let d = Daemon::start(port, &[]);
    let _a = d.ctx.sensor(EntitySpec::sensor(id("test_one"), "One"));
    d.ready();
    let mut obs = Observer::connect(port, "#").await;
    obs.wait_for(&t.device_config(), |m| {
        components(&m.json()).contains("test_one")
    })
    .await;
    d.shutdown().await;

    // Second run: "test" is enabled but does not start; its entity is kept and offline.
    let (hub, handle, link) = Hub::new(HubOptions::default());
    tokio::spawn(hub.run());
    let config = MqttConfig {
        host: "127.0.0.1".into(),
        port: Some(port),
        ..MqttConfig::default()
    };
    let transport = MqttTransport::new(config.clone(), device(), BTreeSet::new())
        .unwrap()
        .with_timings(fast_timings());
    let task = tokio::spawn(async move { Box::new(transport).run(link).await.unwrap() });
    handle.startup_complete();
    obs.wait_for(&t.capability_status("test"), |m| m.text() == "offline")
        .await;
    let msgs = obs.collect(Duration::from_secs(2)).await;
    let last_doc = msgs
        .iter()
        .rev()
        .find(|m| m.topic == t.device_config())
        .map(Msg::json);
    if let Some(doc) = last_doc {
        assert!(
            components(&doc).contains("test_one"),
            "kept while unavailable"
        );
    }
    handle.shutdown();
    task.await.unwrap();

    // Third run: "test" disabled; its entity is removed.
    let d = Daemon::start(port, &["test"]);
    d.hub.integration_stopped("test");
    d.hub.startup_complete();
    obs.wait_for(&t.device_config(), |m| {
        !components(&m.json()).contains("test_one")
            && m.json()["components"].get("test_one").is_none()
    })
    .await;
    d.shutdown().await;
}

#[tokio::test]
async fn home_assistant_restart_triggers_republish() {
    require_it!();
    let (_broker, port) = broker().await;
    let t = topics();
    let d = Daemon::start(port, &[]);
    let s = d.ctx.sensor(EntitySpec::sensor(id("test_one"), "One"));
    s.set(1);
    d.ready();
    let mut obs = Observer::connect(port, &t.state(&id("test_one"))).await;
    obs.wait_for(&t.state(&id("test_one")), |_| true).await; // retained copy

    obs.publish(&t.ha_status(), "online", false).await;
    let m = obs.wait_for(&t.state(&id("test_one")), |_| true).await;
    assert!(!m.retain, "a fresh publish, not the retained copy");
    d.shutdown().await;
}

#[tokio::test]
async fn reconnects_and_resyncs_after_broker_restart() {
    require_it!();
    let (broker, port) = broker().await;
    let t = topics();
    let d = Daemon::start(port, &[]);
    let s = d.ctx.sensor(EntitySpec::sensor(id("test_one"), "One"));
    s.set(1);
    d.ready();
    let mut obs = Observer::connect(port, &t.status()).await;
    obs.wait_for(&t.status(), |m| m.text() == "online").await;

    broker.stop().await.unwrap();
    s.set(2); // changed while the broker is down
    broker.start().await.unwrap();

    // The broker has no persistence, so everything must be republished.
    // Order is not guaranteed, so collect a window and check the latest values.
    let mut obs = Observer::connect(port, "#").await;
    let msgs = obs.collect(Duration::from_secs(4)).await;
    let last = |topic: String| {
        msgs.iter()
            .rev()
            .find(|m| m.topic == topic)
            .map(Msg::text)
            .unwrap_or_else(|| panic!("nothing on {topic}"))
    };
    assert_eq!(last(t.state(&id("test_one"))), "2");
    assert_eq!(last(t.status()), "online");
    assert!(last(t.device_config()).contains("test_one"));
    d.shutdown().await;
}

#[tokio::test]
async fn large_payloads_get_through() {
    require_it!();
    let (_broker, port) = broker().await;
    let t = topics();
    let d = Daemon::start(port, &[]);
    let art = d.ctx.image(EntitySpec::image(
        id("media_art"),
        "Art",
        ImageSource::Bytes {
            content_type: "image/png".into(),
        },
    ));
    let big = bytes::Bytes::from(vec![7u8; 1024 * 1024]);
    art.set_bytes(big.clone());
    d.ready();
    let mut obs = Observer::connect(port, &t.state(&id("media_art"))).await;
    let m = obs.wait_for(&t.state(&id("media_art")), |_| true).await;
    assert_eq!(m.payload.len(), big.len());
    d.shutdown().await;
}

#[tokio::test]
async fn admin_reads_and_clears_everything_retained() {
    require_it!();
    let (_broker, port) = broker().await;
    let t = topics();
    let config = MqttConfig {
        host: "127.0.0.1".into(),
        port: Some(port),
        ..MqttConfig::default()
    };

    let d = Daemon::start(port, &[]);
    let cpu = d.ctx.sensor(EntitySpec::sensor(id("cpu_percent"), "CPU"));
    cpu.set(1);
    cpu.set_attributes(serde_json::Map::from_iter([("k".into(), "v".into())]));
    d.ready();
    // Wait until both retained topics exist before reading them back.
    let mut obs = Observer::connect(port, "deskhand/itnode/cpu_percent/#").await;
    let mut pending = BTreeSet::from([
        t.state(&id("cpu_percent")),
        t.attributes(&id("cpu_percent")),
    ]);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !pending.is_empty() {
            let m = obs.rx.recv().await.expect("observer stream ended");
            pending.remove(&m.topic);
        }
    })
    .await
    .expect("state and attributes published");
    // Something of another node must survive.
    obs.publish("deskhand/othernode/status", "online", true)
        .await;

    // While the daemon runs, the admin client sees it online.
    let mut admin = AdminClient::connect(&config, NODE, Duration::from_secs(5))
        .await
        .unwrap();
    let filters = admin.owned_filters();
    let retained = admin
        .read_retained(&filters, Duration::from_millis(500), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(retained[&t.status()], "online");
    assert!(retained.contains_key(&t.device_config()));
    assert!(retained.contains_key(&t.attributes(&id("cpu_percent"))));
    admin.disconnect().await;
    d.shutdown().await;

    let mut admin = AdminClient::connect(&config, NODE, Duration::from_secs(5))
        .await
        .unwrap();
    let retained = admin
        .read_retained(&filters, Duration::from_millis(500), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(retained[&t.status()], "offline");
    let topics: Vec<String> = retained.keys().cloned().collect();
    admin.clear(&topics, Duration::from_secs(5)).await.unwrap();
    admin.disconnect().await;

    let mut after = AdminClient::connect(&config, NODE, Duration::from_secs(5))
        .await
        .unwrap();
    let left = after
        .read_retained(
            &["#".to_owned()],
            Duration::from_millis(500),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(
        left.keys().cloned().collect::<Vec<_>>(),
        vec!["deskhand/othernode/status".to_owned()]
    );
    after.disconnect().await;
}

#[tokio::test]
async fn sleep_goes_offline_and_resume_reconnects_quickly() {
    require_it!();
    let (_broker, port) = broker().await;
    let t = topics();
    let mut obs = Observer::connect(port, &t.status()).await;
    let d = Daemon::start(port, &[]);
    let state = d
        .ctx
        .sensor(EntitySpec::sensor(id("session_state"), "State").always_available());
    state.set("active");
    d.ready();
    obs.wait_for(&t.status(), |m| m.text() == "online").await;

    // What the power integration does on PrepareForSleep(true).
    state.set("sleeping");
    d.hub.sleep();
    obs.wait_for(&t.status(), |m| m.text() == "offline").await;
    let mut late = Observer::connect(port, &t.state(&id("session_state"))).await;
    let m = late
        .wait_for(&t.state(&id("session_state")), |_| true)
        .await;
    assert_eq!(
        m.text(),
        "sleeping",
        "the sleeping state is retained before disconnecting"
    );
    // Stays offline while asleep (no reconnect attempts).
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        obs.collect(Duration::from_millis(200))
            .await
            .iter()
            .all(|m| m.text() != "online")
    );

    let woke = std::time::Instant::now();
    state.set("active");
    d.hub.resume();
    obs.wait_for(&t.status(), |m| m.text() == "online").await;
    let took = woke.elapsed();
    assert!(took < Duration::from_secs(10), "back online after {took:?}");
    eprintln!("back online {took:?} after resume");
    d.shutdown().await;
}
