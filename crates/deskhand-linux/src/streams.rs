//! Playback streams (PulseAudio sink-inputs), shared between the `audio`
//! integration, which reads and sets them, and `media`, which uses them for
//! players whose MPRIS volume is missing or ignored (Chromium-based
//! browsers).
//!
//! [`Streams`] lives in [`LinuxEnv`](crate::LinuxEnv). Without the `audio`
//! integration running (disabled, or built without `audio-pulse`) the list
//! stays empty and setting a volume fails.

use std::sync::Mutex;

use tokio::sync::watch;

/// One playback stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Playback {
    /// Sound-server stream index.
    pub index: u32,
    /// `application.name`, e.g. `Brave`.
    pub app: String,
    /// `application.process.binary`, e.g. `brave`.
    pub binary: String,
    /// `application.process.id`.
    pub pid: Option<u32>,
    /// 1.0 is 100 %.
    pub volume: f64,
    pub muted: bool,
}

type Setter = Box<dyn Fn(u32, f64) -> bool + Send + Sync>;

/// The current playback streams and a way to set their volume.
pub struct Streams {
    list: watch::Sender<Vec<Playback>>,
    setter: Mutex<Option<Setter>>,
}

impl std::fmt::Debug for Streams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Streams")
            .field("list", &*self.list.borrow())
            .finish_non_exhaustive()
    }
}

impl Default for Streams {
    fn default() -> Self {
        Self {
            list: watch::Sender::new(Vec::new()),
            setter: Mutex::new(None),
        }
    }
}

impl Streams {
    /// Changes to the list.
    pub fn subscribe(&self) -> watch::Receiver<Vec<Playback>> {
        self.list.subscribe()
    }

    pub fn current(&self) -> Vec<Playback> {
        self.list.borrow().clone()
    }

    /// Replaces the list (notifies only if it changed).
    pub fn publish(&self, list: Vec<Playback>) {
        self.list.send_if_modified(|cur| {
            if *cur == list {
                false
            } else {
                *cur = list;
                true
            }
        });
    }

    /// Called by the audio integration once it can set stream volumes.
    /// `setter` returns false when the sound-server thread has stopped.
    pub fn attach(&self, setter: impl Fn(u32, f64) -> bool + Send + Sync + 'static) {
        *self.setter.lock().unwrap() = Some(Box::new(setter));
    }

    /// The audio integration stopped: no streams, no setter.
    pub fn detach(&self) {
        *self.setter.lock().unwrap() = None;
        self.publish(Vec::new());
    }

    /// Sets a stream's volume (1.0 is 100 %), keeping its channel balance.
    pub fn set_volume(&self, index: u32, volume: f64) -> Result<(), String> {
        match &*self.setter.lock().unwrap() {
            Some(set) if set(index, volume.max(0.0)) => Ok(()),
            _ => Err("the audio integration is not running".into()),
        }
    }
}

/// The parent processes of `pid`, nearest first (at most 8).
pub fn ancestors(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut cur = pid;
    while out.len() < 8 {
        let Some(parent) = std::fs::read_to_string(format!("/proc/{cur}/status"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("PPid:"))
                    .and_then(|v| v.trim().parse::<u32>().ok())
            })
            .filter(|p| *p > 1)
        else {
            break;
        };
        out.push(parent);
        cur = parent;
    }
    out
}

/// The streams of a player: those of its process or a child of it
/// (Chromium plays audio from a utility process), else those whose
/// application name is the player's name or whose binary is its app id.
/// `ancestors` is [`ancestors`] outside tests.
pub fn for_player<'a>(
    streams: &'a [Playback],
    pid: Option<u32>,
    name: &str,
    app_id: &str,
    ancestors: impl Fn(u32) -> Vec<u32>,
) -> Vec<&'a Playback> {
    if let Some(pid) = pid {
        let by_process: Vec<&Playback> = streams
            .iter()
            .filter(|s| {
                s.pid
                    .is_some_and(|sp| sp == pid || ancestors(sp).contains(&pid))
            })
            .collect();
        if !by_process.is_empty() {
            return by_process;
        }
    }
    // Sandboxed apps (Flatpak) report pids from their own namespace.
    streams
        .iter()
        .filter(|s| {
            (!name.is_empty() && s.app.eq_ignore_ascii_case(name))
                || (!app_id.is_empty() && s.binary.eq_ignore_ascii_case(app_id))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn stream(index: u32, app: &str, binary: &str, pid: Option<u32>) -> Playback {
        Playback {
            index,
            app: app.into(),
            binary: binary.into(),
            pid,
            volume: 0.5,
            muted: false,
        }
    }

    #[test]
    fn matches_by_process_tree_then_name() {
        let streams = vec![
            stream(1, "Brave", "brave", Some(200)),
            stream(2, "Firefox", "firefox", Some(300)),
            stream(3, "Brave", "brave", None),
        ];
        // 200 is a child of the browser (100).
        let tree = |pid: u32| if pid == 200 { vec![100, 50] } else { vec![] };
        let idx = |v: Vec<&Playback>| v.iter().map(|s| s.index).collect::<Vec<_>>();
        assert_eq!(
            idx(for_player(
                &streams,
                Some(100),
                "Brave",
                "brave-browser",
                tree
            )),
            vec![1]
        );
        assert_eq!(
            idx(for_player(&streams, Some(300), "x", "y", tree)),
            vec![2]
        );
        // No process match: by name (Flatpak pids).
        assert_eq!(
            idx(for_player(&streams, Some(999), "brave", "", tree)),
            vec![1, 3]
        );
        assert_eq!(
            idx(for_player(&streams, None, "", "firefox", tree)),
            vec![2]
        );
        assert!(for_player(&streams, None, "", "", tree).is_empty());
    }

    #[test]
    fn own_process_has_ancestors() {
        assert!(!ancestors(std::process::id()).is_empty());
        assert!(ancestors(u32::MAX).is_empty());
    }

    #[test]
    fn setter_and_detach() {
        let s = Streams::default();
        assert!(s.set_volume(1, 0.5).is_err());
        let last = Arc::new(AtomicU32::new(0));
        let l = last.clone();
        s.attach(move |i, v| {
            l.store(i * 100 + (v * 10.0) as u32, Ordering::Relaxed);
            true
        });
        s.set_volume(4, 0.5).unwrap();
        assert_eq!(last.load(Ordering::Relaxed), 405);
        let mut rx = s.subscribe();
        s.publish(vec![stream(1, "a", "a", None)]);
        assert!(rx.has_changed().unwrap());
        rx.mark_unchanged();
        s.publish(vec![stream(1, "a", "a", None)]);
        assert!(!rx.has_changed().unwrap());
        s.detach();
        assert!(s.current().is_empty());
        assert!(s.set_volume(4, 0.5).is_err());
    }
}
