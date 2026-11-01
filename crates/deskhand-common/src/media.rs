//! Media domain kit: the `media` entities, identical on every OS.
//!
//! A platform implements [`MediaSource`] (MPRIS on Linux) and its
//! integration hands it to [`MediaKit::run`], which owns the entities,
//! picks the active player and routes commands:
//!
//! | entity | kind | |
//! |---|---|---|
//! | `media_state` | enum sensor | playing / paused / stopped / idle (no player); attributes: player, title, artist, album, duration, position, position_updated_at, art_url (remote art only) |
//! | `media_play_pause`, `media_next`, `media_previous`, `media_stop` | buttons | act on the active player |
//! | `media_volume` | number 0-100 % | the active player's volume |
//! | `media_player` | select | `auto` or a specific player |
//! | `media_art` | image | cover art from `file://` and `data:` URLs (remote art is not fetched; see `art_url`) |
//! | `media` | media player | only with `compat_mqtt_media_player = true` |
//!
//! `auto` follows the player that most recently started playing, then the
//! one that played last.
//!
//! ```toml
//! [integrations.media]
//! compat_mqtt_media_player = false
//! ignore_players = ["kdeconnect"]   # player name or app id, case-insensitive
//! ```

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use bytes::Bytes;
use deskhand_core::ctx::{Image, MediaPlayer, Number, Select, Sensor};
use deskhand_core::entity::{EntityKind, ImageSource, MediaCmd, MediaPlayerState, NumberMode};
use deskhand_core::{Attributes, Commands, Ctx, EntityId, EntitySpec, IntegrationError};
use serde::Deserialize;
use serde_json::json;

pub const MEDIA_STATE: EntityId = EntityId::from_static("media_state");
pub const MEDIA_PLAY_PAUSE: EntityId = EntityId::from_static("media_play_pause");
pub const MEDIA_NEXT: EntityId = EntityId::from_static("media_next");
pub const MEDIA_PREVIOUS: EntityId = EntityId::from_static("media_previous");
pub const MEDIA_STOP: EntityId = EntityId::from_static("media_stop");
pub const MEDIA_VOLUME: EntityId = EntityId::from_static("media_volume");
pub const MEDIA_PLAYER: EntityId = EntityId::from_static("media_player");
pub const MEDIA_ART: EntityId = EntityId::from_static("media_art");
/// The compat media player entity.
pub const MEDIA: EntityId = EntityId::from_static("media");

/// `media_player` option that follows the active player.
pub const AUTO: &str = "auto";
pub const STATES: &[&str] = &["playing", "paused", "stopped", "idle"];

/// Largest cover art we publish. The compat media player sends it base64
/// encoded (4/3 larger) and the MQTT packet limit is 4 MiB.
pub const MAX_ART_BYTES: usize = 2 * 1024 * 1024;

/// `[integrations.media]` options.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct MediaOptions {
    /// Also publish a media player entity for the HACS `mqtt_media_player` component.
    pub compat_mqtt_media_player: bool,
    /// Players to leave out, by name or app id (case-insensitive).
    pub ignore_players: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    #[default]
    Stopped,
}

impl PlaybackStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
        }
    }
}

/// One player as the platform sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerInfo {
    /// Unique and stable while the player runs (the MPRIS bus name).
    pub key: String,
    /// Display name (MPRIS `Identity`).
    pub name: String,
    /// Short application id (`firefox`, `spotify`), for `ignore_players`.
    pub app_id: String,
    pub status: PlaybackStatus,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub length: Option<Duration>,
    /// Position as of `position_at`.
    pub position: Option<Duration>,
    pub position_at: Instant,
    pub rate: f64,
    pub art_url: Option<String>,
    /// 0.0 to 1.0, if the player has a volume.
    pub volume: Option<f64>,
}

