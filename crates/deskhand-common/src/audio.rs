//! Audio domain kit: the `audio` entities, identical on every OS.
//!
//! A platform implements [`AudioSource`] (libpulse on Linux) and its
//! integration hands it to [`AudioKit::run`]:
//!
//! | entity | kind | |
//! |---|---|---|
//! | `audio_volume`, `audio_mute` | number 0-100 %, switch | default output |
//! | `audio_output`, `audio_input` | selects | default output / input device |
//! | `mic_volume`, `mic_mute` | number 0-100 %, switch | default input |
//! | `mic_in_use` | binary sensor | something records from a microphone; attribute `apps` |
//! | `headphones_connected` | binary sensor | the default output is headphones or a headset |
//!
//! `mic_in_use` skips recordings of monitor sources (desktop audio capture),
//! paused (corked) streams and the apps in `mic_ignore_apps`, which extends
//! a built-in list of volume meters:
//!
//! ```toml
//! [integrations.audio]
//! mic_ignore_apps = ["easyeffects"]   # application name, id or binary, case-insensitive
//! ```

use std::collections::BTreeMap;

use deskhand_core::ctx::{BinarySensor, Number, Select, Switch};
use deskhand_core::entity::{EntityKind, NumberMode};
use deskhand_core::{Attributes, Commands, Ctx, EntityId, EntitySpec, IntegrationError};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

pub const AUDIO_VOLUME: EntityId = EntityId::from_static("audio_volume");
pub const AUDIO_MUTE: EntityId = EntityId::from_static("audio_mute");
pub const AUDIO_OUTPUT: EntityId = EntityId::from_static("audio_output");
pub const AUDIO_INPUT: EntityId = EntityId::from_static("audio_input");
pub const MIC_VOLUME: EntityId = EntityId::from_static("mic_volume");
pub const MIC_MUTE: EntityId = EntityId::from_static("mic_mute");
pub const MIC_IN_USE: EntityId = EntityId::from_static("mic_in_use");
pub const HEADPHONES_CONNECTED: EntityId = EntityId::from_static("headphones_connected");

/// Volume meters and mixers that record only to show levels.
pub const DEFAULT_MIC_IGNORE: &[&str] = &[
    "org.PulseAudio.pavucontrol",
    "pavucontrol",
    "org.kde.plasma-pa",
    "org.kde.kmixd",
    "kmix",
    "org.gnome.VolumeControl",
    "gnome-control-center",
];

/// `[integrations.audio]` options.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct AudioOptions {
    /// More apps whose recordings do not count for `mic_in_use`.
    pub mic_ignore_apps: Vec<String>,
}

/// An output (sink) or input (source) device.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioDevice {
    /// Platform id (the pulse sink/source name).
    pub name: String,
    /// Display name.
    pub description: String,
    /// 1.0 is 100 %.
    pub volume: f64,
    pub muted: bool,
    /// Headphones or a headset (outputs only).
    pub headphones: bool,
}

/// A stream recording from an input device.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Recording {
    /// Application name (shown in `apps`).
    pub app: String,
    pub app_id: String,
    pub binary: String,
    /// Paused.
    pub corked: bool,
    /// Records a monitor source (desktop audio), not a microphone.
    pub monitor: bool,
}

impl Recording {
    fn matches(&self, name: &str) -> bool {
        [&self.app, &self.app_id, &self.binary]
            .iter()
            .any(|v| !v.is_empty() && v.eq_ignore_ascii_case(name))
    }
}

/// Everything the kit shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioState {
    pub outputs: Vec<AudioDevice>,
    /// Inputs without monitor sources.
    pub inputs: Vec<AudioDevice>,
    pub default_output: Option<String>,
    pub default_input: Option<String>,
    pub recordings: Vec<Recording>,
}

impl AudioState {
    pub fn output(&self) -> Option<&AudioDevice> {
        let name = self.default_output.as_ref()?;
        self.outputs.iter().find(|d| &d.name == name)
    }

