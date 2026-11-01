//! `camera` capability: `camera_in_use` (attributes `apps`, `devices`).
//!
//! inotify open/close events on `/dev/video*` are only the trigger; after
//! a 250 ms settle (which filters out short device probes) `/proc/*/fd` is
//! scanned for open video devices. Only the user's own processes are
//! readable, which is what matters for calls; an app that uses the camera
//! through PipeWire shows up as `pipewire` (docs/dev/spikes.md §7).
//!
//! ```toml
//! [integrations.camera]
//! ignore_processes = ["obs"]   # process names (comm) that don't count
//! ```

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use deskhand_core::{Attributes, Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use futures_util::StreamExt;
use inotify::{Inotify, WatchMask};
use serde::Deserialize;
use serde_json::json;

pub const CAMERA_IN_USE: EntityId = EntityId::from_static("camera_in_use");
const SETTLE: Duration = Duration::from_millis(250);

/// `[integrations.camera]` options.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CameraOptions {
    pub ignore_processes: Vec<String>,
}

/// One process holding a video device open.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CameraUser {
    pub pid: u32,
    pub comm: String,
    pub device: String,
}

/// Every open `/dev/video*` fd under `proc` (normally `/proc`).
pub fn scan(proc: &Path) -> BTreeSet<CameraUser> {
    let mut found = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(proc) else {
        return found;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue; // Another user's process.
        };
        let mut comm = None;
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let target = target.to_string_lossy();
            if !target.starts_with("/dev/video") {
                continue;
            }
            let comm = comm
                .get_or_insert_with(|| {
                    std::fs::read_to_string(entry.path().join("comm"))
                        .unwrap_or_default()
                        .trim()
                        .to_owned()
                })
                .clone();
            found.insert(CameraUser {
                pid,
                comm,
                device: target.into_owned(),
            });
        }
    }
    found
}

fn video_devices(dev: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dev)
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

/// Watches open/close of every video device (adding a watch twice is harmless).
fn watch_devices(watches: &mut inotify::Watches, dev: &Path) {
    for d in video_devices(dev) {
        let mask = WatchMask::OPEN | WatchMask::CLOSE_WRITE | WatchMask::CLOSE_NOWRITE;
        if let Err(e) = watches.add(&d, mask) {
            tracing::debug!(device = %d.display(), error = %e, "cannot watch video device");
        }
    }
}

/// Camera use from `/proc`.
#[derive(Debug)]
pub struct Camera {
    dev: PathBuf,
    proc: PathBuf,
}

impl Camera {
    pub fn new() -> Self {
        Self::with_paths("/dev", "/proc")
    }

    /// Other roots (tests).
    pub fn with_paths(dev: impl Into<PathBuf>, proc: impl Into<PathBuf>) -> Self {
        Self {
            dev: dev.into(),
            proc: proc.into(),
        }
    }
}

impl Default for Camera {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Integration for Camera {
    fn id(&self) -> &'static str {
        "camera.proc"
    }

    fn capability(&self) -> &'static str {
        "camera"
    }

    fn description(&self) -> &'static str {
        "Whether a camera (/dev/video*) is in use, and by which apps"
    }

    async fn probe(&self) -> Probe {
        // Cameras can be plugged in later; /dev and /proc are all we need.
        if self.proc.join("self").exists() && self.dev.is_dir() {
            Probe::available_with(format!(
                "{} video device(s)",
                video_devices(&self.dev).len()
            ))
        } else {
            Probe::unavailable("no /proc or /dev")
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let options = ctx.config::<CameraOptions>()?;
        let inotify = Inotify::init().map_err(IntegrationError::other)?;
        let mut watches = inotify.watches();
        // New devices (a USB camera plugged in) show up as creates in /dev.
        watches
            .add(&self.dev, WatchMask::CREATE | WatchMask::DELETE)
            .map_err(IntegrationError::other)?;
        watch_devices(&mut watches, &self.dev);
        let mut events = inotify
            .into_event_stream([0u8; 4096])
            .map_err(IntegrationError::other)?;

        let sensor = ctx.binary_sensor(
            EntitySpec::binary_sensor(CAMERA_IN_USE, "Camera in use", None).icon("mdi:webcam"),
        );
        let ignore: Vec<String> = options
            .ignore_processes
            .iter()
            .map(|s| s.to_lowercase())
            .collect();
        let proc = self.proc.clone();
        let publish = |users: BTreeSet<CameraUser>| {
            let users: Vec<CameraUser> = users
                .into_iter()
                .filter(|u| !ignore.contains(&u.comm.to_lowercase()))
                .collect();
            let apps: BTreeSet<&str> = users.iter().map(|u| u.comm.as_str()).collect();
            let devices: BTreeSet<&str> = users.iter().map(|u| u.device.as_str()).collect();
            let mut attrs = Attributes::new();
            attrs.insert("apps".into(), json!(apps));
            attrs.insert("devices".into(), json!(devices));
            sensor.set_attributes(attrs);
            sensor.set(!users.is_empty());
        };
        let scan_now = || {
            let proc = proc.clone();
            tokio::task::spawn_blocking(move || scan(&proc))
        };
        publish(scan_now().await.map_err(IntegrationError::other)?);
        ctx.ready();

        let cancel = ctx.cancelled();
        loop {
            let ev = tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                ev = events.next() => ev,
            };
            let Some(ev) = ev else {
                return Err(IntegrationError::msg("inotify stream ended"));
            };
            let ev = ev.map_err(IntegrationError::other)?;
            let mut new_device = ev.mask.contains(inotify::EventMask::CREATE);
            // Settle, taking in whatever else arrives meanwhile.
            let settle = tokio::time::sleep(SETTLE);
            tokio::pin!(settle);
            loop {
                tokio::select! {
                    () = &mut settle => break,
                    ev = events.next() => match ev {
                        Some(Ok(e)) => new_device |= e.mask.contains(inotify::EventMask::CREATE),
                        Some(Err(e)) => return Err(IntegrationError::other(e)),
                        None => return Err(IntegrationError::msg("inotify stream ended")),
                    },
                }
            }
            if new_device {
                watch_devices(&mut watches, &self.dev);
            }
            publish(scan_now().await.map_err(IntegrationError::other)?);
        }
    }
}