impl PlayerInfo {
    /// A stopped player without metadata.
    pub fn new(key: impl Into<String>, name: impl Into<String>) -> Self {
        let key = key.into();
        Self {
            app_id: key.clone(),
            key,
            name: name.into(),
            status: PlaybackStatus::Stopped,
            title: None,
            artist: None,
            album: None,
            length: None,
            position: None,
            position_at: Instant::now(),
            rate: 1.0,
            art_url: None,
            volume: None,
        }
    }

    /// The position now, extrapolated while playing.
    pub fn position_now(&self, now: Instant) -> Option<Duration> {
        let pos = self.position?;
        if self.status != PlaybackStatus::Playing || self.rate.is_nan() || self.rate <= 0.0 {
            return Some(pos);
        }
        let elapsed = now.saturating_duration_since(self.position_at);
        let pos = pos + elapsed.mul_f64(self.rate.min(16.0));
        Some(self.length.map_or(pos, |l| pos.min(l)))
    }
}

/// Error from a media source. Returning one from
/// [`MediaSource::next_change`] ends the integration run (and the supervisor
/// restarts it).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct MediaError(pub String);

impl MediaError {
    pub fn new(e: impl std::fmt::Display) -> Self {
        Self(e.to_string())
    }
}

/// A platform's media players.
#[async_trait::async_trait]
pub trait MediaSource: Send {
    /// All players. The first call returns right away; later calls wait for
    /// a change. Must be cancel-safe: a command may interrupt the wait.
    async fn next_change(&mut self) -> Result<Vec<PlayerInfo>, MediaError>;

    /// Sends a command to the player with this key. Failures are logged.
    async fn command(&mut self, player: &str, cmd: MediaCmd) -> Result<(), MediaError>;
}

/// Picks the active player and keeps the entities up to date.
#[derive(Debug)]
pub struct MediaKit {
    options: MediaOptions,
    players: Vec<PlayerInfo>,
    /// When each player last started playing (or was last seen playing).
    played_at: HashMap<String, Instant>,
    /// `None` = auto, otherwise a player key.
    chosen: Option<String>,
    /// Select option → player key.
    option_keys: BTreeMap<String, String>,
    art: Option<(String, Option<Bytes>)>,
}

/// The kit's entity handles.
struct Entities {
    state: Sensor,
    volume: Number,
    player: Select,
    art: Image,
    compat: Option<MediaPlayer>,
}

/// What a command stream delivers to the kit.
enum Input {
    Cmd(MediaCmd),
    Volume(f64),
    Choose(String),
}

impl MediaKit {
    pub fn new(options: MediaOptions) -> Self {
        Self {
            options,
            players: Vec::new(),
            played_at: HashMap::new(),
            chosen: None,
            option_keys: BTreeMap::new(),
            art: None,
        }
    }

