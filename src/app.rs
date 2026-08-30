//! The application: state, event handling, and the actions views ask for.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use egui::Color32;
use rand::seq::IndexedRandom;

use crate::api::PlayRequest;
use crate::api::models::{
    ArtistRef, Device, PlayableItem, PlaybackState, Playlist, Queue, Track, User, pick_image,
};
use crate::backend::{
    ApiRequest, ApiResponse, AuthStatus, Backend, Command, Event, LocalPlayback, LyricsRequest,
    RemoteAction, Waker,
};
use crate::media::{MediaCommand, MediaState, MediaTrack};
use crate::media_controls::MediaService;
use crate::model::*;
use crate::paths::AppDirs;
use crate::player::{EngineConfig, LoadSpec, LocalState, Playback, PlayerCommand, RepeatMode};
use crate::settings::{SaveError, SessionState, Settings, ThemeChoice};
use crate::single_instance::ControlCommand;
use crate::theme::{self, Palette};
use crate::tray::{TrayCommand, TrayService};
use crate::util;

const REMOTE_POLL_ACTIVE: Duration = Duration::from_secs(4);
const REMOTE_POLL_IDLE: Duration = Duration::from_secs(20);
const REMOTE_FRESH: Duration = Duration::from_secs(45);
const DEVICES_FRESH: Duration = Duration::from_secs(12);
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(280);
const TOAST_LIFETIME: Duration = Duration::from_millis(3200);
const OPTIMISTIC_HOLD: Duration = Duration::from_millis(2500);

/// How long a context the app just started is shown as playing while
/// Spotify's state catches up. During a local takeover the cluster can
/// report the old context, then the new, then the old again; the whole
/// dance settles well inside this window, so no early hand-back.
const ASSUMED_CONTEXT_HOLD: Duration = Duration::from_secs(8);
/// How long the interface trusts its own play/pause over a polled state that
/// has not caught up yet. Spotify can take a moment to report a command it
/// has already carried out, and a button that springs back looks broken.
const PLAYBACK_HOLD: Duration = Duration::from_secs(6);
/// A second look after a command, so the button settles quickly rather than
/// waiting for the ordinary poll.
const REMOTE_RECHECK: Duration = Duration::from_millis(1200);
const CONTAINS_BATCH: usize = 50;
const STATE_SAVE_DEBOUNCE: Duration = Duration::from_secs(2);
const STATE_SAVE_RETRY: Duration = Duration::from_secs(30);
const RESUME_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(120);
const SHUFFLE_INTENT_HOLD: Duration = Duration::from_secs(5);
const MAX_PLAY_URIS: usize = 500;
const WINDOW_MIN_SIZE: [f32; 2] = [400.0, 300.0];
const WINDOW_MAX_SIZE: [f32; 2] = [16_384.0, 8_640.0];
const WINDOW_POSITION_MIN: f32 = -32_768.0;
const WINDOW_POSITION_MAX: f32 = 32_768.0;

fn valid_window_size(size: [f32; 2]) -> bool {
    (WINDOW_MIN_SIZE[0]..=WINDOW_MAX_SIZE[0]).contains(&size[0])
        && (WINDOW_MIN_SIZE[1]..=WINDOW_MAX_SIZE[1]).contains(&size[1])
}

fn valid_window_position(position: [f32; 2]) -> bool {
    (WINDOW_POSITION_MIN..=WINDOW_POSITION_MAX).contains(&position[0])
        && (WINDOW_POSITION_MIN..=WINDOW_POSITION_MAX).contains(&position[1])
}

fn geometry_changed(previous: Option<[f32; 2]>, current: [f32; 2]) -> bool {
    previous.is_none_or(|previous| {
        (previous[0] - current[0]).abs() > 1.0 || (previous[1] - current[1]).abs() > 1.0
    })
}

fn state_save_due(dirty: bool, retrying: bool, last_attempt: Instant, now: Instant) -> bool {
    state_save_wait(dirty, retrying, last_attempt, now) == Some(Duration::ZERO)
}

fn state_save_wait(
    dirty: bool,
    retrying: bool,
    last_attempt: Instant,
    now: Instant,
) -> Option<Duration> {
    dirty.then(|| {
        let interval = if retrying {
            STATE_SAVE_RETRY
        } else {
            STATE_SAVE_DEBOUNCE
        };
        interval.saturating_sub(now.saturating_duration_since(last_attempt))
    })
}

#[derive(Debug, thiserror::Error)]
pub enum StateSaveError {
    #[error("settings save failed: {0}")]
    Settings(Box<SaveError>),
    #[error("session save failed: {0}")]
    Session(Box<SaveError>),
    #[error("settings save failed: {settings}; session save failed: {session}")]
    Both {
        settings: Box<SaveError>,
        session: Box<SaveError>,
    },
}

pub struct RemoteSnapshot {
    pub state: PlaybackState,
    pub received_at: Instant,
}

/// A context the interface asked Spotify to play and shows as playing
/// before any state says so.
struct AssumedContext {
    uri: String,
    at: Instant,
}

struct QueuedPlay {
    request: PlayRequest,
}

#[derive(Default)]
struct UnavailableRecovery {
    recent: Vec<Instant>,
    last_reconnect: Option<Instant>,
}

impl UnavailableRecovery {
    fn record(&mut self, now: Instant) -> bool {
        self.recent
            .retain(|at| now.duration_since(*at) < Duration::from_secs(20));
        self.recent.push(now);
        if self.recent.len() < 3
            || self
                .last_reconnect
                .is_some_and(|at| now.duration_since(at) <= Duration::from_secs(60))
        {
            return false;
        }
        self.recent.clear();
        self.last_reconnect = Some(now);
        true
    }
}

