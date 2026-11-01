//! `audio` capability on Linux: libpulse (PulseAudio, or PipeWire's pulse
//! server) feeding the platform-neutral [`AudioKit`].
//!
//! libpulse objects are not `Send`, so a dedicated thread owns the
//! mainloop and context. It subscribes to sink, source, source-output and
//! server events and re-reads everything after a burst of events, sending
//! the full [`AudioState`] to the async side. Commands go the other way
//! through a channel the thread checks between short mainloop iterations.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use deskhand_common::audio::{AudioCmd, AudioDevice, AudioError, AudioState, Direction, Recording};
use deskhand_common::{AudioKit, AudioOptions, AudioSource};
use deskhand_core::{Ctx, Integration, IntegrationError, Probe};
use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::subscribe::InterestMaskSet;
use libpulse_binding::context::{Context, FlagSet, State};
use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};
use libpulse_binding::operation::{Operation, State as OpState};
use libpulse_binding::proplist::{Proplist, properties};
use libpulse_binding::time::MicroSeconds;
use libpulse_binding::volume::{ChannelVolumes, Volume};
use tokio::sync::mpsc;

/// Longest the thread blocks in the mainloop before checking for commands.
const TICK: Duration = Duration::from_millis(50);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_NAME: &str = "Deskhand";

/// Audio through libpulse.
#[derive(Debug, Default)]
pub struct AudioPulse;

impl AudioPulse {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Integration for AudioPulse {
    fn id(&self) -> &'static str {
        "audio.pulse"
    }

    fn capability(&self) -> &'static str {
        "audio"
    }

    fn description(&self) -> &'static str {
        "Volume, mute, devices, microphone use and headphones through PulseAudio/PipeWire"
    }

    async fn probe(&self) -> Probe {
        let r = tokio::task::spawn_blocking(|| {
            connect().map(|(_, ctx)| ctx.get_server().unwrap_or_default())
        })
        .await;
        match r {
            Ok(Ok(server)) => Probe::available_with(server),
            Ok(Err(e)) => Probe::unavailable(e),
            Err(e) => Probe::unavailable(e.to_string()),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let options = ctx.config::<AudioOptions>()?;
        let source = PulseSource::start().map_err(IntegrationError::other)?;
        AudioKit::new(options).run(&ctx, source).await
    }
}

/// The async side of the pulse thread.
pub struct PulseSource {
    states: mpsc::UnboundedReceiver<Result<AudioState, String>>,
    commands: std_mpsc::Sender<AudioCmd>,
    stop: Arc<AtomicBool>,
}

impl std::fmt::Debug for PulseSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PulseSource").finish_non_exhaustive()
    }
}

impl PulseSource {
    pub fn start() -> Result<Self, AudioError> {
        let (state_tx, states) = mpsc::unbounded_channel();
        let (commands, cmd_rx) = std_mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        std::thread::Builder::new()
            .name("deskhand-pulse".into())
            .spawn(move || {
                if let Err(e) = pulse_thread(&state_tx, &cmd_rx, &thread_stop) {
                    let _ = state_tx.send(Err(e));
                }
            })
            .map_err(|e| AudioError(e.to_string()))?;
        Ok(Self {
            states,
            commands,
            stop,
        })
    }
}

impl Drop for PulseSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[async_trait::async_trait]
impl AudioSource for PulseSource {
    async fn next_change(&mut self) -> Result<AudioState, AudioError> {
        match self.states.recv().await {
            Some(Ok(s)) => Ok(s),
            Some(Err(e)) => Err(AudioError(e)),
            None => Err(AudioError("pulse thread stopped".into())),
        }
    }

    async fn command(&mut self, cmd: AudioCmd) -> Result<(), AudioError> {
        self.commands
            .send(cmd)
            .map_err(|_| AudioError("pulse thread stopped".into()))
    }
}