    /// Registers the entities, then follows `source` until cancelled.
    pub async fn run(
        mut self,
        ctx: &Ctx,
        mut source: impl MediaSource,
    ) -> Result<(), IntegrationError> {
        let (entities, mut inputs) = register(ctx, self.options.compat_mqtt_media_player);
        let players = source
            .next_change()
            .await
            .map_err(IntegrationError::other)?;
        self.update(players);
        self.publish(&entities).await;
        ctx.ready();

        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                change = source.next_change() => {
                    self.update(change.map_err(IntegrationError::other)?);
                }
                Some(input) = inputs.recv() => match input {
                    Input::Choose(option) => self.choose(&option),
                    Input::Volume(v) => self.send(&mut source, MediaCmd::Volume(v)).await,
                    Input::Cmd(cmd) => self.send(&mut source, cmd).await,
                },
            }
            self.publish(&entities).await;
        }
    }

    async fn send(&self, source: &mut impl MediaSource, cmd: MediaCmd) {
        let Some(player) = self.active() else {
            tracing::debug!(?cmd, "media command without a player ignored");
            return;
        };
        tracing::debug!(player = %player.key, ?cmd, "media command");
        if let Err(e) = source.command(&player.key, cmd).await {
            tracing::warn!(player = %player.name, ?cmd, error = %e, "media command failed");
        }
    }

    fn ignored(&self, p: &PlayerInfo) -> bool {
        self.options
            .ignore_players
            .iter()
            .any(|i| i.eq_ignore_ascii_case(&p.name) || i.eq_ignore_ascii_case(&p.app_id))
    }

    /// Takes a new player list.
    pub fn update(&mut self, players: Vec<PlayerInfo>) {
        let now = Instant::now();
        let mut players: Vec<PlayerInfo> =
            players.into_iter().filter(|p| !self.ignored(p)).collect();
        players.sort_by(|a, b| a.key.cmp(&b.key));
        for p in &players {
            let was_playing = self
                .players
                .iter()
                .any(|o| o.key == p.key && o.status == PlaybackStatus::Playing);
            if p.status == PlaybackStatus::Playing && !was_playing {
                self.played_at.insert(p.key.clone(), now);
            }
        }
        self.played_at
            .retain(|k, _| players.iter().any(|p| &p.key == k));
        if let Some(k) = &self.chosen
            && !players.iter().any(|p| &p.key == k)
        {
            self.chosen = None;
        }
        self.option_keys = option_names(&players);
        self.players = players;
    }

    fn choose(&mut self, option: &str) {
        if option == AUTO {
            self.chosen = None;
        } else if let Some(key) = self.option_keys.get(option) {
            self.chosen = Some(key.clone());
        } else {
            tracing::warn!(option, "unknown media player option");
        }
    }

    /// The player commands go to and entities show.
    pub fn active(&self) -> Option<&PlayerInfo> {
        if let Some(k) = &self.chosen {
            return self.players.iter().find(|p| &p.key == k);
        }
        let last_played = |p: &&PlayerInfo| self.played_at.get(&p.key).copied();
        self.players
            .iter()
            .filter(|p| p.status == PlaybackStatus::Playing)
            .max_by_key(last_played)
            .or_else(|| {
                self.players
                    .iter()
                    .filter(|p| self.played_at.contains_key(&p.key))
                    .max_by_key(last_played)
            })
            .or_else(|| self.players.first())
    }

    fn selected_option(&self) -> String {
        match &self.chosen {
            None => AUTO.to_owned(),
            Some(k) => self
                .option_keys
                .iter()
                .find(|(_, key)| *key == k)
                .map_or_else(|| AUTO.to_owned(), |(o, _)| o.clone()),
        }
    }

    async fn art_for(&mut self, url: Option<&str>) -> Option<Bytes> {
        let url = url?;
        if let Some((cached, bytes)) = &self.art
            && cached == url
        {
            return bytes.clone();
        }
        let bytes = load_art(url).await;
        self.art = Some((url.to_owned(), bytes.clone()));
        bytes
    }

    async fn publish(&mut self, e: &Entities) {
        let now = Instant::now();
        let mut options = vec![AUTO.to_owned()];
        options.extend(self.option_keys.keys().cloned());
        e.player.set_options(options);
        e.player.set(self.selected_option());

        let Some(p) = self.active().cloned() else {
            e.state.set("idle");
            e.state.set_attributes(Attributes::new());
            e.volume.set_unknown();
            e.art.set_unknown();
            if let Some(c) = &e.compat {
                c.set(MediaPlayerState {
                    state: "idle".into(),
                    ..Default::default()
                });
            }
            return;
        };
        let position = p.position_now(now);
        let secs = |d: Option<Duration>| d.map(|d| d.as_secs());
        let mut attrs = Attributes::new();
        attrs.insert("player".into(), json!(p.name));
        attrs.insert("title".into(), json!(p.title));
        attrs.insert("artist".into(), json!(p.artist));
        attrs.insert("album".into(), json!(p.album));
        attrs.insert("duration".into(), json!(secs(p.length)));
        attrs.insert("position".into(), json!(secs(position)));
        if position.is_some() {
            attrs.insert(
                "position_updated_at".into(),
                json!(crate::time::rfc3339(SystemTime::now())),
            );
        }
        // Only remote art is useful to HA as a URL (and `data:` URLs would
        // bloat the attributes); local art goes to `media_art`.
        let remote_art = p
            .art_url
            .as_ref()
            .filter(|u| u.starts_with("https://") || u.starts_with("http://"));
        attrs.insert("art_url".into(), json!(remote_art));
        e.state.set_attributes(attrs);
        e.state.set(p.status.as_str());
        match p.volume {
            Some(v) => e.volume.set((v * 100.0).round()),
            None => e.volume.set_unknown(),
        }
        let art = self.art_for(p.art_url.as_deref()).await;
        match &art {
            Some(bytes) => e.art.set_bytes(bytes.clone()),
            None => e.art.set_unknown(),
        }
        if let Some(c) = &e.compat {
            c.set(MediaPlayerState {
                state: p.status.as_str().into(),
                title: p.title.clone(),
                artist: p.artist.clone(),
                album: p.album.clone(),
                duration: secs(p.length),
                position: secs(position),
                volume: p.volume,
                media_type: None,
                art,
            });
        }
    }
}

