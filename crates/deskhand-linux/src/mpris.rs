//! `media` capability on Linux: an MPRIS multiplexer feeding the
//! platform-neutral [`MediaKit`](deskhand_common::MediaKit).
//!
//! Players are the `org.mpris.MediaPlayer2.*` names on the session bus.
//! `NameOwnerChanged` (arg0namespace) tracks players coming and going; any
//! signal a player sends on `/org/mpris/MediaPlayer2` (`PropertiesChanged`,
//! `Seeked`) marks it dirty, and dirty players are re-read with `GetAll`
//! after a short quiet period. Re-reading also refreshes `Position`, which
//! MPRIS never signals while playing.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use deskhand_common::media::{MediaError, PlaybackStatus, PlayerInfo};
use deskhand_common::{MediaKit, MediaOptions, MediaSource};
use deskhand_core::entity::MediaCmd;
use deskhand_core::{Ctx, Integration, IntegrationError, Probe};
use futures_util::StreamExt;
use zbus::zvariant::{ObjectPath, OwnedValue};
use zbus::{Connection, MatchRule, MessageStream, Proxy};

use crate::env::LinuxEnv;
use crate::util::{Props, dbus_err, get_all, prop};

pub const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_NS: &str = "org.mpris.MediaPlayer2";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
const NO_TRACK: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";
/// Signals arriving within this window are handled with one re-read.
const QUIET: Duration = Duration::from_millis(100);
/// Longest a burst of signals delays a re-read.
const MAX_SETTLE: Duration = Duration::from_millis(500);

/// Media players over MPRIS.
#[derive(Debug)]
pub struct MediaMpris {
    env: Arc<LinuxEnv>,
}

impl MediaMpris {
    pub fn new(env: Arc<LinuxEnv>) -> Self {
        Self { env }
    }
}

