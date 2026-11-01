//! M5 spike: "microphone in use" from libpulse source outputs (works against
//! PulseAudio and PipeWire's pulse server).
//!
//! ```text
//! mic_probe [SECONDS=10] [--ignore app1,app2]
//! ```
//! Every second, lists all source outputs (recording streams) with the
//! fields the filter needs and whether each one counts:
//! - streams recording from a monitor source (desktop audio capture) don't count;
//! - corked (paused) streams don't count;
//! - streams from ignored applications (volume meters, pavucontrol) don't count.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::{Context, FlagSet, State};
use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};
use libpulse_binding::operation::{Operation, State as OpState};
use libpulse_binding::proplist::properties;

#[derive(Debug, Clone)]
struct Output {
    index: u32,
    app: String,
    binary: String,
    source: u32,
    corked: bool,
}

fn wait<T: ?Sized>(mainloop: &mut Mainloop, op: Operation<T>) -> Result<(), String> {
    while op.get_state() == OpState::Running {
        match mainloop.iterate(true) {
            IterateResult::Success(_) => {}
            other => return Err(format!("mainloop: {other:?}")),
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let secs: u64 = args
        .first()
        .filter(|a| !a.starts_with("--"))
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(10);
    let ignore: Vec<String> = args
        .iter()
        .position(|a| a == "--ignore")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.split(',').map(str::to_lowercase).collect())
        .unwrap_or_default();

    let mut mainloop = Mainloop::new().ok_or("no mainloop")?;
    let mut context = Context::new(&mainloop, "deskhand-mic-probe").ok_or("no context")?;
    context.connect(None, FlagSet::NOFLAGS, None)?;
    loop {
        mainloop.iterate(true);
        match context.get_state() {
            State::Ready => break,
            State::Failed | State::Terminated => return Err("cannot connect to pulse".into()),
            _ => {}
        }
    }
    println!("connected to {:?}", context.get_server());

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        let monitors: Rc<RefCell<BTreeMap<u32, (String, bool)>>> = Rc::default();
        let m = monitors.clone();
        let op = context.introspect().get_source_info_list(move |r| {
            if let ListResult::Item(s) = r {
                let name = s.name.as_deref().unwrap_or("").to_owned();
                m.borrow_mut()
                    .insert(s.index, (name, s.monitor_of_sink.is_some()));
            }
        });
        wait(&mut mainloop, op)?;

        let outputs: Rc<RefCell<Vec<Output>>> = Rc::default();
        let o = outputs.clone();
        let op = context.introspect().get_source_output_info_list(move |r| {
            if let ListResult::Item(s) = r {
                o.borrow_mut().push(Output {
                    index: s.index,
                    app: s
                        .proplist
                        .get_str(properties::APPLICATION_NAME)
                        .unwrap_or_default(),
                    binary: s
                        .proplist
                        .get_str(properties::APPLICATION_PROCESS_BINARY)
                        .unwrap_or_default(),
                    source: s.source,
                    corked: s.corked,
                });
            }
        });
        wait(&mut mainloop, op)?;

        let t = start.elapsed().as_secs_f64();
        let sources = monitors.borrow();
        let mut in_use = Vec::new();
        for out in outputs.borrow().iter() {
            let (source_name, is_monitor) = sources
                .get(&out.source)
                .cloned()
                .unwrap_or_else(|| ("?".into(), false));
            let ignored = ignore.contains(&out.app.to_lowercase())
                || ignore.contains(&out.binary.to_lowercase());
            let counts = !is_monitor && !out.corked && !ignored;
            println!(
                "{t:6.1}s  #{} app={:?} binary={:?} source={source_name} monitor={is_monitor} corked={} ignored={ignored} -> {}",
                out.index,
                out.app,
                out.binary,
                out.corked,
                if counts { "COUNTS" } else { "skip" }
            );
            if counts {
                in_use.push(out.app.clone());
            }
        }
        println!(
            "{t:6.1}s  mic_in_use={} apps={in_use:?}",
            !in_use.is_empty()
        );
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}