    pub fn input(&self) -> Option<&AudioDevice> {
        let name = self.default_input.as_ref()?;
        self.inputs.iter().find(|d| &d.name == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Output,
    Input,
}

/// A change to apply, addressed by device name.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioCmd {
    /// 1.0 is 100 %.
    Volume(Direction, String, f64),
    Mute(Direction, String, bool),
    Default(Direction, String),
}

/// Error from an audio source; from `next_change` it ends the run.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct AudioError(pub String);

/// A platform's audio system.
#[async_trait::async_trait]
pub trait AudioSource: Send {
    /// The full state. The first call returns as soon as it is known; later
    /// calls wait for a change. Must be cancel-safe.
    async fn next_change(&mut self) -> Result<AudioState, AudioError>;

    async fn command(&mut self, cmd: AudioCmd) -> Result<(), AudioError>;
}

struct Entities {
    volume: Number,
    mute: Switch,
    output: Select,
    input: Select,
    mic_volume: Number,
    mic_mute: Switch,
    mic_in_use: BinarySensor,
    headphones: BinarySensor,
}

enum Input {
    Volume(Direction, f64),
    Mute(Direction, bool),
    Choose(Direction, String),
}

/// Keeps the audio entities in sync with an [`AudioSource`].
#[derive(Debug)]
pub struct AudioKit {
    ignore: Vec<String>,
    state: AudioState,
    outputs: BTreeMap<String, String>,
    inputs: BTreeMap<String, String>,
}

impl AudioKit {
    pub fn new(options: AudioOptions) -> Self {
        let mut ignore: Vec<String> = DEFAULT_MIC_IGNORE.iter().map(|s| s.to_string()).collect();
        ignore.extend(options.mic_ignore_apps);
        Self {
            ignore,
            state: AudioState::default(),
            outputs: BTreeMap::new(),
            inputs: BTreeMap::new(),
        }
    }

    /// Apps recording from a microphone right now (sorted, deduplicated).
    pub fn recording_apps(&self) -> Vec<String> {
        let mut apps: Vec<String> = self
            .state
            .recordings
            .iter()
            .filter(|r| !r.monitor && !r.corked)
            .filter(|r| !self.ignore.iter().any(|i| r.matches(i)))
            .map(|r| {
                if r.app.is_empty() {
                    r.binary.clone()
                } else {
                    r.app.clone()
                }
            })
            .collect();
        apps.sort();
        apps.dedup();
        apps
    }

    fn update(&mut self, state: AudioState) {
        self.outputs = option_names(&state.outputs);
        self.inputs = option_names(&state.inputs);
        self.state = state;
    }

    fn device(&self, dir: Direction) -> Option<&AudioDevice> {
        match dir {
            Direction::Output => self.state.output(),
            Direction::Input => self.state.input(),
        }
    }

    /// Turns an entity command into a source command.
    fn command_for(&self, input: Input) -> Option<AudioCmd> {
        match input {
            Input::Volume(dir, v) => Some(AudioCmd::Volume(
                dir,
                self.device(dir)?.name.clone(),
                (v / 100.0).clamp(0.0, 1.5),
            )),
            Input::Mute(dir, m) => Some(AudioCmd::Mute(dir, self.device(dir)?.name.clone(), m)),
            Input::Choose(dir, option) => {
                let options = match dir {
                    Direction::Output => &self.outputs,
                    Direction::Input => &self.inputs,
                };
                match options.get(&option) {
                    Some(name) => Some(AudioCmd::Default(dir, name.clone())),
                    None => {
                        tracing::warn!(option, "unknown audio device option");
                        None
                    }
                }
            }
        }
    }

