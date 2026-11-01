//! `command_sensors` capability: `[[sensor]]` entries run a command on an
//! interval and publish its output.
//!
//! - `kind = "sensor"` (default): trimmed stdout is the state.
//! - `kind = "binary_sensor"`: stdout `on/true/yes/1` or `off/false/no/0`.
//! - `json_attributes = true`: stdout is a JSON object; its `state` key is the
//!   state and every other key becomes an attribute.
//!
//! A failing command (non-zero exit, timeout, unparsable output) makes the
//! state unknown.

use std::collections::BTreeSet;
use std::time::Duration;

use deskhand_core::config::Parsed;
use deskhand_core::ctx::{BinarySensor, Sensor};
use deskhand_core::entity::{BinarySensorClass, EntityKind, SensorClass, StateClass};
use deskhand_core::{
    Attributes, Config, ConfigError, Ctx, EntitySpec, Integration, IntegrationError, Probe, State,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::task::JoinSet;

use crate::process::{self, CommandLine};
use crate::scripts::{EntryError, entry_id};

/// Entity kind of a command sensor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensorKind {
    #[default]
    Sensor,
    BinarySensor,
}

/// One `[[sensor]]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SensorConfig {
    /// Entity id (used as-is, so it can match ids from other tools).
    pub id: String,
    pub name: Option<String>,
    pub command: CommandLine,
    #[serde(default)]
    pub shell: bool,
    #[serde(default = "default_interval", with = "humantime_serde")]
    pub interval: Duration,
    #[serde(default = "default_timeout", with = "humantime_serde")]
    pub timeout: Duration,
    #[serde(default)]
    pub kind: SensorKind,
    pub unit: Option<String>,
    /// Any HA device class for the kind (`temperature`, `connectivity`, ...).
    pub device_class: Option<String>,
    /// `measurement`, `total` or `total_increasing`.
    pub state_class: Option<String>,
    pub icon: Option<String>,
    #[serde(default)]
    pub json_attributes: bool,
}

fn default_interval() -> Duration {
    Duration::from_secs(60)
}

fn default_timeout() -> Duration {
    Duration::from_secs(10)
}

#[derive(Debug, Clone)]
struct CommandSensor {
    spec: EntitySpec,
    argv: Vec<String>,
    interval: Duration,
    timeout: Duration,
    json: bool,
}

/// Publishes the output of configured commands.
#[derive(Debug, Clone, Default)]
pub struct CommandSensors {
    sensors: Vec<CommandSensor>,
}

impl CommandSensors {
    pub const CAPABILITY: &'static str = "command_sensors";

    /// Reads the `[[sensor]]` array (unknown keys are reported, not rejected).
    pub fn load(config: &Config) -> Result<Parsed<Vec<SensorConfig>>, ConfigError> {
        config.section("sensor")
    }

    pub fn new(configs: Vec<SensorConfig>) -> Result<Self, EntryError> {
        let mut seen = BTreeSet::new();
        let mut sensors = Vec::with_capacity(configs.len());
        for c in configs {
            let id = entry_id("sensor", "", &c.id, &mut seen)?;
            let invalid = |reason: String| EntryError::Invalid {
                section: "sensor",
                id: c.id.clone(),
                reason,
            };
            let argv = c
                .command
                .argv(c.shell)
                .map_err(|source| EntryError::Command {
                    section: "sensor",
                    id: c.id.clone(),
                    source,
                })?;
            if c.interval < Duration::from_secs(1) {
                return Err(invalid("interval must be at least 1s".into()));
            }
            if c.timeout.is_zero() {
                return Err(invalid("timeout must be positive".into()));
            }
            let name = c.name.clone().unwrap_or_else(|| c.id.clone());
            let mut spec = match c.kind {
                SensorKind::Sensor => {
                    let mut spec = EntitySpec::sensor(id, name);
                    if let EntityKind::Sensor {
                        device_class,
                        state_class,
                        unit,
                        ..
                    } = &mut spec.kind
                    {
                        *unit = c.unit.clone().map(Into::into);
                        *device_class = c.device_class.clone().map(SensorClass::Custom);
                        *state_class = match c.state_class.as_deref() {
                            None => None,
                            Some("measurement") => Some(StateClass::Measurement),
                            Some("total") => Some(StateClass::Total),
                            Some("total_increasing") => Some(StateClass::TotalIncreasing),
                            Some(other) => {
                                return Err(invalid(format!(
                                    "state_class {other:?} is not measurement, total or total_increasing"
                                )));
                            }
                        };
                    }
                    spec
                }
                SensorKind::BinarySensor => {
                    if c.unit.is_some() || c.state_class.is_some() {
                        return Err(invalid(
                            "unit and state_class do not apply to binary sensors".into(),
                        ));
                    }
                    EntitySpec::binary_sensor(
                        id,
                        name,
                        c.device_class.clone().map(BinarySensorClass::Custom),
                    )
                }
            };
            if let Some(icon) = c.icon {
                spec = spec.icon(icon);
            }
            sensors.push(CommandSensor {
                spec,
                argv,
                interval: c.interval,
                timeout: c.timeout,
                json: c.json_attributes,
            });
        }
        Ok(Self { sensors })
    }