#[cfg(test)]
mod tests {
    use deskhand_core::State;

    use super::*;
    use crate::testutil::{rig, wait_attr, wait_state};

    /// A fake `/proc` with one process holding a fake video device.
    fn fake_proc(root: &Path, pid: u32, comm: &str, target: &Path) {
        let p = root.join(pid.to_string());
        std::fs::create_dir_all(p.join("fd")).unwrap();
        std::fs::write(p.join("comm"), format!("{comm}\n")).unwrap();
        std::os::unix::fs::symlink(target, p.join("fd").join("3")).unwrap();
    }

    #[test]
    fn scan_finds_only_video_devices() {
        let dir = tempfile::tempdir().unwrap();
        let proc = dir.path().join("proc");
        fake_proc(&proc, 100, "firefox", Path::new("/dev/video0"));
        fake_proc(&proc, 200, "bash", Path::new("/dev/null"));
        std::fs::create_dir_all(proc.join("self")).unwrap();
        let found = scan(&proc);
        assert_eq!(
            found.into_iter().collect::<Vec<_>>(),
            vec![CameraUser {
                pid: 100,
                comm: "firefox".into(),
                device: "/dev/video0".into()
            }]
        );
        // The real /proc works and sees no camera users in the test environment
        // (or some; either way it must not fail).
        let _ = scan(Path::new("/proc"));
    }

    #[tokio::test]
    async fn follows_opens_and_closes() {
        let dir = tempfile::tempdir().unwrap();
        let dev = dir.path().join("dev");
        let proc = dir.path().join("proc");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::create_dir_all(proc.join("self")).unwrap();
        let video = dev.join("video0");
        std::fs::write(&video, b"").unwrap();
        let integration = Camera::with_paths(&dev, &proc);
        assert!(integration.probe().await.is_available());
        let (mut t, _cancel) = rig(integration, "camera");
        wait_state(&mut t, &CAMERA_IN_USE, State::Bool(false)).await;

        // A process "opens" the camera: the inotify OPEN triggers a scan.
        fake_proc(&proc, 4242, "zoom", Path::new("/dev/video0"));
        drop(std::fs::File::open(&video).unwrap());
        wait_state(&mut t, &CAMERA_IN_USE, State::Bool(true)).await;
        wait_attr(&mut t, &CAMERA_IN_USE, "apps", json!(["zoom"])).await;
        wait_attr(&mut t, &CAMERA_IN_USE, "devices", json!(["/dev/video0"])).await;

        // A camera plugged in later is watched too.
        let video1 = dev.join("video1");
        std::fs::write(&video1, b"").unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        std::fs::remove_dir_all(proc.join("4242")).unwrap();
        fake_proc(&proc, 4343, "cheese", Path::new("/dev/video1"));
        drop(std::fs::File::open(&video1).unwrap());
        wait_attr(&mut t, &CAMERA_IN_USE, "apps", json!(["cheese"])).await;

        // Closed.
        std::fs::remove_dir_all(proc.join("4343")).unwrap();
        drop(std::fs::File::open(&video1).unwrap());
        wait_state(&mut t, &CAMERA_IN_USE, State::Bool(false)).await;
    }
}