    /// Registers the entities, then follows `source` until cancelled.
    pub async fn run(
        mut self,
        ctx: &Ctx,
        mut source: impl AudioSource,
    ) -> Result<(), IntegrationError> {
        let (e, mut inputs) = register(ctx);
        let first = source
            .next_change()
            .await
            .map_err(IntegrationError::other)?;
        self.update(first);
        self.publish(&e);
        ctx.ready();

        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                change = source.next_change() => {
                    self.update(change.map_err(IntegrationError::other)?);
                }
                Some(input) = inputs.recv() => {
                    let Some(cmd) = self.command_for(input) else { continue };
                    tracing::debug!(?cmd, "audio command");
                    if let Err(err) = source.command(cmd.clone()).await {
                        tracing::warn!(?cmd, error = %err, "audio command failed");
                    }
                    // The source reports the result as a change.
                    continue;
                }
            }
            self.publish(&e);
        }
    }

    fn publish(&self, e: &Entities) {
        let s = &self.state;
        publish_select(&e.output, &self.outputs, s.default_output.as_deref());
        publish_select(&e.input, &self.inputs, s.default_input.as_deref());
        let pct = |d: &AudioDevice| (d.volume * 100.0).round();
        match s.output() {
            Some(d) => {
                e.volume.set(pct(d));
                e.mute.set(d.muted);
                e.headphones.set(d.headphones);
            }
            None => {
                e.volume.set_unknown();
                e.mute.set_unknown();
                e.headphones.set(false);
            }
        }
        match s.input() {
            Some(d) => {
                e.mic_volume.set(pct(d));
                e.mic_mute.set(d.muted);
            }
            None => {
                e.mic_volume.set_unknown();
                e.mic_mute.set_unknown();
            }
        }
        let apps = self.recording_apps();
        let mut attrs = Attributes::new();
        attrs.insert("apps".into(), json!(apps));
        e.mic_in_use.set_attributes(attrs);
        e.mic_in_use.set(!apps.is_empty());
    }
}

fn publish_select(select: &Select, options: &BTreeMap<String, String>, current: Option<&str>) {
    select.set_options(options.keys().cloned().collect());
    if let Some((option, _)) = options.iter().find(|(_, n)| Some(n.as_str()) == current) {
        select.set(option.clone());
    }
}

/// Unique select options (descriptions, ` 2` for duplicates) → device name.
fn option_names(devices: &[AudioDevice]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for d in devices {
        let base = if d.description.trim().is_empty() {
            d.name.clone()
        } else {
            d.description.clone()
        };
        let mut name = base.clone();
        let mut n = 2;
        while out.contains_key(&name) {
            name = format!("{base} {n}");
            n += 1;
        }
        out.insert(name, d.name.clone());
    }
    out
}

fn volume_spec(id: EntityId, name: &str, icon: &str) -> EntitySpec {
    let mut spec = EntitySpec::number(id, name, 0.0, 100.0, 1.0)
        .unit("%")
        .icon(icon);
    if let EntityKind::Number { mode, .. } = &mut spec.kind {
        *mode = NumberMode::Slider;
    }
    spec
}

/// Forwards one command stream into the kit's input channel.
fn forward<T: Send + 'static>(
    mut commands: Commands<T>,
    tx: &mpsc::Sender<Input>,
    map: impl Fn(T) -> Input + Send + 'static,
) {
    let tx = tx.clone();
    tokio::spawn(async move {
        while let Some(c) = commands.recv().await {
            if tx.send(map(c)).await.is_err() {
                break;
            }
        }
    });
}