    pub fn len(&self) -> usize {
        self.sensors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sensors.is_empty()
    }
}

#[derive(Debug, Clone)]
enum Handle {
    Sensor(Sensor),
    Binary(BinarySensor),
}

impl Handle {
    fn publish(&self, value: Option<Value>) {
        match (self, value) {
            (_, None) => self.set_unknown(),
            (Self::Sensor(s), Some(Value::String(text))) => s.set(text),
            (Self::Sensor(s), Some(Value::Number(n))) => s.set(
                n.as_i64()
                    .map_or_else(|| State::from(n.as_f64()), State::Int),
            ),
            (Self::Sensor(s), Some(Value::Bool(b))) => s.set(if b { "on" } else { "off" }),
            (Self::Sensor(s), Some(other)) => s.set(other.to_string()),
            (Self::Binary(b), Some(v)) => match parse_bool(&v) {
                Some(on) => b.set(on),
                None => {
                    tracing::warn!(id = %b.id(), value = %v, "binary sensor output is not on/off");
                    b.set_unknown();
                }
            },
        }
    }

    fn set_unknown(&self) {
        match self {
            Self::Sensor(s) => s.set_unknown(),
            Self::Binary(b) => b.set_unknown(),
        }
    }

    fn set_attributes(&self, attributes: Attributes) {
        match self {
            Self::Sensor(s) => s.set_attributes(attributes),
            Self::Binary(b) => b.set_attributes(attributes),
        }
    }
}

