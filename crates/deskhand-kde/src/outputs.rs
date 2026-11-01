//! `outputs` capability (opt-in): an `output_<name>` switch per connected
//! output that enables or disables it, through `kscreen-doctor`.
//!
//! Opt-in with `[integrations.outputs] enabled = true`: disabling an output
//! rearranges windows and panels. The last enabled output is never
//! disabled. KScreen has no simple change signal, so the outputs are re-read
//! every 10 s and right after a command.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use deskhand_core::ctx::Switch;
use deskhand_core::{Attributes, Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use deskhand_linux::LinuxEnv;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::{find_tool, tool_output};

const KSCREEN_DOCTOR: &str = "kscreen-doctor";
const POLL: Duration = Duration::from_secs(10);

/// One output from `kscreen-doctor -j`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Output {
    pub id: u32,
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub connected: bool,
}

#[derive(Debug, Deserialize)]
struct Doc {
    #[serde(default)]
    outputs: Vec<Output>,
}

/// The connected outputs in `kscreen-doctor -j` output.
pub fn parse(json: &[u8]) -> Result<Vec<Output>, String> {
    let doc: Doc = serde_json::from_slice(json).map_err(|e| format!("kscreen-doctor -j: {e}"))?;
    Ok(doc.outputs.into_iter().filter(|o| o.connected).collect())
}

pub fn entity_id(name: &str) -> EntityId {
    EntityId::slugify(&format!("output_{name}"))
}

/// Why `off` must not be applied to `name`, if so.
pub fn refuse_disable(outputs: &[Output], name: &str) -> Option<&'static str> {
    let enabled = outputs.iter().filter(|o| o.enabled).count();
    let target = outputs.iter().find(|o| o.name == name)?;
    (target.enabled && enabled <= 1).then_some("it is the only enabled output")
}

struct OutputEntity {
    switch: Switch,
    forwarder: AbortHandle,
}

impl Drop for OutputEntity {
    fn drop(&mut self) {
        // The forwarder owns the command stream; stopping it lets the entity unregister.
        self.forwarder.abort();
    }
}

/// Output enable/disable switches.
#[derive(Debug)]
pub struct Outputs {
    env: Arc<LinuxEnv>,
    /// `[integrations.outputs] enabled = true`.
    wanted: bool,
    /// Overrides the `kscreen-doctor` lookup (tests).
    tool: Option<PathBuf>,
}

impl Outputs {
    pub fn new(env: Arc<LinuxEnv>, wanted: bool) -> Self {
        Self {
            env,
            wanted,
            tool: None,
        }
    }

    pub fn with_tool(mut self, tool: PathBuf) -> Self {
        self.tool = Some(tool);
        self
    }

    fn tool(&self) -> Option<PathBuf> {
        self.tool.clone().or_else(|| find_tool(KSCREEN_DOCTOR))
    }
}

