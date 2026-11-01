//! M5 spike: Wayland `ext_idle_notifier_v1`.
//!
//! Lists the compositor's globals (name and version), then prints `idle` /
//! `resumed` events for a notification with the given timeout.
//!
//! ```text
//! idle_probe [timeout_ms=1000] [--legacy]
//! ```
//! Without `--legacy` it uses v2 `get_input_idle_notification`, which
//! ignores idle inhibitors (a playing video); `--legacy` uses
//! `get_idle_notification`, which honours them. Needs WAYLAND_DISPLAY and
//! XDG_RUNTIME_DIR. Stop with Ctrl-C.

use std::time::Instant;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

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

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: <ExtIdleNotifierV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let t = state.start.elapsed().as_secs_f64();
        match event {
            ext_idle_notification_v1::Event::Idled => println!("{t:8.2}s idle"),
            ext_idle_notification_v1::Event::Resumed => println!("{t:8.2}s resumed"),
            _ => {}
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let legacy = args.iter().any(|a| a == "--legacy");
    let timeout_ms: u32 = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(|a| a.parse())
        .transpose()?
        .unwrap_or(1000);

    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    println!("globals:");
    let mut list = globals.contents().clone_list();
    list.sort_by(|a, b| a.interface.cmp(&b.interface));
    for g in &list {
        println!("  {} v{}", g.interface, g.version);
    }

    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ())?;
    let notifier: ExtIdleNotifierV1 = globals.bind(&qh, 1..=2, ())?;
    let version = notifier.version();
    let _notification = if version >= 2 && !legacy {
        println!("using get_input_idle_notification (v{version}), timeout {timeout_ms} ms");
        notifier.get_input_idle_notification(timeout_ms, &seat, &qh, ())
    } else {
        println!("using get_idle_notification (v{version}), timeout {timeout_ms} ms");
        notifier.get_idle_notification(timeout_ms, &seat, &qh, ())
    };
    let mut state = State {
        start: Instant::now(),
    };
    loop {
        queue.blocking_dispatch(&mut state)?;
    }
}
