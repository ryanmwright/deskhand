//! `scripts` capability: `[[script]]` entries become Home Assistant buttons.
//!
//! This is the only way Home Assistant can make Deskhand run a program, and
//! only programs listed in the config file can run. A press carries no
//! arguments. Each script also gets a diagnostic `<id>_result` sensor with
//! the outcome of the last run (attributes: exit code, start time, duration).

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

use deskhand_core::config::Parsed;
use deskhand_core::{
    Attributes, Commands, Config, ConfigError, Ctx, EntityId, EntitySpec, Integration,
    IntegrationError, Probe,
};
use serde::Deserialize;
use serde_json::json;
use tokio::task::JoinSet;

use crate::process::{self, CommandLine, CommandLineError, Outcome};
use crate::time::rfc3339;

/// Result sensor values.
pub const RESULT_OPTIONS: &[&str] = &["running", "success", "failed", "timeout"];

/// One `[[script]]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ScriptConfig {
    /// Entity id suffix: the button is `script_<id>`.
    pub id: String,
    pub name: Option<String>,
    pub command: CommandLine,
    #[serde(default)]
    pub shell: bool,
    #[serde(default = "default_timeout", with = "humantime_serde")]
    pub timeout: Duration,
    pub icon: Option<String>,
}

fn default_timeout() -> Duration {
    Duration::from_secs(60)
}

