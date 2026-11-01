//! M5 spike: camera in use, detected by scanning `/proc/*/fd` for
//! `/dev/video*`, with inotify open/close events on the device nodes as the
//! trigger (so nothing polls).
//!
//! ```text
//! camera_probe [SECONDS=30]
//! ```
//! Prints every inotify event and the resulting set of (pid, comm, device).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use inotify::{EventMask, Inotify, WatchMask};

/// (pid, comm, device) for every open `/dev/video*` fd we can see.
fn scan() -> (BTreeSet<(u32, String, String)>, usize) {
    let mut found = BTreeSet::new();
    let mut denied = 0;
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return (found, denied);
    };
    for entry in procs.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let fd_dir = entry.path().join("fd");
        let fds = match std::fs::read_dir(&fd_dir) {
            Ok(f) => f,
            Err(_) => {
                denied += 1;
                continue;
            }
        };
        for fd in fds.flatten() {
            if let Ok(target) = std::fs::read_link(fd.path()) {
                let t = target.to_string_lossy();
                if t.starts_with("/dev/video") {
                    let comm = std::fs::read_to_string(entry.path().join("comm"))
                        .unwrap_or_default()
                        .trim()
                        .to_owned();
                    found.insert((pid, comm, t.into_owned()));
                }
            }
        }
    }
    (found, denied)
}

fn video_devices() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir("/dev")
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("video"))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let secs: u64 = std::env::args()
        .nth(1)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(30);
    let devices = video_devices();
    println!("devices: {devices:?}");
    let mut inotify = Inotify::init()?;
    for d in &devices {
        inotify.watches().add(
            d,
            WatchMask::OPEN | WatchMask::CLOSE_WRITE | WatchMask::CLOSE_NOWRITE,
        )?;
    }
    // New devices (USB camera plugged in) show up as /dev creates.
    inotify
        .watches()
        .add(Path::new("/dev"), WatchMask::CREATE | WatchMask::DELETE)?;

    let start = Instant::now();
    let (initial, denied) = scan();
    println!("initial users: {initial:?} ({denied} /proc entries unreadable)");
    let mut last = initial;
    let mut buffer = [0u8; 4096];
    while start.elapsed() < Duration::from_secs(secs) {
        let events: Vec<String> = match inotify.read_events(&mut buffer) {
            Ok(evs) => evs
                .filter(|e| {
                    !e.mask.contains(EventMask::CREATE) && !e.mask.contains(EventMask::DELETE)
                        || e.name
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("video"))
                })
                .map(|e| format!("{:?} {:?}", e.mask, e.name))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if events.is_empty() {
            continue;
        }
        // Let short probes (open + close) settle before scanning.
        std::thread::sleep(Duration::from_millis(250));
        let t = start.elapsed().as_secs_f64();
        let (now, _) = scan();
        println!("{t:7.2}s events {events:?}");
        if now != last {
            println!("{t:7.2}s users: {now:?}");
            last = now;
        }
    }
    Ok(())
}