/// Unique select options for the players: their names, with ` 2`, ` 3`
/// appended to duplicates (in key order).
fn option_names(players: &[PlayerInfo]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for p in players {
        let base = if p.name.trim().is_empty() || p.name == AUTO {
            p.app_id.clone()
        } else {
            p.name.clone()
        };
        let mut name = base.clone();
        let mut n = 2;
        while out.contains_key(&name) {
            name = format!("{base} {n}");
            n += 1;
        }
        out.insert(name, p.key.clone());
    }
    out
}

fn register(ctx: &Ctx, compat: bool) -> (Entities, tokio::sync::mpsc::Receiver<Input>) {
    let state = ctx.sensor(
        EntitySpec::sensor(MEDIA_STATE, "Media state")
            .enum_options(STATES.iter().map(|s| s.to_string()).collect())
            .icon("mdi:play-pause"),
    );
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let forward = |mut c: Commands<()>, cmd: MediaCmd| {
        let tx = tx.clone();
        tokio::spawn(async move {
            while c.recv().await.is_some() {
                if tx.send(Input::Cmd(cmd)).await.is_err() {
                    break;
                }
            }
        });
    };
    for (id, name, icon, cmd) in [
        (
            MEDIA_PLAY_PAUSE,
            "Play/pause",
            "mdi:play-pause",
            MediaCmd::PlayPause,
        ),
        (MEDIA_NEXT, "Next track", "mdi:skip-next", MediaCmd::Next),
        (
            MEDIA_PREVIOUS,
            "Previous track",
            "mdi:skip-previous",
            MediaCmd::Previous,
        ),
        (MEDIA_STOP, "Stop", "mdi:stop", MediaCmd::Stop),
    ] {
        forward(ctx.button(EntitySpec::button(id, name).icon(icon)), cmd);
    }
    let mut volume_spec = EntitySpec::number(MEDIA_VOLUME, "Media volume", 0.0, 100.0, 1.0)
        .unit("%")
        .icon("mdi:volume-high");
    if let EntityKind::Number { mode, .. } = &mut volume_spec.kind {
        *mode = NumberMode::Slider;
    }
    let (volume, mut volume_cmds) = ctx.number(volume_spec);
    let tx2 = tx.clone();
    tokio::spawn(async move {
        while let Some(v) = volume_cmds.recv().await {
            if tx2
                .send(Input::Volume((v / 100.0).clamp(0.0, 1.0)))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let (player, mut player_cmds) = ctx.select(
        EntitySpec::select(MEDIA_PLAYER, "Media player", vec![AUTO.to_owned()])
            .icon("mdi:music-box-multiple"),
    );
    let tx3 = tx.clone();
    tokio::spawn(async move {
        while let Some(o) = player_cmds.recv().await {
            if tx3.send(Input::Choose(o)).await.is_err() {
                break;
            }
        }
    });
    let art = ctx.image(
        EntitySpec::image(
            MEDIA_ART,
            "Media art",
            ImageSource::Bytes {
                content_type: "image/jpeg".into(),
            },
        )
        .icon("mdi:album"),
    );
    let compat = compat.then(|| {
        let (handle, mut cmds) =
            ctx.media_player(EntitySpec::new(MEDIA, "Media", EntityKind::MediaPlayer));
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(c) = cmds.recv().await {
                if tx.send(Input::Cmd(c)).await.is_err() {
                    break;
                }
            }
        });
        handle
    });
    (
        Entities {
            state,
            volume,
            player,
            art,
            compat,
        },
        rx,
    )
}

/// Reads cover art from a `file://` or base64 `data:` URL. Remote URLs are
/// not fetched (they stay visible in the `art_url` attribute).
pub async fn load_art(url: &str) -> Option<Bytes> {
    let bytes = if let Some(path) = url.strip_prefix("file://") {
        let path = percent_decode(path.strip_prefix("localhost").unwrap_or(path))?;
        let meta = tokio::fs::metadata(&path).await.ok()?;
        if !meta.is_file() || meta.len() > MAX_ART_BYTES as u64 {
            tracing::debug!(%path, size = meta.len(), "cover art skipped");
            return None;
        }
        tokio::fs::read(&path).await.ok()?
    } else {
        let rest = url.strip_prefix("data:")?;
        let (meta, data) = rest.split_once(',')?;
        if !meta.ends_with(";base64") || data.len() > MAX_ART_BYTES * 4 / 3 + 4 {
            return None;
        }
        base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .ok()?
    };
    (!bytes.is_empty() && bytes.len() <= MAX_ART_BYTES).then(|| Bytes::from(bytes))
}

/// Decodes `%XX` escapes of a URL path. `None` if the result is not UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use deskhand_core::testing::{rig_with, wait_attr, wait_state};
    use deskhand_core::{CommandValue, Config, Integration, Probe, State};
    use tokio::sync::mpsc;

    use super::*;

    type Calls = Arc<Mutex<Vec<(String, MediaCmd)>>>;

    struct FakeSource {
        initial: Option<Vec<PlayerInfo>>,
        updates: mpsc::UnboundedReceiver<Vec<PlayerInfo>>,
        calls: Calls,
    }

    #[async_trait::async_trait]
    impl MediaSource for FakeSource {
        async fn next_change(&mut self) -> Result<Vec<PlayerInfo>, MediaError> {
            if let Some(p) = self.initial.take() {
                return Ok(p);
            }
            self.updates
                .recv()
                .await
                .ok_or_else(|| MediaError::new("closed"))
        }

        async fn command(&mut self, player: &str, cmd: MediaCmd) -> Result<(), MediaError> {
            self.calls.lock().unwrap().push((player.to_owned(), cmd));
            Ok(())
        }
    }

    struct TestMedia(Mutex<Option<FakeSource>>);

    #[async_trait::async_trait]
    impl Integration for TestMedia {
        fn id(&self) -> &'static str {
            "media.test"
        }
        fn capability(&self) -> &'static str {
            "media"
        }
        fn description(&self) -> &'static str {
            "test"
        }
        async fn probe(&self) -> Probe {
            Probe::available()
        }
        async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
            let source = self.0.lock().unwrap().take().expect("runs once");
            let options = ctx.config::<MediaOptions>()?;
            MediaKit::new(options).run(&ctx, source).await
        }
    }

    fn player(key: &str, name: &str, status: PlaybackStatus) -> PlayerInfo {
        PlayerInfo {
            status,
            title: Some(format!("{name} song")),
            volume: Some(0.5),
            ..PlayerInfo::new(key, name)
        }
    }

    fn start(
        toml: &str,
        initial: Vec<PlayerInfo>,
    ) -> (
        deskhand_core::testing::TestRig,
        mpsc::UnboundedSender<Vec<PlayerInfo>>,
        Calls,
    ) {
        let (tx, updates) = mpsc::unbounded_channel();
        let calls = Calls::default();
        let source = FakeSource {
            initial: Some(initial),
            updates,
            calls: calls.clone(),
        };
        let config = Config::from_toml_str(toml, None)
            .unwrap()
            .integration("media")
            .unwrap();
        let (rig, _cancel) = rig_with(TestMedia(Mutex::new(Some(source))), config);
        (rig, tx, calls)
    }

    async fn wait_calls(t: &mut deskhand_core::testing::TestRig, calls: &Calls, n: usize) {
        for _ in 0..200 {
            if calls.lock().unwrap().len() >= n {
                return;
            }
            t.settle(Duration::from_millis(20)).await;
        }
        panic!(
            "expected {n} media commands, got {:?}",
            calls.lock().unwrap()
        );
    }

    fn text(s: &str) -> State {
        State::Text(s.into())
    }

    #[tokio::test]
    async fn auto_follows_the_player_that_started_playing() {
        use PlaybackStatus::*;
        let (mut t, tx, calls) = start(
            "",
            vec![player("a", "Alpha", Paused), player("b", "Beta", Stopped)],
        );
        // Nothing has played yet: the first player.
        wait_attr(&mut t, &MEDIA_STATE, "player", json!("Alpha")).await;
        wait_state(&mut t, &MEDIA_STATE, text("paused")).await;
        wait_state(&mut t, &MEDIA_VOLUME, State::Float(50.0)).await;
        wait_state(&mut t, &MEDIA_PLAYER, text(AUTO)).await;

        tx.send(vec![
            player("a", "Alpha", Paused),
            player("b", "Beta", Playing),
        ])
        .unwrap();
        wait_attr(&mut t, &MEDIA_STATE, "player", json!("Beta")).await;
        wait_state(&mut t, &MEDIA_STATE, text("playing")).await;
        t.command(MEDIA_PLAY_PAUSE.as_str(), CommandValue::Press);
        t.command(MEDIA_VOLUME.as_str(), CommandValue::Number(30.0));
        wait_calls(&mut t, &calls, 2).await;
        // Beta pauses and stays the active one (it played last).
        tx.send(vec![
            player("a", "Alpha", Paused),
            player("b", "Beta", Paused),
        ])
        .unwrap();
        wait_state(&mut t, &MEDIA_STATE, text("paused")).await;
        wait_attr(&mut t, &MEDIA_STATE, "player", json!("Beta")).await;
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                ("b".to_owned(), MediaCmd::PlayPause),
                ("b".to_owned(), MediaCmd::Volume(0.3)),
            ]
        );

        // A specific player is chosen; it wins even when another plays.
        t.command(MEDIA_PLAYER.as_str(), CommandValue::Text("Alpha".into()));
        wait_state(&mut t, &MEDIA_PLAYER, text("Alpha")).await;
        tx.send(vec![
            player("a", "Alpha", Paused),
            player("b", "Beta", Playing),
        ])
        .unwrap();
        wait_attr(&mut t, &MEDIA_STATE, "player", json!("Alpha")).await;
        t.command(MEDIA_NEXT.as_str(), CommandValue::Press);
        wait_calls(&mut t, &calls, 3).await;
        assert_eq!(
            calls.lock().unwrap().last(),
            Some(&("a".to_owned(), MediaCmd::Next))
        );
        // The chosen player quits: back to auto.
        tx.send(vec![player("b", "Beta", Playing)]).unwrap();
        wait_state(&mut t, &MEDIA_PLAYER, text(AUTO)).await;
        wait_attr(&mut t, &MEDIA_STATE, "player", json!("Beta")).await;

        tx.send(vec![]).unwrap();
        wait_state(&mut t, &MEDIA_STATE, text("idle")).await;
        wait_state(&mut t, &MEDIA_VOLUME, State::Unknown).await;
    }

    #[tokio::test]
    async fn compat_player_ignore_list_and_art() {
        let dir = tempfile::tempdir().unwrap();
        let art = dir.path().join("cover art.jpg");
        std::fs::write(&art, b"\xff\xd8jpeg").unwrap();
        let url = format!("file://{}", art.display()).replace(' ', "%20");
        let mut p = player("org.mpris.MediaPlayer2.mpv", "mpv", PlaybackStatus::Playing);
        p.art_url = Some(url);
        p.length = Some(Duration::from_secs(200));
        p.position = Some(Duration::from_secs(42));
        let mut ignored = player("kde", "KDE Connect", PlaybackStatus::Playing);
        ignored.app_id = "kdeconnect".into();
        let (mut t, _tx, calls) = start(
            "[integrations.media]\ncompat_mqtt_media_player = true\nignore_players = ['KDECONNECT']",
            vec![ignored, p],
        );
        wait_attr(&mut t, &MEDIA_STATE, "player", json!("mpv")).await;
        wait_state(
            &mut t,
            &MEDIA_ART,
            State::Bytes(Bytes::from_static(b"\xff\xd8jpeg")),
        )
        .await;
        let compat = loop {
            if let Some(State::Media(m)) = t.state(&MEDIA) {
                break m.clone();
            }
            t.settle(Duration::from_millis(50)).await;
        };
        assert_eq!(compat.state, "playing");
        assert_eq!(compat.title.as_deref(), Some("mpv song"));
        assert_eq!(compat.duration, Some(200));
        assert!(compat.position.is_some_and(|p| (42..=43).contains(&p)));
        assert!(compat.art.is_some());
        let options = t.attributes(&MEDIA_STATE).cloned().unwrap();
        assert_eq!(options["duration"], json!(200));

        t.command(MEDIA.as_str(), CommandValue::Media(MediaCmd::Seek(10.0)));
        wait_calls(&mut t, &calls, 1).await;
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(
                "org.mpris.MediaPlayer2.mpv".to_owned(),
                MediaCmd::Seek(10.0)
            )]
        );
    }

    #[test]
    fn option_names_are_unique() {
        let players = vec![
            PlayerInfo::new("a", "Firefox"),
            PlayerInfo::new("b", "Firefox"),
            PlayerInfo::new("c", ""),
        ];
        let names: Vec<_> = option_names(&players).into_iter().collect();
        assert_eq!(
            names,
            vec![
                ("Firefox".to_owned(), "a".to_owned()),
                ("Firefox 2".to_owned(), "b".to_owned()),
                ("c".to_owned(), "c".to_owned()),
            ]
        );
    }

    #[test]
    fn position_extrapolates_while_playing() {
        let start = Instant::now();
        let mut p = player("a", "A", PlaybackStatus::Playing);
        p.position = Some(Duration::from_secs(10));
        p.position_at = start;
        p.length = Some(Duration::from_secs(12));
        let later = start + Duration::from_secs(5);
        assert_eq!(p.position_now(later), Some(Duration::from_secs(12)));
        p.length = None;
        assert_eq!(p.position_now(later), Some(Duration::from_secs(15)));
        p.status = PlaybackStatus::Paused;
        assert_eq!(p.position_now(later), Some(Duration::from_secs(10)));
    }

    #[tokio::test]
    async fn art_urls() {
        assert_eq!(
            load_art("data:image/png;base64,aGVsbG8=").await,
            Some(Bytes::from_static(b"hello"))
        );
        assert_eq!(load_art("data:text/plain,hello").await, None);
        assert_eq!(load_art("https://example.com/a.jpg").await, None);
        assert_eq!(load_art("file:///nonexistent/a.jpg").await, None);
        assert_eq!(percent_decode("/a%20b%2").as_deref(), Some("/a b%2"));
    }
}