fn register(ctx: &Ctx) -> (Entities, mpsc::Receiver<Input>) {
    use Direction::{Input as In, Output as Out};
    let (tx, rx) = mpsc::channel(16);
    let (volume, c) = ctx.number(volume_spec(AUDIO_VOLUME, "Volume", "mdi:volume-high"));
    forward(c, &tx, |v| Input::Volume(Out, v));
    let (mute, c) = ctx.switch(EntitySpec::switch(AUDIO_MUTE, "Mute").icon("mdi:volume-off"));
    forward(c, &tx, |m| Input::Mute(Out, m));
    let (output, c) = ctx
        .select(EntitySpec::select(AUDIO_OUTPUT, "Audio output", Vec::new()).icon("mdi:speaker"));
    forward(c, &tx, |o| Input::Choose(Out, o));
    let (input, c) = ctx
        .select(EntitySpec::select(AUDIO_INPUT, "Audio input", Vec::new()).icon("mdi:microphone"));
    forward(c, &tx, |o| Input::Choose(In, o));
    let (mic_volume, c) = ctx.number(volume_spec(
        MIC_VOLUME,
        "Microphone volume",
        "mdi:microphone",
    ));
    forward(c, &tx, |v| Input::Volume(In, v));
    let (mic_mute, c) =
        ctx.switch(EntitySpec::switch(MIC_MUTE, "Microphone mute").icon("mdi:microphone-off"));
    forward(c, &tx, |m| Input::Mute(In, m));
    let mic_in_use = ctx.binary_sensor(
        EntitySpec::binary_sensor(MIC_IN_USE, "Microphone in use", None)
            .icon("mdi:microphone-message"),
    );
    let headphones = ctx.binary_sensor(
        EntitySpec::binary_sensor(HEADPHONES_CONNECTED, "Headphones", None).icon("mdi:headphones"),
    );
    (
        Entities {
            volume,
            mute,
            output,
            input,
            mic_volume,
            mic_mute,
            mic_in_use,
            headphones,
        },
        rx,
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use deskhand_core::testing::{TestRig, rig_with, wait_attr, wait_state};
    use deskhand_core::{CommandValue, Config, Integration, Probe, State};

    use super::*;

    type Calls = Arc<Mutex<Vec<AudioCmd>>>;

    /// Applies commands to its own state, like a real sound server.
    struct FakeSource {
        state: AudioState,
        changed: bool,
        updates: mpsc::UnboundedReceiver<AudioState>,
        calls: Calls,
    }

    #[async_trait::async_trait]
    impl AudioSource for FakeSource {
        async fn next_change(&mut self) -> Result<AudioState, AudioError> {
            if !std::mem::take(&mut self.changed) {
                self.state = self
                    .updates
                    .recv()
                    .await
                    .ok_or_else(|| AudioError("closed".into()))?;
            }
            Ok(self.state.clone())
        }

        async fn command(&mut self, cmd: AudioCmd) -> Result<(), AudioError> {
            self.calls.lock().unwrap().push(cmd.clone());
            let s = &mut self.state;
            fn devices(s: &mut AudioState, d: Direction) -> &mut Vec<AudioDevice> {
                match d {
                    Direction::Output => &mut s.outputs,
                    Direction::Input => &mut s.inputs,
                }
            }
            match cmd {
                AudioCmd::Volume(d, name, v) => {
                    for dev in devices(s, d).iter_mut().filter(|x| x.name == name) {
                        dev.volume = v;
                    }
                }
                AudioCmd::Mute(d, name, m) => {
                    for dev in devices(s, d).iter_mut().filter(|x| x.name == name) {
                        dev.muted = m;
                    }
                }
                AudioCmd::Default(Direction::Output, name) => s.default_output = Some(name),
                AudioCmd::Default(Direction::Input, name) => s.default_input = Some(name),
            }
            self.changed = true;
            Ok(())
        }
    }

    struct TestAudio(Mutex<Option<FakeSource>>);

    #[async_trait::async_trait]
    impl Integration for TestAudio {
        fn id(&self) -> &'static str {
            "audio.test"
        }
        fn capability(&self) -> &'static str {
            "audio"
        }
        fn description(&self) -> &'static str {
            "test"
        }
        async fn probe(&self) -> Probe {
            Probe::available()
        }
        async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
            let source = self.0.lock().unwrap().take().expect("runs once");
            AudioKit::new(ctx.config::<AudioOptions>()?)
                .run(&ctx, source)
                .await
        }
    }

    fn device(name: &str, description: &str, volume: f64) -> AudioDevice {
        AudioDevice {
            name: name.into(),
            description: description.into(),
            volume,
            ..AudioDevice::default()
        }
    }

    fn recording(app: &str) -> Recording {
        Recording {
            app: app.into(),
            ..Recording::default()
        }
    }

    fn initial() -> AudioState {
        AudioState {
            outputs: vec![
                device("speakers", "Speakers", 0.5),
                AudioDevice {
                    headphones: true,
                    ..device("headset", "Headset", 0.8)
                },
            ],
            inputs: vec![device("mic", "Built-in mic", 1.0)],
            default_output: Some("speakers".into()),
            default_input: Some("mic".into()),
            recordings: vec![],
        }
    }

    fn start(toml: &str) -> (TestRig, mpsc::UnboundedSender<AudioState>, Calls) {
        let (tx, updates) = mpsc::unbounded_channel();
        let calls = Calls::default();
        let source = FakeSource {
            state: initial(),
            changed: true,
            updates,
            calls: calls.clone(),
        };
        let config = Config::from_toml_str(toml, None)
            .unwrap()
            .integration("audio")
            .unwrap();
        let (rig, _cancel) = rig_with(TestAudio(Mutex::new(Some(source))), config);
        (rig, tx, calls)
    }

    fn text(s: &str) -> State {
        State::Text(s.into())
    }

    #[tokio::test]
    async fn controls_and_follows_devices() {
        let (mut t, tx, calls) = start("");
        wait_state(&mut t, &AUDIO_VOLUME, State::Float(50.0)).await;
        wait_state(&mut t, &AUDIO_OUTPUT, text("Speakers")).await;
        wait_state(&mut t, &AUDIO_INPUT, text("Built-in mic")).await;
        wait_state(&mut t, &MIC_VOLUME, State::Float(100.0)).await;
        wait_state(&mut t, &HEADPHONES_CONNECTED, State::Bool(false)).await;
        wait_state(&mut t, &MIC_IN_USE, State::Bool(false)).await;

        t.command(AUDIO_VOLUME.as_str(), CommandValue::Number(35.0));
        wait_state(&mut t, &AUDIO_VOLUME, State::Float(35.0)).await;
        t.command(AUDIO_MUTE.as_str(), CommandValue::Bool(true));
        wait_state(&mut t, &AUDIO_MUTE, State::Bool(true)).await;
        t.command(AUDIO_OUTPUT.as_str(), CommandValue::Text("Headset".into()));
        wait_state(&mut t, &AUDIO_OUTPUT, text("Headset")).await;
        wait_state(&mut t, &HEADPHONES_CONNECTED, State::Bool(true)).await;
        wait_state(&mut t, &AUDIO_VOLUME, State::Float(80.0)).await;
        t.command(MIC_MUTE.as_str(), CommandValue::Bool(true));
        wait_state(&mut t, &MIC_MUTE, State::Bool(true)).await;
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                AudioCmd::Volume(Direction::Output, "speakers".into(), 0.35),
                AudioCmd::Mute(Direction::Output, "speakers".into(), true),
                AudioCmd::Default(Direction::Output, "headset".into()),
                AudioCmd::Mute(Direction::Input, "mic".into(), true),
            ]
        );

        // Changes made elsewhere (a new device, another default) are followed.
        let mut s = initial();
        s.outputs.push(device("hdmi", "Speakers", 1.0));
        s.default_output = Some("hdmi".into());
        tx.send(s).unwrap();
        wait_state(&mut t, &AUDIO_OUTPUT, text("Speakers 2")).await;
        wait_state(&mut t, &AUDIO_VOLUME, State::Float(100.0)).await;

        // No default device: unknown.
        tx.send(AudioState::default()).unwrap();
        wait_state(&mut t, &AUDIO_VOLUME, State::Unknown).await;
        wait_state(&mut t, &MIC_MUTE, State::Unknown).await;
    }

    #[tokio::test]
    async fn mic_in_use_filters_recordings() {
        let (mut t, tx, _calls) = start("[integrations.audio]\nmic_ignore_apps = ['EasyEffects']");
        wait_state(&mut t, &MIC_IN_USE, State::Bool(false)).await;
        let mut s = initial();
        s.recordings = vec![
            Recording {
                monitor: true,
                ..recording("OBS")
            },
            Recording {
                corked: true,
                ..recording("Zoom")
            },
            Recording {
                app_id: "org.PulseAudio.pavucontrol".into(),
                ..recording("Volume Control")
            },
            recording("easyeffects"),
        ];
        tx.send(s.clone()).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        wait_state(&mut t, &MIC_IN_USE, State::Bool(false)).await;

        s.recordings.push(recording("Firefox"));
        s.recordings.push(Recording {
            binary: "chromium".into(),
            ..Recording::default()
        });
        s.recordings.push(recording("Firefox"));
        tx.send(s).unwrap();
        wait_state(&mut t, &MIC_IN_USE, State::Bool(true)).await;
        wait_attr(&mut t, &MIC_IN_USE, "apps", json!(["Firefox", "chromium"])).await;
    }
}