#[async_trait::async_trait]
impl Integration for MediaMpris {
    fn id(&self) -> &'static str {
        "media.mpris"
    }

    fn capability(&self) -> &'static str {
        "media"
    }

    fn description(&self) -> &'static str {
        "Media players over MPRIS: state, controls, volume, cover art"
    }

    async fn probe(&self) -> Probe {
        // Players come and go; all we need is the session bus.
        match self.env.session_bus().await {
            Ok(_) => Probe::available(),
            Err(e) => Probe::unavailable(format!("session bus: {e}")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let options = ctx.config::<MediaOptions>()?;
        let conn = self.env.session_bus().await.map_err(dbus_err)?;
        let source = MprisSource::connect(conn).await.map_err(dbus_err)?;
        MediaKit::new(options).run(&ctx, source).await
    }
}

struct Player {
    /// Current owner of the well-known name.
    owner: String,
    info: PlayerInfo,
    track_id: Option<String>,
}

/// The [`MediaSource`] for MPRIS.
pub struct MprisSource {
    conn: Connection,
    owners: MessageStream,
    signals: MessageStream,
    /// Well-known name → player.
    players: BTreeMap<String, Player>,
    /// Well-known names to re-read.
    dirty: BTreeSet<String>,
    /// Something changed that the caller has not seen yet.
    pending: bool,
}

impl std::fmt::Debug for MprisSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MprisSource")
            .field("players", &self.players.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl MprisSource {
    pub async fn connect(conn: Connection) -> zbus::Result<Self> {
        // Subscribe before listing, so no player can slip in between.
        let owners = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.DBus")?
            .interface("org.freedesktop.DBus")?
            .member("NameOwnerChanged")?
            .arg0ns(MPRIS_NS)?
            .build();
        let owners = MessageStream::for_match_rule(owners, &conn, Some(256)).await?;
        let signals = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .path(MPRIS_PATH)?
            .build();
        let signals = MessageStream::for_match_rule(signals, &conn, Some(1024)).await?;

        let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
        let mut players = BTreeMap::new();
        let mut dirty = BTreeSet::new();
        for name in dbus.list_names().await? {
            if !name.starts_with(MPRIS_PREFIX) {
                continue;
            }
            let Ok(owner) = dbus.get_name_owner(name.inner().clone()).await else {
                continue;
            };
            players.insert(name.to_string(), new_player(name.as_str(), owner.as_str()));
            dirty.insert(name.to_string());
        }
        Ok(Self {
            conn,
            owners,
            signals,
            players,
            dirty,
            pending: true,
        })
    }

    /// Applies one bus message. True if it concerned a player.
    fn absorb(&mut self, msg: &zbus::Message, from_owners: bool) -> bool {
        if from_owners {
            let Ok((name, old, new)) = msg.body().deserialize::<(String, String, String)>() else {
                return false;
            };
            if !name.starts_with(MPRIS_PREFIX) {
                return false;
            }
            if !old.is_empty() {
                self.players.remove(&name);
                self.dirty.remove(&name);
            }
            if !new.is_empty() {
                self.players.insert(name.clone(), new_player(&name, &new));
                self.dirty.insert(name);
            }
            return true;
        }
        let Some(sender) = msg.header().sender().map(|s| s.to_string()) else {
            return false;
        };
        let mut hit = false;
        for (name, p) in &self.players {
            if p.owner == sender {
                self.dirty.insert(name.clone());
                hit = true;
            }
        }
        hit
    }

    /// Waits for the next relevant message.
    async fn wait(&mut self) -> Result<(), MediaError> {
        loop {
            let (msg, from_owners) = tokio::select! {
                m = self.owners.next() => (m, true),
                m = self.signals.next() => (m, false),
            };
            let msg = msg
                .ok_or_else(|| MediaError::new("session bus connection closed"))?
                .map_err(MediaError::new)?;
            if self.absorb(&msg, from_owners) {
                self.pending = true;
                return Ok(());
            }
        }
    }

    /// Reads every dirty player.
    async fn refresh(&mut self) {
        while let Some(name) = self.dirty.first().cloned() {
            if let Some(p) = self.players.get_mut(&name) {
                read_player(&self.conn, p).await;
            }
            self.dirty.remove(&name);
        }
    }

    fn player(&self, key: &str) -> Result<&Player, MediaError> {
        self.players
            .get(key)
            .ok_or_else(|| MediaError::new(format!("player {key} is gone")))
    }

    async fn proxy(&self, owner: &str) -> Result<Proxy<'static>, MediaError> {
        Proxy::new(&self.conn, owner.to_owned(), MPRIS_PATH, PLAYER_IFACE)
            .await
            .map_err(MediaError::new)
    }
}

#[async_trait::async_trait]
impl MediaSource for MprisSource {
    async fn next_change(&mut self) -> Result<Vec<PlayerInfo>, MediaError> {
        if !self.pending {
            self.wait().await?;
        }
        // Coalesce bursts (a track change sends several PropertiesChanged),
        // but not forever: some players signal continuously.
        let deadline = tokio::time::Instant::now() + MAX_SETTLE;
        while let Ok(r) = tokio::time::timeout(QUIET, self.wait()).await {
            r?;
            if tokio::time::Instant::now() >= deadline {
                break;
            }
        }
        self.refresh().await;
        self.pending = false;
        Ok(self.players.values().map(|p| p.info.clone()).collect())
    }

    async fn command(&mut self, key: &str, cmd: MediaCmd) -> Result<(), MediaError> {
        let player = self.player(key)?;
        let proxy = self.proxy(&player.owner).await?;
        let r = match cmd {
            MediaCmd::Play => proxy.call_method("Play", &()).await.map(drop),
            MediaCmd::Pause => proxy.call_method("Pause", &()).await.map(drop),
            MediaCmd::PlayPause => proxy.call_method("PlayPause", &()).await.map(drop),
            MediaCmd::Stop => proxy.call_method("Stop", &()).await.map(drop),
            MediaCmd::Next => proxy.call_method("Next", &()).await.map(drop),
            MediaCmd::Previous => proxy.call_method("Previous", &()).await.map(drop),
            MediaCmd::Volume(v) => proxy
                .set_property("Volume", v.clamp(0.0, 1.0))
                .await
                .map_err(zbus::Error::from),
            MediaCmd::Seek(secs) => {
                let target = (secs.max(0.0) * 1e6) as i64;
                match player.track_id.as_deref() {
                    Some(track) => {
                        let track = ObjectPath::try_from(track).map_err(MediaError::new)?;
                        proxy
                            .call_method("SetPosition", &(track, target))
                            .await
                            .map(drop)
                    }
                    // Without a track id only a relative seek is possible.
                    None => {
                        let now = player
                            .info
                            .position_now(Instant::now())
                            .map_or(0, |d| d.as_micros() as i64);
                        proxy.call_method("Seek", &(target - now,)).await.map(drop)
                    }
                }
            }
        };
        r.map_err(MediaError::new)
    }
}

fn new_player(name: &str, owner: &str) -> Player {
    let suffix = name.strip_prefix(MPRIS_PREFIX).unwrap_or(name);
    // `firefox.instance_1_23` → `firefox`.
    let app_id = suffix.split(".instance").next().unwrap_or(suffix);
    let mut info = PlayerInfo::new(name, app_id);
    info.app_id = app_id.to_owned();
    Player {
        owner: owner.to_owned(),
        info,
        track_id: None,
    }
}

async fn read_player(conn: &Connection, p: &mut Player) {
    let read_at = Instant::now();
    let player = match get_all(conn, &p.owner, MPRIS_PATH, PLAYER_IFACE).await {
        Ok(props) => props,
        Err(e) => {
            tracing::debug!(player = %p.info.key, error = %e, "reading MPRIS player failed");
            return;
        }
    };
    if let Ok(root) = get_all(conn, &p.owner, MPRIS_PATH, MPRIS_NS).await {
        if let Some(identity) = prop::<String>(&root, "Identity").filter(|s| !s.is_empty()) {
            p.info.name = identity;
        }
        if let Some(entry) = prop::<String>(&root, "DesktopEntry").filter(|s| !s.is_empty()) {
            p.info.app_id = entry;
        }
    }
    apply_player_props(p, &player, read_at);
}

fn apply_player_props(p: &mut Player, props: &Props, read_at: Instant) {
    let info = &mut p.info;
    info.status = match prop::<String>(props, "PlaybackStatus").as_deref() {
        Some("Playing") => PlaybackStatus::Playing,
        Some("Paused") => PlaybackStatus::Paused,
        _ => PlaybackStatus::Stopped,
    };
    info.volume = prop::<f64>(props, "Volume").map(|v| v.clamp(0.0, 1.0));
    info.rate = prop::<f64>(props, "Rate").unwrap_or(1.0);
    info.position = prop::<i64>(props, "Position")
        .filter(|us| *us >= 0)
        .map(|us| Duration::from_micros(us as u64));
    info.position_at = read_at;

    let meta: HashMap<String, OwnedValue> = props
        .get("Metadata")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| HashMap::try_from(v).ok())
        .unwrap_or_default();
    let text = |k: &str| {
        meta.get(k)
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
            .filter(|s| !s.is_empty())
    };
    info.title = text("xesam:title");
    info.album = text("xesam:album");
    info.art_url = text("mpris:artUrl");
    info.artist = meta
        .get("xesam:artist")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| {
            Vec::<String>::try_from(v.try_clone().ok()?)
                .ok()
                .or_else(|| String::try_from(v).ok().map(|s| vec![s]))
        })
        .map(|a| a.join(", "))
        .filter(|s| !s.is_empty());
    info.length = meta
        .get("mpris:length")
        .and_then(micros)
        .filter(|d| !d.is_zero());
    p.track_id = meta
        .get("mpris:trackid")
        .and_then(|v| {
            let v = v.try_clone().ok()?;
            zbus::zvariant::OwnedObjectPath::try_from(v.try_clone().ok()?)
                .map(|o| o.to_string())
                .ok()
                // Some players send the track id as a plain string.
                .or_else(|| String::try_from(v).ok())
        })
        .filter(|t| t != NO_TRACK && ObjectPath::try_from(t.as_str()).is_ok());
}