fn parse_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_f64().map(|f| f != 0.0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" | "1" => Some(true),
            "off" | "false" | "no" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Splits command output into state and attributes.
fn parse_output(stdout: &str, json: bool) -> Result<(Value, Option<Attributes>), String> {
    if !json {
        return Ok((Value::String(stdout.trim().to_owned()), None));
    }
    match serde_json::from_str::<Value>(stdout) {
        Ok(Value::Object(mut map)) => {
            let state = map.remove("state").unwrap_or(Value::Null);
            Ok((state, Some(map)))
        }
        Ok(_) => Err("output is JSON but not an object".into()),
        Err(e) => Err(format!("output is not JSON: {e}")),
    }
}

#[async_trait::async_trait]
impl Integration for CommandSensors {
    fn id(&self) -> &'static str {
        "command_sensors"
    }

    fn capability(&self) -> &'static str {
        Self::CAPABILITY
    }

    fn description(&self) -> &'static str {
        "Sensors fed by the [[sensor]] commands from the config"
    }

    async fn probe(&self) -> Probe {
        match self.sensors.len() {
            0 => Probe::unavailable("no [[sensor]] entries in the config"),
            n => Probe::available_with(format!("{n} sensor(s)")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let (first_tx, mut first_rx) = tokio::sync::mpsc::channel(self.sensors.len().max(1));
        let mut tasks = JoinSet::new();
        for sensor in self.sensors.clone() {
            let handle = match sensor.spec.kind {
                EntityKind::BinarySensor { .. } => {
                    Handle::Binary(ctx.binary_sensor(sensor.spec.clone()))
                }
                _ => Handle::Sensor(ctx.sensor(sensor.spec.clone())),
            };
            tasks.spawn(poll(sensor, handle, ctx.clone(), first_tx.clone()));
        }
        drop(first_tx);
        // Publish once every sensor has a first value, but don't hold the
        // whole device back for a slow command.
        let first_round = async { while first_rx.recv().await.is_some() {} };
        let _ = tokio::time::timeout(Duration::from_secs(3), first_round).await;
        ctx.ready();
        ctx.cancelled().cancelled().await;
        while tasks.join_next().await.is_some() {}
        Ok(())
    }
}

async fn poll(
    sensor: CommandSensor,
    handle: Handle,
    ctx: Ctx,
    first: tokio::sync::mpsc::Sender<()>,
) {
    let cancel = ctx.cancelled();
    let id = sensor.spec.id.clone();
    let mut first = Some(first);
    let mut ticker = tokio::time::interval(sensor.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut failing = false;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = ticker.tick() => {}
        }
        let result = match process::run(&sensor.argv, sensor.timeout, &cancel).await {
            Ok(out) if out.outcome.success() => parse_output(&out.stdout, sensor.json),
            Ok(out) if out.outcome == process::Outcome::Cancelled => return,
            Ok(out) => Err(format!(
                "{}: {}",
                out.outcome,
                process::tail(&out.stderr, 300)
            )),
            Err(e) => Err(format!("cannot start: {e}")),
        };
        match result {
            Ok((state, attributes)) => {
                if failing {
                    tracing::info!(%id, "command sensor recovered");
                    failing = false;
                }
                if let Some(attributes) = attributes {
                    handle.set_attributes(attributes);
                }
                handle.publish(if state.is_null() { None } else { Some(state) });
            }
            Err(error) => {
                // Log once per failure streak, not every interval.
                if !failing {
                    tracing::warn!(%id, %error, "command sensor failed");
                    failing = true;
                }
                handle.set_unknown();
            }
        }
        if let Some(tx) = first.take() {
            let _ = tx.try_send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::testing::FakeTransport;
    use deskhand_core::{EntityId, Hub, HubEvent, HubOptions, IntegrationConfig};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn parse(toml: &str) -> Result<CommandSensors, String> {
        let config = Config::from_toml_str(toml, None).map_err(|e| e.to_string())?;
        let parsed = CommandSensors::load(&config).map_err(|e| e.to_string())?;
        CommandSensors::new(parsed.value).map_err(|e| e.to_string())
    }

    #[test]
    fn config_validation() {
        let s = parse(
            r#"
            [[sensor]]
            id = "room_temp"
            name = "Room temperature"
            command = ["cat", "/tmp/t"]
            interval = "30s"
            unit = "°C"
            device_class = "temperature"
            state_class = "measurement"

            [[sensor]]
            id = "vpn_up"
            command = "ip link show wg0 >/dev/null && echo on || echo off"
            shell = true
            kind = "binary_sensor"
            device_class = "connectivity"
            "#,
        )
        .unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s.sensors[0].spec.kind.platform(), "sensor");
        assert_eq!(s.sensors[1].spec.kind.platform(), "binary_sensor");
        assert_eq!(s.sensors[0].interval, Duration::from_secs(30));

        let bad_class = parse("[[sensor]]\nid='a'\ncommand=['x']\nstate_class='avg'");
        assert!(bad_class.unwrap_err().contains("state_class"));
        let fast = parse("[[sensor]]\nid='a'\ncommand=['x']\ninterval='10ms'");
        assert!(fast.unwrap_err().contains("interval"));
        let unit_on_binary =
            parse("[[sensor]]\nid='a'\ncommand=['x']\nkind='binary_sensor'\nunit='%'");
        assert!(unit_on_binary.is_err());
    }

    #[test]
    fn output_parsing() {
        assert_eq!(parse_output(" 21.5\n", false).unwrap().0, json!("21.5"));
        let (state, attrs) = parse_output(r#"{"state": 3, "detail": "x"}"#, true).unwrap();
        assert_eq!(state, json!(3));
        assert_eq!(attrs.unwrap()["detail"], "x");
        assert!(parse_output("[1]", true).is_err());
        assert!(parse_output("nope", true).is_err());
        assert_eq!(parse_bool(&json!("ON\n")), Some(true));
        assert_eq!(parse_bool(&json!(0)), Some(false));
        assert_eq!(parse_bool(&json!("maybe")), None);
    }

    #[tokio::test]
    async fn publishes_values_and_unknown_on_failure() {
        let sensors = parse(
            r#"
            [[sensor]]
            id = "answer"
            command = ["echo", "42"]
            [[sensor]]
            id = "flag"
            command = ["echo", "yes"]
            kind = "binary_sensor"
            [[sensor]]
            id = "rich"
            command = ["echo", '{"state": "ok", "count": 2}']
            json_attributes = true
            [[sensor]]
            id = "broken"
            command = ["false"]
            "#,
        )
        .unwrap();
        let (hub, handle, link) = Hub::new(HubOptions::default());
        tokio::spawn(hub.run());
        let mut t = FakeTransport::new(link);
        let cancel = CancellationToken::new();
        let ctx = handle.ctx(IntegrationConfig::new("command_sensors"), cancel.clone());
        handle.integration_started("command_sensors");
        let task = tokio::spawn(async move { sensors.run(ctx).await });
        handle.startup_complete();
        t.connect();
        let snapshot = loop {
            if let Some(HubEvent::Snapshot(s)) = t.next_within(Duration::from_secs(10)).await {
                break s;
            }
        };
        let state = |id: &'static str| snapshot.states.get(&EntityId::from_static(id)).cloned();
        assert_eq!(state("answer"), Some("42".into()));
        assert_eq!(state("flag"), Some(State::Bool(true)));
        assert_eq!(state("rich"), Some("ok".into()));
        assert_eq!(state("broken"), Some(State::Unknown));
        let attrs = &snapshot.attributes[&EntityId::from_static("rich")];
        assert_eq!(attrs["count"], 2);
        cancel.cancel();
        task.await.unwrap().unwrap();
    }
}