/// Problems with a `[[script]]` or `[[sensor]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EntryError {
    #[error("[[{section}]] {id:?}: id must be lowercase letters, digits and _ (at most 48)")]
    BadId { section: &'static str, id: String },
    #[error("[[{section}]] {id:?}: duplicate id")]
    Duplicate { section: &'static str, id: String },
    #[error("[[{section}]] {id:?}: {source}")]
    Command {
        section: &'static str,
        id: String,
        #[source]
        source: CommandLineError,
    },
    #[error("[[{section}]] {id:?}: {reason}")]
    Invalid {
        section: &'static str,
        id: String,
        reason: String,
    },
}

/// Checks an entry id and builds `<prefix><id>` from it.
pub(crate) fn entry_id(
    section: &'static str,
    prefix: &str,
    id: &str,
    seen: &mut BTreeSet<String>,
) -> Result<EntityId, EntryError> {
    let bad = || EntryError::BadId {
        section,
        id: id.to_owned(),
    };
    if id.is_empty() || id.len() > 48 {
        return Err(bad());
    }
    let entity = EntityId::new(format!("{prefix}{id}")).map_err(|_| bad())?;
    if !seen.insert(id.to_owned()) {
        return Err(EntryError::Duplicate {
            section,
            id: id.to_owned(),
        });
    }
    Ok(entity)
}

#[derive(Debug, Clone)]
struct Script {
    button: EntityId,
    result: EntityId,
    name: String,
    argv: Vec<String>,
    timeout: Duration,
    icon: Option<String>,
}

/// Runs configured scripts when their buttons are pressed.
#[derive(Debug, Clone, Default)]
pub struct Scripts {
    scripts: Vec<Script>,
}

impl Scripts {
    pub const CAPABILITY: &'static str = "scripts";

    /// Reads the `[[script]]` array (unknown keys are reported, not rejected).
    pub fn load(config: &Config) -> Result<Parsed<Vec<ScriptConfig>>, ConfigError> {
        config.section("script")
    }

    pub fn new(configs: Vec<ScriptConfig>) -> Result<Self, EntryError> {
        let mut seen = BTreeSet::new();
        let mut scripts = Vec::with_capacity(configs.len());
        for c in configs {
            let button = entry_id("script", "script_", &c.id, &mut seen)?;
            let argv = c
                .command
                .argv(c.shell)
                .map_err(|source| EntryError::Command {
                    section: "script",
                    id: c.id.clone(),
                    source,
                })?;
            if c.timeout.is_zero() {
                return Err(EntryError::Invalid {
                    section: "script",
                    id: c.id,
                    reason: "timeout must be positive".into(),
                });
            }
            scripts.push(Script {
                result: button.join("result"),
                button,
                name: c.name.unwrap_or_else(|| c.id.clone()),
                argv,
                timeout: c.timeout,
                icon: c.icon,
            });
        }
        Ok(Self { scripts })
    }

    pub fn len(&self) -> usize {
        self.scripts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.scripts.is_empty()
    }
}

#[async_trait::async_trait]
impl Integration for Scripts {
    fn id(&self) -> &'static str {
        "scripts"
    }

    fn capability(&self) -> &'static str {
        Self::CAPABILITY
    }

    fn description(&self) -> &'static str {
        "Buttons that run the [[script]] commands from the config"
    }

    async fn probe(&self) -> Probe {
        match self.scripts.len() {
            0 => Probe::unavailable("no [[script]] entries in the config"),
            n => Probe::available_with(format!("{n} script(s)")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let mut tasks = JoinSet::new();
        for script in self.scripts.clone() {
            let mut button = EntitySpec::button(script.button.clone(), script.name.clone());
            if let Some(icon) = &script.icon {
                button = button.icon(icon.clone());
            }
            let presses = ctx.button(button);
            let result = ctx.sensor(
                EntitySpec::sensor(script.result.clone(), format!("{} result", script.name))
                    .enum_options(RESULT_OPTIONS.iter().map(|s| s.to_string()).collect())
                    .icon("mdi:script-text-outline")
                    .diagnostic(),
            );
            result.set_unknown();
            tasks.spawn(serve(script, presses, result, ctx.clone()));
        }
        ctx.ready();
        let cancel = ctx.cancelled();
        cancel.cancelled().await;
        // Each task kills its running command on cancel; give them a moment.
        while tasks.join_next().await.is_some() {}
        Ok(())
    }
}

/// Handles presses for one script. A press while it is running is ignored.
async fn serve(
    script: Script,
    mut presses: Commands<()>,
    result: deskhand_core::ctx::Sensor,
    ctx: Ctx,
) {
    let cancel = ctx.cancelled();
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            press = presses.recv() => if press.is_none() { return },
        }
        let started = SystemTime::now();
        result.set("running");
        tracing::info!(script = %script.button, "running script");
        let (state, attributes) = match process::run(&script.argv, script.timeout, &cancel).await {
            Ok(out) => {
                let state = match out.outcome {
                    Outcome::Exited(0) => "success",
                    Outcome::TimedOut => "timeout",
                    Outcome::Cancelled => return,
                    Outcome::Exited(_) | Outcome::Signalled => "failed",
                };
                if out.outcome.success() {
                    tracing::info!(script = %script.button, duration = ?out.duration, "script finished");
                } else {
                    tracing::warn!(
                        script = %script.button,
                        outcome = %out.outcome,
                        stderr = process::tail(&out.stderr, 500),
                        "script failed"
                    );
                }
                tracing::debug!(script = %script.button, stdout = process::tail(&out.stdout, 2000), "script output");
                let mut attrs = Attributes::new();
                attrs.insert("last_started".into(), rfc3339(started).into());
                attrs.insert(
                    "duration_s".into(),
                    json!((out.duration.as_secs_f64() * 10.0).round() / 10.0),
                );
                if let Outcome::Exited(code) = out.outcome {
                    attrs.insert("exit_code".into(), code.into());
                }
                (state, attrs)
            }
            Err(e) => {
                tracing::warn!(script = %script.button, error = %e, "cannot start script");
                let mut attrs = Attributes::new();
                attrs.insert("last_started".into(), rfc3339(started).into());
                attrs.insert("error".into(), e.to_string().into());
                ("failed", attrs)
            }
        };
        result.set_attributes(attributes);
        result.set(state);
        let mut skipped = 0;
        while presses.try_recv().is_some() {
            skipped += 1;
        }
        if skipped > 0 {
            tracing::info!(script = %script.button, skipped, "ignored presses while the script was running");
        }
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::testing::FakeTransport;
    use deskhand_core::{CommandValue, Hub, HubEvent, HubOptions, IntegrationConfig, State};
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn parse(toml: &str) -> Result<Scripts, String> {
        let config = Config::from_toml_str(toml, None).map_err(|e| e.to_string())?;
        let parsed = Scripts::load(&config).map_err(|e| e.to_string())?;
        Scripts::new(parsed.value).map_err(|e| e.to_string())
    }

    #[test]
    fn config_validation() {
        let ok = parse(
            r#"
            [[script]]
            id = "backup"
            name = "Run backup"
            command = ["restic", "backup"]
            timeout = "10m"

            [[script]]
            id = "pipe"
            command = "echo a | wc -c"
            shell = true
            "#,
        )
        .unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!(ok.scripts[0].button.as_str(), "script_backup");
        assert_eq!(ok.scripts[0].result.as_str(), "script_backup_result");
        assert_eq!(ok.scripts[0].timeout, Duration::from_secs(600));
        assert_eq!(ok.scripts[1].name, "pipe");

        let dup = parse("[[script]]\nid='a'\ncommand=['x']\n[[script]]\nid='a'\ncommand=['y']");
        assert!(dup.unwrap_err().contains("duplicate"));
        let bad = parse("[[script]]\nid='Bad Id'\ncommand=['x']");
        assert!(bad.unwrap_err().contains("lowercase"));
        let str_no_shell = parse("[[script]]\nid='a'\ncommand='x y'");
        assert!(str_no_shell.unwrap_err().contains("shell = true"));
        assert!(parse("").unwrap().is_empty());
    }

    async fn next_state(t: &mut FakeTransport, id: &str) -> (State, Option<Attributes>) {
        let mut attrs = None;
        loop {
            match t.next_within(Duration::from_secs(10)).await {
                Some(HubEvent::Attributes { id: i, attributes }) if i.as_str() == id => {
                    attrs = Some(attributes)
                }
                Some(HubEvent::State { id: i, state, .. }) if i.as_str() == id => {
                    return (state, attrs);
                }
                Some(_) => {}
                None => panic!("no state for {id}"),
            }
        }
    }

    #[tokio::test]
    async fn press_runs_the_command_and_reports_the_result() {
        let scripts = parse(
            r#"
            [[script]]
            id = "ok"
            command = ["true"]
            [[script]]
            id = "fail"
            command = "exit 4"
            shell = true
            [[script]]
            id = "slow"
            command = ["sleep", "5"]
            timeout = "100ms"
            "#,
        )
        .unwrap();
        let (hub, handle, link) = Hub::new(HubOptions::default());
        tokio::spawn(hub.run());
        let mut t = FakeTransport::new(link);
        let cancel = CancellationToken::new();
        let ctx = handle.ctx(IntegrationConfig::new("scripts"), cancel.clone());
        handle.integration_started("scripts");
        let task = tokio::spawn(async move { scripts.run(ctx).await });
        handle.startup_complete();
        t.connect();
        // Snapshot first, then commands.
        loop {
            if let Some(HubEvent::Snapshot(s)) = t.next_within(Duration::from_secs(10)).await {
                assert!(s.entities.iter().any(|d| d.spec.id.as_str() == "script_ok"));
                break;
            }
        }

        t.command("script_ok", CommandValue::Press);
        assert_eq!(
            next_state(&mut t, "script_ok_result").await.0,
            "running".into()
        );
        let (state, attrs) = next_state(&mut t, "script_ok_result").await;
        assert_eq!(state, "success".into());
        assert_eq!(attrs.unwrap()["exit_code"], 0);

        t.command("script_fail", CommandValue::Press);
        next_state(&mut t, "script_fail_result").await;
        let (state, attrs) = next_state(&mut t, "script_fail_result").await;
        assert_eq!(state, "failed".into());
        assert_eq!(attrs.unwrap()["exit_code"], 4);

        t.command("script_slow", CommandValue::Press);
        next_state(&mut t, "script_slow_result").await;
        assert_eq!(
            next_state(&mut t, "script_slow_result").await.0,
            "timeout".into()
        );

        cancel.cancel();
        task.await.unwrap().unwrap();
    }
}