/// `mpris:length` is specified as `x` but players also send `t`, `i` or `u`.
fn micros(v: &OwnedValue) -> Option<Duration> {
    let v = v.try_clone().ok()?;
    let us = i64::try_from(v.try_clone().ok()?)
        .ok()
        .or_else(|| u64::try_from(v.try_clone().ok()?).ok().map(|u| u as i64))
        .or_else(|| i32::try_from(v.try_clone().ok()?).ok().map(i64::from))
        .or_else(|| u32::try_from(v).ok().map(i64::from))?;
    (us >= 0).then(|| Duration::from_micros(us as u64))
}

#[cfg(test)]
mod tests {
    use deskhand_common::media::{
        MEDIA, MEDIA_ART, MEDIA_NEXT, MEDIA_PLAY_PAUSE, MEDIA_PLAYER, MEDIA_STATE, MEDIA_VOLUME,
    };
    use deskhand_core::{CommandValue, Config, State};
    use deskhand_testkit::mocks::MockMpris;
    use deskhand_testkit::{TestBus, require_dbus};
    use serde_json::json;

    use super::*;
    use crate::testutil::{rig_with, wait_attr, wait_state};

    fn text(s: &str) -> State {
        State::Text(s.into())
    }

    async fn wait_call(p: &MockMpris, call: &str) {
        for _ in 0..200 {
            if p.calls().iter().any(|c| c == call) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{call} not called; calls: {:?}", p.calls());
    }

    #[tokio::test]
    async fn follows_players_and_controls_them() {
        require_dbus!();
        let bus = TestBus::start().unwrap();
        let mut vlc = MockMpris::start(&bus, "vlc", "VLC media player")
            .await
            .unwrap();
        let firefox = MockMpris::start(&bus, "firefox.instance_1_42", "Mozilla Firefox")
            .await
            .unwrap();
        let env = LinuxEnv::with_addresses(bus.session_address(), bus.system_address(), &[]);
        let integration = MediaMpris::new(env);
        assert!(integration.probe().await.is_available());
        let config = Config::from_toml_str(
            "[integrations.media]\ncompat_mqtt_media_player = true",
            None,
        )
        .unwrap()
        .integration("media")
        .unwrap();
        let (mut t, _cancel) = rig_with(integration, config);
        wait_state(&mut t, &MEDIA_STATE, text("stopped")).await;

        // Firefox starts playing and becomes the active player.
        firefox.set_status("Playing").await.unwrap();
        wait_state(&mut t, &MEDIA_STATE, text("playing")).await;
        wait_attr(&mut t, &MEDIA_STATE, "player", json!("Mozilla Firefox")).await;
        wait_attr(&mut t, &MEDIA_STATE, "title", json!("Track 1")).await;
        wait_attr(&mut t, &MEDIA_STATE, "artist", json!("Artist")).await;
        wait_attr(&mut t, &MEDIA_STATE, "duration", json!(200)).await;
        wait_state(&mut t, &MEDIA_VOLUME, State::Float(50.0)).await;

        // Buttons and volume act on it; its own changes come back.
        t.command(MEDIA_PLAY_PAUSE.as_str(), CommandValue::Press);
        wait_call(&firefox, "PlayPause").await;
        wait_state(&mut t, &MEDIA_STATE, text("paused")).await;
        t.command(MEDIA_VOLUME.as_str(), CommandValue::Number(30.0));
        wait_call(&firefox, "Volume=0.3").await;
        firefox.set_volume(0.8).await.unwrap();
        wait_state(&mut t, &MEDIA_VOLUME, State::Float(80.0)).await;
        t.command(MEDIA_NEXT.as_str(), CommandValue::Press);
        wait_attr(&mut t, &MEDIA_STATE, "title", json!("Track 2")).await;

        // Seeking through the compat media player uses SetPosition.
        t.command(MEDIA.as_str(), CommandValue::Media(MediaCmd::Seek(93.0)));
        wait_call(&firefox, "SetPosition(/org/mock/track/2,93000000)").await;
        wait_attr(&mut t, &MEDIA_STATE, "position", json!(93)).await;

        // A specific player can be chosen.
        t.command(
            MEDIA_PLAYER.as_str(),
            CommandValue::Text("VLC media player".into()),
        );
        wait_attr(&mut t, &MEDIA_STATE, "player", json!("VLC media player")).await;
        wait_state(&mut t, &MEDIA_STATE, text("stopped")).await;
        let compat_stopped = |t: &deskhand_core::testing::TestRig| matches!(t.state(&MEDIA), Some(State::Media(m)) if m.state == "stopped");
        for _ in 0..100 {
            if compat_stopped(&t) {
                break;
            }
            t.settle(Duration::from_millis(20)).await;
        }
        assert!(compat_stopped(&t), "compat player: {:?}", t.state(&MEDIA));

        // Cover art from a file URL.
        let dir = tempfile::tempdir().unwrap();
        let art = dir.path().join("cover.png");
        std::fs::write(&art, b"\x89PNG art").unwrap();
        vlc.set_track("Song", &format!("file://{}", art.display()))
            .await
            .unwrap();
        wait_state(
            &mut t,
            &MEDIA_ART,
            State::Bytes(bytes::Bytes::from_static(b"\x89PNG art")),
        )
        .await;

        // The chosen player quits: back to auto, which follows Firefox.
        vlc.quit().await;
        wait_state(&mut t, &MEDIA_PLAYER, text("auto")).await;
        wait_attr(&mut t, &MEDIA_STATE, "player", json!("Mozilla Firefox")).await;
    }
}