fn connect() -> Result<(Mainloop, Context), String> {
    let mut mainloop = Mainloop::new().ok_or("cannot create a pulse mainloop")?;
    let mut proplist = Proplist::new().ok_or("cannot create a proplist")?;
    let _ = proplist.set_str(properties::APPLICATION_NAME, CLIENT_NAME);
    let _ = proplist.set_str(properties::APPLICATION_ID, deskhand_core::APP_ID);
    let mut context = Context::new_with_proplist(&mainloop, CLIENT_NAME, &proplist)
        .ok_or("cannot create a pulse context")?;
    // Never start a sound server ourselves.
    context
        .connect(None, FlagSet::NOAUTOSPAWN, None)
        .map_err(|e| format!("cannot connect to the sound server: {e}"))?;
    let start = Instant::now();
    loop {
        match context.get_state() {
            State::Ready => return Ok((mainloop, context)),
            State::Failed | State::Terminated => {
                return Err(format!(
                    "cannot connect to the sound server: {}",
                    context.errno()
                ));
            }
            _ => {}
        }
        if start.elapsed() > CONNECT_TIMEOUT {
            return Err("timed out connecting to the sound server".into());
        }
        iterate(&mut mainloop, TICK)?;
    }
}

/// One mainloop iteration, blocking at most `timeout`.
fn iterate(mainloop: &mut Mainloop, timeout: Duration) -> Result<(), String> {
    mainloop
        .prepare(Some(MicroSeconds(timeout.as_micros() as u64)))
        .map_err(|e| format!("pulse mainloop: {e}"))?;
    mainloop
        .poll()
        .map_err(|e| format!("pulse mainloop: {e}"))?;
    mainloop
        .dispatch()
        .map_err(|e| format!("pulse mainloop: {e}"))?;
    Ok(())
}

/// Runs the mainloop until `op` finishes.
fn wait<T: ?Sized>(mainloop: &mut Mainloop, op: &Operation<T>) -> Result<(), String> {
    while op.get_state() == OpState::Running {
        match mainloop.iterate(true) {
            IterateResult::Success(_) => {}
            other => return Err(format!("pulse mainloop: {other:?}")),
        }
    }
    Ok(())
}

/// Last known channel volumes per device, so a new level keeps the balance.
type Volumes = HashMap<String, ChannelVolumes>;

fn pulse_thread(
    tx: &mpsc::UnboundedSender<Result<AudioState, String>>,
    commands: &std_mpsc::Receiver<AudioCmd>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let (mut mainloop, mut context) = connect()?;
    tracing::debug!(server = ?context.get_server(), "connected to the sound server");
    let dirty = Rc::new(Cell::new(true));
    let d = dirty.clone();
    context.set_subscribe_callback(Some(Box::new(move |_, _, _| d.set(true))));
    let op = context.subscribe(
        InterestMaskSet::SINK
            | InterestMaskSet::SOURCE
            | InterestMaskSet::SOURCE_OUTPUT
            | InterestMaskSet::SERVER
            | InterestMaskSet::CARD,
        |_| {},
    );
    wait(&mut mainloop, &op)?;

    let mut volumes = Volumes::new();
    while !stop.load(Ordering::Relaxed) {
        while let Ok(cmd) = commands.try_recv() {
            apply(&mut mainloop, &mut context, &volumes, &cmd)?;
            dirty.set(true);
        }
        if dirty.replace(false) {
            let state = query(&mut mainloop, &context, &mut volumes)?;
            if tx.send(Ok(state)).is_err() {
                break;
            }
        }
        iterate(&mut mainloop, TICK)?;
        if !matches!(context.get_state(), State::Ready) {
            return Err("lost the connection to the sound server".into());
        }
    }
    context.disconnect();
    Ok(())
}

fn apply(
    mainloop: &mut Mainloop,
    context: &mut Context,
    volumes: &Volumes,
    cmd: &AudioCmd,
) -> Result<(), String> {
    let mut introspect = context.introspect();
    match cmd {
        AudioCmd::Volume(dir, name, v) => {
            let level = Volume((v * f64::from(Volume::NORMAL.0)).round() as u32);
            let mut cv = volumes.get(name).copied().unwrap_or_default();
            if cv.scale(level).is_none() {
                cv.set(2, level);
            }
            let op = match dir {
                Direction::Output => introspect.set_sink_volume_by_name(name, &cv, None),
                Direction::Input => introspect.set_source_volume_by_name(name, &cv, None),
            };
            wait(mainloop, &op)
        }
        AudioCmd::Mute(dir, name, m) => {
            let op = match dir {
                Direction::Output => introspect.set_sink_mute_by_name(name, *m, None),
                Direction::Input => introspect.set_source_mute_by_name(name, *m, None),
            };
            wait(mainloop, &op)
        }
        AudioCmd::Default(dir, name) => {
            let op = match dir {
                Direction::Output => context.set_default_sink(name, |_| {}),
                Direction::Input => context.set_default_source(name, |_| {}),
            };
            wait(mainloop, &op)
        }
    }
}

