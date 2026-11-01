//! `dpms` capability: screen power.
//!
//! - `dpms.kwin`: a `screens_on` switch through KWin's
//!   `org_kde_kwin_dpms_manager` Wayland protocol, which reports each
//!   output's mode and sets it (docs/dev/spikes.md §5). On while any output
//!   is on; switching sets every output. Any input also turns the screens
//!   back on, and that is reported. Attribute `outputs`: mode per output.
//! - `dpms.powerdevil` (fallback, e.g. X11): a `screens_off` button that
//!   invokes PowerDevil's "Turn Off Screen" shortcut.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use deskhand_core::{Attributes, Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use deskhand_linux::LinuxEnv;
use deskhand_linux::util::dbus_err;
use serde_json::json;
use tokio::sync::mpsc;

use crate::shortcuts::{KGLOBALACCEL, invoke_shortcut};

pub const SCREENS_ON: EntityId = EntityId::from_static("screens_on");
pub const SCREENS_OFF: EntityId = EntityId::from_static("screens_off");

/// An output's power mode, as the protocol reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    On,
    Standby,
    Suspend,
    Off,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Standby => "standby",
            Self::Suspend => "suspend",
            Self::Off => "off",
        }
    }
}

/// Output name (or `output<N>` until KWin names it) → mode, for outputs
/// that support DPMS.
pub type Modes = BTreeMap<String, Mode>;

/// `screens_on` and its attributes for `modes`; `None` without outputs.
pub fn summarize(modes: &Modes) -> Option<(bool, Attributes)> {
    if modes.is_empty() {
        return None;
    }
    let mut attrs = Attributes::new();
    attrs.insert(
        "outputs".into(),
        json!(
            modes
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str()))
                .collect::<BTreeMap<_, _>>()
        ),
    );
    Some((modes.values().any(|m| *m == Mode::On), attrs))
}

