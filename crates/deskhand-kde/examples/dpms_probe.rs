//! M11 spike: read and set the screen power state through KWin's
//! `org_kde_kwin_dpms_manager` Wayland protocol.
//!
//! ```text
//! dpms_probe watch [SECONDS=20]     # print supported/mode per output
//! dpms_probe set on|standby|suspend|off [SECONDS=10]
//! ```

use std::time::{Duration, Instant};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_plasma::dpms::client::org_kde_kwin_dpms::{self, OrgKdeKwinDpms};
use wayland_protocols_plasma::dpms::client::org_kde_kwin_dpms_manager::OrgKdeKwinDpmsManager;

struct State {
    start: Instant,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        s: &mut Self,
        _: &wl_output::WlOutput,
        e: wl_output::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name: n } = e {
            println!(
                "{:>6.2}s output {name}: name {n}",
                s.start.elapsed().as_secs_f64()
            );
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
        s: &mut Self,
        _: &OrgKdeKwinDpms,
        e: org_kde_kwin_dpms::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        println!(
            "{:>6.2}s dpms {name}: {e:?}",
            s.start.elapsed().as_secs_f64()
        );
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("watch");
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let list = globals.contents().clone_list();
    for g in list.iter().filter(|g| g.interface.contains("dpms")) {
        println!("global {} v{}", g.interface, g.version);
    }
    let manager: OrgKdeKwinDpmsManager = globals.bind(&qh, 1..=1, ())?;
    let mut dpms = Vec::new();
    for g in list.iter().filter(|g| g.interface == "wl_output") {
        let output: wl_output::WlOutput =
            globals
                .registry()
                .bind(g.name, g.version.min(4), &qh, g.name);
        dpms.push(manager.get(&output, &qh, g.name));
    }
    let mut state = State {
        start: Instant::now(),
    };
    queue.roundtrip(&mut state)?;
    let secs: u64;
    match cmd {
        "watch" => secs = args.get(1).map(|s| s.parse()).transpose()?.unwrap_or(20),
        "set" => {
            let mode = match args.get(1).map(String::as_str) {
                Some("on") => org_kde_kwin_dpms::Mode::On,
                Some("standby") => org_kde_kwin_dpms::Mode::Standby,
                Some("suspend") => org_kde_kwin_dpms::Mode::Suspend,
                Some("off") => org_kde_kwin_dpms::Mode::Off,
                _ => return Err("set on|standby|suspend|off".into()),
            };
            secs = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(10);
            for d in &dpms {
                println!("set {mode:?}");
                d.set(mode as u32);
            }
        }
        other => return Err(format!("unknown command {other}").into()),
    }
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        queue.flush()?;
        queue.dispatch_pending(&mut state)?;
        if let Some(guard) = queue.prepare_read() {
            let _ = guard.read();
        }
        queue.dispatch_pending(&mut state)?;
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}