fn level(cv: &ChannelVolumes) -> f64 {
    f64::from(cv.max().0) / f64::from(Volume::NORMAL.0)
}

/// Headphones by active port name (`analog-output-headphones`,
/// `headset-output`) or Bluetooth form factor.
pub fn is_headphones(port: Option<&str>, form_factor: Option<&str>) -> bool {
    let port = port.unwrap_or_default().to_ascii_lowercase();
    port.contains("headphone")
        || port.contains("headset")
        || matches!(form_factor, Some("headphone" | "headset"))
}

fn query(
    mainloop: &mut Mainloop,
    context: &Context,
    volumes: &mut Volumes,
) -> Result<AudioState, String> {
    let introspect = context.introspect();
    let state = Rc::new(RefCell::new(AudioState::default()));
    let vols: Rc<RefCell<Volumes>> = Rc::default();
    let monitors: Rc<RefCell<BTreeSet<u32>>> = Rc::default();

    let s = state.clone();
    let op = introspect.get_server_info(move |info| {
        let mut s = s.borrow_mut();
        s.default_output = info.default_sink_name.as_deref().map(str::to_owned);
        s.default_input = info.default_source_name.as_deref().map(str::to_owned);
    });
    wait(mainloop, &op)?;

    let (s, v) = (state.clone(), vols.clone());
    let op = introspect.get_sink_info_list(move |r| {
        if let ListResult::Item(i) = r {
            let name = i.name.as_deref().unwrap_or_default().to_owned();
            let port = i.active_port.as_ref().and_then(|p| p.name.as_deref());
            let form_factor = i.proplist.get_str(properties::DEVICE_FORM_FACTOR);
            v.borrow_mut().insert(name.clone(), i.volume);
            s.borrow_mut().outputs.push(AudioDevice {
                description: i.description.as_deref().unwrap_or_default().to_owned(),
                volume: level(&i.volume),
                muted: i.mute,
                headphones: is_headphones(port, form_factor.as_deref()),
                name,
            });
        }
    });
    wait(mainloop, &op)?;

    let (s, v, m) = (state.clone(), vols.clone(), monitors.clone());
    let op = introspect.get_source_info_list(move |r| {
        if let ListResult::Item(i) = r {
            if i.monitor_of_sink.is_some() {
                m.borrow_mut().insert(i.index);
                return;
            }
            let name = i.name.as_deref().unwrap_or_default().to_owned();
            v.borrow_mut().insert(name.clone(), i.volume);
            s.borrow_mut().inputs.push(AudioDevice {
                description: i.description.as_deref().unwrap_or_default().to_owned(),
                volume: level(&i.volume),
                muted: i.mute,
                headphones: false,
                name,
            });
        }
    });
    wait(mainloop, &op)?;

    let (s, m) = (state.clone(), monitors.clone());
    let op = introspect.get_source_output_info_list(move |r| {
        if let ListResult::Item(o) = r {
            let prop = |k| o.proplist.get_str(k).unwrap_or_default();
            s.borrow_mut().recordings.push(Recording {
                app: prop(properties::APPLICATION_NAME),
                app_id: prop(properties::APPLICATION_ID),
                binary: prop(properties::APPLICATION_PROCESS_BINARY),
                corked: o.corked,
                monitor: m.borrow().contains(&o.source),
            });
        }
    });
    wait(mainloop, &op)?;

    *volumes = vols.take();
    let mut out = state.take();
    // Our own client never records, but be safe against self-reporting.
    out.recordings.retain(|r| r.app != CLIENT_NAME);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headphone_detection() {
        assert!(is_headphones(Some("analog-output-headphones"), None));
        assert!(is_headphones(Some("headset-output"), None));
        assert!(is_headphones(None, Some("headset")));
        assert!(!is_headphones(
            Some("analog-output-speaker"),
            Some("internal")
        ));
        assert!(!is_headphones(None, None));
    }
}