mod wayland {
    use std::collections::BTreeMap;
    use std::os::fd::AsFd;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use rustix::event::{PollFd, PollFlags, Timespec};
    use tokio::sync::mpsc;
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_output, wl_registry};
    use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
    use wayland_protocols_plasma::dpms::client::org_kde_kwin_dpms::{self, OrgKdeKwinDpms};
    use wayland_protocols_plasma::dpms::client::org_kde_kwin_dpms_manager::OrgKdeKwinDpmsManager;

    use super::{Mode, Modes};

    pub(super) const MANAGER: &str = "org_kde_kwin_dpms_manager";

    #[derive(Default)]
    struct Output {
        name: Option<String>,
        supported: bool,
        mode: Option<Mode>,
        output: Option<wl_output::WlOutput>,
        dpms: Option<OrgKdeKwinDpms>,
    }

    struct State {
        manager: OrgKdeKwinDpmsManager,
        /// Registry name → output.
        outputs: BTreeMap<u32, Output>,
        tx: mpsc::UnboundedSender<Modes>,
        last: Option<Modes>,
    }

    impl State {
        fn add(
            &mut self,
            registry: &wl_registry::WlRegistry,
            name: u32,
            version: u32,
            qh: &QueueHandle<Self>,
        ) {
            let output: wl_output::WlOutput = registry.bind(name, version.clamp(1, 4), qh, name);
            let dpms = self.manager.get(&output, qh, name);
            self.outputs.insert(
                name,
                Output {
                    output: Some(output),
                    dpms: Some(dpms),
                    ..Output::default()
                },
            );
        }

        fn modes(&self) -> Modes {
            self.outputs
                .iter()
                .filter(|(_, o)| o.supported)
                .filter_map(|(n, o)| {
                    let name = o.name.clone().unwrap_or_else(|| format!("output{n}"));
                    Some((name, o.mode?))
                })
                .collect()
        }

        fn publish(&mut self) {
            let modes = self.modes();
            if self.last.as_ref() != Some(&modes) {
                let _ = self.tx.send(modes.clone());
                self.last = Some(modes);
            }
        }

        fn set_all(&self, on: bool) {
            let mode = if on {
                org_kde_kwin_dpms::Mode::On
            } else {
                org_kde_kwin_dpms::Mode::Off
            };
            for o in self.outputs.values().filter(|o| o.supported) {
                if let Some(d) = &o.dpms {
                    d.set(mode as u32);
                }
            }
        }
    }

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            match event {
                wl_registry::Event::Global {
                    name,
                    interface,
                    version,
                } if interface == wl_output::WlOutput::interface().name => {
                    state.add(registry, name, version, qh);
                }
                wl_registry::Event::GlobalRemove { name } => {
                    if let Some(o) = state.outputs.remove(&name) {
                        if let Some(d) = o.dpms {
                            d.release();
                        }
                        if let Some(out) = o.output
                            && out.version() >= 3
                        {
                            out.release();
                        }
                        state.publish();
                    }
                }
                _ => {}
            }
        }
    }

    impl Dispatch<wl_output::WlOutput, u32> for State {
        fn event(
            state: &mut Self,
            _: &wl_output::WlOutput,
            event: wl_output::Event,
            name: &u32,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wl_output::Event::Name { name: n } = event
                && let Some(o) = state.outputs.get_mut(name)
            {
                o.name = Some(n);
            }
        }
    }

    impl Dispatch<OrgKdeKwinDpmsManager, ()> for State {
        fn event(
            _: &mut Self,
            _: &OrgKdeKwinDpmsManager,
            _: <OrgKdeKwinDpmsManager as Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<OrgKdeKwinDpms, u32> for State {
        fn event(
            state: &mut Self,
            _: &OrgKdeKwinDpms,
            event: org_kde_kwin_dpms::Event,
            name: &u32,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let Some(o) = state.outputs.get_mut(name) else {
                return;
            };
            match event {
                org_kde_kwin_dpms::Event::Supported { supported } => o.supported = supported != 0,
                org_kde_kwin_dpms::Event::Mode { mode } => {
                    o.mode = match mode {
                        // The protocol declares the mode as a plain uint.
                        0 => Some(Mode::On),
                        1 => Some(Mode::Standby),
                        2 => Some(Mode::Suspend),
                        3 => Some(Mode::Off),
                        _ => None,
                    }
                }
                // Mode and Supported come in batches ended by Done.
                org_kde_kwin_dpms::Event::Done => state.publish(),
                _ => {}
            }
        }
    }

    /// Whether the compositor offers the protocol.
    pub(super) fn probe() -> Result<(), String> {
        let conn = Connection::connect_to_env().map_err(|e| format!("no Wayland display: {e}"))?;
        let (globals, _queue) = registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
        if globals
            .contents()
            .clone_list()
            .iter()
            .any(|g| g.interface == MANAGER)
        {
            Ok(())
        } else {
            Err(format!("compositor does not offer {MANAGER}"))
        }
    }

    /// Blocking event loop; returns when `stop` is set or the connection fails.
    pub(super) fn run(
        tx: mpsc::UnboundedSender<Modes>,
        commands: std::sync::mpsc::Receiver<bool>,
        stop: Arc<AtomicBool>,
    ) -> Result<(), String> {
        let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
        let qh = queue.handle();
        let manager: OrgKdeKwinDpmsManager =
            globals.bind(&qh, 1..=1, ()).map_err(|e| e.to_string())?;
        let mut state = State {
            manager,
            outputs: BTreeMap::new(),
            tx,
            last: None,
        };
        for g in globals.contents().clone_list() {
            if g.interface == wl_output::WlOutput::interface().name {
                state.add(globals.registry(), g.name, g.version, &qh);
            }
        }
        // Commands are checked between reads, so keep the wait short.
        let wait = Timespec {
            tv_sec: 0,
            tv_nsec: 100_000_000,
        };
        while !stop.load(Ordering::Relaxed) && !state.tx.is_closed() {
            while let Ok(on) = commands.try_recv() {
                state.set_all(on);
            }
            queue
                .dispatch_pending(&mut state)
                .map_err(|e| e.to_string())?;
            queue.flush().map_err(|e| e.to_string())?;
            let Some(guard) = queue.prepare_read() else {
                continue;
            };
            let readable = {
                let fd = guard.connection_fd();
                let fd = fd.as_fd();
                let mut fds = [PollFd::new(&fd, PollFlags::IN)];
                matches!(rustix::event::poll(&mut fds, Some(&wait)), Ok(n) if n > 0)
            };
            if readable {
                guard.read().map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
}

/// `screens_on` switch through KWin's DPMS protocol.
#[derive(Debug)]
pub struct DpmsKwin {
    env: Arc<LinuxEnv>,
}

impl DpmsKwin {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }
}

#[async_trait::async_trait]
impl Integration for DpmsKwin {
    fn id(&self) -> &'static str {
        "dpms.kwin"
    }

    fn capability(&self) -> &'static str {
        "dpms"
    }

    fn description(&self) -> &'static str {
        "Screens on/off switch through KWin's DPMS Wayland protocol"
    }

    async fn probe(&self) -> Probe {
        if !self.env.desktop_is("KDE") {
            return Probe::unavailable("not a KDE Plasma session");
        }
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Probe::unavailable("not a Wayland session (WAYLAND_DISPLAY unset)");
        }
        match tokio::task::spawn_blocking(wayland::probe).await {
            Ok(Ok(())) => Probe::available(),
            Ok(Err(e)) => Probe::unavailable(e),
            Err(e) => Probe::unavailable(e.to_string()),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("deskhand-dpms".into())
            .spawn(move || {
                if let Err(e) = wayland::run(tx, cmd_rx, thread_stop) {
                    tracing::warn!(error = %e, "Wayland DPMS thread stopped");
                }
            })
            .map_err(IntegrationError::other)?;
        let (switch, mut commands) =
            ctx.switch(EntitySpec::switch(SCREENS_ON, "Screens").icon("mdi:monitor"));
        let apply = |modes: &Modes| match summarize(modes) {
            Some((on, attrs)) => {
                switch.set_attributes(attrs);
                switch.set(on);
            }
            None => switch.set_unknown(),
        };
        // The first batch arrives after one roundtrip.
        match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await {
            Ok(Some(m)) => apply(&m),
            Ok(None) => return Err(IntegrationError::msg("Wayland DPMS thread stopped")),
            Err(_) => switch.set_unknown(),
        }
        ctx.ready();

        let cancel = ctx.cancelled();
        let res = loop {
            tokio::select! {
                () = cancel.cancelled() => break Ok(()),
                m = rx.recv() => match m {
                    Some(m) => apply(&m),
                    None => break Err(IntegrationError::msg("Wayland DPMS thread stopped")),
                },
                Some(on) = commands.recv() => {
                    tracing::info!(on, "screens from Home Assistant");
                    let _ = cmd_tx.send(on);
                }
            }
        };
        stop.store(true, Ordering::Relaxed);
        let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        res
    }
}

