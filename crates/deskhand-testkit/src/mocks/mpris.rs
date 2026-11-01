//! Mock MPRIS player (`org.mpris.MediaPlayer2.<name>` on the session bus).
//! Each player has its own connection; [`MockMpris::quit`] drops it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

use crate::TestBus;

pub const PATH: &str = "/org/mpris/MediaPlayer2";

#[derive(Debug, Clone)]
struct State {
    identity: String,
    status: String,
    volume: f64,
    position_us: i64,
    title: String,
    artist: Vec<String>,
    length_us: i64,
    art_url: String,
    track: u32,
    calls: Vec<String>,
}

type Shared = Arc<Mutex<State>>;

struct Root {
    state: Shared,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    #[zbus(property)]
    async fn identity(&self) -> String {
        self.state.lock().unwrap().identity.clone()
    }

    #[zbus(property)]
    async fn can_quit(&self) -> bool {
        false
    }
}

struct Player {
    state: Shared,
}

impl Player {
    fn record(&self, call: String) {
        self.state.lock().unwrap().calls.push(call);
    }
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    async fn play_pause(
        &self,
        #[zbus(signal_emitter)] e: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        self.record("PlayPause".into());
        {
            let mut s = self.state.lock().unwrap();
            s.status = if s.status == "Playing" {
                "Paused"
            } else {
                "Playing"
            }
            .into();
        }
        self.playback_status_changed(&e).await?;
        Ok(())
    }

    async fn play(&self) {
        self.record("Play".into());
    }

    async fn pause(&self) {
        self.record("Pause".into());
    }

    async fn stop(&self) {
        self.record("Stop".into());
    }

    async fn next(&self, #[zbus(signal_emitter)] e: SignalEmitter<'_>) -> zbus::fdo::Result<()> {
        self.record("Next".into());
        {
            let mut s = self.state.lock().unwrap();
            s.track += 1;
            s.title = format!("Track {}", s.track);
        }
        self.metadata_changed(&e).await?;
        Ok(())
    }

    async fn previous(&self) {
        self.record("Previous".into());
    }

    async fn seek(
        &self,
        offset: i64,
        #[zbus(signal_emitter)] e: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        self.record(format!("Seek({offset})"));
        let pos = {
            let mut s = self.state.lock().unwrap();
            s.position_us = (s.position_us + offset).max(0);
            s.position_us
        };
        Self::seeked(&e, pos).await?;
        Ok(())
    }

    async fn set_position(
        &self,
        track: ObjectPath<'_>,
        position: i64,
        #[zbus(signal_emitter)] e: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        self.record(format!("SetPosition({track},{position})"));
        self.state.lock().unwrap().position_us = position;
        Self::seeked(&e, position).await?;
        Ok(())
    }

    #[zbus(signal)]
    async fn seeked(e: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

    #[zbus(property)]
    async fn playback_status(&self) -> String {
        self.state.lock().unwrap().status.clone()
    }

    #[zbus(property)]
    async fn metadata(&self) -> HashMap<String, OwnedValue> {
        let s = self.state.lock().unwrap().clone();
        let mut m = HashMap::new();
        let mut put = |k: &str, v: Value<'_>| {
            m.insert(k.to_owned(), v.try_to_owned().expect("plain value"));
        };
        let track = ObjectPath::try_from(format!("/org/mock/track/{}", s.track)).expect("path");
        put("mpris:trackid", Value::from(track));
        put("xesam:title", Value::from(s.title));
        put("xesam:artist", Value::from(s.artist));
        put("mpris:length", Value::from(s.length_us));
        if !s.art_url.is_empty() {
            put("mpris:artUrl", Value::from(s.art_url));
        }
        m
    }

    #[zbus(property)]
    async fn volume(&self) -> f64 {
        self.state.lock().unwrap().volume
    }

    #[zbus(property)]
    async fn set_volume(&mut self, v: f64) {
        self.record(format!("Volume={v}"));
        self.state.lock().unwrap().volume = v;
    }

    #[zbus(property(emits_changed_signal = "false"))]
    async fn position(&self) -> i64 {
        self.state.lock().unwrap().position_us
    }

    #[zbus(property)]
    async fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    async fn can_control(&self) -> bool {
        true
    }
}

/// One mock player.
#[derive(Debug)]
pub struct MockMpris {
    conn: Option<zbus::Connection>,
    state: Shared,
}

impl MockMpris {
    /// Claims `org.mpris.MediaPlayer2.<name>` with the given `Identity`,
    /// stopped, at volume 0.5, playing "Track 1" (200 s).
    pub async fn start(bus: &TestBus, name: &str, identity: &str) -> zbus::Result<Self> {
        let state: Shared = Arc::new(Mutex::new(State {
            identity: identity.into(),
            status: "Stopped".into(),
            volume: 0.5,
            position_us: 0,
            title: "Track 1".into(),
            artist: vec!["Artist".into()],
            length_us: 200_000_000,
            art_url: String::new(),
            track: 1,
            calls: Vec::new(),
        }));
        let conn = zbus::connection::Builder::address(bus.session_address())?
            .serve_at(
                PATH,
                Root {
                    state: state.clone(),
                },
            )?
            .serve_at(
                PATH,
                Player {
                    state: state.clone(),
                },
            )?
            .name(format!("org.mpris.MediaPlayer2.{name}"))?
            .build()
            .await?;
        Ok(Self {
            conn: Some(conn),
            state,
        })
    }

    pub fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn volume(&self) -> f64 {
        self.state.lock().unwrap().volume
    }

    async fn player(&self) -> zbus::Result<zbus::object_server::InterfaceRef<Player>> {
        let conn = self.conn.as_ref().expect("player running");
        conn.object_server().interface::<_, Player>(PATH).await
    }

    /// Sets `PlaybackStatus` (`Playing`, `Paused`, `Stopped`).
    pub async fn set_status(&self, status: &str) -> zbus::Result<()> {
        self.state.lock().unwrap().status = status.into();
        let p = self.player().await?;
        p.get()
            .await
            .playback_status_changed(p.signal_emitter())
            .await
    }

    /// Sets the volume as if changed in the player.
    pub async fn set_volume(&self, volume: f64) -> zbus::Result<()> {
        self.state.lock().unwrap().volume = volume;
        let p = self.player().await?;
        p.get().await.volume_changed(p.signal_emitter()).await
    }

    /// Replaces the current track.
    pub async fn set_track(&self, title: &str, art_url: &str) -> zbus::Result<()> {
        {
            let mut s = self.state.lock().unwrap();
            s.track += 1;
            s.title = title.into();
            s.art_url = art_url.into();
            s.position_us = 0;
        }
        let p = self.player().await?;
        p.get().await.metadata_changed(p.signal_emitter()).await
    }

    /// Jumps to a position and emits `Seeked`.
    pub async fn seek_to(&self, position_us: i64) -> zbus::Result<()> {
        self.state.lock().unwrap().position_us = position_us;
        let p = self.player().await?;
        Player::seeked(p.signal_emitter(), position_us).await
    }

    /// The player exits (its connection closes).
    pub async fn quit(&mut self) {
        if let Some(c) = self.conn.take() {
            let _ = c.close().await;
        }
    }
}