#[async_trait::async_trait]
impl Integration for Outputs {
    fn id(&self) -> &'static str {
        "outputs.kscreen"
    }

    fn capability(&self) -> &'static str {
        "outputs"
    }

    fn description(&self) -> &'static str {
        "Enable or disable outputs through kscreen-doctor (opt-in)"
    }

    async fn probe(&self) -> Probe {
        if !self.wanted {
            return Probe::unavailable("opt-in: set [integrations.outputs] enabled = true");
        }
        if !self.env.desktop_is("KDE") {
            return Probe::unavailable("not a KDE Plasma session");
        }
        let Some(tool) = self.tool() else {
            return Probe::unavailable("kscreen-doctor not found");
        };
        match tool_output(&tool, &["-j"]).await.and_then(|j| parse(&j)) {
            Ok(o) => Probe::available_with(format!("{} connected output(s)", o.len())),
            Err(e) => Probe::unavailable(e),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let tool = self
            .tool()
            .ok_or_else(|| IntegrationError::msg("kscreen-doctor not found"))?;
        let read = async || {
            tool_output(&tool, &["-j"])
                .await
                .and_then(|j| parse(&j))
                .map_err(IntegrationError::msg)
        };
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<(String, bool)>();
        let mut entities: BTreeMap<String, OutputEntity> = BTreeMap::new();
        let reconcile = |outputs: &[Output], entities: &mut BTreeMap<String, OutputEntity>| {
            entities.retain(|name, _| outputs.iter().any(|o| o.name == *name));
            for o in outputs {
                let e = entities.entry(o.name.clone()).or_insert_with(|| {
                    let (switch, mut commands) = ctx.switch(
                        EntitySpec::switch(entity_id(&o.name), format!("Output {}", o.name))
                            .icon("mdi:monitor-shimmer"),
                    );
                    let tx = cmd_tx.clone();
                    let name = o.name.clone();
                    let forwarder = tokio::spawn(async move {
                        while let Some(on) = commands.recv().await {
                            if tx.send((name.clone(), on)).is_err() {
                                break;
                            }
                        }
                    })
                    .abort_handle();
                    OutputEntity { switch, forwarder }
                });
                let mut attrs = Attributes::new();
                attrs.insert("id".into(), json!(o.id));
                e.switch.set_attributes(attrs);
                e.switch.set(o.enabled);
            }
        };
        let mut outputs = read().await?;
        reconcile(&outputs, &mut entities);
        ctx.ready();

        let cancel = ctx.cancelled();
        let mut poll = tokio::time::interval(POLL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        poll.tick().await;
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                _ = poll.tick() => {}
                Some((name, on)) = cmd_rx.recv() => {
                    if let Some(o) = outputs.iter().find(|o| o.name == name) {
                        match (on, refuse_disable(&outputs, &name)) {
                            (false, Some(why)) => {
                                tracing::warn!(output = %name, "not disabling the output: {why}");
                            }
                            _ => {
                                let arg = format!("output.{}.{}", o.id, if on { "enable" } else { "disable" });
                                tracing::info!(output = %name, on, "output from Home Assistant");
                                if let Err(e) = tool_output(&tool, &[&arg]).await {
                                    tracing::warn!(output = %name, error = %e, "changing the output failed");
                                }
                            }
                        }
                    }
                }
            }
            match read().await {
                Ok(o) => {
                    outputs = o;
                    reconcile(&outputs, &mut entities);
                }
                Err(e) => tracing::debug!(error = %e, "reading outputs failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::testing::{rig, wait_state};
    use deskhand_core::{CommandValue, State};

    use super::*;

    const JSON: &str = r#"{"outputs":[
        {"id":1,"name":"eDP-1","enabled":true,"connected":true,"modes":[]},
        {"id":2,"name":"HDMI-A-1","enabled":false,"connected":true},
        {"id":3,"name":"DP-2","enabled":false,"connected":false}
    ],"screen":{}}"#;

    #[test]
    fn parses_connected_outputs() {
        let o = parse(JSON.as_bytes()).unwrap();
        assert_eq!(o.len(), 2);
        assert_eq!(o[1].name, "HDMI-A-1");
        assert!(!o[1].enabled);
        assert_eq!(entity_id("HDMI-A-1").as_str(), "output_hdmi_a_1");
    }

    #[test]
    fn never_disables_the_last_output() {
        let o = parse(JSON.as_bytes()).unwrap();
        assert!(refuse_disable(&o, "eDP-1").is_some());
        assert!(refuse_disable(&o, "HDMI-A-1").is_none());
        let mut both = o.clone();
        both[1].enabled = true;
        assert!(refuse_disable(&both, "eDP-1").is_none());
    }

    /// A fake kscreen-doctor: `-j` prints the state file, `output.<id>.enable|disable`
    /// edits it and logs the call.
    fn fake_tool(dir: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(dir.join("state.json"), JSON).unwrap();
        let script = dir.join("kscreen-doctor");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
cd {dir}
if [ "$1" = "-j" ]; then cat state.json; exit 0; fi
echo "$1" >> calls
case "$1" in
  output.2.enable) sed -i 's/"id":2,"name":"HDMI-A-1","enabled":false/"id":2,"name":"HDMI-A-1","enabled":true/' state.json ;;
  output.2.disable) sed -i 's/"id":2,"name":"HDMI-A-1","enabled":true/"id":2,"name":"HDMI-A-1","enabled":false/' state.json ;;
esac
"#,
                dir = dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[tokio::test]
    async fn switches_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let tool = fake_tool(dir.path());
        let env =
            LinuxEnv::with_addresses("unix:path=/nonexistent", "unix:path=/nonexistent", &["KDE"]);
        assert!(
            !Outputs::new(env.clone(), false)
                .probe()
                .await
                .is_available()
        );
        let integration = Outputs::new(env, true).with_tool(tool);
        assert!(integration.probe().await.is_available());
        let (mut t, _cancel) = rig(integration, "outputs");
        let edp = entity_id("eDP-1");
        let hdmi = entity_id("HDMI-A-1");
        wait_state(&mut t, &edp, State::Bool(true)).await;
        wait_state(&mut t, &hdmi, State::Bool(false)).await;

        // The only enabled output: refused, and the switch stays on.
        t.command(edp.as_str(), CommandValue::Bool(false));
        t.settle(Duration::from_millis(300)).await;
        assert_eq!(t.state(&edp), Some(&State::Bool(true)));
        assert!(!dir.path().join("calls").exists());

        t.command(hdmi.as_str(), CommandValue::Bool(true));
        wait_state(&mut t, &hdmi, State::Bool(true)).await;
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert_eq!(calls, "output.2.enable\n");
    }
}