/// The playing item as the interface sees it, whichever device plays it.
#[derive(Clone, Debug, PartialEq)]
pub struct NowPlaying {
    pub local: bool,
    pub device_name: Option<String>,
    pub uri: String,
    pub id: Option<String>,
    pub title: String,
    pub artists: Vec<ArtistRef>,
    pub subtitle: String,
    pub album_name: String,
    pub album_id: Option<String>,
    pub show_id: Option<String>,
    pub art_url: Option<String>,
    pub art_small: Option<String>,
    pub duration_ms: u32,
    pub position_ms: u32,
    pub playing: bool,
    pub loading: bool,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub volume_percent: u8,
    pub can_control: bool,
    pub is_episode: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Local,
    Remote(Option<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaybackOwner {
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteCommand {
    Action(RemoteAction),
    AddToQueue,
    Transfer,
}

fn remote_command_issue(device: Option<&Device>, command: RemoteCommand) -> Option<&'static str> {
    let device = device?;
    if device.is_restricted {
        return Some(
            "Spotify marks this device as restricted, so it cannot accept remote controls",
        );
    }
    if command == RemoteCommand::Action(RemoteAction::Volume)
        && device.supports_volume == Some(false)
    {
        return Some("This device does not expose volume control to Spotify apps");
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShuffleDispatch {
    Deferred,
    Local,
    Remote,
    Unsupported,
}

/// How the application is being started.
#[derive(Clone, Copy, Debug)]
pub struct AppOptions {
    /// Register the MPRIS media-control service (Linux).
    pub media_controls: bool,
    /// Register the system-tray item (Linux).
    pub tray: bool,
}

impl Default for AppOptions {
    fn default() -> Self {
        Self {
            media_controls: true,
            tray: true,
        }
    }
}

pub struct App {
    pub dirs: AppDirs,
    pub settings: Settings,
    settings_dirty: bool,
    settings_save_retrying: bool,
    last_settings_save_attempt: Instant,
    session_dirty: bool,
    session_save_retrying: bool,
    last_session_save_attempt: Instant,
    pub backend: Backend,
    media_controls: Option<MediaService>,
    tray: Option<TrayService>,
    pub window_hidden: bool,
    /// The window should close but the process should stay in the tray.
    pub hide_intent: bool,
    /// A hidden app was asked to show itself; the outer loop recreates the
    /// window.
    pub wants_show: bool,
    /// Commands from control clients (a second `fastpotify <verb>` launch,
    /// a Raycast script), on the platforms where they do not arrive through
    /// MPRIS. Drained every frame.
    control_commands: Option<std::sync::Arc<std::sync::Mutex<Vec<ControlCommand>>>>,
    /// Where the now-playing snapshot goes for the control channel's
    /// `nowplaying` verb to answer from.
    control_now_playing: Option<std::sync::Arc<std::sync::Mutex<String>>>,
    /// Where the last device response goes for the control channel's
    /// `devices` verb to answer from.
    control_devices: Option<std::sync::Arc<std::sync::Mutex<String>>>,
    control_devices_stale: bool,
    /// Sample data is loaded and nothing is asked of Spotify.
    pub offline: bool,
    pub palette: Palette,
    applied_dark: Option<bool>,
    /// The saved zoom has been applied to this window's context once.
    zoom_applied: bool,

    pub auth: AuthStatus,
    pub user: Option<User>,
    pub local_device_id: Option<String>,
    /// Local playback is authorized and the engine is connected.
    pub local_ready: bool,
    pub local_playback: LocalPlayback,
    pub local: LocalState,
    pub remote: Option<RemoteSnapshot>,
    remote_polled_at: Instant,
    remote_poll_pending: bool,
    /// Serial of the newest playback poll sent; older answers are stale.
    remote_poll_seq: u64,
    pub devices: Vec<Device>,
    pub devices_loading: bool,
    devices_fetched_at: Option<Instant>,
    pub selected_device: Option<String>,
    pub queue: Loadable<Queue>,
    queue_fetched_at: Option<Instant>,

    pub library: Library,
    pub home: HomeData,
    pub search: SearchState,
    pub playlist_pages: HashMap<String, PlaylistPage>,
    pub album_pages: HashMap<String, AlbumPage>,
    pub artist_pages: HashMap<String, ArtistPage>,
    pub show_pages: HashMap<String, ShowPage>,
    pub track_cache: HashMap<String, Track>,
    track_requests: HashSet<String>,

    pub history: Vec<Page>,
    pub history_index: usize,

    pub saved: HashMap<String, bool>,
    saved_pending: HashSet<String>,
    pub accents: HashMap<String, Color32>,
    accent_pending: HashSet<String>,

    pub dialog: Option<Dialog>,
    pub show_queue_panel: bool,
    pub show_lyrics_panel: bool,
    /// The track the lyrics below are for.
    pub lyrics_uri: Option<String>,
    /// `Loaded(None)` when nobody has transcribed the track.
    pub lyrics: Loadable<Option<crate::lyrics::Lyrics>>,
    /// Whether the panel scrolls to the line being sung. Off once the
    /// reader scrolls by hand, on again with the Follow button or a new
    /// track.
    pub lyrics_following: bool,
    /// The line the panel last positioned itself for (`Some(None)` before
    /// the first line), so it moves once per change; `None` until it has
    /// positioned itself at all for this track.
    pub lyrics_line_shown: Option<Option<usize>>,
    pub show_devices: bool,
    pub toasts: Vec<Toast>,
    pub actions: Vec<Action>,
    volume_before_mute: Option<u8>,
    /// What was just asked to play, until Spotify visibly reacts: the keys
    /// (context and track URIs) whose play buttons show a spinner.
    pending_play_keys: Vec<String>,
    pending_play_at: Option<Instant>,
    /// A play request made while the local engine was connecting or
    /// reconnecting; it starts once the engine is ready and reports connected.
    queued_play: Option<QueuedPlay>,
    /// When to take a confirming look at remote playback after a command.
    remote_recheck_at: Option<Instant>,
    pub seek_preview: Option<f32>,
    pub volume_preview: Option<f32>,
    /// Last valid window geometry, restored whenever the window is recreated.
    last_window_size: Option<[f32; 2]>,
    last_window_pos: Option<[f32; 2]>,
    last_eviction: Instant,
    pub sign_in_url: Option<String>,
    /// The optional personal Web API application currently ready for routing.
    pub web_app: Option<String>,
    pending_remote_position: Option<(u32, Instant)>,
    pending_remote_volume: Option<(u8, Instant)>,
    /// A local volume set here that the engine has not echoed back yet. It
    /// reports `VolumeChanged` asynchronously while position snapshots land
    /// every second, so a snapshot must not undo the change on its way past.
    pending_local_volume: Option<(u16, Instant)>,
    optimistic_playing: Option<(bool, Instant)>,
    unavailable_recovery: UnavailableRecovery,
    /// Shuffle as the listener set it, carried across playback contexts.
    shuffle_wanted: bool,
    /// A shuffle-only command waiting for the connecting local engine.
    pending_local_shuffle: Option<bool>,
    /// Prevent an echo of a local change from looking like another client.
    shuffle_set_at: Option<Instant>,
    /// The context the interface just started, shown as playing until
    /// Spotify's own state says the same thing.
    assumed_context: Option<AssumedContext>,
    last_now_playing_uri: Option<String>,
    pub playlist_busy: bool,
    pub quit_requested: bool,
    /// The axis a scroll gesture settled on, and when it last moved.
    scroll_lock: Option<(ScrollAxis, Instant)>,
    /// Whether the current scroll gesture comes from a trackpad.
    scroll_from_trackpad: bool,
    /// Recent scroll positions, to read the gesture's speed when it ends.
    scroll_history: egui::util::History<egui::Vec2>,
    /// Where the gesture has scrolled to so far, for the history.
    scroll_accum: egui::Vec2,
    /// The speed still carrying the page after the fingers lifted.
    glide: Option<egui::Vec2>,
    /// When the last scroll event arrived, for lifts nobody announces.
    scroll_last_event: Option<Instant>,
    /// How each table is sorted, per page, for as long as the app runs.
    pub table_sorts: HashMap<Page, TableSort>,
    /// User ids resolved to display names; `None` while unknown, so an id
    /// is asked about only once per run.
    pub user_names: HashMap<String, Option<String>>,
    /// Context URIs most recently played, newest first: the sidebar's
    /// order. Kept with the session, so it survives a restart.
    pub recent_contexts: Vec<String>,
    /// What was playing when the app last closed, to resume from cold.
    resume_context: Option<String>,
    resume_track: Option<String>,
    resume_position_ms: u32,
    last_resume_checkpoint: Instant,
    /// A newer release than this build, once GitHub has said so.
    pub update: Option<crate::updates::Release>,
    last_update_check: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrollAxis {
    Horizontal,
    Vertical,
}

/// A trackpad gesture that pauses this long has ended; the next movement
/// picks its axis afresh.
const SCROLL_GESTURE_GAP: Duration = Duration::from_millis(150);

/// How far short Linux trackpad deltas land of what other players scroll.
const TRACKPAD_SCALE: f32 = 1.8;

/// The glide's exponential decay time, in seconds; the speed below which a
/// lift starts no glide; and the speed at which a glide stops, points per
/// second.
const GLIDE_DECAY: f32 = 0.35;
const GLIDE_START: f32 = 120.0;
const GLIDE_STOP: f32 = 40.0;

impl App {
    pub fn new(waker: &Waker, dirs: AppDirs, mut settings: Settings, options: AppOptions) -> Self {
        settings.normalize_layout();
        let engine_config = engine_config(&dirs, &settings);
        let backend = Backend::spawn(
            dirs.clone(),
            engine_config,
            settings.web_client_id.clone(),
            waker.clone(),
        );
        let wake = waker.clone();
        let media_controls = options
            .media_controls
            .then(|| MediaService::spawn(move || wake.wake()));
        let wake = waker.clone();
        let tray = options
            .tray
            .then(|| TrayService::spawn(move || wake.wake()))
            .flatten();

        let session = SessionState::load(&dirs.session_file());
        let first_page = session
            .last_page
            .as_deref()
            .and_then(Page::decode)
            .filter(|page| !matches!(page, Page::Settings | Page::Queue))
            .unwrap_or(Page::Home);
        let last_window_size = session.window_size.filter(|size| valid_window_size(*size));
        let last_window_pos = session
            .window_pos
            .filter(|position| valid_window_position(*position));

        let mut app = Self {
            dirs,
            settings,
            settings_dirty: false,
            settings_save_retrying: false,
            last_settings_save_attempt: Instant::now(),
            session_dirty: false,
            session_save_retrying: false,
            last_session_save_attempt: Instant::now(),
            backend,
            media_controls,
            tray,
            window_hidden: false,
            hide_intent: false,
            wants_show: false,
            control_commands: None,
            control_now_playing: None,
            control_devices: None,
            control_devices_stale: true,
            offline: false,
            palette: Palette::dark(),
            applied_dark: None,
            zoom_applied: false,
            auth: AuthStatus::Starting,
            user: None,
            local_device_id: None,
            local_ready: false,
            local_playback: LocalPlayback::Unavailable,
            local: LocalState::default(),
            remote: None,
            remote_polled_at: Instant::now() - REMOTE_POLL_IDLE,
            remote_poll_pending: false,
            remote_poll_seq: 0,
            devices: Vec::new(),
            devices_loading: false,
            devices_fetched_at: None,
            selected_device: None,
            queue: Loadable::NotLoaded,
            queue_fetched_at: None,
            library: Library::default(),
            home: HomeData::default(),
            search: SearchState::default(),
            playlist_pages: HashMap::new(),
            album_pages: HashMap::new(),
            artist_pages: HashMap::new(),
            show_pages: HashMap::new(),
            track_cache: HashMap::new(),
            track_requests: HashSet::new(),
            history: vec![first_page],
            history_index: 0,
            saved: HashMap::new(),
            saved_pending: HashSet::new(),
            accents: HashMap::new(),
            accent_pending: HashSet::new(),
            dialog: None,
            show_queue_panel: session.queue_open.unwrap_or(false),
            show_lyrics_panel: false,
            lyrics_uri: None,
            lyrics: Loadable::NotLoaded,
            lyrics_following: true,
            lyrics_line_shown: None,
            show_devices: false,
            toasts: Vec::new(),
            actions: Vec::new(),
            volume_before_mute: None,
            pending_play_keys: Vec::new(),
            pending_play_at: None,
            queued_play: None,
            remote_recheck_at: None,
            seek_preview: None,
            volume_preview: None,
            last_window_size,
            last_window_pos,
            last_eviction: Instant::now(),
            sign_in_url: None,
            web_app: None,
            pending_remote_position: None,
            pending_remote_volume: None,
            pending_local_volume: None,
            optimistic_playing: None,
            unavailable_recovery: UnavailableRecovery::default(),
            shuffle_wanted: session.shuffle_on,
            pending_local_shuffle: None,
            shuffle_set_at: None,
            assumed_context: None,
            last_now_playing_uri: None,
            playlist_busy: false,
            quit_requested: false,
            scroll_lock: None,
            scroll_from_trackpad: false,
            scroll_history: egui::util::History::new(2..16, 0.1),
            scroll_accum: egui::Vec2::ZERO,
            glide: None,
            scroll_last_event: None,
            table_sorts: session
                .sorts
                .iter()
                .filter_map(|(page, sort)| Some((Page::decode(page)?, *sort)))
                .collect(),
            user_names: HashMap::new(),
            recent_contexts: session.recent_contexts.clone(),
            resume_context: session.last_context.clone(),
            resume_track: session.last_track.clone(),
            resume_position_ms: session.last_position_ms,
            last_resume_checkpoint: Instant::now(),
            update: None,
            last_update_check: None,
        };
        app.local.volume = app.settings.volume;
        app
    }

    /// Watches the queue control clients fill and keeps their now-playing
    /// snapshot fresh.
    pub fn set_remote_control(&mut self, guard: &crate::single_instance::Guard) {
        self.control_commands = Some(guard.commands());
        self.control_now_playing = Some(guard.now_playing_slot());
        self.control_devices = Some(guard.devices_slot());
    }

    /// Per-window setup: fonts, icons, loaders, theme. Called every time a
    /// window is (re)created around this long-lived application state.
    pub fn attach(&mut self, ctx: &egui::Context) {
        theme::install(ctx);
        ctx.add_bytes_loader(std::sync::Arc::new(self.backend.art().clone()));
        ctx.set_theme(match self.settings.theme {
            ThemeChoice::Dark => egui::ThemePreference::Dark,
            ThemeChoice::Light => egui::ThemePreference::Light,
            ThemeChoice::System => egui::ThemePreference::System,
        });
        self.applied_dark = None;
        self.zoom_applied = false;
        self.window_hidden = false;
        self.hide_intent = false;
        self.wants_show = false;
        // MPRemoteCommandCenter must be registered only after eframe has made
        // NSApplication. The process-level service stays attached while this
        // window is later closed to the status item.
        #[cfg(target_os = "macos")]
        if let Some(controls) = &mut self.media_controls {
            controls.attach();
        }
        if let Some(tray) = &mut self.tray {
            tray.attach();
        }
        if let Some(size) = self.last_window_size {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                size[0], size[1],
            )));
        }
        if let Some(position) = self.last_window_pos {
            // This is a no-op on Wayland; the bounds keep a stale monitor
            // layout from marooning the window elsewhere.
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                position[0],
                position[1],
            )));
        }
        // egui's consensus wheel speed is 40 points per line, about a third
        // of what every other player scrolls per notch; trackpads report
        // pixels and are unaffected (#32).
        ctx.options_mut(|options| options.input_options.line_scroll_speed = 120.0);
    }

    /// The window is gone but the process stays: audio, the tray, and the
    /// media controls keep running until Show or Quit.
    pub fn window_gone(&mut self) {
        self.window_hidden = true;
        self.hide_intent = false;
        self.wants_show = false;
        if let Some(tray) = &mut self.tray {
            tray.hidden();
        }
    }

    // ---- derived state -----------------------------------------------------

    pub fn page(&self) -> &Page {
        &self.history[self.history_index]
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.auth, AuthStatus::Connected { .. })
    }

    pub fn user_id(&self) -> Option<&str> {
        self.user.as_ref().map(|user| user.id.as_str())
    }

    pub fn is_saved(&self, uri: &str) -> Option<bool> {
        self.saved.get(uri).copied()
    }

    fn remote_fresh(&self) -> Option<&RemoteSnapshot> {
        self.remote
            .as_ref()
            .filter(|remote| remote.received_at.elapsed() < REMOTE_FRESH)
    }

    fn playback_owner(&self, device_id: Option<&str>) -> PlaybackOwner {
        if device_id.is_some() && device_id == self.local_device_id.as_deref() {
            PlaybackOwner::Local
        } else {
            PlaybackOwner::Remote
        }
    }

    fn known_remote_device(&self, device_id: Option<&str>) -> Option<&Device> {
        let active = self
            .remote_fresh()
            .and_then(|remote| remote.state.device.as_ref());
        match device_id {
            None => active,
            Some(device_id) => active
                .filter(|device| device.id.as_deref() == Some(device_id))
                .or_else(|| {
                    self.devices
                        .iter()
                        .find(|device| device.id.as_deref() == Some(device_id))
                }),
        }
    }

    fn remote_issue(
        &self,
        command: RemoteCommand,
        device_id: Option<&str>,
    ) -> Option<&'static str> {
        remote_command_issue(self.known_remote_device(device_id), command)
    }

    fn allow_remote_command(&mut self, command: RemoteCommand, device_id: Option<&str>) -> bool {
        if device_id.is_none() && self.remote_fresh().is_none() {
            self.toast("Choose an active device first");
            return false;
        }
        let Some(message) = self.remote_issue(command, device_id) else {
            return true;
        };
        self.toast(message);
        false
    }

    pub(crate) fn target_can_control(&self) -> bool {
        match self.target() {
            Target::Local => true,
            Target::Remote(device_id) => {
                (device_id.is_some() || self.remote_fresh().is_some())
                    && self
                        .remote_issue(
                            RemoteCommand::Action(RemoteAction::Play),
                            device_id.as_deref(),
                        )
                        .is_none()
            }
        }
    }

    pub(crate) fn target_can_set_volume(&self) -> bool {
        match self.target() {
            Target::Local => true,
            Target::Remote(device_id) => {
                (device_id.is_some() || self.remote_fresh().is_some())
                    && self
                        .remote_issue(
                            RemoteCommand::Action(RemoteAction::Volume),
                            device_id.as_deref(),
                        )
                        .is_none()
            }
        }
    }

    /// Where playback commands go: this computer's player or a remote device.
    pub fn target(&self) -> Target {
        if self.local_ready && self.local.is_active() {
            return Target::Local;
        }
        if let Some(selected) = &self.selected_device
            && self.playback_owner(Some(selected)) == PlaybackOwner::Remote
        {
            return Target::Remote(Some(selected.clone()));
        }
        if let Some(remote) = self.remote_fresh() {
            let device = remote.state.device.as_ref();
            let owner = self.playback_owner(device.and_then(|device| device.id.as_deref()));
            if owner == PlaybackOwner::Remote
                && (remote.state.is_playing || remote.state.item.is_some())
            {
                return Target::Remote(device.and_then(|device| device.id.clone()));
            }
        }
        if self.local_ready {
            Target::Local
        } else {
            Target::Remote(None)
        }
    }

    /// Whether the optional personal Web API session is ready.
    pub fn own_web_app(&self) -> bool {
        self.web_app.is_some()
    }

    /// The context playing as the interface should show it: the one just
    /// asked for until Spotify's state confirms it, then Spotify's own.
    pub fn playing_context_uri(&self) -> Option<String> {
        let remote = self
            .remote
            .as_ref()
            .and_then(|remote| remote.state.context.as_ref())
            .map(|context| context.uri.clone());
        if let Some(assumed) = &self.assumed_context {
            let held = assumed.at.elapsed() < ASSUMED_CONTEXT_HOLD;
            // A filtered or sorted view plays as plain tracks, so Spotify
            // never names its source context. Keep the context until an
            // actual state contradicts it or playback stops.
            let contradicted = remote.as_deref().is_some_and(|uri| uri != assumed.uri);
            if held || (!contradicted && self.believed_playing()) {
                return Some(assumed.uri.clone());
            }
        }
        remote
    }

    /// Whether something plays, as the interface should show it: what it
    /// just asked for, before any state reports back.
    pub fn believed_playing(&self) -> bool {
        if let Some((playing, at)) = self.optimistic_playing
            && at.elapsed() < PLAYBACK_HOLD
        {
            return playing;
        }
        self.now_playing().is_some_and(|now| now.playing)
    }

    pub fn playing_context_shuffle(&self) -> bool {
        self.shuffle_wanted
    }

    /// The playing thing as a playable item, for menus that act on it:
    /// the cached full track when known, a minimal one otherwise.
    pub fn now_playing_item(&self) -> Option<PlayableItem> {
        let now = self.now_playing()?;
        if now.is_episode {
            return None;
        }
        if let Some(track) = now.id.as_deref().and_then(|id| self.track_cache.get(id)) {
            return Some(PlayableItem::Track(track.clone()));
        }
        Some(PlayableItem::Track(Track {
            id: now.id.clone(),
            uri: now.uri.clone(),
            name: now.title.clone(),
            artists: now.artists.clone(),
            duration_ms: now.duration_ms,
            ..Track::default()
        }))
    }

    pub fn now_playing(&self) -> Option<NowPlaying> {
        if self.local.is_active() {
            let track = self.local.track.as_ref()?;
            let cached = track
                .uri
                .rsplit(':')
                .next()
                .and_then(|id| self.track_cache.get(id));
            let artists = cached
                .map(|cached| cached.artists.clone())
                .unwrap_or_else(|| {
                    track
                        .artists
                        .iter()
                        .map(|name| ArtistRef {
                            id: None,
                            name: name.clone(),
                            uri: None,
                        })
                        .collect()
                });
            let playing = match self.optimistic_playing {
                Some((playing, at)) if at.elapsed() < PLAYBACK_HOLD => playing,
                _ => self.local.playback == Playback::Playing,
            };
            return Some(NowPlaying {
                local: true,
                device_name: None,
                uri: track.uri.clone(),
                id: util::uri_id(&track.uri).map(str::to_string),
                title: track.title.clone(),
                subtitle: track.artist_names(),
                artists,
                album_name: track.album.clone(),
                album_id: cached
                    .and_then(|cached| cached.album.as_ref())
                    .map(|album| album.id.clone()),
                show_id: None,
                art_url: track.art_url.clone(),
                art_small: track
                    .art_small_url
                    .clone()
                    .or_else(|| track.art_url.clone()),
                duration_ms: track.duration_ms,
                position_ms: self.local.position_now(),
                playing,
                loading: self.local.playback == Playback::Loading,
                shuffle: self.shuffle_wanted,
                repeat: self.local.repeat,
                volume_percent: volume_to_percent(self.local.volume),
                can_control: true,
                is_episode: track.is_episode,
            });
        }
        let remote = self.remote_fresh()?;
        let item = remote.state.item.as_ref()?;
        let device = remote.state.device.as_ref();
        let playing = match self.optimistic_playing {
            Some((playing, at)) if at.elapsed() < PLAYBACK_HOLD => playing,
            _ => remote.state.is_playing,
        };
        let position = match self.pending_remote_position {
            Some((position, at)) if at.elapsed() < OPTIMISTIC_HOLD => position,
            _ => {
                let base = remote.state.progress_ms.unwrap_or(0);
                if remote.state.is_playing {
                    (base as u64 + remote.received_at.elapsed().as_millis() as u64)
                        .min(item.duration_ms() as u64) as u32
                } else {
                    base
                }
            }
        };
        let volume = match self.pending_remote_volume {
            Some((volume, at)) if at.elapsed() < OPTIMISTIC_HOLD => volume,
            _ => device
                .and_then(|device| device.volume_percent)
                .unwrap_or(50),
        };
        let (artists, album_name, album_id, show_id, is_episode) = match item {
            PlayableItem::Track(track) => (
                track.artists.clone(),
                track
                    .album
                    .as_ref()
                    .map(|album| album.name.clone())
                    .unwrap_or_default(),
                track.album.as_ref().map(|album| album.id.clone()),
                None,
                false,
            ),
            PlayableItem::Episode(episode) => (
                Vec::new(),
                episode
                    .show
                    .as_ref()
                    .map(|show| show.name.clone())
                    .unwrap_or_default(),
                None,
                episode.show.as_ref().map(|show| show.id.clone()),
                true,
            ),
        };
        Some(NowPlaying {
            local: false,
            device_name: device.map(|device| device.name.clone()),
            uri: item.uri().to_string(),
            id: item.id().map(str::to_string),
            title: item.name().to_string(),
            subtitle: item.subtitle(),
            artists,
            album_name,
            album_id,
            show_id,
            art_url: item.image(640).map(str::to_string),
            art_small: item.image(64).map(str::to_string),
            duration_ms: item.duration_ms(),
            position_ms: position,
            playing,
            loading: false,
            shuffle: self.shuffle_wanted,
            repeat: RepeatMode::from_api(&remote.state.repeat_state),
            volume_percent: volume,
            can_control: device.is_none_or(|device| !device.is_restricted),
            is_episode,
        })
    }

    /// The play request for `key` (a context or track URI) is still waiting
    /// for Spotify to react.
    pub fn play_pending(&self, key: &str) -> bool {
        self.pending_fresh() && self.pending_play_keys.iter().any(|k| k == key)
    }

    pub fn any_play_pending(&self) -> bool {
        self.pending_fresh() && !self.pending_play_keys.is_empty()
    }

    fn pending_fresh(&self) -> bool {
        // A request queued behind a connecting engine stays pending for as
        // long as the engine may take; an ordinary request times out fast.
        self.queued_play.is_some()
            || self
                .pending_play_at
                .is_some_and(|at| at.elapsed() < Duration::from_secs(8))
    }

    fn set_play_pending(&mut self, keys: Vec<String>) {
        self.pending_play_keys = keys;
        self.pending_play_at = Some(Instant::now());
    }

    fn clear_play_pending(&mut self) {
        self.pending_play_keys.clear();
        self.pending_play_at = None;
    }

    /// The colour to tint the interface with, from the playing art.
    pub fn now_playing_tint(&self) -> Option<Color32> {
        if !self.settings.accent_from_art {
            return None;
        }
        let now = self.now_playing()?;
        let url = now.art_small.or(now.art_url)?;
        self.accents.get(&url).copied()
    }

    pub fn tint_for(&mut self, url: Option<&str>) -> Option<Color32> {
        let url = url?;
        if let Some(color) = self.accents.get(url) {
            return Some(*color);
        }
        if self.accent_pending.insert(url.to_string()) {
            self.backend.send(Command::Accent {
                url: url.to_string(),
            });
        }
        None
    }

    // ---- frame ---------------------------------------------------------------

    fn handle_events(&mut self) {
        for event in self.backend.poll() {
            if self.offline {
                continue;
            }
            match event {
                Event::Auth(status) => self.handle_auth(status),
                Event::Playback(status) => self.handle_playback(status),
                Event::Local(state) => self.handle_local(*state),
                Event::Api(response) => self.handle_api(*response),
                Event::Accent { url, color } => {
                    self.accent_pending.remove(&url);
                    let tint = self.palette.tint_from_art(color);
                    self.accents.insert(url, tint);
                }
                Event::Error(message) => self.toast_error(message),
                Event::Lyrics {
                    uri,
                    allow_lrclib,
                    result,
                } => {
                    if self.lyrics_uri.as_deref() == Some(uri.as_str())
                        && allow_lrclib == self.settings.lrclib_lyrics
                    {
                        self.lyrics = match result {
                            Ok(found) => Loadable::Loaded(found),
                            Err(error) => Loadable::Failed(error),
                        };
                    }
                }
                Event::PlaylistCache {
                    id,
                    snapshot,
                    items,
                } => {
                    if let Some(page) = self.playlist_pages.get_mut(&id) {
                        page.pending_cache = Some((snapshot, items));
                    }
                    self.try_adopt_playlist_cache(&id);
                }
                Event::UserName { id, name } => {
                    self.user_names.insert(id, name);
                }
                Event::WebApp { client_id } => self.web_app = client_id,
                Event::UpdateAvailable { version, url } => {
                    let notice = crate::updates::Release { version, url };
                    if self.update.as_ref() != Some(&notice) {
                        self.toast(format!("Fastpotify {} is out", notice.version));
                    }
                    self.update = Some(notice);
                }
            }
        }
    }

    fn handle_auth(&mut self, status: AuthStatus) {
        match &status {
            AuthStatus::Connected { .. } => {
                self.sign_in_url = None;
                self.reset_data();
                self.load_playlists();
                self.ensure_loaded(self.page().clone());
                self.poll_remote(true);
            }
            AuthStatus::WaitingForBrowser { url } => self.sign_in_url = Some(url.clone()),
            AuthStatus::SignedOut => {
                self.capture_resume_before_playback_loss(Instant::now());
                self.sign_in_url = None;
                self.web_app = None;
                self.user = None;
                self.pending_local_shuffle = None;
                if self.queued_play.take().is_some() {
                    self.clear_play_pending();
                }
                self.local = LocalState::default();
                self.local_ready = false;
                self.local_device_id = None;
                self.local_playback = LocalPlayback::Unavailable;
                self.remote = None;
                self.reset_data();
            }
            AuthStatus::Failed(message) => {
                self.sign_in_url = None;
                self.toast_error(message.clone());
            }
            _ => {}
        }
        self.auth = status;
    }

    fn handle_playback(&mut self, status: LocalPlayback) {
        if !matches!(&status, LocalPlayback::Ready { .. })
            && self.local.connected
            && self.local.is_active()
        {
            self.capture_resume_before_playback_loss(Instant::now());
        }
        match &status {
            LocalPlayback::Ready { device_id } => {
                self.local_device_id = Some(device_id.clone());
                self.local_ready = true;
            }
            LocalPlayback::Unavailable => {
                self.local_ready = false;
                self.local.connected = false;
                self.local_device_id = None;
                self.pending_local_shuffle = None;
                if self.queued_play.take().is_some() {
                    self.clear_play_pending();
                }
            }
            LocalPlayback::Failed(message) => {
                self.local_ready = false;
                self.local.connected = false;
                self.pending_local_shuffle = None;
                if self.queued_play.take().is_some() {
                    self.clear_play_pending();
                }
                self.toast_error(format!("Local playback: {message}"));
            }
            LocalPlayback::Authorizing | LocalPlayback::Connecting => {
                self.local_ready = false;
                self.local.connected = false;
            }
        }
        self.local_playback = status;
        self.dispatch_ready_local_work();
    }

    fn reset_data(&mut self) {
        self.library = Library::default();
        self.home = HomeData::default();
        self.playlist_pages.clear();
        self.album_pages.clear();
        self.artist_pages.clear();
        self.show_pages.clear();
        self.saved.clear();
        self.saved_pending.clear();
        self.queue = Loadable::NotLoaded;
        self.devices.clear();
        self.control_devices_stale = true;
        self.devices_fetched_at = None;
        self.search.results = Loadable::NotLoaded;
        self.search.committed.clear();
    }

    fn handle_local(&mut self, state: LocalState) {
        let observed_at = Instant::now();
        let track_changed = state.track != self.local.track;
        let paused = state.playback == Playback::Paused && self.local.playback != Playback::Paused;
        let was_selected_active = self.local.connected && self.local.is_active();
        let is_selected_active = state.connected && state.is_active();
        if was_selected_active && !is_selected_active {
            self.capture_resume_before_playback_loss(observed_at);
        }
        self.reconcile_authoritative_shuffle(
            state.connected && state.is_active(),
            state.shuffle,
            PlaybackOwner::Local,
        );
        if state.playback != self.local.playback {
            self.optimistic_playing = None;
            if matches!(state.playback, Playback::Playing | Playback::Loading) {
                self.clear_play_pending();
            }
        }
        if state.track != self.local.track {
            self.clear_play_pending();
        }
        let held_volume = self.held_local_volume(state.volume);
        if held_volume.is_none() && state.volume != self.settings.volume {
            self.settings.volume = state.volume;
            self.settings_dirty = true;
        }
        if state.seek_sequence != self.local.seek_sequence
            && let Some(controls) = &self.media_controls
        {
            controls.seeked(state.position_ms);
        }
        if let Some(error) = &state.error
            && self.local.error.as_deref() != Some(error.as_str())
        {
            self.toast_error(error.clone());
            if error.starts_with("This item isn't available")
                && self.unavailable_recovery.record(Instant::now())
            {
                self.backend.send(Command::Reconnect);
                self.toast("Spotify's audio service faltered; reconnecting local playback");
            }
        }
        self.local = state;
        if let Some(volume) = held_volume {
            self.local.volume = volume;
        }
        if track_changed || (!was_selected_active && is_selected_active) {
            self.on_now_playing_changed();
        }
        if paused {
            self.update_resume_point_at(true, observed_at);
        }
        self.dispatch_ready_local_work();
    }

    fn on_now_playing_changed(&mut self) {
        let Some(now) = self.now_playing() else {
            return;
        };
        if self.last_now_playing_uri.as_deref() == Some(now.uri.as_str()) {
            return;
        }
        self.last_now_playing_uri = Some(now.uri.clone());
        self.resume_context = self.playing_context_uri();
        self.resume_track = Some(now.uri.clone());
        self.resume_position_ms = now.position_ms;
        self.last_resume_checkpoint = Instant::now();
        self.mark_session_dirty();
        if now.local
            && !now.is_episode
            && let Some(id) = &now.id
            && !self.track_cache.contains_key(id)
            && self.track_requests.insert(id.clone())
        {
            self.backend.api(ApiRequest::Track { id: id.clone() });
        }
        self.request_contains(vec![now.uri.clone()]);
        if let Some(url) = now.art_small.or(now.art_url) {
            self.tint_for(Some(&url));
        }
        if matches!(self.page(), Page::Queue) || self.show_queue_panel {
            self.refresh_queue(true);
        }
        if self.show_lyrics_panel {
            self.request_lyrics();
        }
    }

    /// Asks for the playing track's lyrics unless they are here or on the
    /// way. Podcasts have no lyrics to ask for.
    pub fn request_lyrics(&mut self) {
        let Some(now) = self.now_playing() else {
            return;
        };
        if self.lyrics_uri.as_deref() == Some(now.uri.as_str())
            && !matches!(self.lyrics, Loadable::NotLoaded | Loadable::Failed(_))
        {
            return;
        }
        self.lyrics_uri = Some(now.uri.clone());
        self.lyrics_following = true;
        self.lyrics_line_shown = None;
        if now.is_episode || self.offline {
            self.lyrics = Loadable::Loaded(None);
            return;
        }
        self.lyrics = Loadable::Loading;
        self.backend.send(Command::Lyrics(Box::new(LyricsRequest {
            uri: now.uri,
            allow_lrclib: self.settings.lrclib_lyrics,
            query: crate::lyrics::Query {
                artist: now
                    .artists
                    .first()
                    .map(|artist| artist.name.clone())
                    .unwrap_or_default(),
                title: now.title,
                album: now.album_name,
                duration_ms: now.duration_ms,
            },
        })));
    }

    fn tick(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        if !self.zoom_applied {
            self.zoom_applied = true;
            if (ctx.zoom_factor() - self.settings.zoom).abs() > 0.001 {
                ctx.set_zoom_factor(self.settings.zoom);
            }
        } else {
            let requested = ctx.zoom_factor();
            let zoom = if requested.is_finite() {
                requested.clamp(crate::settings::ZOOM_MIN, crate::settings::ZOOM_MAX)
            } else {
                1.0
            };
            if (requested - zoom).abs() > 0.001 {
                ctx.set_zoom_factor(zoom);
            }
            if (zoom - self.settings.zoom).abs() > 0.001 {
                self.settings.zoom = zoom;
                self.mark_settings_dirty();
            }
        }
        self.toasts
            .retain(|toast| toast.created.elapsed() < TOAST_LIFETIME);

        if self.settings.check_for_updates
            && !self.offline
            && self
                .last_update_check
                .is_none_or(|at| at.elapsed() >= crate::updates::CHECK_INTERVAL)
        {
            self.last_update_check = Some(now);
            self.backend.send(Command::CheckForUpdates);
        }

        if self.is_connected() && !self.offline {
            let interval = match self.target() {
                Target::Local if self.local.is_active() => REMOTE_POLL_IDLE,
                _ => REMOTE_POLL_ACTIVE,
            };
            if !self.remote_poll_pending && self.remote_polled_at.elapsed() >= interval {
                self.poll_remote(false);
            }
            if let Some(due) = self.remote_recheck_at
                && Instant::now() >= due
            {
                self.remote_recheck_at = None;
                self.poll_remote(true);
            }
            if self.show_devices
                && !self.devices_loading
                && self
                    .devices_fetched_at
                    .is_none_or(|at| at.elapsed() > DEVICES_FRESH)
            {
                self.refresh_devices();
            }
            if (self.show_queue_panel || matches!(self.page(), Page::Queue))
                && !self.queue.is_loading()
                && self
                    .queue_fetched_at
                    .is_none_or(|at| at.elapsed() > Duration::from_secs(20))
            {
                self.refresh_queue(false);
            }
        }

        if let Some(typed) = self.search.typed_at {
            if typed.elapsed() >= SEARCH_DEBOUNCE {
                self.search.typed_at = None;
                let query = self.search.query.trim().to_string();
                self.run_search(query);
            } else {
                ctx.request_repaint_after(SEARCH_DEBOUNCE - typed.elapsed());
            }
        }

        if self.last_eviction.elapsed() > Duration::from_secs(20) {
            self.last_eviction = now;
            self.backend.art().evict_stale(ctx);
        }
        if let Some(Err(error)) = self.try_autosave_settings_at(now) {
            log::warn!(
                "{error}; settings remain pending and will retry in {} seconds",
                STATE_SAVE_RETRY.as_secs()
            );
        }
        self.update_resume_point_at(false, now);
        if let Some(Err(error)) = self.try_autosave_session_at(now) {
            log::warn!(
                "{error}; session state remains pending and will retry in {} seconds",
                STATE_SAVE_RETRY.as_secs()
            );
        }
        for wait in [
            state_save_wait(
                self.settings_dirty,
                self.settings_save_retrying,
                self.last_settings_save_attempt,
                now,
            ),
            state_save_wait(
                self.session_dirty,
                self.session_save_retrying,
                self.last_session_save_attempt,
                now,
            ),
        ]
        .into_iter()
        .flatten()
        {
            ctx.request_repaint_after(wait);
        }
    }

    /// Note that a setting changed, so the file is saved shortly.
    pub fn mark_settings_dirty(&mut self) {
        self.settings_dirty = true;
    }

    /// Note that restorable UI or playback state changed.
    pub fn mark_session_dirty(&mut self) {
        self.session_dirty = true;
    }

    fn try_autosave_settings_at(&mut self, now: Instant) -> Option<Result<(), SaveError>> {
        state_save_due(
            self.settings_dirty,
            self.settings_save_retrying,
            self.last_settings_save_attempt,
            now,
        )
        .then(|| self.save_settings_at(now))
    }

    fn try_autosave_session_at(&mut self, now: Instant) -> Option<Result<(), SaveError>> {
        state_save_due(
            self.session_dirty,
            self.session_save_retrying,
            self.last_session_save_attempt,
            now,
        )
        .then(|| self.save_session_at(now))
    }

    fn save_settings_at(&mut self, now: Instant) -> Result<(), SaveError> {
        self.last_settings_save_attempt = now;
        let result = if self.offline {
            // Demo data must never overwrite the person's real preferences.
            Ok(())
        } else {
            self.settings.save(&self.dirs.settings_file())
        };
        match result {
            Ok(()) => {
                self.settings_dirty = false;
                self.settings_save_retrying = false;
                Ok(())
            }
            Err(error) => {
                self.settings_dirty = true;
                self.settings_save_retrying = true;
                Err(error)
            }
        }
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        let dark = ctx.theme() == egui::Theme::Dark;
        if self.applied_dark != Some(dark) {
            self.palette = if dark {
                Palette::dark()
            } else {
                Palette::light()
            };
            theme::apply(ctx, &self.palette);
            self.applied_dark = Some(dark);
            self.accents.clear();
            self.accent_pending.clear();
        }
    }

    fn handle_tray(&mut self) {
        let Some(commands) = self.tray.as_ref().map(TrayService::drain_commands) else {
            return;
        };
        for command in commands {
            match command {
                TrayCommand::Show => self.actions.push(Action::ShowWindow),
                TrayCommand::ShowHide => self.actions.push(if self.window_hidden {
                    Action::ShowWindow
                } else {
                    Action::HideWindow
                }),
                TrayCommand::PlayPause => self.actions.push(Action::TogglePlay),
                TrayCommand::Next => self.actions.push(Action::Next),
                TrayCommand::Previous => self.actions.push(Action::Previous),
                TrayCommand::Quit => self.actions.push(Action::Quit),
            }
        }
    }

    fn handle_control_commands(&mut self) {
        let Some(queue) = &self.control_commands else {
            return;
        };
        let commands: Vec<ControlCommand> =
            std::mem::take(&mut *queue.lock().unwrap_or_else(|p| p.into_inner()));
        for command in commands {
            let playing = self.now_playing().is_some_and(|now| now.playing);
            let action = match command {
                ControlCommand::Show => Some(Action::ShowWindow),
                ControlCommand::PlayPause => Some(Action::TogglePlay),
                ControlCommand::Play => (!playing).then_some(Action::TogglePlay),
                ControlCommand::Pause => playing.then_some(Action::TogglePlay),
                ControlCommand::Next => Some(Action::Next),
                ControlCommand::Previous => Some(Action::Previous),
                ControlCommand::SeekBy(offset) => Some(Action::SeekBy(offset)),
                ControlCommand::VolumeBy(delta) => Some(Action::VolumeBy(delta)),
                ControlCommand::SetVolume(volume) => Some(Action::SetVolume(volume.min(100))),
                ControlCommand::ToggleMute => Some(Action::ToggleMute),
                ControlCommand::ToggleShuffle => Some(Action::ToggleShuffle),
                ControlCommand::CycleRepeat => Some(Action::CycleRepeat),
                ControlCommand::SetShuffle(shuffle) => Some(Action::SetShuffle(shuffle)),
                ControlCommand::SetRepeat(mode) => Some(Action::SetRepeat(mode)),
                ControlCommand::SeekTo(position_ms) => Some(Action::Seek(position_ms)),
                ControlCommand::ToggleSaved => match self.now_playing() {
                    Some(now) if now.is_episode => {
                        self.toast("The external like control supports music tracks only");
                        None
                    }
                    Some(now) => Some(Action::ToggleSaved(now.uri)),
                    None => None,
                },
                ControlCommand::PlayUri(uri) if util::uri_kind(&uri) == Some("track") => {
                    Some(Action::PlayUris {
                        uris: vec![uri],
                        index: 0,
                    })
                }
                ControlCommand::PlayUri(uri) => Some(Action::PlayContext {
                    uri,
                    offset_uri: None,
                    offset_index: None,
                }),
                ControlCommand::Transfer(device_id) => Some(Action::Transfer(device_id)),
                ControlCommand::RefreshDevices => Some(Action::RefreshDevices),
            };
            if let Some(action) = action {
                self.actions.push(action);
            }
        }
    }

    fn handle_media_commands(&mut self) {
        let Some(commands) = self
            .media_controls
            .as_ref()
            .map(MediaService::drain_commands)
        else {
            return;
        };
        for command in commands {
            let playing = self.now_playing().is_some_and(|now| now.playing);
            let action = match command {
                MediaCommand::Play => (!playing).then_some(Action::TogglePlay),
                MediaCommand::Pause | MediaCommand::Stop => playing.then_some(Action::TogglePlay),
                MediaCommand::PlayPause => Some(Action::TogglePlay),
                MediaCommand::Next => Some(Action::Next),
                MediaCommand::Previous => Some(Action::Previous),
                MediaCommand::SeekBy(offset) => Some(Action::SeekBy(offset)),
                MediaCommand::SetPosition {
                    track_uri,
                    position_ms,
                } => self
                    .now_playing()
                    .filter(|now| now.uri == track_uri)
                    .map(|_| Action::Seek(position_ms)),
                MediaCommand::SetVolume(volume) => Some(Action::SetVolume(
                    (volume.clamp(0.0, 1.0) * 100.0).round() as u8,
                )),
                MediaCommand::SetShuffle(shuffle) => Some(Action::SetShuffle(shuffle)),
                MediaCommand::SetRepeat(mode) => Some(Action::SetRepeat(mode)),
                MediaCommand::OpenUri(uri) => Some(Action::PlayContext {
                    uri,
                    offset_uri: None,
                    offset_index: None,
                }),
                MediaCommand::Raise => Some(Action::ShowWindow),
                MediaCommand::Quit => Some(Action::Quit),
            };
            if let Some(action) = action {
                self.actions.push(action);
            }
        }
    }

    fn sync_media_controls(&mut self) {
        let state = match self.now_playing() {
            Some(now) => MediaState {
                playback: if now.playing {
                    Playback::Playing
                } else if now.loading {
                    Playback::Loading
                } else {
                    Playback::Paused
                },
                track: Some(MediaTrack {
                    uri: now.uri.clone(),
                    title: now.title.clone(),
                    artists: now
                        .artists
                        .iter()
                        .map(|artist| artist.name.clone())
                        .collect(),
                    album: now.album_name.clone(),
                    art_url: now.art_url.clone(),
                    duration_ms: now.duration_ms,
                }),
                position_ms: now.position_ms,
                volume: f64::from(now.volume_percent) / 100.0,
                shuffle: now.shuffle,
                repeat: now.repeat,
                can_control: now.can_control,
            },
            None => MediaState::default(),
        };
        if let Some(controls) = &mut self.media_controls {
            controls.update(state);
        }
        let playing = self.now_playing().is_some_and(|now| now.playing);
        if let Some(tray) = &mut self.tray {
            tray.set_playing(playing);
        }
        if let Some(slot) = &self.control_now_playing {
            let snapshot = self.control_snapshot();
            *slot.lock().unwrap_or_else(|p| p.into_inner()) = snapshot;
        }
        if self.control_devices_stale
            && let Some(slot) = self.control_devices.clone()
        {
            let snapshot = self.control_devices_snapshot();
            *slot.lock().unwrap_or_else(|poison| poison.into_inner()) = snapshot;
            self.control_devices_stale = false;
        }
    }

    /// One line for the control channel's `nowplaying` verb: tab-separated
    /// `state, title, artists, album, position_ms, duration_ms, volume,
    /// shuffle, repeat, art_url, saved, device`, or
    /// [`crate::single_instance::NOTHING_PLAYING`]. The original nine fields
    /// stay in place for existing scripts.
    fn control_snapshot(&self) -> String {
        let Some(now) = self.now_playing() else {
            return crate::single_instance::NOTHING_PLAYING.to_owned();
        };
        let state = if now.playing { "playing" } else { "paused" };
        let saved = match self.is_saved(&now.uri) {
            Some(true) => "yes",
            Some(false) => "no",
            None => "unknown",
        };
        let device = match (now.device_name.as_deref(), now.local) {
            (Some(name), _) => name,
            (None, true) => self.settings.device_name.as_str(),
            (None, false) => "",
        };
        // Tabs separate the fields, so a tab inside one would shift the rest.
        let clean = |text: &str| text.replace(['\t', '\r', '\n'], " ");
        format!(
            "{state}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{saved}\t{}",
            clean(&now.title),
            clean(&now.subtitle),
            clean(&now.album_name),
            now.position_ms,
            now.duration_ms,
            now.volume_percent,
            if now.shuffle { "on" } else { "off" },
            now.repeat.api_name(),
            clean(now.art_url.as_deref().unwrap_or_default()),
            clean(device),
        )
    }

    fn control_devices_snapshot(&self) -> String {
        let devices: Vec<_> = self
            .devices
            .iter()
            .filter_map(|device| {
                Some(serde_json::json!({
                    "id": device.id.as_deref()?,
                    "name": device.name,
                    "kind": device.kind,
                    "active": device.is_active,
                    "restricted": device.is_restricted,
                    "supports_volume": device.supports_volume,
                }))
            })
            .collect();
        serde_json::to_string(&devices)
            .unwrap_or_else(|_| crate::single_instance::NO_DEVICES.to_owned())
    }

    // ---- loading ---------------------------------------------------------------

    fn load_playlists(&mut self) {
        if self.library.playlists.is_loading() {
            return;
        }
        self.library.playlists = Loadable::Loading;
        self.library.playlists_next = None;
        self.backend.api(ApiRequest::MyPlaylists { offset: 0 });
    }

    pub fn ensure_loaded(&mut self, page: Page) {
        if !self.is_connected() {
            return;
        }
        match page {
            Page::Home => self.load_home(false),
            Page::TopSongs => self.load_top_songs(false),
            Page::Search => {}
            Page::LikedSongs => {
                if !self.library.liked.loaded_once {
                    self.load_more(Page::LikedSongs);
                }
            }
            Page::Albums => {
                if !self.library.albums.loaded_once {
                    self.load_more(Page::Albums);
                }
            }
            Page::Artists => {
                if !self.library.artists.loaded_once {
                    self.load_more(Page::Artists);
                }
            }
            Page::Podcasts => {
                if !self.library.shows.loaded_once {
                    self.load_more(Page::Podcasts);
                }
            }
            Page::Episodes => {
                if !self.library.episodes.loaded_once {
                    self.load_more(Page::Episodes);
                }
            }
            Page::Playlist(id) => {
                let page = self.playlist_pages.entry(id.clone()).or_default();
                if page.playlist.needs_load() {
                    page.playlist = Loadable::Loading;
                    self.backend.api(ApiRequest::Playlist { id: id.clone() });
                }
                if !page.items.loaded_once && page.items.can_load_more() {
                    page.items.loading = true;
                    self.backend.api(ApiRequest::PlaylistItems {
                        id: id.clone(),
                        offset: 0,
                    });
                    // The disk may hold the whole list already; it is
                    // adopted only if Spotify's snapshot still matches.
                    self.backend
                        .send(Command::LoadPlaylistCache { id: id.clone() });
                }
                self.request_contains(vec![format!("spotify:playlist:{id}")]);
            }
            Page::Album(id) => {
                let page = self.album_pages.entry(id.clone()).or_default();
                if page.album.needs_load() {
                    page.album = Loadable::Loading;
                    self.backend.api(ApiRequest::Album { id: id.clone() });
                }
                self.request_contains(vec![format!("spotify:album:{id}")]);
            }
            Page::Artist(id) => {
                let page = self.artist_pages.entry(id.clone()).or_default();
                if page.artist.needs_load() {
                    page.artist = Loadable::Loading;
                    self.backend.api(ApiRequest::Artist { id: id.clone() });
                }
                let filter = page.filter;
                self.load_artist_albums(&id, filter);
                if page_related_needs_load(&self.artist_pages, &id) {
                    if let Some(page) = self.artist_pages.get_mut(&id) {
                        page.related = Loadable::Loading;
                    }
                    self.backend
                        .api(ApiRequest::RelatedArtists { id: id.clone() });
                }
                self.request_contains(vec![format!("spotify:artist:{id}")]);
            }
            Page::Show(id) => {
                let page = self.show_pages.entry(id.clone()).or_default();
                if page.show.needs_load() {
                    page.show = Loadable::Loading;
                    self.backend.api(ApiRequest::Show { id: id.clone() });
                }
                self.request_contains(vec![format!("spotify:show:{id}")]);
            }
            Page::Queue => self.refresh_queue(true),
            Page::Settings => {}
        }
    }

    fn load_artist_albums(&mut self, id: &str, filter: DiscographyFilter) {
        let Some(page) = self.artist_pages.get_mut(id) else {
            return;
        };
        let list = page.albums.entry(filter.groups().to_string()).or_default();
        if !list.loaded_once && list.can_load_more() {
            list.loading = true;
            self.backend.api(ApiRequest::ArtistAlbums {
                id: id.to_string(),
                groups: filter.groups().to_string(),
                offset: 0,
            });
        }
    }

    fn load_home(&mut self, force: bool) {
        if self.home.requested
            && !force
            && self
                .home
                .loaded_at
                .is_some_and(|at| at.elapsed() < Duration::from_secs(600))
        {
            return;
        }
        self.home.requested = true;
        self.home.loaded_at = Some(Instant::now());
        self.home.recently_played = Loadable::Loading;
        self.home.top_artists = Loadable::Loading;
        self.home.top_tracks = Loadable::Loading;
        self.backend.api(ApiRequest::RecentlyPlayed);
        self.backend.api(ApiRequest::TopArtists);
        self.backend.api(ApiRequest::TopTracks {
            offset: 0,
            full: false,
        });
        for term in DISCOVER_TERMS {
            self.home
                .discover
                .insert((*term).to_string(), Loadable::Loading);
            self.backend.api(ApiRequest::Discover {
                term: (*term).to_string(),
            });
        }
    }

    fn load_top_songs(&mut self, force: bool) {
        if self.home.top_songs_loading || (!force && self.home.top_songs_complete) {
            return;
        }
        self.home.top_songs = Loadable::Loading;
        self.home.top_songs_loading = true;
        self.home.top_songs_complete = false;
        self.backend.api(ApiRequest::TopTracks {
            offset: 0,
            full: true,
        });
    }

    pub fn load_more(&mut self, page: Page) {
        match page {
            Page::LikedSongs => {
                let list = &mut self.library.liked;
                if let Some(offset) = list.next_offset.filter(|_| list.can_load_more()) {
                    list.loading = true;
                    self.backend.api(ApiRequest::SavedTracks { offset });
                }
            }
            Page::Albums => {
                let list = &mut self.library.albums;
                if let Some(offset) = list.next_offset.filter(|_| list.can_load_more()) {
                    list.loading = true;
                    self.backend.api(ApiRequest::SavedAlbums { offset });
                }
            }
            Page::Artists => {
                let list = &mut self.library.artists;
                if list.can_load_more() {
                    list.loading = true;
                    self.backend.api(ApiRequest::FollowedArtists {
                        after: list.after.clone(),
                    });
                }
            }
            Page::Podcasts => {
                let list = &mut self.library.shows;
                if let Some(offset) = list.next_offset.filter(|_| list.can_load_more()) {
                    list.loading = true;
                    self.backend.api(ApiRequest::SavedShows { offset });
                }
            }
            Page::Episodes => {
                let list = &mut self.library.episodes;
                if let Some(offset) = list.next_offset.filter(|_| list.can_load_more()) {
                    list.loading = true;
                    self.backend.api(ApiRequest::SavedEpisodes { offset });
                }
            }
            Page::Playlist(id) => {
                if let Some(page) = self.playlist_pages.get_mut(&id) {
                    let list = &mut page.items;
                    if let Some(offset) = list.next_offset.filter(|_| list.can_load_more()) {
                        list.loading = true;
                        self.backend.api(ApiRequest::PlaylistItems { id, offset });
                    }
                }
            }
            Page::Album(id) => {
                if let Some(page) = self.album_pages.get_mut(&id) {
                    let list = &mut page.tracks;
                    if let Some(offset) = list.next_offset.filter(|_| list.can_load_more()) {
                        list.loading = true;
                        self.backend.api(ApiRequest::AlbumTracks { id, offset });
                    }
                }
            }
            Page::Show(id) => {
                if let Some(page) = self.show_pages.get_mut(&id) {
                    let list = &mut page.episodes;
                    if let Some(offset) = list.next_offset.filter(|_| list.can_load_more()) {
                        list.loading = true;
                        self.backend.api(ApiRequest::ShowEpisodes { id, offset });
                    }
                }
            }
            Page::Home => {
                if let Some(offset) = self.library.playlists_next.take() {
                    self.backend.api(ApiRequest::MyPlaylists { offset });
                }
            }
            _ => {}
        }
    }

    fn reload(&mut self, page: Page) {
        match &page {
            Page::Home => self.load_home(true),
            Page::TopSongs => self.load_top_songs(true),
            Page::LikedSongs => self.library.liked.reset(),
            Page::Albums => self.library.albums.reset(),
            Page::Artists => self.library.artists.reset(),
            Page::Podcasts => self.library.shows.reset(),
            Page::Episodes => self.library.episodes.reset(),
            Page::Playlist(id) => {
                self.playlist_pages.remove(id);
            }
            Page::Album(id) => {
                self.album_pages.remove(id);
            }
            Page::Artist(id) => {
                self.artist_pages.remove(id);
            }
            Page::Show(id) => {
                self.show_pages.remove(id);
            }
            Page::Queue => self.queue = Loadable::NotLoaded,
            _ => {}
        }
        self.ensure_loaded(page);
    }

    fn poll_remote(&mut self, _immediate: bool) {
        if !self.is_connected() {
            return;
        }
        self.remote_poll_pending = true;
        self.remote_polled_at = Instant::now();
        self.remote_poll_seq += 1;
        self.backend.api(ApiRequest::PlaybackState {
            seq: self.remote_poll_seq,
        });
    }

    fn refresh_devices(&mut self) {
        if !self.is_connected() || self.devices_loading {
            return;
        }
        self.devices_loading = true;
        self.backend.api(ApiRequest::Devices);
    }

    fn refresh_queue(&mut self, force: bool) {
        if !self.is_connected() {
            return;
        }
        if self.queue.is_loading() && !force {
            return;
        }
        if !matches!(self.queue, Loadable::Loaded(_)) {
            self.queue = Loadable::Loading;
        }
        self.queue_fetched_at = Some(Instant::now());
        self.backend.api(ApiRequest::Queue);
    }

    fn run_search(&mut self, query: String) {
        if query.is_empty() {
            self.search.results = Loadable::NotLoaded;
            self.search.committed.clear();
            return;
        }
        if query == self.search.committed && !self.search.results.needs_load() {
            return;
        }
        self.search.serial += 1;
        self.search.committed = query.clone();
        self.search.results = Loadable::Loading;
        self.backend.api(ApiRequest::Search {
            query,
            serial: self.search.serial,
        });
    }

    /// Asks Spotify whether these items are in the library, in batches.
    /// Resolve adder ids that have no known name yet.
    pub fn request_user_names(&mut self, ids: Vec<String>) {
        let unknown: Vec<String> = ids
            .into_iter()
            .filter(|id| !self.user_names.contains_key(id))
            .collect();
        if unknown.is_empty() {
            return;
        }
        for id in &unknown {
            self.user_names.insert(id.clone(), None);
        }
        self.backend.send(Command::UserNames(unknown));
    }

    pub fn request_contains(&mut self, uris: Vec<String>) {
        let Some(user_id) = self.user_id().map(str::to_string) else {
            return;
        };
        let mut batch = Vec::new();
        for uri in uris {
            if uri.is_empty()
                || self.saved.contains_key(&uri)
                || self.saved_pending.contains(&uri)
                || uri.starts_with("spotify:local")
            {
                continue;
            }
            self.saved_pending.insert(uri.clone());
            batch.push(uri);
            if batch.len() == CONTAINS_BATCH {
                self.backend.api(ApiRequest::Contains {
                    uris: std::mem::take(&mut batch),
                    user_id: user_id.clone(),
                });
            }
        }
        if !batch.is_empty() {
            self.backend.api(ApiRequest::Contains {
                uris: batch,
                user_id,
            });
        }
    }

    // ---- api responses -------------------------------------------------------

    fn handle_api(&mut self, response: ApiResponse) {
        match response {
            ApiResponse::Me(result) => match result {
                Ok(user) => {
                    self.user = Some(user);
                    let page = self.page().clone();
                    self.ensure_loaded(page);
                    if let Some(now) = self.now_playing() {
                        self.request_contains(vec![now.uri]);
                    }
                }
                Err(error) => self.toast_error(format!("Couldn't load your profile: {error}")),
            },
            ApiResponse::Devices(result) => {
                self.devices_loading = false;
                self.devices_fetched_at = Some(Instant::now());
                match result {
                    Ok(devices) => {
                        self.devices = devices;
                        self.control_devices_stale = true;
                        if let Some(selected) = &self.selected_device
                            && !self
                                .devices
                                .iter()
                                .any(|device| device.id.as_deref() == Some(selected.as_str()))
                        {
                            self.selected_device = None;
                        }
                    }
                    Err(error) => self.toast_error(format!("Couldn't list devices: {error}")),
                }
            }
            ApiResponse::PlaybackState { seq, result } => {
                if seq != self.remote_poll_seq {
                    // An older poll finishing late describes the past.
                    return;
                }
                self.remote_poll_pending = false;
                match result {
                    Ok(state) => {
                        let observed_at = Instant::now();
                        let selected_remote_disappears =
                            state.as_ref().is_none_or(|state| state.item.is_none())
                                && self.now_playing().is_some_and(|now| !now.local);
                        if selected_remote_disappears {
                            self.capture_resume_before_playback_loss(observed_at);
                        }
                        let previous_uri = self.remote.as_ref().and_then(|remote| {
                            remote
                                .state
                                .item
                                .as_ref()
                                .map(|item| item.uri().to_string())
                        });
                        let was_playing = self
                            .remote
                            .as_ref()
                            .is_some_and(|remote| remote.state.is_playing);
                        self.remote = state.map(|state| RemoteSnapshot {
                            state,
                            received_at: observed_at,
                        });
                        if let Some((active, shuffle, owner)) = self.remote.as_ref().map(|remote| {
                            let device_id = remote
                                .state
                                .device
                                .as_ref()
                                .and_then(|device| device.id.as_deref());
                            (
                                remote.state.is_playing || remote.state.item.is_some(),
                                remote.state.shuffle_state,
                                self.playback_owner(device_id),
                            )
                        }) {
                            self.reconcile_authoritative_shuffle(active, shuffle, owner);
                        }
                        if let Some(context) = self
                            .remote
                            .as_ref()
                            .and_then(|remote| remote.state.context.as_ref())
                            .map(|context| context.uri.clone())
                        {
                            // Mid-takeover the cluster still names the old
                            // context; noting that would dance the sidebar
                            // back and forth.
                            let stale = self.assumed_context.as_ref().is_some_and(|assumed| {
                                assumed.at.elapsed() < ASSUMED_CONTEXT_HOLD
                                    && assumed.uri != context
                            });
                            if !stale {
                                self.note_recent_context(&context);
                            }
                        }
                        let uri = self.remote.as_ref().and_then(|remote| {
                            remote
                                .state
                                .item
                                .as_ref()
                                .map(|item| item.uri().to_string())
                        });
                        if let Some(remote) = &self.remote
                            && let Some(device) = &remote.state.device
                            && device.id.is_some()
                            && let Some(known) =
                                self.devices.iter_mut().find(|known| known.id == device.id)
                        {
                            known.is_active = true;
                            known.volume_percent = device.volume_percent;
                        }
                        if let Some((wanted, _)) = self.optimistic_playing
                            && self
                                .remote
                                .as_ref()
                                .is_some_and(|remote| remote.state.is_playing == wanted)
                        {
                            self.optimistic_playing = None;
                        }
                        if uri != previous_uri {
                            self.on_now_playing_changed();
                        }
                        if was_playing
                            && self.remote.as_ref().is_some_and(|remote| {
                                !remote.state.is_playing && remote.state.item.is_some()
                            })
                        {
                            self.update_resume_point_at(true, observed_at);
                        }
                    }
                    Err(error) => log::debug!("playback state unavailable: {error}"),
                }
            }
            ApiResponse::Queue(result) => {
                self.queue = Loadable::from_result(result);
                if let Some(queue) = self.queue.get() {
                    let uris: Vec<String> = queue
                        .queue
                        .iter()
                        .map(|item| item.uri().to_string())
                        .collect();
                    self.request_contains(uris);
                }
            }
            ApiResponse::RecentlyPlayed(result) => {
                if let Ok(history) = &result {
                    // Oldest first, so the newest ends up at the front.
                    let contexts: Vec<String> = history
                        .iter()
                        .rev()
                        .filter_map(|play| play.context.as_ref().map(|context| context.uri.clone()))
                        .collect();
                    for context in contexts {
                        self.note_recent_context(&context);
                    }
                }
                self.home.recently_played = Loadable::from_result(result);
            }
            ApiResponse::TopTracks {
                offset,
                full,
                result,
            } => {
                if full {
                    match result {
                        Ok(page) => {
                            let received = page.items.len() as u32;
                            let tracks = page.items;
                            let uris: Vec<String> =
                                tracks.iter().map(|track| track.uri.clone()).collect();
                            self.request_contains(uris);
                            if offset == 0 {
                                self.home.top_songs = Loadable::Loaded(tracks);
                            } else if let Some(current) = self.home.top_songs.get_mut() {
                                current.extend(tracks);
                            }
                            if page.next.is_some() && received > 0 && offset + received < 100 {
                                self.backend.api(ApiRequest::TopTracks {
                                    offset: offset + received,
                                    full: true,
                                });
                            } else {
                                self.home.top_songs_loading = false;
                                self.home.top_songs_complete = true;
                            }
                        }
                        Err(error) => {
                            self.home.top_songs = Loadable::Failed(error.to_string());
                            self.home.top_songs_loading = false;
                        }
                    }
                } else if let Ok(page) = result {
                    let tracks = page.items;
                    let seeds: Vec<String> = tracks
                        .iter()
                        .filter_map(|track| track.id.clone())
                        .take(5)
                        .collect();
                    if !seeds.is_empty() && self.home.recommendations.needs_load() {
                        self.home.recommendations = Loadable::Loading;
                        self.backend.api(ApiRequest::Recommendations {
                            seed_tracks: seeds,
                            seed_artists: Vec::new(),
                        });
                    }
                    let uris: Vec<String> = tracks.iter().map(|track| track.uri.clone()).collect();
                    self.request_contains(uris);
                    self.home.top_tracks = Loadable::Loaded(tracks);
                } else if offset == 0
                    && let Err(error) = result
                {
                    self.home.top_tracks = Loadable::Failed(error.to_string());
                }
            }
            ApiResponse::TopArtists(result) => {
                self.home.top_artists = Loadable::from_result(result);
            }
            ApiResponse::Recommendations(result) => {
                if let Ok(tracks) = &result {
                    let uris: Vec<String> = tracks.iter().map(|track| track.uri.clone()).collect();
                    self.request_contains(uris);
                }
                self.home.recommendations = Loadable::from_result(result);
            }
            ApiResponse::Discover { term, result } => {
                let filtered = result.map(|playlists| {
                    let needle = term.to_lowercase();
                    let mut seen = std::collections::HashSet::new();
                    let mut matching: Vec<Playlist> = playlists
                        .into_iter()
                        .filter(|playlist| {
                            let owner = playlist.owner.id.as_deref().unwrap_or("");
                            playlist.name.to_lowercase().contains(&needle)
                                && (owner == "spotify" || playlist.owner_name() == "Spotify")
                                && seen.insert(playlist.name.to_lowercase())
                        })
                        .collect();
                    matching.truncate(6);
                    matching
                });
                self.home
                    .discover
                    .insert(term, Loadable::from_result(filtered));
            }
            ApiResponse::MyPlaylists { offset, result } => match result {
                Ok(page) => {
                    let has_more = page.next.is_some() && !page.items.is_empty();
                    let received = page.items.len() as u32;
                    match &mut self.library.playlists {
                        Loadable::Loaded(existing) if offset > 0 => existing.extend(page.items),
                        slot => *slot = Loadable::Loaded(page.items),
                    }
                    self.library.playlists_next = has_more.then_some(offset + received);
                    if has_more {
                        self.load_more(Page::Home);
                    }
                    if let Some(playlists) = self.library.playlists.get() {
                        for playlist in playlists {
                            self.saved.insert(playlist.uri.clone(), true);
                        }
                    }
                }
                Err(error) => {
                    if offset == 0 {
                        self.library.playlists = Loadable::Failed(error.to_string());
                    } else {
                        self.toast_error(format!("Couldn't load more playlists: {error}"));
                    }
                }
            },
            ApiResponse::Playlist { id, result } => {
                if let Ok(playlist) = &result
                    && let Some(image) = pick_image(&playlist.images, 300)
                {
                    self.tint_for(Some(image));
                }
                if let Some(page) = self.playlist_pages.get_mut(&id) {
                    page.playlist = Loadable::from_result(result);
                }
                self.try_adopt_playlist_cache(&id);
            }
            ApiResponse::PlaylistItems { id, offset, result } => {
                let mut uris = Vec::new();
                let mut adders: Vec<String> = Vec::new();
                if let Some(page) = self.playlist_pages.get_mut(&id) {
                    match result {
                        Ok(_) if page.cache_complete => {
                            // A page in flight from before the cache
                            // adopted; the list is already whole.
                        }
                        Ok(items) => {
                            uris = items
                                .items
                                .iter()
                                .filter_map(|item| item.playable())
                                .map(|item| item.uri().to_string())
                                .collect();
                            adders = items
                                .items
                                .iter()
                                .filter_map(|item| item.added_by.as_ref()?.id.clone())
                                .filter(|id| !id.is_empty())
                                .collect();
                            page.contributors.extend(adders.iter().cloned());
                            page.items.absorb(offset, items);
                            // The rows load from the top, and songs a friend
                            // added often sit at the end; look there once.
                            if !page.tail_checked {
                                page.tail_checked = true;
                                let loaded = page.items.items.len() as u32;
                                if let Some(total) =
                                    page.items.total.filter(|total| *total > loaded)
                                {
                                    self.backend.api(ApiRequest::PlaylistSample {
                                        id: id.clone(),
                                        offset: total.saturating_sub(100),
                                    });
                                }
                            }
                        }
                        Err(error) => page.items.fail(friendly_page_error(&error)),
                    }
                }
                self.request_contains(uris);
                self.request_user_names(adders);
                // The whole list is here; remember it under its snapshot.
                if let Some(page) = self.playlist_pages.get(&id)
                    && page.items.is_complete()
                    && !page.cache_complete
                    && let Some(snapshot) = page
                        .playlist
                        .get()
                        .and_then(|playlist| playlist.snapshot_id.clone())
                {
                    self.backend.send(Command::StorePlaylistCache {
                        id: id.clone(),
                        snapshot,
                        items: page.items.items.clone(),
                    });
                }
                // A sorted table means the whole list, not the loaded part.
                if self.table_sorts.contains_key(&Page::Playlist(id.clone())) {
                    self.load_more(Page::Playlist(id));
                }
            }
            ApiResponse::PlaylistSample { id, result } => {
                let mut adders: Vec<String> = Vec::new();
                if let Ok(items) = result
                    && let Some(page) = self.playlist_pages.get_mut(&id)
                {
                    adders = items
                        .items
                        .iter()
                        .filter_map(|item| item.added_by.as_ref()?.id.clone())
                        .filter(|id| !id.is_empty())
                        .collect();
                    page.contributors.extend(adders.iter().cloned());
                }
                self.request_user_names(adders);
            }
            ApiResponse::PlaylistCreated(result) => {
                self.playlist_busy = false;
                match result {
                    Ok(playlist) => {
                        self.toast(format!("Created {}", playlist.name));
                        if let Some(playlists) = self.library.playlists.get_mut() {
                            playlists.insert(0, playlist.clone());
                        }
                        self.saved.insert(playlist.uri.clone(), true);
                        if let Some(Dialog::CreatePlaylist { add_uris, .. }) = self.dialog.take()
                            && !add_uris.is_empty()
                        {
                            self.backend.api(ApiRequest::AddToPlaylist {
                                playlist_id: playlist.id.clone(),
                                playlist_name: playlist.name.clone(),
                                uris: add_uris,
                            });
                        }
                        self.open(Page::Playlist(playlist.id));
                    }
                    Err(error) => {
                        self.toast_error(format!("Couldn't create the playlist: {error}"))
                    }
                }
            }
            ApiResponse::PlaylistUpdated { id, result } => {
                self.playlist_busy = false;
                match result {
                    Ok(()) => {
                        self.toast("Playlist updated");
                        self.playlist_pages.remove(&id);
                        self.load_playlists();
                        if matches!(self.page(), Page::Playlist(current) if *current == id) {
                            self.ensure_loaded(Page::Playlist(id));
                        }
                    }
                    Err(error) => {
                        self.toast_error(format!("Couldn't update the playlist: {error}"))
                    }
                }
            }
            ApiResponse::PlaylistItemsChanged {
                id,
                message,
                result,
            } => {
                self.playlist_busy = false;
                match result {
                    Ok(snapshot) => {
                        if !message.is_empty() {
                            self.toast(message);
                        }
                        if let Some(page) = self.playlist_pages.get_mut(&id) {
                            if let Some(playlist) = page.playlist.get_mut()
                                && snapshot.is_some()
                            {
                                playlist.snapshot_id = snapshot;
                            }
                            page.items.reset();
                            page.contributors.clear();
                            page.tail_checked = false;
                            page.cache_complete = false;
                            page.pending_cache = None;
                        }
                        if matches!(self.page(), Page::Playlist(current) if *current == id) {
                            self.ensure_loaded(Page::Playlist(id.clone()));
                        }
                        if let Some(playlists) = self.library.playlists.get_mut() {
                            for playlist in playlists.iter_mut().filter(|p| p.id == id) {
                                playlist.snapshot_id = None;
                            }
                        }
                        self.load_playlists();
                    }
                    Err(error) => {
                        self.toast_error(format!("Playlist change failed: {error}"));
                        if let Some(page) = self.playlist_pages.get_mut(&id) {
                            page.items.reset();
                            page.contributors.clear();
                            page.tail_checked = false;
                            page.cache_complete = false;
                            page.pending_cache = None;
                        }
                        self.ensure_loaded(Page::Playlist(id));
                    }
                }
            }
            ApiResponse::PlaylistFollowChanged {
                id,
                followed,
                result,
            } => match result {
                Ok(()) => {
                    self.saved
                        .insert(format!("spotify:playlist:{id}"), followed);
                    self.toast(if followed {
                        "Added to Your Library"
                    } else {
                        "Removed from Your Library"
                    });
                    self.load_playlists();
                    if !followed && matches!(self.page(), Page::Playlist(current) if *current == id)
                    {
                        self.open(Page::Home);
                    }
                }
                Err(error) => {
                    self.saved
                        .insert(format!("spotify:playlist:{id}"), !followed);
                    self.toast_error(format!("Couldn't update the playlist: {error}"));
                }
            },
            ApiResponse::SavedTracks { offset, result } => {
                match result {
                    Ok(page) => {
                        for item in &page.items {
                            self.saved.insert(item.track.uri.clone(), true);
                        }
                        self.library.liked.absorb(offset, page);
                    }
                    Err(error) => self.library.liked.fail(error.to_string()),
                }
                // A sorted table means the whole list, not the loaded part.
                if self.table_sorts.contains_key(&Page::LikedSongs) {
                    self.load_more(Page::LikedSongs);
                }
            }
            ApiResponse::SavedAlbums { offset, result } => match result {
                Ok(page) => {
                    for item in &page.items {
                        self.saved.insert(item.album.uri.clone(), true);
                    }
                    self.library.albums.absorb(offset, page);
                }
                Err(error) => self.library.albums.fail(error.to_string()),
            },
            ApiResponse::FollowedArtists { after, result } => {
                let list = &mut self.library.artists;
                list.loading = false;
                list.loaded_once = true;
                match result {
                    Ok(page) => {
                        if after.is_none() {
                            list.items.clear();
                        }
                        let received = page.items.len();
                        for artist in &page.items {
                            self.saved.insert(artist.uri.clone(), true);
                        }
                        list.items.extend(page.items);
                        let next = page.cursors.and_then(|cursors| cursors.after);
                        list.complete = next.is_none() || received == 0;
                        list.after = next;
                        list.error = None;
                    }
                    Err(error) => list.error = Some(error.to_string()),
                }
            }
            ApiResponse::SavedShows { offset, result } => match result {
                Ok(page) => {
                    for item in &page.items {
                        self.saved.insert(item.show.uri.clone(), true);
                    }
                    self.library.shows.absorb(offset, page);
                }
                Err(error) => self.library.shows.fail(error.to_string()),
            },
            ApiResponse::SavedEpisodes { offset, result } => match result {
                Ok(page) => {
                    for item in &page.items {
                        self.saved.insert(item.episode.uri.clone(), true);
                    }
                    self.library.episodes.absorb(offset, page);
                }
                Err(error) => self.library.episodes.fail(error.to_string()),
            },
            ApiResponse::SavedChanged {
                uris,
                saved,
                result,
            } => match result {
                Ok(()) => {
                    for uri in &uris {
                        self.saved.insert(uri.clone(), saved);
                        match util::uri_kind(uri) {
                            Some("track") => {
                                if self.library.liked.loaded_once {
                                    if saved {
                                        let total = self
                                            .library
                                            .liked
                                            .total
                                            .map(|total| total.saturating_add(1));
                                        self.library.liked.reset();
                                        self.library.liked.total = total;
                                        if matches!(self.page(), Page::LikedSongs) {
                                            self.load_more(Page::LikedSongs);
                                        }
                                    } else {
                                        self.library
                                            .liked
                                            .items
                                            .retain(|item| item.track.uri != *uri);
                                        if let Some(total) = self.library.liked.total.as_mut() {
                                            *total = total.saturating_sub(1);
                                        }
                                    }
                                }
                            }
                            Some("album") => self.library.albums.reset(),
                            Some("artist") => self.library.artists.reset(),
                            Some("show") => self.library.shows.reset(),
                            Some("episode") => self.library.episodes.reset(),
                            _ => {}
                        }
                    }
                    let message = match (uris.first().and_then(|uri| util::uri_kind(uri)), saved) {
                        (Some("track"), true) => "Added to Liked Songs",
                        (Some("track"), false) => "Removed from Liked Songs",
                        (Some("artist"), true) => "Following artist",
                        (Some("artist"), false) => "Unfollowed artist",
                        (_, true) => "Saved to Your Library",
                        (_, false) => "Removed from Your Library",
                    };
                    self.toast(message);
                }
                Err(error) => {
                    for uri in &uris {
                        self.saved.insert(uri.clone(), !saved);
                    }
                    self.toast_error(format!("Couldn't update your library: {error}"));
                }
            },
            ApiResponse::Contains { uris, result } => {
                for uri in &uris {
                    self.saved_pending.remove(uri);
                }
                if let Ok(flags) = result {
                    for (uri, flag) in uris.into_iter().zip(flags) {
                        self.saved.insert(uri, flag);
                    }
                }
            }
            ApiResponse::Search {
                query,
                serial,
                result,
            } => {
                if serial != self.search.serial || query != self.search.committed {
                    return;
                }
                if let Ok(results) = &result {
                    let uris: Vec<String> = results
                        .tracks
                        .iter()
                        .flat_map(|page| page.items.iter())
                        .map(|track| track.uri.clone())
                        .collect();
                    self.request_contains(uris);
                    self.settings.remember_search(&query);
                    self.settings_dirty = true;
                }
                self.search.results = Loadable::from_result(result);
            }
            ApiResponse::Artist { id, result } => {
                if let Ok(artist) = &result {
                    if let Some(image) = pick_image(&artist.images, 300) {
                        self.tint_for(Some(image));
                    }
                    let name = artist.name.clone();
                    if let Some(page) = self.artist_pages.get_mut(&id)
                        && page.top_tracks.needs_load()
                    {
                        page.top_tracks = Loadable::Loading;
                        self.backend.api(ApiRequest::ArtistTopTracks {
                            id: id.clone(),
                            name,
                        });
                    }
                }
                if let Some(page) = self.artist_pages.get_mut(&id) {
                    page.artist = Loadable::from_result(result);
                }
            }
            ApiResponse::ArtistTopTracks { id, result } => {
                if let Ok(tracks) = &result {
                    let uris: Vec<String> = tracks.iter().map(|track| track.uri.clone()).collect();
                    self.request_contains(uris);
                }
                if let Some(page) = self.artist_pages.get_mut(&id) {
                    page.top_tracks = Loadable::from_result(result);
                }
            }
            ApiResponse::ArtistAlbums {
                id,
                groups,
                offset,
                result,
            } => {
                if let Some(page) = self.artist_pages.get_mut(&id) {
                    let list = page.albums.entry(groups).or_default();
                    match result {
                        Ok(albums) => list.absorb(offset, albums),
                        Err(error) => list.fail(error.to_string()),
                    }
                }
            }
            ApiResponse::RelatedArtists { id, result } => {
                if let Some(page) = self.artist_pages.get_mut(&id) {
                    page.related = Loadable::from_result(result);
                }
            }
            ApiResponse::Album { id, result } => {
                let mut uris = Vec::new();
                if let Ok(album) = &result
                    && let Some(image) = pick_image(&album.images, 300)
                {
                    self.tint_for(Some(image));
                }
                if let Some(page) = self.album_pages.get_mut(&id) {
                    match result {
                        Ok(mut album) => {
                            if let Some(tracks) = album.tracks.take() {
                                uris = tracks.items.iter().map(|track| track.uri.clone()).collect();
                                page.tracks.absorb(0, tracks);
                            }
                            page.album = Loadable::Loaded(album);
                            if !page.tracks.loaded_once {
                                page.tracks.loading = true;
                                self.backend.api(ApiRequest::AlbumTracks { id, offset: 0 });
                            }
                        }
                        Err(error) => page.album = Loadable::Failed(error.to_string()),
                    }
                }
                self.request_contains(uris);
            }
            ApiResponse::AlbumTracks { id, offset, result } => {
                let mut uris = Vec::new();
                if let Some(page) = self.album_pages.get_mut(&id) {
                    match result {
                        Ok(tracks) => {
                            uris = tracks.items.iter().map(|track| track.uri.clone()).collect();
                            page.tracks.absorb(offset, tracks);
                        }
                        Err(error) => page.tracks.fail(error.to_string()),
                    }
                }
                self.request_contains(uris);
                // A sorted table means the whole list, not the loaded part.
                if self.table_sorts.contains_key(&Page::Album(id.clone())) {
                    self.load_more(Page::Album(id));
                }
            }
            ApiResponse::Show { id, result } => {
                if let Ok(show) = &result
                    && let Some(image) = pick_image(&show.images, 300)
                {
                    self.tint_for(Some(image));
                }
                if let Some(page) = self.show_pages.get_mut(&id) {
                    match result {
                        Ok(mut show) => {
                            if let Some(episodes) = show.episodes.take() {
                                page.episodes.absorb(0, episodes);
                            }
                            page.show = Loadable::Loaded(show);
                            if !page.episodes.loaded_once {
                                page.episodes.loading = true;
                                self.backend.api(ApiRequest::ShowEpisodes { id, offset: 0 });
                            }
                        }
                        Err(error) => page.show = Loadable::Failed(error.to_string()),
                    }
                }
            }
            ApiResponse::ShowEpisodes { id, offset, result } => {
                if let Some(page) = self.show_pages.get_mut(&id) {
                    match result {
                        Ok(episodes) => page.episodes.absorb(offset, episodes),
                        Err(error) => page.episodes.fail(error.to_string()),
                    }
                }
            }
            ApiResponse::Track { id, result } => {
                self.track_requests.remove(&id);
                if let Ok(track) = result {
                    self.track_cache.insert(id, track);
                }
            }
            ApiResponse::Remote { action, result } => {
                if matches!(action, RemoteAction::Play | RemoteAction::Pause) {
                    self.clear_play_pending();
                }
                match result {
                    Ok(()) => {
                        self.remote_recheck_at = Some(Instant::now() + REMOTE_RECHECK);
                    }
                    Err(error) => {
                        self.optimistic_playing = None;
                        self.pending_remote_position = None;
                        self.pending_remote_volume = None;
                        let hint = if error.status() == Some(404) {
                            " Choose a device from the devices menu first."
                        } else {
                            ""
                        };
                        self.toast_error(format!(
                            "{}: {error}.{hint}",
                            remote_action_label(action)
                        ));
                    }
                }
                self.poll_remote_soon();
            }
            ApiResponse::Transferred { device_id, result } => match result {
                Ok(()) => {
                    self.selected_device = Some(device_id);
                    self.show_devices = false;
                    self.poll_remote_soon();
                    self.refresh_devices();
                }
                Err(error) => self.toast_error(format!("Couldn't switch device: {error}")),
            },
            ApiResponse::QueueAdded { label, result } => match result {
                Ok(()) => {
                    self.toast(format!("Added {label} to queue"));
                    self.refresh_queue(true);
                }
                Err(error) => self.toast_error(format!("Couldn't add to queue: {error}")),
            },
        }
    }

    fn poll_remote_soon(&mut self) {
        self.remote_polled_at = Instant::now() - REMOTE_POLL_IDLE + Duration::from_millis(700);
    }

    // ---- navigation ------------------------------------------------------------

    pub fn open(&mut self, page: Page) {
        if *self.page() == page {
            self.ensure_loaded(page);
            return;
        }
        self.history.truncate(self.history_index + 1);
        self.history.push(page.clone());
        if self.history.len() > 60 {
            self.history.remove(0);
        }
        self.history_index = self.history.len() - 1;
        self.mark_session_dirty();
        self.show_devices = false;
        self.ensure_loaded(page);
    }

    pub fn can_go_back(&self) -> bool {
        self.history_index > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.history_index + 1 < self.history.len()
    }

    // ---- playback --------------------------------------------------------------

    fn remote(&mut self, action: RemoteAction, device_id: Option<String>) -> bool {
        if device_id.is_none() && self.remote_fresh().is_none() {
            // Spotify would only answer "no active device found".
            self.clear_play_pending();
            self.toast("Nothing is playing. Pick something first");
            return false;
        }
        if !self.allow_remote_command(RemoteCommand::Action(action), device_id.as_deref()) {
            if matches!(action, RemoteAction::Play | RemoteAction::Pause) {
                self.clear_play_pending();
            }
            return false;
        }
        self.backend.api(ApiRequest::Remote {
            action,
            device_id,
            play: None,
            position_ms: 0,
            percent: 0,
            flag: false,
            repeat: String::new(),
        });
        true
    }

    /// Remembers `uri` as the most recently played context, for the
    /// sidebar's order.
    fn note_recent_context(&mut self, uri: &str) {
        if !uri.contains(":playlist:") && !uri.contains(":album:") && !uri.contains(":collection") {
            return;
        }
        if self.recent_contexts.first().is_some_and(|held| held == uri) {
            return;
        }
        self.recent_contexts.retain(|held| held != uri);
        self.recent_contexts.insert(0, uri.to_string());
        self.recent_contexts.truncate(60);
        self.mark_session_dirty();
    }

    /// A random playable track from a context already loaded in memory.
    fn random_track_in<R>(&self, context_uri: &str, rng: &mut R) -> Option<String>
    where
        R: rand::Rng + ?Sized,
    {
        let uris: Vec<&str> = if let Some(id) = context_uri.strip_prefix("spotify:playlist:") {
            self.playlist_pages
                .get(id)?
                .items
                .items
                .iter()
                .filter_map(|item| item.playable())
                .filter(|item| match item {
                    PlayableItem::Track(track) => {
                        track.is_playable != Some(false) && !track.is_local
                    }
                    PlayableItem::Episode(_) => true,
                })
                .map(PlayableItem::uri)
                .collect()
        } else if let Some(id) = context_uri.strip_prefix("spotify:album:") {
            self.album_pages
                .get(id)?
                .tracks
                .items
                .iter()
                .filter(|track| track.is_playable != Some(false) && !track.is_local)
                .map(|track| track.uri.as_str())
                .collect()
        } else if context_uri.ends_with(":collection") {
            self.library
                .liked
                .items
                .iter()
                .map(|item| &item.track)
                .filter(|track| track.is_playable != Some(false) && !track.is_local)
                .map(|track| track.uri.as_str())
                .collect()
        } else {
            return None;
        };
        uris.choose(rng).map(|uri| (*uri).to_string())
    }

    fn assume_context(&mut self, uri: String) {
        self.note_recent_context(&uri);
        self.assumed_context = Some(AssumedContext {
            uri,
            at: Instant::now(),
        });
    }

    fn prepare_play_request(&self, request: PlayRequest, shuffle: bool) -> PlayRequest {
        self.prepare_play_request_with_rng(request, shuffle, &mut rand::rng())
    }

    fn prepare_play_request_with_rng<R>(
        &self,
        mut request: PlayRequest,
        shuffle: bool,
        rng: &mut R,
    ) -> PlayRequest
    where
        R: rand::Rng + ?Sized,
    {
        let mut generated_shuffle_index = None;
        if shuffle && request.offset_uri.is_none() && request.offset_position.is_none() {
            if request.uris.is_empty() {
                if let Some(context) = request.context_uri.as_deref() {
                    request.offset_uri = self.random_track_in(context, rng);
                }
            } else {
                let selected = rng.random_range(0..request.uris.len());
                generated_shuffle_index = Some(selected);
                request.offset_position = Some(selected as u32);
            }
        }
        if request.uris.len() > MAX_PLAY_URIS {
            if let Some(selected) = generated_shuffle_index {
                let start = selected
                    .saturating_sub(MAX_PLAY_URIS / 2)
                    .min(request.uris.len() - MAX_PLAY_URIS);
                request.uris = request.uris[start..start + MAX_PLAY_URIS].to_vec();
                request.offset_position = Some((selected - start) as u32);
                return request;
            }
            let selected = request
                .offset_position
                .map(|index| index as usize)
                .or_else(|| {
                    let uri = request.offset_uri.as_deref()?;
                    request.uris.iter().position(|candidate| candidate == uri)
                })
                .unwrap_or(0) as u32;
            let had_index = request.offset_position.is_some();
            let (uris, index) = cap_uris(std::mem::take(&mut request.uris), selected);
            request.uris = uris;
            if had_index {
                request.offset_position = Some(index);
            }
        }
        request
    }

    /// Apply the listener's shuffle mode and start one ordered play request.
    fn play_request(&mut self, mut request: PlayRequest, enable_shuffle: bool) {
        let target = self.target();
        if let Target::Remote(device_id) = &target
            && (device_id.is_some() || self.remote_fresh().is_some())
            && !self.allow_remote_command(
                RemoteCommand::Action(RemoteAction::Play),
                device_id.as_deref(),
            )
        {
            return;
        }
        if enable_shuffle && !self.shuffle_wanted {
            self.shuffle_wanted = true;
            self.mark_session_dirty();
        }
        let shuffle = self.shuffle_wanted;
        // A play queued behind a reconnect is transformed only when it is
        // actually dispatched, so a shuffle toggle while waiting still wins.
        let queued_request = request.clone();
        request = self.prepare_play_request(request, shuffle);
        let mut keys: Vec<String> = Vec::new();
        if let Some(context) = &request.context_uri {
            keys.push(context.clone());
        }
        if let Some(offset) = &request.offset_uri {
            keys.push(offset.clone());
        }
        match request.offset_position {
            // The play starts at a chosen row; only that row is starting.
            Some(position) => {
                if let Some(uri) = request.uris.get(position as usize) {
                    keys.push(uri.clone());
                }
            }
            // No chosen row: the list starts at its first song.
            None if request.offset_uri.is_none() => {
                if let Some(first) = request.uris.first() {
                    keys.push(first.clone());
                }
            }
            None => {}
        }
        self.set_play_pending(keys);
        if let Some(context) = request.context_uri.clone() {
            // Light the page and the sidebar up at once; Spotify's own
            // state takes a poll or two to say the same thing.
            self.assume_context(context);
        }
        match target {
            Target::Local if !self.local.connected => {
                self.queued_play = Some(QueuedPlay {
                    request: queued_request,
                });
            }
            Target::Local => {
                self.queued_play = None;
                self.pending_local_shuffle = None;
                self.shuffle_set_at = Some(Instant::now());
                self.backend.player(PlayerCommand::Load(LoadSpec {
                    context_uri: request.context_uri.clone(),
                    uris: request.uris.clone(),
                    offset_uri: request.offset_uri.clone(),
                    offset_index: request.offset_position,
                    position_ms: request.position_ms,
                    play: true,
                    shuffle: Some(shuffle),
                }));
                self.optimistic_playing = Some((true, Instant::now()));
            }
            Target::Remote(device_id) if device_id.is_some() || self.remote_fresh().is_some() => {
                self.queued_play = None;
                self.pending_local_shuffle = None;
                self.shuffle_set_at = Some(Instant::now());
                let shuffle_matches = self.remote.as_ref().is_some_and(|remote| {
                    remote.state.shuffle_state == shuffle
                        && remote
                            .state
                            .device
                            .as_ref()
                            .and_then(|device| device.id.as_deref())
                            == device_id.as_deref()
                });
                if shuffle_matches {
                    self.backend.api(ApiRequest::Remote {
                        action: RemoteAction::Play,
                        device_id,
                        play: Some(request),
                        position_ms: 0,
                        percent: 0,
                        flag: false,
                        repeat: String::new(),
                    });
                } else {
                    self.backend.api(ApiRequest::PlayWithShuffle {
                        device_id,
                        play: request,
                        shuffle,
                    });
                }
                self.optimistic_playing = Some((true, Instant::now()));
            }
            Target::Remote(_) => {
                // No remote device is active, and this computer's player is
                // not ready. Never ask Spotify to play "nowhere": either
                // wait for the connecting engine or ask for a device.
                if matches!(
                    self.local_playback,
                    LocalPlayback::Connecting | LocalPlayback::Authorizing
                ) {
                    self.queued_play = Some(QueuedPlay {
                        request: queued_request,
                    });
                } else {
                    self.clear_play_pending();
                    self.queued_play = None;
                    self.toast("Choose a device, or enable playback on this computer");
                    self.show_devices = true;
                    self.refresh_devices();
                }
            }
        }
    }

    fn dispatch_ready_local_work(&mut self) {
        if !self.local_ready || !self.local.connected {
            return;
        }
        if let Some(QueuedPlay { request }) = self.queued_play.take() {
            // The local LoadSpec carries the current global shuffle mode.
            self.pending_local_shuffle = None;
            self.play_request(request, false);
        } else if let Some(shuffle) = self.pending_local_shuffle.take() {
            if self.target() != Target::Local {
                let remote = self.remote.as_ref().and_then(|remote| {
                    let device_id = remote
                        .state
                        .device
                        .as_ref()
                        .and_then(|device| device.id.as_deref());
                    (self.playback_owner(device_id) == PlaybackOwner::Remote).then_some((
                        remote.state.is_playing || remote.state.item.is_some(),
                        remote.state.shuffle_state,
                    ))
                });
                if let Some((active, shuffle)) = remote {
                    self.reconcile_authoritative_shuffle(active, shuffle, PlaybackOwner::Remote);
                }
                return;
            }
            self.local.shuffle = shuffle;
            self.shuffle_set_at = Some(Instant::now());
            self.backend.player(PlayerCommand::Shuffle(shuffle));
        }
    }

    /// Adopt a playlist's disk cache once both it and the live playlist
    /// are here and Spotify's snapshot still matches; a stale cache is
    /// discarded, never shown.
    fn try_adopt_playlist_cache(&mut self, id: &str) {
        let mut uris = Vec::new();
        let mut adders: Vec<String> = Vec::new();
        if let Some(page) = self.playlist_pages.get_mut(id) {
            let Some(snapshot_now) = page
                .playlist
                .get()
                .and_then(|playlist| playlist.snapshot_id.clone())
            else {
                return;
            };
            match &page.pending_cache {
                Some((held, _)) if *held == snapshot_now => {}
                Some(_) => {
                    // The playlist changed since; the cache is history.
                    page.pending_cache = None;
                    return;
                }
                None => return,
            }
            if page.items.is_complete() || page.cache_complete {
                page.pending_cache = None;
                return;
            }
            let Some((_, items)) = page.pending_cache.take() else {
                return;
            };
            uris = items
                .iter()
                .filter_map(|item| item.playable())
                .map(|item| item.uri().to_string())
                .collect();
            adders = items
                .iter()
                .filter_map(|item| item.added_by.as_ref()?.id.clone())
                .filter(|id| !id.is_empty())
                .collect();
            page.contributors.extend(adders.iter().cloned());
            page.items.total = Some(items.len() as u32);
            page.items.items = items;
            page.items.next_offset = None;
            page.items.loading = false;
            page.items.loaded_once = true;
            page.items.error = None;
            page.cache_complete = true;
        }
        self.request_contains(uris);
        self.request_user_names(adders);
    }

    /// Play what was playing when the app last closed. `false` when
    /// nothing is known to resume.
    fn resume_last(&mut self) -> bool {
        let Some(track) = self.resume_track.clone() else {
            return false;
        };
        let mut request = match self.resume_context.clone() {
            Some(context) => PlayRequest::context(context).starting_at_uri(track),
            None => PlayRequest::tracks(vec![track]),
        };
        request.position_ms = self.resume_position_ms;
        self.play_request(request, false);
        true
    }

    fn toggle_play(&mut self) {
        let playing = self.now_playing().map(|now| now.playing);
        match self.target() {
            Target::Local => {
                if self.local.is_active() {
                    self.backend.player(PlayerCommand::Toggle);
                } else if let Some(remote) = self.remote_fresh() {
                    // Nothing is playing locally: resume on this computer.
                    let uri = remote
                        .state
                        .item
                        .as_ref()
                        .map(|item| item.uri().to_string());
                    let position = remote.state.progress_ms.unwrap_or(0);
                    if let Some(uri) = uri {
                        let mut request = match &remote.state.context {
                            Some(context) if !context.uri.is_empty() => {
                                PlayRequest::context(context.uri.clone()).starting_at_uri(uri)
                            }
                            _ => PlayRequest::tracks(vec![uri]),
                        };
                        request.position_ms = position;
                        self.play_request(request, false);
                        return;
                    }
                    if !self.resume_last() {
                        self.toast("Pick something to play");
                    }
                    return;
                } else {
                    if !self.resume_last() {
                        self.toast("Pick something to play");
                    }
                    return;
                }
            }
            Target::Remote(device_id) => {
                if device_id.is_none() && self.remote_fresh().is_none() {
                    self.toast("Pick a song, album, or playlist");
                    return;
                }
                self.set_play_pending(vec!["::toggle".into()]);
                let sent = if playing == Some(true) {
                    self.remote(RemoteAction::Pause, device_id)
                } else {
                    self.remote(RemoteAction::Play, device_id)
                };
                if !sent {
                    return;
                }
            }
        }
        if let Some(playing) = playing {
            self.optimistic_playing = Some((!playing, Instant::now()));
        }
    }

    fn seek(&mut self, position_ms: u32) {
        match self.target() {
            Target::Local => self.backend.player(PlayerCommand::Seek(position_ms)),
            Target::Remote(device_id) => {
                if !self.allow_remote_command(
                    RemoteCommand::Action(RemoteAction::Seek),
                    device_id.as_deref(),
                ) {
                    return;
                }
                self.pending_remote_position = Some((position_ms, Instant::now()));
                self.backend.api(ApiRequest::Remote {
                    action: RemoteAction::Seek,
                    device_id,
                    play: None,
                    position_ms,
                    percent: 0,
                    flag: false,
                    repeat: String::new(),
                });
            }
        }
    }

    /// The volume this side set that the engine has yet to confirm, if the
    /// hold is still good. Clears itself once the engine agrees or it expires.
    fn held_local_volume(&mut self, reported: u16) -> Option<u16> {
        match self.pending_local_volume {
            Some((volume, at)) if volume != reported && at.elapsed() < OPTIMISTIC_HOLD => {
                Some(volume)
            }
            _ => {
                self.pending_local_volume = None;
                None
            }
        }
    }

    /// `settle` is false while the slider is still moving: the level is heard
    /// at once, and Spotify is told where it ended up on release.
    fn set_volume(&mut self, percent: u8, settle: bool) {
        let percent = percent.min(100);
        match self.target() {
            Target::Local => {
                let volume = percent_to_volume(percent);
                self.local.volume = volume;
                self.pending_local_volume = Some((volume, Instant::now()));
                // The engine echoes `VolumeChanged` only while this device
                // holds the Connect session, so the snapshot that would
                // otherwise persist this may never arrive.
                if self.settings.volume != volume {
                    self.settings.volume = volume;
                    self.settings_dirty = true;
                }
                self.backend.player(if settle {
                    PlayerCommand::Volume(volume)
                } else {
                    PlayerCommand::VolumePreview(volume)
                });
            }
            Target::Remote(_) if !settle => {}
            Target::Remote(device_id) => {
                if !self.allow_remote_command(
                    RemoteCommand::Action(RemoteAction::Volume),
                    device_id.as_deref(),
                ) {
                    return;
                }
                self.pending_remote_volume = Some((percent, Instant::now()));
                self.backend.api(ApiRequest::Remote {
                    action: RemoteAction::Volume,
                    device_id,
                    play: None,
                    position_ms: 0,
                    percent,
                    flag: false,
                    repeat: String::new(),
                });
            }
        }
    }

    fn adopt_external_shuffle(&mut self, shuffle: bool) {
        self.pending_local_shuffle = None;
        self.shuffle_set_at = None;
        if self.shuffle_wanted != shuffle {
            self.shuffle_wanted = shuffle;
            self.mark_session_dirty();
        }
    }

    fn reconcile_authoritative_shuffle(
        &mut self,
        active: bool,
        shuffle: bool,
        owner: PlaybackOwner,
    ) {
        if !active {
            return;
        }
        if owner == PlaybackOwner::Local
            && let Some(pending) = self.pending_local_shuffle
        {
            if pending == shuffle {
                self.adopt_external_shuffle(shuffle);
            }
            return;
        }
        if self
            .shuffle_set_at
            .is_none_or(|at| at.elapsed() >= SHUFFLE_INTENT_HOLD)
        {
            self.adopt_external_shuffle(shuffle);
        }
    }

    fn set_shuffle(&mut self, shuffle: bool) -> ShuffleDispatch {
        let target = self.target();
        if let Target::Remote(device_id) = &target
            && (device_id.is_some() || self.remote_fresh().is_some())
            && !self.allow_remote_command(
                RemoteCommand::Action(RemoteAction::Shuffle),
                device_id.as_deref(),
            )
        {
            return ShuffleDispatch::Unsupported;
        }
        self.adopt_external_shuffle(shuffle);
        self.shuffle_set_at = Some(Instant::now());
        match target {
            Target::Local => {
                if !self.local_ready || !self.local.connected || self.queued_play.is_some() {
                    self.pending_local_shuffle = Some(shuffle);
                    return ShuffleDispatch::Deferred;
                }
                self.pending_local_shuffle = None;
                self.local.shuffle = shuffle;
                self.backend.player(PlayerCommand::Shuffle(shuffle));
                ShuffleDispatch::Local
            }
            Target::Remote(None) if self.remote_fresh().is_none() => {
                self.pending_local_shuffle = (self.queued_play.is_some()
                    || matches!(
                        self.local_playback,
                        LocalPlayback::Connecting | LocalPlayback::Authorizing
                    ))
                .then_some(shuffle);
                ShuffleDispatch::Deferred
            }
            Target::Remote(device_id) => {
                self.pending_local_shuffle = None;
                if let Some(remote) = self.remote.as_mut() {
                    remote.state.shuffle_state = shuffle;
                }
                self.backend.api(ApiRequest::Remote {
                    action: RemoteAction::Shuffle,
                    device_id,
                    play: None,
                    position_ms: 0,
                    percent: 0,
                    flag: shuffle,
                    repeat: String::new(),
                });
                ShuffleDispatch::Remote
            }
        }
    }

    fn set_repeat(&mut self, mode: RepeatMode) {
        match self.target() {
            Target::Local => {
                self.local.repeat = mode;
                self.backend.player(PlayerCommand::Repeat(mode));
            }
            Target::Remote(device_id) => {
                if !self.allow_remote_command(
                    RemoteCommand::Action(RemoteAction::Repeat),
                    device_id.as_deref(),
                ) {
                    return;
                }
                if let Some(remote) = self.remote.as_mut() {
                    remote.state.repeat_state = mode.api_name().to_string();
                }
                self.backend.api(ApiRequest::Remote {
                    action: RemoteAction::Repeat,
                    device_id,
                    play: None,
                    position_ms: 0,
                    percent: 0,
                    flag: false,
                    repeat: mode.api_name().to_string(),
                });
            }
        }
    }

    fn transfer(&mut self, device_id: String) {
        if self.playback_owner(Some(&device_id)) == PlaybackOwner::Local {
            self.selected_device = None;
            self.show_devices = false;
            let was_playing = self.now_playing().is_some_and(|now| now.playing);
            self.backend.player(PlayerCommand::Activate);
            if let Some(remote) = self.remote_fresh()
                && let Some(item) = &remote.state.item
            {
                let uri = item.uri().to_string();
                let position = {
                    let base = remote.state.progress_ms.unwrap_or(0);
                    if remote.state.is_playing {
                        base + remote.received_at.elapsed().as_millis() as u32
                    } else {
                        base
                    }
                };
                let mut request = match &remote.state.context {
                    Some(context) if !context.uri.is_empty() => {
                        PlayRequest::context(context.uri.clone()).starting_at_uri(uri)
                    }
                    _ => PlayRequest::tracks(vec![uri]),
                };
                request.position_ms = position;
                self.backend.player(PlayerCommand::Load(LoadSpec {
                    context_uri: request.context_uri,
                    uris: request.uris,
                    offset_uri: request.offset_uri,
                    offset_index: None,
                    position_ms: request.position_ms,
                    play: was_playing,
                    shuffle: None,
                }));
            }
            self.poll_remote_soon();
            self.dispatch_ready_local_work();
            return;
        }
        if !self.allow_remote_command(RemoteCommand::Transfer, Some(&device_id)) {
            return;
        }
        let play = self.now_playing().is_some_and(|now| now.playing);
        self.pending_local_shuffle = None;
        self.backend.api(ApiRequest::Transfer { device_id, play });
    }

    fn add_to_queue(&mut self, uri: String, label: String) {
        let device_id = match self.target() {
            Target::Local => self.local_device_id.clone(),
            Target::Remote(device_id) => {
                if device_id.is_none() && self.remote_fresh().is_none() {
                    self.toast("Choose a device before adding to the queue");
                    self.show_devices = true;
                    self.refresh_devices();
                    return;
                }
                if !self.allow_remote_command(RemoteCommand::AddToQueue, device_id.as_deref()) {
                    return;
                }
                device_id
            }
        };
        self.backend.api(ApiRequest::AddToQueue {
            uri,
            device_id,
            label,
        });
    }

    fn set_saved(&mut self, uri: String, saved: bool) {
        self.saved.insert(uri.clone(), saved);
        if uri.starts_with("spotify:playlist:") {
            let id = util::uri_id(&uri).unwrap_or_default().to_string();
            self.backend
                .api(ApiRequest::FollowPlaylist { id, follow: saved });
            return;
        }
        self.backend.api(ApiRequest::SetSaved {
            uris: vec![uri],
            saved,
        });
    }

    // ---- actions -----------------------------------------------------------------

    fn apply_actions(&mut self, ctx: &egui::Context) {
        let mut actions = std::mem::take(&mut self.actions);
        while !actions.is_empty() {
            for action in actions.drain(..) {
                self.apply(action, ctx);
            }
            actions = std::mem::take(&mut self.actions);
        }
    }

    fn apply(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::Open(page) => self.open(page),
            Action::OpenUri(uri) => {
                if let Some(page) = Page::from_uri(&uri) {
                    self.open(page);
                }
            }
            Action::Back => {
                if self.can_go_back() {
                    self.history_index -= 1;
                    self.mark_session_dirty();
                    let page = self.page().clone();
                    self.ensure_loaded(page);
                }
            }
            Action::Forward => {
                if self.can_go_forward() {
                    self.history_index += 1;
                    self.mark_session_dirty();
                    let page = self.page().clone();
                    self.ensure_loaded(page);
                }
            }
            Action::PlayContext {
                uri,
                offset_uri,
                offset_index,
            } => {
                let mut request = PlayRequest::context(uri);
                request.offset_uri = offset_uri;
                request.offset_position = offset_index;
                self.play_request(request, false);
            }
            Action::PlayUris { uris, index } => {
                if uris.is_empty() {
                    return;
                }
                let (uris, index) = cap_uris(uris, index);
                let request = PlayRequest::tracks(uris).starting_at_index(index);
                self.play_request(request, false);
            }
            Action::PlayView { context_uri, uris } => {
                if uris.is_empty() {
                    return;
                }
                let request = PlayRequest::tracks(uris);
                self.play_request(request, false);
                self.assume_context(context_uri);
            }
            Action::PlayFromRow {
                context,
                uri,
                index,
            } => match context {
                RowContext::Context {
                    uri: context_uri, ..
                } => {
                    let request = PlayRequest::context(context_uri).starting_at_uri(uri);
                    self.play_request(request, false);
                }
                RowContext::Uris(uris) => {
                    let (uris, index) = cap_uris(uris, index);
                    let request = PlayRequest::tracks(uris).starting_at_index(index);
                    self.play_request(request, false);
                }
                RowContext::View { uris, context_uri } => {
                    let (uris, index) = cap_uris(uris, index);
                    let request = PlayRequest::tracks(uris).starting_at_index(index);
                    self.play_request(request, false);
                    self.assume_context(context_uri);
                }
            },
            Action::ShufflePlay(uri) => self.play_request(PlayRequest::context(uri), true),
            Action::TogglePlay => self.toggle_play(),
            Action::Next => match self.target() {
                Target::Local => self.backend.player(PlayerCommand::Next),
                Target::Remote(device_id) => {
                    self.remote(RemoteAction::Next, device_id);
                }
            },
            Action::Previous => match self.target() {
                Target::Local => self.backend.player(PlayerCommand::Previous),
                Target::Remote(device_id) => {
                    self.remote(RemoteAction::Previous, device_id);
                }
            },
            Action::Seek(position_ms) => self.seek(position_ms),
            Action::SeekBy(offset) => {
                if let Some(now) = self.now_playing() {
                    let target = (i64::from(now.position_ms) + offset)
                        .clamp(0, i64::from(now.duration_ms))
                        as u32;
                    self.seek(target);
                }
            }
            Action::SetVolume(percent) => {
                self.volume_before_mute = None;
                self.set_volume(percent, true);
            }
            Action::PreviewVolume(percent) => self.set_volume(percent, false),
            Action::VolumeBy(delta) => {
                if let Some(now) = self.now_playing() {
                    let next =
                        (i16::from(now.volume_percent) + i16::from(delta)).clamp(0, 100) as u8;
                    self.volume_before_mute = None;
                    self.set_volume(next, true);
                } else if self.is_connected() {
                    let current = volume_to_percent(self.local.volume);
                    let next = (i16::from(current) + i16::from(delta)).clamp(0, 100) as u8;
                    self.set_volume(next, true);
                }
            }
            Action::ToggleMute => {
                let current = self
                    .now_playing()
                    .map(|now| now.volume_percent)
                    .unwrap_or_else(|| volume_to_percent(self.local.volume));
                if current == 0 {
                    let restore = self.volume_before_mute.take().unwrap_or(50).max(5);
                    self.set_volume(restore, true);
                } else {
                    self.volume_before_mute = Some(current);
                    self.set_volume(0, true);
                }
            }
            Action::ToggleShuffle => {
                self.set_shuffle(!self.shuffle_wanted);
            }
            Action::SetShuffle(shuffle) => {
                self.set_shuffle(shuffle);
            }
            Action::CycleRepeat => {
                let mode = self.now_playing().map(|now| now.repeat).unwrap_or_default();
                self.set_repeat(mode.next());
            }
            Action::SetRepeat(mode) => self.set_repeat(mode),
            Action::AddToQueue { uri, label } => self.add_to_queue(uri, label),
            Action::ToggleSaved(uri) => {
                let saved = self.saved.get(&uri).copied().unwrap_or(false);
                self.set_saved(uri, !saved);
            }
            Action::AddToPlaylist {
                playlist_id,
                playlist_name,
                uris,
            } => {
                self.playlist_busy = true;
                self.backend.api(ApiRequest::AddToPlaylist {
                    playlist_id,
                    playlist_name,
                    uris,
                });
            }
            Action::RemoveFromPlaylist { playlist_id, uris } => {
                let snapshot_id = self
                    .playlist_pages
                    .get(&playlist_id)
                    .and_then(|page| page.playlist.get())
                    .and_then(|playlist| playlist.snapshot_id.clone());
                if let Some(page) = self.playlist_pages.get_mut(&playlist_id) {
                    page.items.items.retain(|item| {
                        item.playable()
                            .is_none_or(|playable| !uris.iter().any(|uri| uri == playable.uri()))
                    });
                }
                self.playlist_busy = true;
                self.backend.api(ApiRequest::RemoveFromPlaylist {
                    playlist_id,
                    uris,
                    snapshot_id,
                });
            }
            Action::MoveInPlaylist {
                playlist_id,
                from,
                to,
            } => {
                let snapshot_id = self
                    .playlist_pages
                    .get(&playlist_id)
                    .and_then(|page| page.playlist.get())
                    .and_then(|playlist| playlist.snapshot_id.clone());
                if let Some(page) = self.playlist_pages.get_mut(&playlist_id) {
                    let items = &mut page.items.items;
                    if (from as usize) < items.len() && (to as usize) <= items.len() {
                        let item = items.remove(from as usize);
                        let insert_at = if to > from { to - 1 } else { to } as usize;
                        items.insert(insert_at.min(items.len()), item);
                    }
                }
                self.playlist_busy = true;
                self.backend.api(ApiRequest::ReorderPlaylist {
                    playlist_id,
                    range_start: from,
                    insert_before: to,
                    snapshot_id,
                });
            }
            Action::ShowDialog(dialog) => self.dialog = Some(dialog),
            Action::CloseDialog => self.dialog = None,
            Action::CreatePlaylist {
                name,
                public,
                add_uris,
            } => {
                let Some(user_id) = self.user_id().map(str::to_string) else {
                    return;
                };
                self.playlist_busy = true;
                self.dialog = Some(Dialog::CreatePlaylist {
                    name: name.clone(),
                    public,
                    add_uris,
                });
                self.backend.api(ApiRequest::CreatePlaylist {
                    user_id,
                    name,
                    public,
                    description: String::new(),
                });
            }
            Action::UpdatePlaylist {
                id,
                name,
                description,
                public,
            } => {
                self.dialog = None;
                self.playlist_busy = true;
                self.backend.api(ApiRequest::UpdatePlaylist {
                    id,
                    name: Some(name),
                    description: Some(description),
                    public: Some(public),
                });
            }
            Action::DeletePlaylist(id) => {
                self.dialog = None;
                self.saved.insert(format!("spotify:playlist:{id}"), false);
                if let Some(playlists) = self.library.playlists.get_mut() {
                    playlists.retain(|playlist| playlist.id != id);
                }
                self.backend
                    .api(ApiRequest::FollowPlaylist { id, follow: false });
            }
            Action::Transfer(device_id) => self.transfer(device_id),
            Action::RefreshDevices => {
                self.devices_fetched_at = None;
                self.refresh_devices();
            }
            Action::RefreshQueue => self.refresh_queue(true),
            Action::CopyLink(uri) => {
                if let Some(url) = util::open_spotify_url(&uri) {
                    ctx.copy_text(url);
                    self.toast("Link copied");
                }
            }
            Action::OpenInSpotify(uri) => {
                if let Some(url) = util::open_spotify_url(&uri) {
                    ctx.open_url(egui::OpenUrl::new_tab(url));
                }
            }
            Action::Search(query) => {
                self.search.query = query.clone();
                self.search.typed_at = None;
                self.open(Page::Search);
                self.run_search(query.trim().to_string());
            }
            Action::SetSearchFilter(filter) => self.search.filter = filter,
            Action::FocusSearch => {
                self.search.focus_requested = true;
                if !matches!(self.page(), Page::Search) {
                    self.open(Page::Search);
                }
            }
            Action::LoadMore(page) => self.load_more(page),
            Action::LoadMoreArtistAlbums(id) => {
                let Some(page) = self.artist_pages.get_mut(&id) else {
                    return;
                };
                let groups = page.filter.groups().to_string();
                let list = page.albums.entry(groups.clone()).or_default();
                if let Some(offset) = list.next_offset.filter(|_| list.can_load_more()) {
                    list.loading = true;
                    self.backend
                        .api(ApiRequest::ArtistAlbums { id, groups, offset });
                }
            }
            Action::SetDiscographyFilter { artist_id, filter } => {
                if let Some(page) = self.artist_pages.get_mut(&artist_id) {
                    page.filter = filter;
                }
                self.load_artist_albums(&artist_id, filter);
            }
            Action::ToggleShowAllTop(id) => {
                if let Some(page) = self.artist_pages.get_mut(&id) {
                    page.show_all_top = !page.show_all_top;
                }
            }
            Action::Reload(page) => self.reload(page),
            Action::SignIn => self.backend.send(Command::SignIn),
            Action::CancelSignIn => {
                self.backend.send(Command::CancelSignIn);
                self.sign_in_url = None;
                self.auth = AuthStatus::SignedOut;
            }
            Action::ConfigurePersonalWebApp => {
                if let Err(error) = self.save_settings_at(Instant::now()) {
                    self.toast_error(format!("Couldn't save settings: {error}"));
                    return;
                }
                self.backend.send(Command::ConfigurePersonalWebApp(
                    self.settings.web_client_id.clone(),
                ));
            }
            Action::SignOut => {
                self.backend.send(Command::SignOut);
                self.history = vec![Page::Home];
                self.history_index = 0;
                self.mark_session_dirty();
            }
            Action::ToggleSidebar => {
                self.settings.sidebar_visible = !self.settings.sidebar_visible;
                self.mark_settings_dirty();
            }
            Action::ToggleQueuePanel => {
                self.show_queue_panel = !self.show_queue_panel;
                self.mark_session_dirty();
                if self.show_queue_panel {
                    self.show_lyrics_panel = false;
                    self.refresh_queue(true);
                }
            }
            Action::ToggleLyricsPanel => {
                self.show_lyrics_panel = !self.show_lyrics_panel;
                if self.show_lyrics_panel {
                    if self.show_queue_panel {
                        self.mark_session_dirty();
                    }
                    self.show_queue_panel = false;
                    self.lyrics_following = true;
                    self.request_lyrics();
                }
            }
            Action::ToggleDevicesPopup => {
                self.show_devices = !self.show_devices;
                if self.show_devices {
                    self.refresh_devices();
                }
            }
            Action::SettingsChanged => {
                self.settings_dirty = true;
                ctx.set_theme(match self.settings.theme {
                    ThemeChoice::Dark => egui::ThemePreference::Dark,
                    ThemeChoice::Light => egui::ThemePreference::Light,
                    ThemeChoice::System => egui::ThemePreference::System,
                });
            }
            Action::LyricsSourceChanged => {
                self.lyrics_uri = None;
                self.lyrics = Loadable::NotLoaded;
                if self.show_lyrics_panel {
                    self.request_lyrics();
                }
            }
            Action::RestartEngine => {
                if let Err(error) = self.save_settings_at(Instant::now()) {
                    self.toast_error(format!("Couldn't save settings: {error}"));
                }
                let config = engine_config(&self.dirs, &self.settings);
                self.backend.send(Command::RestartEngine(config));
                if self.local_ready {
                    self.toast("Restarting local playback");
                }
            }
            Action::ShowWindow => {
                if self.window_hidden {
                    // No window exists; the outer loop creates one.
                    self.wants_show = true;
                } else {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
            }
            Action::HideWindow => {
                if self.tray.is_some() {
                    self.hide_intent = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Action::EnablePlayback => {
                let free = self
                    .user
                    .as_ref()
                    .and_then(|user| user.product.as_deref())
                    .is_some_and(|product| product != "premium");
                if free {
                    self.toast_error("Local playback needs Spotify Premium");
                } else if !self.local_ready
                    && !matches!(
                        self.local_playback,
                        LocalPlayback::Authorizing | LocalPlayback::Connecting
                    )
                {
                    self.settings.playback_authorized = true;
                    self.settings_dirty = true;
                    self.backend.send(Command::AuthorizePlayback);
                    self.toast("Opening Spotify to enable playback here");
                }
            }
            Action::OpenUrl(url) => ctx.open_url(egui::OpenUrl::new_tab(url)),
            Action::ClearArtCache => match self.backend.art().clear_disk_cache() {
                Ok(bytes) => {
                    ctx.forget_all_images();
                    self.toast(format!(
                        "Cleared {:.1} MB of artwork",
                        bytes as f64 / 1_048_576.0
                    ));
                }
                Err(error) => self.toast_error(format!("Couldn't clear artwork: {error}")),
            },
            Action::Quit => {
                self.quit_requested = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    pub fn toast(&mut self, message: impl Into<String>) {
        self.toasts.push(Toast {
            message: message.into(),
            kind: ToastKind::Info,
            created: Instant::now(),
        });
        self.toasts.truncate(4);
    }

    pub fn toast_error(&mut self, message: impl Into<String>) {
        let message = message.into();
        log::warn!("{message}");
        self.toasts.push(Toast {
            message,
            kind: ToastKind::Error,
            created: Instant::now(),
        });
    }

    /// Playlists the signed-in user can add to.
    pub fn editable_playlists(&self) -> Vec<(String, String)> {
        let Some(user_id) = self.user_id() else {
            return Vec::new();
        };
        self.library
            .playlists
            .get()
            .map(|playlists| {
                playlists
                    .iter()
                    .filter(|playlist| playlist.owned_by(user_id) || playlist.collaborative)
                    .map(|playlist| (playlist.id.clone(), playlist.name.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl App {
    /// Everything that must keep happening whether or not a window exists:
    /// backend events, MPRIS, the tray, polling, and pending actions. The
    /// headless loop in `main` drives this with a windowless context while
    /// the app lives in the tray.
    pub fn background_frame(&mut self, ctx: &egui::Context) {
        self.handle_control_commands();
        self.handle_events();
        self.handle_media_commands();
        self.handle_tray();
        self.tick(ctx);
        self.apply_actions(ctx);
        self.sync_media_controls();
    }

    pub fn frame_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.apply_theme(ctx);
        self.lock_scroll_axis(ctx);
        crate::ui::show(self, ui);
        self.apply_actions(ctx);
        self.sync_media_controls();

        let mut geometry_dirty = false;
        if let Some(rect) = ctx.input(|input| input.viewport().inner_rect) {
            let size = [rect.width(), rect.height()];
            if valid_window_size(size) && geometry_changed(self.last_window_size, size) {
                self.last_window_size = Some(size);
                geometry_dirty = true;
            }
        }
        if let Some(rect) = ctx.input(|input| input.viewport().outer_rect) {
            let position = [rect.min.x, rect.min.y];
            if valid_window_position(position) && geometry_changed(self.last_window_pos, position) {
                self.last_window_pos = Some(position);
                geometry_dirty = true;
            }
        }
        if geometry_dirty {
            self.mark_session_dirty();
        }

        let playing = self.now_playing().is_some_and(|now| now.playing);
        if playing {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if !self.toasts.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        if self.any_play_pending() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        if self.is_connected() {
            ctx.request_repaint_after(REMOTE_POLL_ACTIVE);
        }
        if ctx.input(|input| input.viewport().close_requested())
            && !self.quit_requested
            && self.settings.keep_playing_in_background
            && self.tray.is_some()
        {
            // The window genuinely closes; the process stays in the tray and
            // the outer loop recreates a window on demand. No compositor
            // tricks: this works the same on every desktop.
            self.hide_intent = true;
        }
    }

    /// Keeps a scroll gesture on one axis.
    ///
    /// A trackpad reports a little of the other axis during a one-axis
    /// gesture, so a page whose rows scroll sideways drifted diagonally. The
    /// axis is chosen from the first movement of a gesture and held until it
    /// pauses, the way the platforms' own scrolling behaves.
    fn lock_scroll_axis(&mut self, ctx: &egui::Context) {
        let (raw, from_trackpad, ended) = ctx.input(|input| {
            let mut sum = egui::Vec2::ZERO;
            let mut pointish = false;
            let mut ended = false;
            for event in &input.events {
                if let egui::Event::MouseWheel {
                    unit, delta, phase, ..
                } = event
                {
                    sum += *delta;
                    pointish |= *unit == egui::MouseWheelUnit::Point;
                    ended |= matches!(phase, egui::TouchPhase::End | egui::TouchPhase::Cancel);
                }
            }
            (sum, pointish, ended)
        });
        let now = Instant::now();
        if raw != egui::Vec2::ZERO {
            self.scroll_from_trackpad = from_trackpad;
        }
        // Linux compositors hand touchpad deltas through unscaled and they
        // land well short of what other players scroll; wheels arrive as
        // lines and are scaled already. macOS feels right as delivered.
        let trackpad_here = cfg!(target_os = "linux") && self.scroll_from_trackpad;
        if trackpad_here {
            ctx.input_mut(|input| input.smooth_scroll_delta *= TRACKPAD_SCALE);
        }
        // macOS glides after the fingers lift; Linux hands over raw deltas
        // that stop dead. While the fingers move, remember where the gesture
        // has been; the frame it ends, carry its speed of the last tenth of
        // a second on, decaying, the way native scroll views here feel.
        if trackpad_here && raw != egui::Vec2::ZERO {
            self.glide = None;
            self.scroll_accum += raw * TRACKPAD_SCALE;
            self.scroll_history
                .add(ctx.input(|input| input.time), self.scroll_accum);
            self.scroll_last_event = Some(now);
            // Wayland announces the lift; where nothing does, the quiet-gap
            // check below needs a frame to run in.
            ctx.request_repaint_after(Duration::from_millis(60));
        } else if raw != egui::Vec2::ZERO || ctx.input(|input| input.pointer.any_down()) {
            // A wheel takes over, or a press catches the page.
            self.glide = None;
            self.scroll_history.clear();
            self.scroll_last_event = None;
        }
        let quiet = self
            .scroll_last_event
            .is_some_and(|at| now.duration_since(at).as_secs_f32() > 0.15);
        if ended || quiet {
            let mut velocity = self.scroll_history.velocity().unwrap_or(egui::Vec2::ZERO);
            if let Some((axis, _)) = self.scroll_lock {
                match axis {
                    ScrollAxis::Horizontal => velocity.y = 0.0,
                    ScrollAxis::Vertical => velocity.x = 0.0,
                }
            }
            self.glide = (velocity.length() > GLIDE_START).then_some(velocity);
            self.scroll_history.clear();
            self.scroll_accum = egui::Vec2::ZERO;
            self.scroll_last_event = None;
        }
        if let Some(velocity) = self.glide {
            if raw == egui::Vec2::ZERO {
                let dt = ctx.input(|input| input.stable_dt).clamp(0.001, 0.05);
                ctx.input_mut(|input| input.smooth_scroll_delta += velocity * dt);
                let slower = velocity * (-dt / GLIDE_DECAY).exp();
                self.glide = (slower.length() > GLIDE_STOP).then_some(slower);
            }
            ctx.request_repaint();
        }
        let held = self
            .scroll_lock
            .filter(|(_, at)| now.duration_since(*at) < SCROLL_GESTURE_GAP)
            .map(|(axis, _)| axis);
        let moved = raw != egui::Vec2::ZERO;
        let axis = match held {
            Some(axis) => axis,
            None if moved && raw.x.abs() > raw.y.abs() * 1.2 => ScrollAxis::Horizontal,
            None if moved => ScrollAxis::Vertical,
            None => {
                self.scroll_lock = None;
                return;
            }
        };
        if moved {
            self.scroll_lock = Some((axis, now));
        }
        ctx.input_mut(|input| match axis {
            ScrollAxis::Horizontal => input.smooth_scroll_delta.y = 0.0,
            ScrollAxis::Vertical => input.smooth_scroll_delta.x = 0.0,
        });
    }

    /// Persist state when a window closes (to the tray or for good).
    pub fn save_state(&mut self) -> Result<(), StateSaveError> {
        let now = Instant::now();
        self.update_resume_point_at(true, now);
        let settings = self.save_settings_at(now).err();
        let session = self.write_session_at(now).err();
        match (settings, session) {
            (None, None) => Ok(()),
            (Some(error), None) => Err(StateSaveError::Settings(Box::new(error))),
            (None, Some(error)) => Err(StateSaveError::Session(Box::new(error))),
            (Some(settings), Some(session)) => Err(StateSaveError::Both {
                settings: Box::new(settings),
                session: Box::new(session),
            }),
        }
    }

    fn update_resume_point_at(&mut self, force: bool, checkpoint_at: Instant) {
        let Some(now_playing) = self.now_playing() else {
            return;
        };
        if now_playing.local && !self.local.connected {
            // A reconnecting engine remains visible optimistically, but its
            // interpolated clock is no longer a usable persistence source.
            return;
        }
        let context = self.playing_context_uri();
        let identity_changed = self.resume_context != context
            || self.resume_track.as_deref() != Some(now_playing.uri.as_str());
        let checkpoint_due = checkpoint_at.saturating_duration_since(self.last_resume_checkpoint)
            >= RESUME_CHECKPOINT_INTERVAL
            && self.resume_position_ms != now_playing.position_ms;
        let changed = force || identity_changed || checkpoint_due;
        if !changed {
            return;
        }
        self.resume_context = context;
        self.resume_track = Some(now_playing.uri);
        self.resume_position_ms = now_playing.position_ms;
        self.last_resume_checkpoint = checkpoint_at;
        self.mark_session_dirty();
    }

    fn capture_resume_before_playback_loss(&mut self, observed_at: Instant) {
        self.update_resume_point_at(true, observed_at);
        self.last_now_playing_uri = None;
    }

    fn save_session_at(&mut self, now: Instant) -> Result<(), SaveError> {
        self.update_resume_point_at(true, now);
        self.write_session_at(now)
    }

    fn write_session_at(&mut self, now: Instant) -> Result<(), SaveError> {
        self.last_session_save_attempt = now;
        let result = if self.offline {
            Ok(())
        } else {
            let mut sorts: Vec<_> = self
                .table_sorts
                .iter()
                .map(|(page, sort)| (page.encode(), *sort))
                .collect();
            sorts.sort_by(|a, b| a.0.cmp(&b.0));
            SessionState {
                last_page: Some(self.page().encode()),
                recent_contexts: self.recent_contexts.clone(),
                last_context: self.resume_context.clone(),
                last_track: self.resume_track.clone(),
                last_position_ms: self.resume_position_ms,
                shuffle_on: self.shuffle_wanted,
                sorts,
                window_size: self.last_window_size,
                window_pos: self.last_window_pos,
                queue_open: Some(self.show_queue_panel),
            }
            .save(&self.dirs.session_file())
        };
        match result {
            Ok(()) => {
                self.session_dirty = false;
                self.session_save_retrying = false;
                Ok(())
            }
            Err(error) => {
                self.session_dirty = true;
                self.session_save_retrying = true;
                Err(error)
            }
        }
    }

    /// Final teardown at real quit.
    pub fn shutdown(&mut self) -> Result<(), StateSaveError> {
        let result = self.save_state();
        self.backend.shutdown();
        result
    }
}

pub fn engine_config(dirs: &AppDirs, settings: &Settings) -> EngineConfig {
    EngineConfig {
        device_name: settings.device_name.trim().to_string(),
        bitrate_kbps: settings.bitrate,
        normalisation: settings.normalisation,
        autoplay: settings.autoplay,
        gapless: settings.gapless,
        backend: settings.platform_backend(),
        audio_device: settings
            .audio_device
            .clone()
            .filter(|device| !device.trim().is_empty()),
        initial_volume: settings.volume,
        volume_dir: dirs.volume_dir(),
        audio_cache_dir: settings.audio_cache.then(|| dirs.audio_cache_dir()),
        audio_cache_limit: Some(settings.audio_cache_mb.max(64) * 1024 * 1024),
    }
}

pub fn volume_to_percent(volume: u16) -> u8 {
    ((u32::from(volume) * 100 + u32::from(u16::MAX) / 2) / u32::from(u16::MAX)) as u8
}

pub fn percent_to_volume(percent: u8) -> u16 {
    ((u32::from(percent.min(100)) * u32::from(u16::MAX)) / 100) as u16
}

fn page_related_needs_load(pages: &HashMap<String, ArtistPage>, id: &str) -> bool {
    pages.get(id).is_some_and(|page| page.related.needs_load())
}

fn remote_action_label(action: RemoteAction) -> &'static str {
    match action {
        RemoteAction::Play => "Couldn't start playback",
        RemoteAction::Pause => "Couldn't pause",
        RemoteAction::Next => "Couldn't skip",
        RemoteAction::Previous => "Couldn't go back",
        RemoteAction::Seek => "Couldn't seek",
        RemoteAction::Volume => "Couldn't change the volume",
        RemoteAction::Shuffle => "Couldn't change shuffle",
        RemoteAction::Repeat => "Couldn't change repeat",
    }
}

fn friendly_page_error(error: &crate::api::ApiError) -> String {
    match error.status() {
        Some(403) | Some(404) => {
            "Spotify doesn't make this playlist's songs available to third-party apps.".to_string()
        }
        _ => error.to_string(),
    }
}

/// Spotify balks at gigantic track lists, so a play that starts deep in
/// one keeps the five hundred songs from its start onward.
fn cap_uris(uris: Vec<String>, index: u32) -> (Vec<String>, u32) {
    if uris.len() <= MAX_PLAY_URIS {
        return (uris, index);
    }
    let start = (index as usize).min(uris.len() - 1);
    let end = (start + MAX_PLAY_URIS).min(uris.len());
    (uris[start..end].to_vec(), 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_geometry_rejects_unusable_session_values() {
        assert!(valid_window_size([1240.0, 800.0]));
        assert!(valid_window_size([3840.0, 2160.0]));
        assert!(!valid_window_size([399.0, 800.0]));
        assert!(!valid_window_size([100_000.0, 2160.0]));
        assert!(!valid_window_size([1240.0, f32::NAN]));
        assert!(valid_window_position([-1920.0, 0.0]));
        assert!(valid_window_position([-7680.0, 4320.0]));
        assert!(!valid_window_position([-100_000.0, 0.0]));
        assert!(!valid_window_position([f32::INFINITY, 0.0]));
    }

    #[test]
    fn geometry_changes_ignore_subpixel_noise() {
        assert!(!geometry_changed(Some([1000.0, 700.0]), [1000.5, 699.5]));
        assert!(geometry_changed(Some([1000.0, 700.0]), [1002.0, 700.0]));
        assert!(geometry_changed(None, [1000.0, 700.0]));
    }

    #[test]
    fn volume_conversions_round_trip() {
        assert_eq!(volume_to_percent(u16::MAX), 100);
        assert_eq!(volume_to_percent(0), 0);
        assert_eq!(volume_to_percent(percent_to_volume(70)), 70);
        assert_eq!(percent_to_volume(200), u16::MAX);
    }

    struct HeadlessApp {
        app: App,
        root: std::path::PathBuf,
    }

    impl std::ops::Deref for HeadlessApp {
        type Target = App;

        fn deref(&self) -> &Self::Target {
            &self.app
        }
    }

    impl std::ops::DerefMut for HeadlessApp {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.app
        }
    }

    impl Drop for HeadlessApp {
        fn drop(&mut self) {
            self.app.backend.shutdown();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn headless_app() -> HeadlessApp {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "fastpotify-app-test-{}-{started}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let dirs = AppDirs {
            config: root.join("config"),
            state: root.join("state"),
            cache: root.join("cache"),
        };
        let mut app = App::new(
            &Waker::default(),
            dirs,
            Settings::default(),
            AppOptions {
                media_controls: false,
                tray: false,
            },
        );
        app.backend.set_offline(true);
        app.local_ready = true;
        HeadlessApp { app, root }
    }

    fn egui_pass(ctx: &egui::Context, run_ui: impl FnMut(&mut egui::Ui)) {
        let mut output = ctx.run_ui(Default::default(), run_ui);
        output.textures_delta.clear();
    }

    fn snapshot_at(percent: u8) -> LocalState {
        LocalState {
            volume: percent_to_volume(percent),
            ..LocalState::default()
        }
    }

    fn active_local_snapshot(app: &App, shuffle: bool, playback: Playback) -> LocalState {
        LocalState {
            connected: true,
            playback,
            track: Some(crate::player::LocalTrack {
                uri: "spotify:track:active".into(),
                title: "Active".into(),
                duration_ms: 240_000,
                ..crate::player::LocalTrack::default()
            }),
            position_ms: 30_000,
            volume: app.local.volume,
            shuffle,
            ..LocalState::default()
        }
    }

    fn active_remote_snapshot(shuffle: bool) -> PlaybackState {
        PlaybackState {
            shuffle_state: shuffle,
            item: Some(PlayableItem::Track(Track {
                uri: "spotify:track:remote".into(),
                name: "Remote".into(),
                duration_ms: 240_000,
                ..Track::default()
            })),
            ..PlaybackState::default()
        }
    }

    fn active_api_snapshot_on(shuffle: bool, device_id: &str) -> PlaybackState {
        let mut state = active_remote_snapshot(shuffle);
        state.device = Some(Device {
            id: Some(device_id.into()),
            is_active: true,
            ..Device::default()
        });
        state
    }

    fn deliver_remote_state(app: &mut App, state: Option<PlaybackState>) {
        app.remote_poll_seq += 1;
        app.handle_api(ApiResponse::PlaybackState {
            seq: app.remote_poll_seq,
            result: Ok(state),
        });
    }

    fn deliver_remote_snapshot(app: &mut App, state: PlaybackState) {
        deliver_remote_state(app, Some(state));
    }

    #[test]
    fn remote_device_capabilities_have_one_conservative_policy() {
        let restricted = Device {
            is_restricted: true,
            supports_volume: Some(true),
            ..Device::default()
        };
        for command in [
            RemoteCommand::Action(RemoteAction::Play),
            RemoteCommand::Action(RemoteAction::Seek),
            RemoteCommand::Action(RemoteAction::Volume),
            RemoteCommand::Action(RemoteAction::Shuffle),
            RemoteCommand::Action(RemoteAction::Repeat),
            RemoteCommand::AddToQueue,
            RemoteCommand::Transfer,
        ] {
            assert!(remote_command_issue(Some(&restricted), command).is_some());
        }

        let fixed_volume = Device {
            supports_volume: Some(false),
            ..Device::default()
        };
        assert!(
            remote_command_issue(
                Some(&fixed_volume),
                RemoteCommand::Action(RemoteAction::Volume),
            )
            .is_some()
        );
        assert!(
            remote_command_issue(
                Some(&fixed_volume),
                RemoteCommand::Action(RemoteAction::Play),
            )
            .is_none()
        );
        assert!(remote_command_issue(None, RemoteCommand::Transfer).is_none());
    }

    #[test]
    fn unsupported_remote_controls_never_reach_the_backend() {
        let mut app = headless_app();
        let mut state = active_api_snapshot_on(false, "restricted");
        let device = state.device.as_mut().expect("remote device");
        device.is_restricted = true;
        device.supports_volume = Some(false);
        deliver_remote_snapshot(&mut app, state);
        app.selected_device = Some("restricted".into());
        app.backend.take_api_requests();

        assert!(!app.remote(RemoteAction::Next, Some("restricted".into())));
        app.seek(20_000);
        app.set_volume(80, true);
        assert_eq!(app.set_shuffle(true), ShuffleDispatch::Unsupported);
        app.set_repeat(RepeatMode::Track);
        app.add_to_queue("spotify:track:queued".into(), "Queued".into());
        app.play_request(
            PlayRequest::tracks(vec!["spotify:track:play".into()]),
            false,
        );

        assert!(app.backend.take_api_requests().is_empty());
        assert!(app.pending_remote_position.is_none());
        assert!(app.pending_remote_volume.is_none());
        assert!(app.optimistic_playing.is_none());
        assert!(!app.shuffle_wanted);
        assert!(!app.remote.as_ref().unwrap().state.shuffle_state);
    }

    #[test]
    fn fixed_volume_devices_keep_their_other_controls() {
        let mut app = headless_app();
        let mut state = active_api_snapshot_on(false, "speaker");
        state
            .device
            .as_mut()
            .expect("remote device")
            .supports_volume = Some(false);
        deliver_remote_snapshot(&mut app, state);
        app.selected_device = Some("speaker".into());
        app.backend.take_api_requests();

        app.set_volume(80, true);
        assert!(app.backend.take_api_requests().is_empty());
        assert!(app.remote(RemoteAction::Next, Some("speaker".into())));
        assert!(matches!(
            app.backend.take_api_requests().as_slice(),
            [ApiRequest::Remote {
                action: RemoteAction::Next,
                device_id: Some(device_id),
                ..
            }] if device_id == "speaker"
        ));
    }

    #[test]
    fn active_idless_device_is_routed_without_guessing_an_id() {
        let mut app = headless_app();
        app.local_ready = false;
        let mut state = active_remote_snapshot(false);
        state.device = Some(Device {
            id: None,
            is_active: true,
            ..Device::default()
        });
        deliver_remote_snapshot(&mut app, state);
        app.backend.take_api_requests();

        app.play_request(
            PlayRequest::tracks(vec!["spotify:track:play".into()]),
            false,
        );
        assert!(matches!(
            app.backend.take_api_requests().as_slice(),
            [ApiRequest::Remote {
                action: RemoteAction::Play,
                device_id: None,
                ..
            }]
        ));
        assert_eq!(app.set_shuffle(true), ShuffleDispatch::Remote);
        assert!(matches!(
            app.backend.take_api_requests().as_slice(),
            [ApiRequest::Remote {
                action: RemoteAction::Shuffle,
                device_id: None,
                ..
            }]
        ));
    }

    #[test]
    fn restricted_transfer_does_not_replace_the_selected_target() {
        let mut app = headless_app();
        app.selected_device = Some("current".into());
        app.devices.push(Device {
            id: Some("blocked".into()),
            is_restricted: true,
            ..Device::default()
        });
        app.backend.take_api_requests();

        app.transfer("blocked".into());

        assert_eq!(app.selected_device.as_deref(), Some("current"));
        assert!(app.backend.take_api_requests().is_empty());
    }

    #[test]
    fn a_volume_set_here_is_saved_immediately() {
        let mut app = headless_app();
        app.set_volume(80, true);
        assert_eq!(volume_to_percent(app.settings.volume), 80);
        assert!(app.settings_dirty);
    }

    #[test]
    fn panel_and_sidebar_actions_dirty_their_own_state_files() {
        let mut app = headless_app();
        let ctx = egui::Context::default();

        app.actions.push(Action::ToggleQueuePanel);
        app.apply_actions(&ctx);
        assert!(app.show_queue_panel);
        assert!(app.session_dirty);

        app.session_dirty = false;
        app.actions.push(Action::ToggleSidebar);
        app.apply_actions(&ctx);
        assert!(!app.settings.sidebar_visible);
        assert!(app.settings_dirty);
        assert!(!app.session_dirty);
    }

    #[test]
    fn successful_settings_save_cleans_only_after_durable_write() {
        let mut app = headless_app();
        app.settings.device_name = "Durable desktop".into();
        app.mark_settings_dirty();

        app.save_settings_at(Instant::now()).expect("save settings");

        assert!(!app.settings_dirty);
        assert!(!app.settings_save_retrying);
        assert_eq!(
            Settings::load(&app.dirs.settings_file()).device_name,
            "Durable desktop"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_settings_path_retains_dirty_state_until_rate_limited_retry() {
        use std::os::unix::fs::symlink;

        let mut app = headless_app();
        std::fs::create_dir_all(&app.dirs.config).expect("create config directory");
        let target = app.dirs.config.join("outside.json");
        std::fs::write(&target, b"untouched").expect("create symlink target");
        let settings_path = app.dirs.settings_file();
        symlink(&target, &settings_path).expect("create unsafe settings path");
        app.settings.device_name = "Pending settings".into();
        app.mark_settings_dirty();

        let error = app.save_state().expect_err("unsafe destination must fail");
        match error {
            StateSaveError::Settings(error) => {
                assert!(matches!(*error, SaveError::Write { .. }));
            }
            other => panic!("only the unsafe settings path should fail: {other}"),
        }

        assert!(app.settings_dirty);
        assert!(app.settings_save_retrying);
        let failed_at = app.last_settings_save_attempt;
        assert!(
            app.try_autosave_settings_at(failed_at + STATE_SAVE_RETRY - Duration::from_millis(1))
                .is_none(),
            "a failed save must not retry every frame"
        );
        assert_eq!(std::fs::read(&target).expect("read target"), b"untouched");

        std::fs::remove_file(&settings_path).expect("remove unsafe symlink");
        app.try_autosave_settings_at(failed_at + STATE_SAVE_RETRY)
            .expect("retry is due")
            .expect("retry succeeds");
        assert!(!app.settings_dirty);
        assert!(!app.settings_save_retrying);
        assert_eq!(
            Settings::load(&settings_path).device_name,
            "Pending settings"
        );
    }

    #[test]
    fn oversized_session_retains_dirty_state_and_the_last_valid_file() {
        let mut app = headless_app();
        app.mark_session_dirty();
        app.write_session_at(Instant::now())
            .expect("write valid session");
        let path = app.dirs.session_file();
        let before = std::fs::read(&path).expect("read valid session");
        app.recent_contexts = vec!["x".repeat(1024 * 1024 + 1)];
        app.mark_session_dirty();

        assert!(matches!(
            app.write_session_at(Instant::now())
                .expect_err("reject oversized session"),
            SaveError::TooLarge {
                kind: "session",
                ..
            }
        ));

        assert!(app.session_dirty);
        assert!(app.session_save_retrying);
        assert_eq!(std::fs::read(path).expect("read preserved session"), before);
    }

    #[test]
    fn resume_position_uses_long_checkpoint_cadence_but_track_change_is_immediate() {
        let mut app = headless_app();
        let baseline = Instant::now();
        let mut state = active_local_snapshot(&app, false, Playback::Playing);
        state.position_ms = 10_000;
        app.local = state;
        app.resume_context = None;
        app.resume_track = Some("spotify:track:active".into());
        app.resume_position_ms = 10_000;
        app.last_resume_checkpoint = baseline;
        app.session_dirty = false;
        app.local.position_ms = 45_000;

        app.update_resume_point_at(false, baseline + Duration::from_secs(30));
        assert_eq!(app.resume_position_ms, 10_000);
        assert!(!app.session_dirty);

        app.update_resume_point_at(false, baseline + RESUME_CHECKPOINT_INTERVAL);
        assert_eq!(app.resume_position_ms, 45_000);
        assert!(app.session_dirty);

        app.session_dirty = false;
        app.local.track.as_mut().expect("local track").uri = "spotify:track:next".into();
        app.local.position_ms = 12_345;
        app.update_resume_point_at(
            false,
            baseline + RESUME_CHECKPOINT_INTERVAL + Duration::from_secs(1),
        );
        assert_eq!(app.resume_track.as_deref(), Some("spotify:track:next"));
        assert_eq!(app.resume_position_ms, 12_345);
        assert!(app.session_dirty);
    }

    #[test]
    fn pause_transition_and_explicit_window_save_flush_current_resume() {
        let mut app = headless_app();
        app.local = active_local_snapshot(&app, false, Playback::Playing);
        app.resume_context = None;
        app.resume_track = Some("spotify:track:active".into());
        app.resume_position_ms = 30_000;
        app.session_dirty = false;
        let mut paused = active_local_snapshot(&app, false, Playback::Paused);
        paused.position_ms = 34_567;

        app.handle_local(paused);

        assert_eq!(app.resume_position_ms, 34_567);
        assert!(app.session_dirty);
        app.save_state().expect("save state on window close");
        assert!(!app.session_dirty);
        assert_eq!(
            SessionState::load(&app.dirs.session_file()).last_position_ms,
            34_567
        );
    }

    #[test]
    fn shutdown_flushes_the_latest_resume_even_without_a_due_checkpoint() {
        let mut app = headless_app();
        let session_path = app.dirs.session_file();
        let mut state = active_local_snapshot(&app, false, Playback::Paused);
        state.position_ms = 51_234;
        app.local = state;
        app.resume_context = None;
        app.resume_track = Some("spotify:track:active".into());
        app.resume_position_ms = 50_000;
        app.session_dirty = false;

        app.shutdown().expect("save before backend shutdown");

        assert_eq!(SessionState::load(&session_path).last_position_ms, 51_234);
    }

    #[test]
    fn stopped_and_disconnected_local_transitions_preserve_the_active_resume_point() {
        enum LocalLoss {
            StoppedSnapshot,
            DisconnectedSnapshot,
            DisconnectedPlayback,
        }

        for loss in [
            LocalLoss::StoppedSnapshot,
            LocalLoss::DisconnectedSnapshot,
            LocalLoss::DisconnectedPlayback,
        ] {
            let mut app = headless_app();
            let mut active = active_local_snapshot(&app, false, Playback::Playing);
            active.position_ms = 30_000;
            active.position_at = Some(Instant::now() - Duration::from_secs(3));
            app.local = active;
            app.assumed_context = Some(AssumedContext {
                uri: "spotify:album:local-resume".into(),
                at: Instant::now(),
            });
            app.resume_context = None;
            app.resume_track = Some("spotify:track:older".into());
            app.resume_position_ms = 1_000;
            app.last_resume_checkpoint = Instant::now();
            app.session_dirty = false;
            let volume = app.local.volume;
            match loss {
                LocalLoss::StoppedSnapshot => app.handle_local(LocalState {
                    connected: true,
                    playback: Playback::Stopped,
                    volume,
                    ..LocalState::default()
                }),
                LocalLoss::DisconnectedSnapshot => {
                    let mut disconnected = app.local.clone();
                    disconnected.connected = false;
                    app.handle_local(disconnected);
                }
                LocalLoss::DisconnectedPlayback => {
                    app.handle_playback(LocalPlayback::Connecting);
                }
            }
            // A shutdown later in a reconnect must not keep advancing the
            // optimistic, disconnected clock past the captured point.
            app.local.position_at = Some(Instant::now() - Duration::from_secs(60));

            let captured = app.resume_position_ms;
            assert!(
                (33_000..35_000).contains(&captured),
                "the last playing position is interpolated before replacement: {captured}"
            );
            assert_eq!(
                app.resume_context.as_deref(),
                Some("spotify:album:local-resume")
            );
            assert_eq!(app.resume_track.as_deref(), Some("spotify:track:active"));
            assert!(app.session_dirty);

            let session_path = app.dirs.session_file();
            app.shutdown().expect("persist local resume on shutdown");
            let saved = SessionState::load(&session_path);
            assert_eq!(saved.last_context, app.resume_context);
            assert_eq!(saved.last_track, app.resume_track);
            assert_eq!(saved.last_position_ms, captured);
        }
    }

    #[test]
    fn same_track_restart_after_stopped_replaces_the_completed_resume_point() {
        let mut app = headless_app();
        let mut active = active_local_snapshot(&app, false, Playback::Playing);
        active.position_ms = 230_000;
        app.local = active;
        app.last_now_playing_uri = Some("spotify:track:active".into());
        app.resume_track = Some("spotify:track:active".into());
        app.resume_position_ms = 200_000;
        let mut stopped = app.local.clone();
        stopped.playback = Playback::Stopped;
        stopped.position_ms = 240_000;

        app.handle_local(stopped);
        assert_eq!(app.resume_position_ms, 230_000);

        let mut restarted = app.local.clone();
        restarted.playback = Playback::Playing;
        restarted.position_ms = 0;
        app.handle_local(restarted);

        assert_eq!(app.resume_track.as_deref(), Some("spotify:track:active"));
        assert_eq!(app.resume_position_ms, 0);
    }

    #[test]
    fn disappearing_remote_snapshots_preserve_the_active_resume_point() {
        for no_item in [false, true] {
            let mut app = headless_app();
            app.remote = Some(RemoteSnapshot {
                state: PlaybackState {
                    shuffle_state: app.shuffle_wanted,
                    context: Some(crate::api::models::Context {
                        uri: "spotify:album:remote-resume".into(),
                        kind: "album".into(),
                    }),
                    progress_ms: Some(50_000),
                    is_playing: true,
                    item: Some(PlayableItem::Track(Track {
                        uri: "spotify:track:remote-loss".into(),
                        name: "Remote loss".into(),
                        duration_ms: 240_000,
                        ..Track::default()
                    })),
                    ..PlaybackState::default()
                },
                received_at: Instant::now() - Duration::from_secs(3),
            });
            app.resume_context = None;
            app.resume_track = Some("spotify:track:older".into());
            app.resume_position_ms = 1_000;
            app.last_resume_checkpoint = Instant::now();
            app.session_dirty = false;
            let replacement = no_item.then(PlaybackState::default);

            deliver_remote_state(&mut app, replacement);

            let captured = app.resume_position_ms;
            assert!(
                (53_000..55_000).contains(&captured),
                "the remote position is interpolated before disappearance: {captured}"
            );
            assert_eq!(
                app.resume_context.as_deref(),
                Some("spotify:album:remote-resume")
            );
            assert_eq!(
                app.resume_track.as_deref(),
                Some("spotify:track:remote-loss")
            );
            assert!(app.session_dirty);

            let session_path = app.dirs.session_file();
            app.shutdown().expect("persist remote resume on shutdown");
            let saved = SessionState::load(&session_path);
            assert_eq!(saved.last_context, app.resume_context);
            assert_eq!(saved.last_track, app.resume_track);
            assert_eq!(saved.last_position_ms, captured);
        }
    }

    #[test]
    fn saving_a_track_preserves_the_liked_count_during_refresh() {
        let mut app = headless_app();
        app.library.liked.loaded_once = true;
        app.library.liked.total = Some(40);

        app.handle_api(ApiResponse::SavedChanged {
            uris: vec!["spotify:track:new".into()],
            saved: true,
            result: Ok(()),
        });

        assert_eq!(app.library.liked.total, Some(41));
        assert!(!app.library.liked.loaded_once);
    }

    #[test]
    fn interface_zoom_is_applied_once_then_tracks_bounded_context_changes() {
        let mut app = headless_app();
        let ctx = egui::Context::default();
        app.settings.zoom = 1.4;

        egui_pass(&ctx, |ui| app.tick(ui.ctx()));
        egui_pass(&ctx, |_| {});
        assert!((ctx.zoom_factor() - 1.4).abs() < 0.001);

        app.settings_dirty = false;
        ctx.set_zoom_factor(4.0);
        egui_pass(&ctx, |_| {});
        egui_pass(&ctx, |ui| app.tick(ui.ctx()));
        egui_pass(&ctx, |_| {});
        assert_eq!(ctx.zoom_factor(), crate::settings::ZOOM_MAX);
        assert_eq!(app.settings.zoom, crate::settings::ZOOM_MAX);
        assert!(app.settings_dirty);
    }

    #[test]
    fn a_stale_engine_snapshot_does_not_pull_the_volume_back() {
        let mut app = headless_app();
        app.set_volume(80, true);

        // The engine reports `VolumeChanged` asynchronously, so its next
        // snapshot still carries the volume from before the change.
        app.handle_local(snapshot_at(20));
        assert_eq!(volume_to_percent(app.local.volume), 80);
        assert_eq!(volume_to_percent(app.settings.volume), 80);

        // Once it has caught up, its snapshots are trusted again.
        app.handle_local(snapshot_at(80));
        assert_eq!(volume_to_percent(app.local.volume), 80);
    }

    #[test]
    fn a_volume_changed_outside_the_app_is_adopted() {
        let mut app = headless_app();
        app.handle_local(snapshot_at(35));
        assert_eq!(volume_to_percent(app.local.volume), 35);
        assert_eq!(volume_to_percent(app.settings.volume), 35);
    }

    #[test]
    fn shuffled_context_starts_on_a_loaded_playable_item() {
        let mut app = headless_app();
        let mut page = AlbumPage::default();
        page.tracks.items = vec![
            Track {
                uri: "spotify:track:unavailable".into(),
                is_playable: Some(false),
                ..Track::default()
            },
            Track {
                uri: "spotify:track:local".into(),
                is_local: true,
                ..Track::default()
            },
            Track {
                uri: "spotify:track:playable".into(),
                ..Track::default()
            },
        ];
        app.album_pages.insert("album".into(), page);

        let request = app.prepare_play_request(PlayRequest::context("spotify:album:album"), true);
        assert_eq!(
            request.offset_uri.as_deref(),
            Some("spotify:track:playable")
        );

        let ordered = app.prepare_play_request(PlayRequest::context("spotify:album:album"), false);
        assert!(ordered.offset_uri.is_none());
    }

    #[test]
    fn shuffled_large_view_keeps_a_full_ordered_window_around_near_tail_start() {
        use rand::{Rng as _, SeedableRng as _};

        let app = headless_app();
        let uris: Vec<String> = (0..700)
            .map(|index| format!("spotify:track:{index}"))
            .collect();
        let (seed, selected) = (0..10_000_u64)
            .find_map(|seed| {
                let selected = rand::rngs::StdRng::seed_from_u64(seed).random_range(0..uris.len());
                (selected >= 690).then_some((seed, selected))
            })
            .expect("a seed selecting near the tail");
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);

        let request = app.prepare_play_request_with_rng(PlayRequest::tracks(uris), true, &mut rng);

        let start = 700 - MAX_PLAY_URIS;
        let expected: Vec<String> = (start..700)
            .map(|index| format!("spotify:track:{index}"))
            .collect();
        let offset = selected - start;
        assert_eq!(request.uris, expected, "the original view order is kept");
        assert_eq!(request.uris.len(), MAX_PLAY_URIS);
        assert_eq!(request.offset_position, Some(offset as u32));
        assert_eq!(
            request.uris[offset],
            format!("spotify:track:{selected}"),
            "the selected track is inside the capped request"
        );
        assert_eq!(
            request.uris.iter().collect::<HashSet<_>>().len(),
            MAX_PLAY_URIS,
            "the containing window does not rotate or duplicate entries"
        );
    }

    #[test]
    fn explicitly_clicked_near_tail_row_still_becomes_request_start() {
        let app = headless_app();
        let uris: Vec<String> = (0..700)
            .map(|index| format!("spotify:track:{index}"))
            .collect();

        let request =
            app.prepare_play_request(PlayRequest::tracks(uris).starting_at_index(695), true);

        assert_eq!(request.uris.len(), 5);
        assert_eq!(request.uris[0], "spotify:track:695");
        assert_eq!(request.offset_position, Some(0));
    }

    #[test]
    fn shuffle_changes_from_another_client_become_the_mode() {
        let mut app = headless_app();
        let state = active_local_snapshot(&app, true, Playback::Paused);
        app.handle_local(state);
        assert!(app.shuffle_wanted);
        assert!(app.session_dirty);
    }

    #[test]
    fn first_active_snapshots_reconcile_persisted_shuffle_but_idle_does_not() {
        let mut local = headless_app();
        local.shuffle_wanted = true;
        local.session_dirty = false;
        let state = active_local_snapshot(&local, false, Playback::Paused);
        local.handle_local(state);
        assert!(!local.shuffle_wanted);
        assert!(local.session_dirty);

        let mut remote = headless_app();
        remote.shuffle_wanted = true;
        remote.session_dirty = false;
        deliver_remote_snapshot(&mut remote, active_remote_snapshot(false));
        assert!(!remote.shuffle_wanted);
        assert!(remote.session_dirty);

        let mut idle = headless_app();
        idle.shuffle_wanted = true;
        deliver_remote_snapshot(
            &mut idle,
            PlaybackState {
                shuffle_state: false,
                ..PlaybackState::default()
            },
        );
        assert!(idle.shuffle_wanted, "idle state preserves the desired mode");
    }

    #[test]
    fn unchanged_shuffle_mismatch_is_held_then_reconciled_for_local_and_remote() {
        let mut local = headless_app();
        local.shuffle_wanted = true;
        local.shuffle_set_at = Some(Instant::now());
        let mismatch = active_local_snapshot(&local, false, Playback::Paused);
        local.handle_local(mismatch.clone());
        assert!(local.shuffle_wanted, "a current local intent is held");
        local.shuffle_set_at = Some(Instant::now() - SHUFFLE_INTENT_HOLD);
        local.handle_local(mismatch);
        assert!(
            !local.shuffle_wanted,
            "an unchanged failed local command is authoritative after the hold"
        );

        let mut remote = headless_app();
        remote.shuffle_wanted = true;
        remote.shuffle_set_at = Some(Instant::now());
        let mismatch = active_remote_snapshot(false);
        deliver_remote_snapshot(&mut remote, mismatch.clone());
        assert!(remote.shuffle_wanted, "a current remote intent is held");
        remote.shuffle_set_at = Some(Instant::now() - SHUFFLE_INTENT_HOLD);
        deliver_remote_snapshot(&mut remote, mismatch);
        assert!(
            !remote.shuffle_wanted,
            "an unchanged failed remote command is authoritative after the hold"
        );
    }

    fn play_queued_during_reconnect() -> HeadlessApp {
        let mut app = headless_app();
        app.local.connected = true;
        app.handle_playback(LocalPlayback::Connecting);
        assert!(
            !app.local.connected,
            "the retired engine is no longer current"
        );
        app.play_request(PlayRequest::context("spotify:album:x"), true);

        let queued = app.queued_play.as_ref().expect("queued while disconnected");
        assert_eq!(
            queued.request.context_uri.as_deref(),
            Some("spotify:album:x")
        );
        assert!(app.shuffle_wanted);
        assert!(app.session_dirty);
        assert!(app.optimistic_playing.is_none());
        app
    }

    fn play_queued_with_mode(shuffle: bool) -> HeadlessApp {
        let mut app = headless_app();
        app.shuffle_wanted = shuffle;
        app.local.shuffle = shuffle;
        app.handle_playback(LocalPlayback::Connecting);
        app.play_request(PlayRequest::context("spotify:album:queued"), false);
        assert!(app.queued_play.is_some());
        app
    }

    fn reconnecting_with_pending_shuffle(before: bool, after: bool) -> HeadlessApp {
        let mut app = headless_app();
        app.shuffle_wanted = before;
        app.local.shuffle = before;
        app.handle_playback(LocalPlayback::Ready {
            device_id: "local".into(),
        });
        app.handle_playback(LocalPlayback::Connecting);
        assert_eq!(app.local_device_id.as_deref(), Some("local"));
        assert_eq!(app.set_shuffle(after), ShuffleDispatch::Deferred);
        assert_eq!(app.pending_local_shuffle, Some(after));
        assert!(app.queued_play.is_none());
        app
    }

    #[test]
    fn queued_local_shuffle_toggles_defer_both_directions_without_remote_action() {
        for (before, after) in [(false, true), (true, false)] {
            let mut app = play_queued_with_mode(before);
            let local_before = app.local.shuffle;
            let remote_before = app.remote.as_ref().map(|remote| remote.state.clone());

            let dispatch = app.set_shuffle(after);

            assert_eq!(dispatch, ShuffleDispatch::Deferred);
            assert_eq!(app.shuffle_wanted, after);
            assert_eq!(app.local.shuffle, local_before);
            assert_eq!(
                app.remote.as_ref().map(|remote| remote.state.clone()),
                remote_before
            );
            assert_eq!(app.pending_local_shuffle, Some(after));
            assert!(app.queued_play.is_some());
            assert!(app.toasts.is_empty());
        }
    }

    #[test]
    fn aged_same_local_api_mismatch_waits_for_both_local_readiness_orders() {
        for (before, after) in [(false, true), (true, false)] {
            for ready_first in [false, true] {
                let mut app = reconnecting_with_pending_shuffle(before, after);
                app.shuffle_set_at = Some(Instant::now() - SHUFFLE_INTENT_HOLD);
                deliver_remote_snapshot(&mut app, active_api_snapshot_on(before, "local"));
                assert_eq!(app.target(), Target::Remote(None));
                assert_eq!(app.pending_local_shuffle, Some(after));
                assert_eq!(app.shuffle_wanted, after);
                assert_eq!(app.local.shuffle, before);

                let connected = active_local_snapshot(&app, before, Playback::Paused);
                if ready_first {
                    app.handle_playback(LocalPlayback::Ready {
                        device_id: "local".into(),
                    });
                    assert_eq!(app.pending_local_shuffle, Some(after));
                    app.handle_local(connected);
                } else {
                    app.handle_local(connected);
                    assert_eq!(app.pending_local_shuffle, Some(after));
                    app.handle_playback(LocalPlayback::Ready {
                        device_id: "local".into(),
                    });
                }

                assert_eq!(app.pending_local_shuffle, None);
                assert_eq!(app.local.shuffle, after);
                let dispatched_at = app.shuffle_set_at;
                app.handle_playback(LocalPlayback::Ready {
                    device_id: "local".into(),
                });
                let echo = active_local_snapshot(&app, after, Playback::Paused);
                app.handle_local(echo);
                assert_eq!(
                    app.shuffle_set_at, dispatched_at,
                    "repeated readiness events must not dispatch twice"
                );
            }
        }
    }

    #[test]
    fn matching_same_local_api_snapshot_satisfies_pending_shuffle_without_dispatch() {
        for ready_first in [false, true] {
            let mut app = reconnecting_with_pending_shuffle(false, true);
            app.shuffle_set_at = Some(Instant::now() - SHUFFLE_INTENT_HOLD);

            deliver_remote_snapshot(&mut app, active_api_snapshot_on(true, "local"));

            assert_eq!(app.target(), Target::Remote(None));
            assert_eq!(app.pending_local_shuffle, None);
            assert!(app.shuffle_wanted);
            assert_eq!(app.shuffle_set_at, None);
            let connected = active_local_snapshot(&app, true, Playback::Paused);
            if ready_first {
                app.handle_playback(LocalPlayback::Ready {
                    device_id: "local".into(),
                });
                app.handle_local(connected);
            } else {
                app.handle_local(connected);
                app.handle_playback(LocalPlayback::Ready {
                    device_id: "local".into(),
                });
            }

            assert_eq!(app.pending_local_shuffle, None);
            assert!(app.local.shuffle);
            assert_eq!(
                app.shuffle_set_at, None,
                "an acknowledged mode must not emit another local command"
            );
        }
    }

    #[test]
    fn queued_load_satisfies_pending_shuffle_in_both_readiness_orders() {
        for ready_first in [false, true] {
            let mut app = play_queued_with_mode(false);
            assert_eq!(app.set_shuffle(true), ShuffleDispatch::Deferred);
            assert_eq!(app.pending_local_shuffle, Some(true));
            let connected = connected_snapshot(&app);

            if ready_first {
                app.handle_playback(LocalPlayback::Ready {
                    device_id: "local".into(),
                });
                app.handle_local(connected);
            } else {
                app.handle_local(connected);
                app.handle_playback(LocalPlayback::Ready {
                    device_id: "local".into(),
                });
            }

            assert!(app.queued_play.is_none());
            assert_eq!(app.pending_local_shuffle, None);
            assert!(app.shuffle_wanted);
            assert!(app.optimistic_playing.is_some());
            let dispatched_at = app.shuffle_set_at;
            app.handle_playback(LocalPlayback::Ready {
                device_id: "local".into(),
            });
            assert_eq!(app.shuffle_set_at, dispatched_at);
        }
    }

    #[test]
    fn distinct_remote_takeover_cancels_pending_local_shuffle_before_dispatch() {
        for ready_first in [false, true] {
            let mut app = reconnecting_with_pending_shuffle(false, true);
            deliver_remote_snapshot(&mut app, active_api_snapshot_on(false, "remote"));
            assert_eq!(app.target(), Target::Remote(Some("remote".into())));
            assert_eq!(app.pending_local_shuffle, Some(true));
            app.shuffle_set_at = Some(Instant::now() - SHUFFLE_INTENT_HOLD);
            let connected = connected_snapshot(&app);

            if ready_first {
                app.handle_playback(LocalPlayback::Ready {
                    device_id: "local".into(),
                });
                app.handle_local(connected);
            } else {
                app.handle_local(connected);
                app.handle_playback(LocalPlayback::Ready {
                    device_id: "local".into(),
                });
            }

            assert_eq!(app.pending_local_shuffle, None);
            assert!(!app.local.shuffle, "no stale local command was dispatched");
            assert!(
                !app.shuffle_wanted,
                "the active remote remains authoritative once the hold expires"
            );
            assert_eq!(app.shuffle_set_at, None);
        }
    }

    #[test]
    fn lifecycle_failure_and_sign_out_clear_pending_local_shuffle() {
        for status in [
            LocalPlayback::Unavailable,
            LocalPlayback::Failed("engine stopped".into()),
        ] {
            let mut app = reconnecting_with_pending_shuffle(false, true);
            app.handle_playback(status);
            assert_eq!(app.pending_local_shuffle, None);
        }

        let mut app = reconnecting_with_pending_shuffle(false, true);
        app.handle_auth(AuthStatus::SignedOut);
        assert_eq!(app.pending_local_shuffle, None);
    }

    fn connected_snapshot(app: &App) -> LocalState {
        LocalState {
            connected: true,
            volume: app.local.volume,
            ..LocalState::default()
        }
    }

    fn assert_play_dispatched_once(app: &mut App) {
        assert!(app.queued_play.is_none());
        let dispatched = app
            .optimistic_playing
            .expect("the queued request reached the local player");
        assert!(dispatched.0);

        app.handle_playback(LocalPlayback::Ready {
            device_id: "local".into(),
        });
        app.handle_local(connected_snapshot(app));
        assert_eq!(app.optimistic_playing, Some(dispatched));
    }

    #[test]
    fn queued_play_waits_for_ready_after_current_engine_connects() {
        let mut app = play_queued_during_reconnect();

        let connected = connected_snapshot(&app);
        app.handle_local(connected);

        assert!(app.queued_play.is_some());
        assert!(app.optimistic_playing.is_none());
        app.handle_playback(LocalPlayback::Ready {
            device_id: "local".into(),
        });

        assert_play_dispatched_once(&mut app);
        assert_eq!(
            app.assumed_context
                .as_ref()
                .map(|context| context.uri.as_str()),
            Some("spotify:album:x")
        );
        assert!(app.shuffle_wanted);
    }

    #[test]
    fn queued_play_waits_for_current_engine_connection_after_ready() {
        let mut app = play_queued_during_reconnect();

        app.handle_playback(LocalPlayback::Ready {
            device_id: "local".into(),
        });

        assert!(app.queued_play.is_some());
        assert!(app.optimistic_playing.is_none());
        let connected = connected_snapshot(&app);
        app.handle_local(connected);

        assert_play_dispatched_once(&mut app);
    }

    #[test]
    fn disconnected_between_connected_and_ready_keeps_play_queued() {
        let mut app = play_queued_during_reconnect();

        let connected = connected_snapshot(&app);
        app.handle_local(connected);
        let volume = app.local.volume;
        app.handle_local(LocalState {
            volume,
            ..LocalState::default()
        });
        app.handle_playback(LocalPlayback::Ready {
            device_id: "local".into(),
        });

        assert!(app.queued_play.is_some());
        assert!(app.optimistic_playing.is_none());
        let connected = connected_snapshot(&app);
        app.handle_local(connected);

        assert_play_dispatched_once(&mut app);
    }

    #[test]
    fn unavailable_items_trigger_one_bounded_reconnect() {
        let start = Instant::now();
        let mut recovery = UnavailableRecovery::default();
        assert!(!recovery.record(start));
        assert!(!recovery.record(start + Duration::from_secs(1)));
        assert!(recovery.record(start + Duration::from_secs(2)));

        assert!(!recovery.record(start + Duration::from_secs(30)));
        assert!(!recovery.record(start + Duration::from_secs(31)));
        assert!(!recovery.record(start + Duration::from_secs(32)));

        assert!(!recovery.record(start + Duration::from_secs(63)));
        assert!(!recovery.record(start + Duration::from_secs(64)));
        assert!(recovery.record(start + Duration::from_secs(65)));
    }

    /// What a Raycast script sends becomes the same action a menu pick or a
    /// media key would produce.
    #[test]
    fn a_control_command_becomes_the_action_it_names() {
        // #given
        let mut app = headless_app();
        let queue: std::sync::Arc<std::sync::Mutex<Vec<ControlCommand>>> = Default::default();
        app.control_commands = Some(std::sync::Arc::clone(&queue));

        // #when
        queue.lock().expect("the queue").extend([
            ControlCommand::Next,
            ControlCommand::Previous,
            ControlCommand::SeekBy(-15_000),
            ControlCommand::VolumeBy(10),
            ControlCommand::SetVolume(240),
            ControlCommand::ToggleShuffle,
            ControlCommand::Show,
        ]);
        app.handle_control_commands();

        // #then
        assert!(
            matches!(
                app.actions.as_slice(),
                [
                    Action::Next,
                    Action::Previous,
                    Action::SeekBy(-15_000),
                    Action::VolumeBy(10),
                    // A percentage above the scale is clamped, not wrapped.
                    Action::SetVolume(100),
                    Action::ToggleShuffle,
                    Action::ShowWindow,
                ]
            ),
            "{:?}",
            app.actions
        );
        assert!(queue.lock().expect("the queue").is_empty());
    }

    #[test]
    fn expanded_control_commands_reuse_music_and_device_actions() {
        let mut app = headless_app();
        let active = active_local_snapshot(&app, false, Playback::Playing);
        app.handle_local(active);
        let queue: std::sync::Arc<std::sync::Mutex<Vec<ControlCommand>>> = Default::default();
        app.control_commands = Some(std::sync::Arc::clone(&queue));

        queue.lock().expect("the queue").extend([
            ControlCommand::SetShuffle(true),
            ControlCommand::SetRepeat(RepeatMode::Track),
            ControlCommand::SeekTo(90_000),
            ControlCommand::ToggleSaved,
            ControlCommand::PlayUri("spotify:track:one".to_owned()),
            ControlCommand::PlayUri("spotify:album:many".to_owned()),
            ControlCommand::Transfer("speaker".to_owned()),
            ControlCommand::RefreshDevices,
        ]);
        app.handle_control_commands();

        assert!(
            matches!(
                app.actions.as_slice(),
                [
                    Action::SetShuffle(true),
                    Action::SetRepeat(RepeatMode::Track),
                    Action::Seek(90_000),
                    Action::ToggleSaved(uri),
                    Action::PlayUris { uris, index: 0 },
                    Action::PlayContext {
                        uri: context,
                        offset_uri: None,
                        offset_index: None,
                    },
                    Action::Transfer(device),
                    Action::RefreshDevices,
                ] if uri == "spotify:track:active"
                    && uris == &["spotify:track:one"]
                    && context == "spotify:album:many"
                    && device == "speaker"
            ),
            "{:?}",
            app.actions
        );
        assert!(queue.lock().expect("the queue").is_empty());
    }

    #[test]
    fn expanded_controls_cannot_bypass_a_restricted_remote_device() {
        let mut app = headless_app();
        app.local_ready = false;
        let mut state = active_api_snapshot_on(false, "restricted");
        state.device.as_mut().expect("remote device").is_restricted = true;
        deliver_remote_snapshot(&mut app, state);
        app.selected_device = Some("restricted".to_owned());
        app.backend.take_api_requests();
        let queue: std::sync::Arc<std::sync::Mutex<Vec<ControlCommand>>> = Default::default();
        queue.lock().expect("the queue").extend([
            ControlCommand::SeekTo(90_000),
            ControlCommand::SetShuffle(true),
            ControlCommand::SetRepeat(RepeatMode::Track),
            ControlCommand::PlayUri("spotify:track:one".to_owned()),
            ControlCommand::Transfer("restricted".to_owned()),
        ]);
        app.control_commands = Some(queue);

        app.handle_control_commands();
        app.apply_actions(&egui::Context::default());

        assert!(app.backend.take_api_requests().is_empty());
        assert!(!app.shuffle_wanted);
        assert!(app.actions.is_empty());
        assert!(
            app.toasts
                .iter()
                .all(|toast| toast.message.contains("restricted"))
        );
    }

    #[test]
    fn external_like_refuses_an_episode_honestly() {
        let mut app = headless_app();
        app.handle_local(LocalState {
            connected: true,
            playback: Playback::Playing,
            track: Some(crate::player::LocalTrack {
                uri: "spotify:episode:not-music".to_owned(),
                is_episode: true,
                ..crate::player::LocalTrack::default()
            }),
            ..LocalState::default()
        });
        let queue: std::sync::Arc<std::sync::Mutex<Vec<ControlCommand>>> = Default::default();
        queue
            .lock()
            .expect("the queue")
            .push(ControlCommand::ToggleSaved);
        app.control_commands = Some(queue);

        app.handle_control_commands();

        assert!(app.actions.is_empty());
        assert_eq!(
            app.toasts.last().map(|toast| toast.message.as_str()),
            Some("The external like control supports music tracks only")
        );
    }

    #[test]
    fn control_snapshots_append_state_and_publish_device_capabilities() {
        let mut app = headless_app();
        app.handle_local(LocalState {
            connected: true,
            playback: Playback::Playing,
            track: Some(crate::player::LocalTrack {
                uri: "spotify:track:t1".to_owned(),
                title: "Go\tNow\n".to_owned(),
                artists: vec!["The Band".to_owned()],
                album: "First".to_owned(),
                art_url: Some("https://i.scdn.co/image/abc".to_owned()),
                duration_ms: 200_000,
                ..crate::player::LocalTrack::default()
            }),
            position_ms: 20_000,
            volume: percent_to_volume(35),
            shuffle: true,
            repeat: RepeatMode::Track,
            ..LocalState::default()
        });
        app.saved.insert("spotify:track:t1".to_owned(), true);
        app.devices = vec![
            Device {
                id: Some("abc123".to_owned()),
                name: "Kitchen\tspeaker".to_owned(),
                kind: "Speaker".to_owned(),
                is_active: true,
                is_restricted: true,
                supports_volume: Some(false),
                ..Device::default()
            },
            Device {
                id: None,
                name: "Unaddressable".to_owned(),
                ..Device::default()
            },
        ];

        let fields: Vec<_> = app
            .control_snapshot()
            .split('\t')
            .map(str::to_owned)
            .collect();
        let devices: serde_json::Value =
            serde_json::from_str(&app.control_devices_snapshot()).expect("device JSON");

        assert_eq!(
            fields,
            [
                "playing",
                "Go Now ",
                "The Band",
                "First",
                "20000",
                "200000",
                "35",
                "on",
                "track",
                "https://i.scdn.co/image/abc",
                "yes",
                "Fastpotify",
            ]
        );
        assert_eq!(devices.as_array().map(Vec::len), Some(1));
        assert_eq!(devices[0]["id"], "abc123");
        assert_eq!(devices[0]["name"], "Kitchen\tspeaker");
        assert_eq!(devices[0]["active"], true);
        assert_eq!(devices[0]["restricted"], true);
        assert_eq!(devices[0]["supports_volume"], false);
    }

    #[test]
    fn a_device_response_reaches_the_control_snapshot_once() {
        let mut app = headless_app();
        let slot = std::sync::Arc::new(std::sync::Mutex::new(
            crate::single_instance::NO_DEVICES.to_owned(),
        ));
        app.control_devices = Some(std::sync::Arc::clone(&slot));
        app.control_devices_stale = false;

        app.handle_api(ApiResponse::Devices(Ok(vec![Device {
            id: Some("abc123".to_owned()),
            name: "Kitchen".to_owned(),
            kind: "Speaker".to_owned(),
            is_active: true,
            ..Device::default()
        }])));
        app.sync_media_controls();

        let written: serde_json::Value =
            serde_json::from_str(&slot.lock().expect("the slot")).expect("device JSON");
        assert_eq!(written[0]["id"], "abc123");
        assert!(!app.control_devices_stale);
    }

    /// `play` and `pause` say what state to end in, so the one that would
    /// undo the current state does nothing.
    #[test]
    fn play_and_pause_do_not_toggle_the_wrong_way() {
        let mut app = headless_app();
        let queue: std::sync::Arc<std::sync::Mutex<Vec<ControlCommand>>> = Default::default();
        app.control_commands = Some(std::sync::Arc::clone(&queue));

        // Nothing is playing in a headless app, so `pause` has nothing to do
        // and `play` asks for the toggle.
        queue
            .lock()
            .expect("the queue")
            .extend([ControlCommand::Pause, ControlCommand::Play]);
        app.handle_control_commands();

        assert!(
            matches!(app.actions.as_slice(), [Action::TogglePlay]),
            "{:?}",
            app.actions
        );
    }
}