const POWERDEVIL: &str = "org_kde_powerdevil";
const TURN_OFF_SCREEN: &str = "Turn Off Screen";

/// `screens_off` button through PowerDevil's shortcut.
#[derive(Debug)]
pub struct DpmsPowerDevil {
    env: Arc<LinuxEnv>,
}

impl DpmsPowerDevil {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }
}

#[async_trait::async_trait]
impl Integration for DpmsPowerDevil {
    fn id(&self) -> &'static str {
        "dpms.powerdevil"
    }

    fn capability(&self) -> &'static str {
        "dpms"
    }

    fn description(&self) -> &'static str {
        "Screens off button through PowerDevil's \"Turn Off Screen\" shortcut"
    }

    async fn probe(&self) -> Probe {
        if !self.env.desktop_is("KDE") {
            return Probe::unavailable("not a KDE Plasma session");
        }
        match self.env.session_bus().await {
            Ok(c) if LinuxEnv::has_owner(&c, KGLOBALACCEL).await => Probe::available(),
            Ok(_) => Probe::unavailable("kglobalaccel is not on the session bus"),
            Err(e) => Probe::unavailable(format!("session bus: {e}")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let conn = self.env.session_bus().await.map_err(dbus_err)?;
        let mut presses =
            ctx.button(EntitySpec::button(SCREENS_OFF, "Screens off").icon("mdi:monitor-off"));
        ctx.ready();
        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                p = presses.recv() => {
                    if p.is_none() {
                        return Ok(());
                    }
                    if let Err(e) = invoke_shortcut(&conn, POWERDEVIL, TURN_OFF_SCREEN).await {
                        tracing::warn!(error = %e, "turning the screens off failed");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::CommandValue;
    use deskhand_core::testing::{rig, wait_entity};
    use deskhand_testkit::mocks::MockKGlobalAccel;
    use deskhand_testkit::{TestBus, require_dbus};

    use super::*;

    #[test]
    fn summary() {
        assert_eq!(summarize(&Modes::new()), None);
        let mut modes = Modes::from([("DP-1".into(), Mode::Off), ("HDMI-A-1".into(), Mode::On)]);
        let (on, attrs) = summarize(&modes).unwrap();
        assert!(on);
        assert_eq!(attrs["outputs"], json!({"DP-1": "off", "HDMI-A-1": "on"}));
        modes.insert("HDMI-A-1".into(), Mode::Standby);
        assert!(!summarize(&modes).unwrap().0);
    }

    #[tokio::test]
    async fn powerdevil_button_invokes_the_shortcut() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let accel = MockKGlobalAccel::start(&bus).await.unwrap();
        accel
            .add_component(POWERDEVIL, &[TURN_OFF_SCREEN])
            .await
            .unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &["KDE"]);
        let integration = DpmsPowerDevil::new(env);
        assert!(integration.probe().await.is_available());
        let (mut t, _cancel) = rig(integration, "dpms");
        wait_entity(&mut t, &SCREENS_OFF, true).await;
        t.command(SCREENS_OFF.as_str(), CommandValue::Press);
        let want = format!("invokeShortcut({POWERDEVIL},{TURN_OFF_SCREEN})");
        for _ in 0..50 {
            if accel.calls().contains(&want) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("not invoked: {:?}", accel.calls());
    }
}
