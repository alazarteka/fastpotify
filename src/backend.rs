//! The bridge between the interface thread and everything asynchronous.
//!
//! egui runs on the main thread and must never block. A dedicated tokio
//! runtime hosts the librespot engine, the Web API client, sign-in, and
//! artwork fetches; the two sides talk through channels. Every event wakes
//! the interface with `request_repaint`, so the app stays event-driven and
//! idle when nothing is happening.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use librespot_core::authentication::Credentials;
use librespot_protocol::authentication::AuthenticationType;
use tokio::sync::{Semaphore, mpsc, watch};

use crate::api::gateway::ApiRoute;
use crate::api::models::*;
use crate::api::{
    AccountId, ApiClient, ApiError, ApiGateway, ApiSource, NetActivity, Operation, PlayRequest,
    PlaylistId, SessionState, TokenProvider, WebTokens,
};
use crate::images::{ArtLoader, accent_color};
use crate::paths::AppDirs;
use crate::player::{Engine, EngineConfig, EngineEvent, LoadSpec, LocalState, PlayerCommand};
use crate::secrets::{PrivateFileStore, SecretId, SecretStore};

pub type ApiResult<T> = Result<T, ApiError>;

const PREMIUM_NEEDED: &str = "Local playback needs Spotify Premium.";

#[derive(Clone, Debug, PartialEq)]
pub enum AuthStatus {
    Starting,
    SignedOut,
    WaitingForBrowser { url: String },
    Connecting,
    Connected { username: String },
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteAction {
    Play,
    Pause,
    Next,
    Previous,
    Seek,
    Volume,
    Shuffle,
    Repeat,
}

#[derive(Clone, Debug)]
pub enum ApiRequest {
    Me,
    Devices,
    PlaybackState {
        seq: u64,
    },
    Queue,
    RecentlyPlayed,
    TopTracks {
        offset: u32,
        full: bool,
    },
    TopArtists,
    Recommendations {
        seed_tracks: Vec<String>,
        seed_artists: Vec<String>,
    },
    Discover {
        term: String,
    },
    MyPlaylists {
        offset: u32,
    },
    Playlist {
        id: String,
    },
    PlaylistItems {
        id: String,
        offset: u32,
    },
    /// A slice of a playlist read only for who added its songs; the rows
    /// on screen stay untouched.
    PlaylistSample {
        id: String,
        offset: u32,
    },
    CreatePlaylist {
        name: String,
        public: bool,
        description: String,
    },
    UpdatePlaylist {
        id: String,
        name: Option<String>,
        description: Option<String>,
        public: Option<bool>,
    },
    AddToPlaylist {
        mutation_id: u64,
        playlist_id: String,
        playlist_name: String,
        uris: Vec<String>,
    },
    RemoveFromPlaylist {
        mutation_id: u64,
        playlist_id: String,
        uris: Vec<String>,
        snapshot_id: Option<String>,
    },
    ReorderPlaylist {
        mutation_id: u64,
        playlist_id: String,
        range_start: u32,
        insert_before: u32,
        snapshot_id: Option<String>,
    },
    FollowPlaylist {
        id: String,
        follow: bool,
    },
    SavedTracks {
        offset: u32,
    },
    SavedAlbums {
        offset: u32,
    },
    FollowedArtists {
        after: Option<String>,
    },
    SavedShows {
        offset: u32,
    },
    SavedEpisodes {
        offset: u32,
    },
    SetSaved {
        uris: Vec<String>,
        saved: bool,
    },
    Contains {
        uris: Vec<String>,
        user_id: String,
    },
    Search {
        query: String,
        serial: u64,
    },
    Artist {
        id: String,
    },
    ArtistTopTracks {
        id: String,
        name: String,
    },
    ArtistAlbums {
        id: String,
        groups: String,
        offset: u32,
    },
    RelatedArtists {
        id: String,
    },
    Album {
        id: String,
    },
    AlbumTracks {
        id: String,
        offset: u32,
    },
    Show {
        id: String,
    },
    ShowEpisodes {
        id: String,
        offset: u32,
    },
    Track {
        id: String,
    },
    Remote {
        action: RemoteAction,
        device_id: Option<String>,
        play: Option<PlayRequest>,
        position_ms: u32,
        percent: u8,
        flag: bool,
        repeat: String,
    },
    Transfer {
        device_id: String,
        play: bool,
    },
    /// Apply shuffle, then play, in one ordered backend operation.
    PlayWithShuffle {
        device_id: Option<String>,
        play: PlayRequest,
        shuffle: bool,
    },
    AddToQueue {
        uri: String,
        device_id: Option<String>,
        label: String,
    },
}

impl ApiRequest {
    fn background(&self) -> bool {
        matches!(
            self,
            Self::PlaybackState { .. }
                | Self::RecentlyPlayed
                | Self::TopTracks { .. }
                | Self::TopArtists
                | Self::Recommendations { .. }
                | Self::Discover { .. }
                | Self::MyPlaylists { .. }
                | Self::PlaylistSample { .. }
                | Self::Contains { .. }
        )
    }

    /// The scheduler resources mutated by this request. An omitted device id
    /// aliases whichever device is active when Spotify executes the request;
    /// transfer additionally changes that global alias and its destination.
    fn playback_mutation_lanes(&self) -> Option<PlaybackMutationLanes> {
        match self {
            Self::Remote { device_id, .. }
            | Self::PlayWithShuffle { device_id, .. }
            | Self::AddToQueue { device_id, .. } => Some(match device_id {
                Some(device_id) => PlaybackMutationLanes::explicit(device_id.clone()),
                None => PlaybackMutationLanes::active(),
            }),
            Self::Transfer { device_id, .. } => {
                Some(PlaybackMutationLanes::transfer(device_id.clone()))
            }
            _ => None,
        }
    }

    fn authoritative_playback_mutation(&self) -> bool {
        match self {
            Self::Remote { action, .. } => {
                !matches!(action, RemoteAction::Next | RemoteAction::Previous)
            }
            Self::Transfer { .. } | Self::PlayWithShuffle { .. } => true,
            Self::AddToQueue { .. } => false,
            _ => false,
        }
    }

    fn playlist_mutation(&self) -> Option<(&str, u64)> {
        match self {
            Self::AddToPlaylist {
                playlist_id,
                mutation_id,
                ..
            }
            | Self::RemoveFromPlaylist {
                playlist_id,
                mutation_id,
                ..
            }
            | Self::ReorderPlaylist {
                playlist_id,
                mutation_id,
                ..
            } => Some((playlist_id, *mutation_id)),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum ApiResponse {
    Me(ApiResult<User>),
    Devices(ApiResult<Vec<Device>>),
    PlaybackState {
        seq: u64,
        result: ApiResult<Option<PlaybackState>>,
    },
    Queue(ApiResult<Queue>),
    RecentlyPlayed(ApiResult<Vec<PlayHistory>>),
    TopTracks {
        offset: u32,
        full: bool,
        result: ApiResult<Page<Track>>,
    },
    TopArtists(ApiResult<Vec<Artist>>),
    Recommendations(ApiResult<Vec<Track>>),
    Discover {
        term: String,
        result: ApiResult<Vec<Playlist>>,
    },
    MyPlaylists {
        offset: u32,
        result: ApiResult<Page<Playlist>>,
    },
    Playlist {
        id: String,
        result: ApiResult<Playlist>,
    },
    PlaylistItems {
        id: String,
        offset: u32,
        result: ApiResult<Page<PlaylistItem>>,
    },
    PlaylistSample {
        id: String,
        result: ApiResult<Page<PlaylistItem>>,
    },
    PlaylistCreated(ApiResult<Playlist>),
    PlaylistUpdated {
        id: String,
        result: ApiResult<()>,
    },
    PlaylistItemsChanged {
        mutation_id: u64,
        id: String,
        message: String,
        result: ApiResult<Option<String>>,
    },
    PlaylistFollowChanged {
        id: String,
        followed: bool,
        result: ApiResult<()>,
    },
    SavedTracks {
        offset: u32,
        result: ApiResult<Page<SavedTrack>>,
    },
    SavedAlbums {
        offset: u32,
        result: ApiResult<Page<SavedAlbum>>,
    },
    FollowedArtists {
        after: Option<String>,
        result: ApiResult<CursorPage<Artist>>,
    },
    SavedShows {
        offset: u32,
        result: ApiResult<Page<SavedShow>>,
    },
    SavedEpisodes {
        offset: u32,
        result: ApiResult<Page<SavedEpisode>>,
    },
    SavedChanged {
        uris: Vec<String>,
        saved: bool,
        result: ApiResult<()>,
    },
    Contains {
        uris: Vec<String>,
        result: ApiResult<Vec<bool>>,
    },
    Search {
        query: String,
        serial: u64,
        result: ApiResult<SearchResults>,
    },
    Artist {
        id: String,
        result: ApiResult<Artist>,
    },
    ArtistTopTracks {
        id: String,
        result: ApiResult<Vec<Track>>,
    },
    ArtistAlbums {
        id: String,
        groups: String,
        offset: u32,
        result: ApiResult<Page<Album>>,
    },
    RelatedArtists {
        id: String,
        result: ApiResult<Vec<Artist>>,
    },
    Album {
        id: String,
        result: ApiResult<Album>,
    },
    AlbumTracks {
        id: String,
        offset: u32,
        result: ApiResult<Page<Track>>,
    },
    Show {
        id: String,
        result: ApiResult<Show>,
    },
    ShowEpisodes {
        id: String,
        offset: u32,
        result: ApiResult<Page<Episode>>,
    },
    Track {
        id: String,
        result: ApiResult<Track>,
    },
    Remote {
        action: RemoteAction,
        result: ApiResult<()>,
    },
    Transferred {
        device_id: String,
        result: ApiResult<()>,
    },
    QueueAdded {
        label: String,
        result: ApiResult<()>,
    },
}

impl ApiResponse {
    fn error(&self) -> Option<&ApiError> {
        match self {
            Self::Me(Err(error))
            | Self::Devices(Err(error))
            | Self::Queue(Err(error))
            | Self::RecentlyPlayed(Err(error))
            | Self::TopArtists(Err(error))
            | Self::Recommendations(Err(error))
            | Self::PlaylistCreated(Err(error))
            | Self::PlaybackState {
                result: Err(error), ..
            }
            | Self::TopTracks {
                result: Err(error), ..
            }
            | Self::Discover {
                result: Err(error), ..
            }
            | Self::MyPlaylists {
                result: Err(error), ..
            }
            | Self::Playlist {
                result: Err(error), ..
            }
            | Self::PlaylistItems {
                result: Err(error), ..
            }
            | Self::PlaylistSample {
                result: Err(error), ..
            }
            | Self::PlaylistUpdated {
                result: Err(error), ..
            }
            | Self::PlaylistItemsChanged {
                result: Err(error), ..
            }
            | Self::PlaylistFollowChanged {
                result: Err(error), ..
            }
            | Self::SavedTracks {
                result: Err(error), ..
            }
            | Self::SavedAlbums {
                result: Err(error), ..
            }
            | Self::FollowedArtists {
                result: Err(error), ..
            }
            | Self::SavedShows {
                result: Err(error), ..
            }
            | Self::SavedEpisodes {
                result: Err(error), ..
            }
            | Self::SavedChanged {
                result: Err(error), ..
            }
            | Self::Contains {
                result: Err(error), ..
            }
            | Self::Search {
                result: Err(error), ..
            }
            | Self::Artist {
                result: Err(error), ..
            }
            | Self::ArtistTopTracks {
                result: Err(error), ..
            }
            | Self::ArtistAlbums {
                result: Err(error), ..
            }
            | Self::RelatedArtists {
                result: Err(error), ..
            }
            | Self::Album {
                result: Err(error), ..
            }
            | Self::AlbumTracks {
                result: Err(error), ..
            }
            | Self::Show {
                result: Err(error), ..
            }
            | Self::ShowEpisodes {
                result: Err(error), ..
            }
            | Self::Track {
                result: Err(error), ..
            }
            | Self::Remote {
                result: Err(error), ..
            }
            | Self::Transferred {
                result: Err(error), ..
            }
            | Self::QueueAdded {
                result: Err(error), ..
            } => Some(error),
            _ => None,
        }
    }
}

pub enum Command {
    /// Start (or restart) the Web API sign-in in the browser.
    SignIn,
    CancelSignIn,
    SignOut,
    /// Authorize local playback on this computer (a separate browser grant).
    AuthorizePlayback,
    /// Reload the engine config (audio settings changed).
    RestartEngine(EngineConfig),
    Player(PlayerCommand),
    Api(ApiRequest),
    Accent {
        url: String,
    },
    Shutdown,
    /// Internal: the Web API browser flow produced a grant.
    WebSignedIn {
        source: ApiSource,
        token: Box<crate::auth::StoredToken>,
        epoch: u64,
    },
    WebSignInFailed {
        source: ApiSource,
        message: Option<String>,
        epoch: u64,
        interactive: bool,
        expired: bool,
        generation: Option<u64>,
    },
    /// Internal: one Web API grant passed the canonical account check.
    WebVerified {
        source: ApiSource,
        token: Box<crate::auth::StoredToken>,
        user: Box<User>,
        epoch: u64,
        interactive: bool,
        generation: u64,
    },
    /// Internal: only the session named here has expired.
    WebSessionExpired {
        source: ApiSource,
        generation: u64,
    },
    /// Internal: the Web API said which plan the account is on (`None` when
    /// it could not tell).
    AccountChecked {
        premium: Option<bool>,
    },
    /// Internal: the playback grant produced a streaming access token.
    PlaybackAuthorized {
        access_token: String,
        epoch: u64,
    },
    PlaybackAuthorizationFailed {
        message: Option<String>,
        epoch: u64,
    },
    /// Internal: an engine connection attempt finished.
    EngineConnected {
        engine: Box<Option<Engine>>,
        credential: Option<Credentials>,
        generation: u64,
        epoch: u64,
        error: Option<String>,
    },
    /// Ask the current local engine to reconnect.
    Reconnect,
    /// Internal: one particular librespot session ended on its own.
    EngineEnded {
        generation: u64,
    },
    /// Ask GitHub whether a newer release exists.
    CheckForUpdates,
    /// The words of a track, from LRCLIB.
    Lyrics(Box<LyricsRequest>),
    /// Add, replace, or remove the optional personal Web API application.
    ConfigurePersonalWebApp(Option<String>),
    /// Read a playlist's cached items from disk.
    LoadPlaylistCache {
        id: String,
    },
    /// Remember a fully loaded playlist on disk under its snapshot.
    StorePlaylistCache {
        id: String,
        snapshot: String,
        items: Vec<PlaylistItem>,
    },
    /// Resolve user ids to display names through the streaming session.
    UserNames(Vec<String>),
}

pub struct LyricsRequest {
    /// The track the answer is for, so a stale one is ignored.
    pub uri: String,
    pub query: crate::lyrics::Query,
    pub allow_lrclib: bool,
}

pub enum Event {
    Auth(AuthStatus),
    Playback(LocalPlayback),
    Local(Box<LocalState>),
    Api(Box<ApiResponse>),
    Accent {
        url: String,
        color: [u8; 3],
    },
    Error(String),
    /// A newer release than this build exists.
    UpdateAvailable {
        version: String,
        url: String,
    },
    /// The words of a track, or `None` when nobody has transcribed it.
    Lyrics {
        uri: String,
        allow_lrclib: bool,
        result: Result<Option<crate::lyrics::Lyrics>, String>,
    },
    /// A playlist's items as last cached, with the snapshot they belong to.
    PlaylistCache {
        id: String,
        snapshot: String,
        items: Vec<PlaylistItem>,
    },
    /// A user id resolved to a display name (`None` when nothing answers).
    UserName {
        id: String,
        name: Option<String>,
    },
    /// The optional personal Web API application currently ready for routing.
    WebApp {
        client_id: Option<String>,
    },
}

/// The state of playback on this computer, independent of Web API sign-in.
#[derive(Clone, Debug, PartialEq)]
pub enum LocalPlayback {
    /// Not authorized; local playback is unavailable but the app still works.
    Unavailable,
    /// The browser is open for the playback grant.
    Authorizing,
    /// Connecting the librespot engine.
    Connecting,
    /// This computer is a ready Spotify Connect device.
    Ready {
        device_id: String,
    },
    Failed(String),
}

/// Wakes whichever window currently exists, if any.
///
/// Background services (the runtime, MPRIS, the tray) outlive individual
/// windows: the window is destroyed when it closes to the tray and created
/// again on demand. They therefore hold this handle instead of an
/// `egui::Context`.
#[derive(Clone, Default)]
pub struct Waker(Arc<std::sync::Mutex<Option<egui::Context>>>);

impl Waker {
    pub fn attach(&self, ctx: &egui::Context) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(ctx.clone());
    }

    pub fn detach(&self) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    pub fn wake(&self) {
        if let Some(ctx) = self.0.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            ctx.request_repaint();
        }
    }
}

/// Keeps callbacks from an engine that has been replaced from publishing
/// state behind its successor. The mutex orders retirement with channel send,
/// so a retired callback is either fully delivered first or discarded.
#[derive(Clone, Default)]
struct EngineNotificationGuard(Arc<std::sync::Mutex<u64>>);

impl EngineNotificationGuard {
    fn notifier(
        &self,
        events: std::sync::mpsc::Sender<Event>,
        commands: mpsc::UnboundedSender<Command>,
        waker: Waker,
    ) -> (u64, crate::player::Notify) {
        let generation = {
            let mut current = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
            *current = current.wrapping_add(1);
            *current
        };
        let active = self.clone();
        let notify = Arc::new(move |event| {
            let current = active.0.lock().unwrap_or_else(|poison| poison.into_inner());
            if *current != generation {
                return;
            }
            match event {
                EngineEvent::State(state) => {
                    let _ = events.send(Event::Local(Box::new(state)));
                    waker.wake();
                }
                EngineEvent::SessionEnded => {
                    let _ = commands.send(Command::EngineEnded { generation });
                }
            }
            drop(current);
        });
        (generation, notify)
    }

    fn retire(&self) {
        let mut current = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        *current = current.wrapping_add(1);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum EngineConnectionLifecycle {
    #[default]
    Idle,
    Connecting {
        generation: u64,
        restart_after_completion: bool,
    },
    Active {
        generation: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestartDisposition {
    Ignore,
    Deferred,
    Now,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompletionDisposition {
    Ignore,
    PublishReady,
    Restart,
}

impl EngineConnectionLifecycle {
    fn is_idle(self) -> bool {
        matches!(self, Self::Idle)
    }

    fn begin(&mut self, generation: u64) {
        debug_assert!(matches!(self, Self::Idle));
        *self = Self::Connecting {
            generation,
            restart_after_completion: false,
        };
    }

    fn reconnect_requested(&mut self) -> RestartDisposition {
        match *self {
            Self::Connecting { .. } => RestartDisposition::Ignore,
            Self::Active { .. } => {
                *self = Self::Idle;
                RestartDisposition::Now
            }
            Self::Idle => RestartDisposition::Now,
        }
    }

    fn session_ended(&mut self, generation: u64) -> RestartDisposition {
        match *self {
            Self::Connecting {
                generation: current,
                restart_after_completion,
            } if generation == current => {
                if restart_after_completion {
                    RestartDisposition::Ignore
                } else {
                    *self = Self::Connecting {
                        generation: current,
                        restart_after_completion: true,
                    };
                    RestartDisposition::Deferred
                }
            }
            Self::Active {
                generation: current,
            } if generation == current => {
                *self = Self::Idle;
                RestartDisposition::Now
            }
            _ => RestartDisposition::Ignore,
        }
    }

    fn engine_connected(&mut self, generation: u64) -> CompletionDisposition {
        let Self::Connecting {
            generation: current,
            restart_after_completion,
        } = *self
        else {
            return CompletionDisposition::Ignore;
        };
        if generation != current {
            return CompletionDisposition::Ignore;
        }
        if restart_after_completion {
            *self = Self::Idle;
            CompletionDisposition::Restart
        } else {
            *self = Self::Active { generation };
            CompletionDisposition::PublishReady
        }
    }

    fn retire(&mut self, generation: u64) {
        match *self {
            Self::Connecting {
                generation: current,
                ..
            }
            | Self::Active {
                generation: current,
            } if current == generation => *self = Self::Idle,
            _ => {}
        }
    }

    fn reset(&mut self) {
        *self = Self::Idle;
    }
}

#[cfg(test)]
mod engine_lifecycle_tests {
    use super::*;

    #[test]
    fn retired_engine_cannot_publish_behind_its_successor() {
        let guard = EngineNotificationGuard::default();
        let (events, event_rx) = std::sync::mpsc::channel();
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let (_, retired) = guard.notifier(events.clone(), commands.clone(), Waker::default());
        let (_, current) = guard.notifier(events, commands, Waker::default());

        current(EngineEvent::State(LocalState {
            connected: true,
            ..LocalState::default()
        }));
        retired(EngineEvent::State(LocalState::default()));
        retired(EngineEvent::SessionEnded);

        let Event::Local(state) = event_rx.try_recv().expect("current engine state") else {
            panic!("the current notifier published an unexpected event");
        };
        assert!(state.connected);
        assert!(matches!(
            event_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert!(matches!(
            command_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn ended_connecting_candidate_restarts_once_without_becoming_ready() {
        let guard = EngineNotificationGuard::default();
        let (events, _event_rx) = std::sync::mpsc::channel();
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let (generation, notify) =
            guard.notifier(events.clone(), commands.clone(), Waker::default());
        let mut lifecycle = EngineConnectionLifecycle::default();
        lifecycle.begin(generation);

        notify(EngineEvent::SessionEnded);
        let ended_generation = match command_rx.try_recv() {
            Ok(Command::EngineEnded { generation }) => generation,
            _ => panic!("the session ending did not identify its engine generation"),
        };

        assert_eq!(
            lifecycle.session_ended(ended_generation),
            RestartDisposition::Deferred
        );
        guard.retire();
        assert_eq!(
            lifecycle.session_ended(ended_generation),
            RestartDisposition::Ignore
        );
        assert_eq!(
            lifecycle.engine_connected(generation),
            CompletionDisposition::Restart
        );
        assert_eq!(
            lifecycle.engine_connected(generation),
            CompletionDisposition::Ignore
        );

        let (successor, _successor_notify) = guard.notifier(events, commands, Waker::default());
        lifecycle.begin(successor);
        assert_eq!(
            lifecycle.session_ended(generation),
            RestartDisposition::Ignore
        );
        assert_eq!(
            lifecycle.engine_connected(generation),
            CompletionDisposition::Ignore
        );
        assert_eq!(
            lifecycle.engine_connected(successor),
            CompletionDisposition::PublishReady
        );
    }
}

/// The interface's handle to the runtime.
pub struct Backend {
    commands: mpsc::UnboundedSender<Command>,
    events: std::sync::mpsc::Receiver<Event>,
    art: ArtLoader,
    activity: Arc<NetActivity>,
    thread: Option<std::thread::JoinHandle<()>>,
    offline: bool,
    #[cfg(test)]
    api_requests: std::sync::Mutex<Vec<ApiRequest>>,
    #[cfg(test)]
    player_commands: std::sync::Mutex<Vec<PlayerCommand>>,
}

impl Backend {
    pub fn spawn(
        dirs: AppDirs,
        engine_config: EngineConfig,
        web_client_id: Option<String>,
        waker: Waker,
    ) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("fastpotify-runtime")
            .enable_all()
            .build()
            .expect("unable to start the async runtime");
        let http = reqwest::Client::builder()
            .user_agent(concat!("fastpotify/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("unable to build the HTTP client");
        let art = ArtLoader::new(http.clone(), runtime.handle().clone(), dirs.art_cache_dir());
        let activity = Arc::new(NetActivity::default());

        let worker_activity = Arc::clone(&activity);
        let worker_art = art.clone();
        let worker_commands = command_tx.clone();
        let thread = std::thread::Builder::new()
            .name("fastpotify-backend".to_string())
            .spawn(move || {
                runtime.block_on(async move {
                    let mut worker = Worker::new(
                        dirs,
                        engine_config,
                        web_client_id,
                        http,
                        worker_art,
                        worker_activity,
                        event_tx,
                        worker_commands,
                        waker,
                    );
                    worker.run(command_rx).await;
                });
                // Give librespot's own threads a moment to release the audio device.
                runtime.shutdown_timeout(Duration::from_secs(2));
            })
            .expect("unable to start the backend thread");

        Self {
            commands: command_tx,
            events: event_rx,
            art,
            activity,
            thread: Some(thread),
            offline: false,
            #[cfg(test)]
            api_requests: std::sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            player_commands: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Live network activity, for the interface's busy indicator.
    pub fn activity(&self) -> &NetActivity {
        &self.activity
    }

    /// Stops Spotify-bound commands from leaving the process; artwork and
    /// shutdown still work. Used by the demo mode and by headless tests.
    #[cfg_attr(not(any(test, feature = "demo")), allow(dead_code))]
    pub fn set_offline(&mut self, offline: bool) {
        self.offline = offline;
    }

    pub fn send(&self, command: Command) {
        if self.offline && !matches!(command, Command::Accent { .. } | Command::Shutdown) {
            return;
        }
        let _ = self.commands.send(command);
    }

    pub fn api(&self, request: ApiRequest) {
        #[cfg(test)]
        self.api_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(request.clone());
        self.send(Command::Api(request));
    }

    #[cfg(test)]
    pub(crate) fn take_api_requests(&self) -> Vec<ApiRequest> {
        std::mem::take(
            &mut *self
                .api_requests
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        )
    }

    pub fn player(&self, command: PlayerCommand) {
        #[cfg(test)]
        self.player_commands
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(command.clone());
        self.send(Command::Player(command));
    }

    #[cfg(test)]
    pub(crate) fn take_player_commands(&self) -> Vec<PlayerCommand> {
        std::mem::take(
            &mut *self
                .player_commands
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        )
    }

    pub fn poll(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }

    pub fn art(&self) -> &ArtLoader {
        &self.art
    }

    pub fn shutdown(&mut self) {
        self.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct ActiveAuthorization {
    kind: AuthorizationKind,
    epoch: u64,
    cancel: watch::Sender<bool>,
    cancelled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuthorizationKind {
    Web(ApiSource),
    Playback,
}

#[derive(Default)]
struct AuthorizationLifecycle {
    active: Option<ActiveAuthorization>,
    pending_web: Option<ApiSource>,
}

#[derive(Debug, PartialEq, Eq)]
enum AuthorizationCompletion {
    Current,
    Ignore,
    StartPendingWeb(ApiSource),
}

impl AuthorizationLifecycle {
    fn is_active(&self) -> bool {
        self.active.is_some()
    }

    fn begin(&mut self, kind: AuthorizationKind, cancel: watch::Sender<bool>, epoch: u64) {
        debug_assert!(self.active.is_none());
        self.pending_web = None;
        self.active = Some(ActiveAuthorization {
            kind,
            epoch,
            cancel,
            cancelled: false,
        });
    }

    /// A request matching a live flow is a duplicate. A request made after
    /// cancellation, or from a newer epoch, must wait until that task drops
    /// its prepared listener.
    fn defer_web_request(&mut self, source: ApiSource, current_epoch: u64) -> bool {
        let Some(active) = &self.active else {
            return false;
        };
        if active.kind != AuthorizationKind::Web(source)
            || active.epoch != current_epoch
            || active.cancelled
        {
            self.pending_web = Some(source);
        }
        true
    }

    fn cancel(&mut self) {
        self.pending_web = None;
        if let Some(active) = &mut self.active {
            active.cancelled = true;
            let _ = active.cancel.send(true);
        }
    }

    fn finish(
        &mut self,
        kind: AuthorizationKind,
        epoch: u64,
        current_epoch: u64,
    ) -> AuthorizationCompletion {
        if !self
            .active
            .as_ref()
            .is_some_and(|active| active.kind == kind && active.epoch == epoch)
        {
            return AuthorizationCompletion::Ignore;
        }
        let active = self.active.take().expect("matching authorization exists");
        if let Some(source) = self.pending_web.take() {
            AuthorizationCompletion::StartPendingWeb(source)
        } else if epoch == current_epoch && !active.cancelled {
            AuthorizationCompletion::Current
        } else {
            AuthorizationCompletion::Ignore
        }
    }
}

#[cfg(test)]
mod authorization_lifecycle_tests {
    use super::*;

    #[test]
    fn repeated_web_replacements_wait_for_the_listener_owner() {
        let mut lifecycle = AuthorizationLifecycle::default();
        let (cancel, cancel_rx) = watch::channel(false);
        lifecycle.begin(AuthorizationKind::Web(ApiSource::Shared), cancel, 4);

        lifecycle.cancel();
        assert!(lifecycle.defer_web_request(ApiSource::Personal, 5));
        lifecycle.cancel();
        assert!(lifecycle.defer_web_request(ApiSource::Personal, 6));

        assert!(*cancel_rx.borrow());
        assert_eq!(
            lifecycle.finish(AuthorizationKind::Web(ApiSource::Shared), 4, 6),
            AuthorizationCompletion::StartPendingWeb(ApiSource::Personal)
        );
        assert!(!lifecycle.is_active());
        assert!(!lifecycle.defer_web_request(ApiSource::Personal, 6));
    }

    #[test]
    fn sign_in_requested_after_cancel_is_not_lost() {
        let mut lifecycle = AuthorizationLifecycle::default();
        let (cancel, cancel_rx) = watch::channel(false);
        lifecycle.begin(AuthorizationKind::Web(ApiSource::Shared), cancel, 9);

        lifecycle.cancel();
        assert!(lifecycle.defer_web_request(ApiSource::Shared, 9));

        assert!(*cancel_rx.borrow());
        assert_eq!(
            lifecycle.finish(AuthorizationKind::Web(ApiSource::Shared), 9, 9),
            AuthorizationCompletion::StartPendingWeb(ApiSource::Shared)
        );
    }

    #[test]
    fn cancelled_completion_cannot_become_current() {
        let mut lifecycle = AuthorizationLifecycle::default();
        let (cancel, _cancel_rx) = watch::channel(false);
        lifecycle.begin(AuthorizationKind::Playback, cancel, 12);
        lifecycle.cancel();

        assert_eq!(
            lifecycle.finish(AuthorizationKind::Playback, 12, 12),
            AuthorizationCompletion::Ignore
        );
    }
}

#[cfg(test)]
mod api_routing_tests {
    use super::*;

    fn gateway() -> ApiGateway {
        ApiGateway::new(reqwest::Client::new(), Arc::new(NetActivity::default()))
    }

    #[test]
    fn requests_map_to_the_audited_capability_matrix() {
        let api = gateway();
        assert_eq!(
            operation_for(&api, &ApiRequest::Me),
            Operation::CanonicalAccount
        );
        assert_eq!(
            operation_for(&api, &ApiRequest::Devices),
            Operation::Playback
        );
        assert_eq!(
            operation_for(
                &api,
                &ApiRequest::Contains {
                    uris: vec!["spotify:track:t".into()],
                    user_id: "me".into(),
                },
            ),
            Operation::UserData
        );
        assert_eq!(
            operation_for(
                &api,
                &ApiRequest::Contains {
                    uris: vec!["spotify:playlist:p".into()],
                    user_id: "me".into(),
                },
            ),
            Operation::UnsupportedDevelopmentMode
        );
        assert_eq!(
            operation_for(
                &api,
                &ApiRequest::PlaylistItems {
                    id: "unknown".into(),
                    offset: 0,
                },
            ),
            Operation::PlaylistItems(crate::api::PlaylistAccess::Unknown)
        );
        assert_eq!(
            operation_for(
                &api,
                &ApiRequest::Search {
                    query: "music".into(),
                    serial: 1,
                },
            ),
            Operation::PlaylistSearch
        );
        assert_eq!(
            operation_for(
                &api,
                &ApiRequest::ArtistTopTracks {
                    id: "artist".into(),
                    name: "Artist".into(),
                },
            ),
            Operation::UnsupportedDevelopmentMode
        );
    }

    #[tokio::test]
    async fn playlist_creation_uses_the_personal_me_endpoint() {
        use crate::api::test_support::{read_request, write_response};
        use std::net::{Ipv4Addr, TcpListener};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("loopback listener");
        let port = listener.local_addr().expect("listener address").port();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("playlist request");
            let request = read_request(&stream);
            write_response(
                stream,
                "200 OK",
                &[],
                r#"{"id":"created","name":"Road mix"}"#,
            );
            request
        });
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("HTTP client");
        let api = ApiGateway::new_at(
            http,
            Arc::new(NetActivity::default()),
            &format!("http://127.0.0.1:{port}/v1"),
        );
        let shared = api.begin_verification(ApiSource::Shared, |_| {
            TokenProvider::Fixed("shared-token".into())
        });
        api.install(ApiSource::Shared, shared, AccountId::new("same"))
            .unwrap();
        let personal = api.begin_verification(ApiSource::Personal, |_| {
            TokenProvider::Fixed("personal-token".into())
        });
        api.install(ApiSource::Personal, personal, AccountId::new("same"))
            .unwrap();

        let response = handle(
            &api,
            ApiRequest::CreatePlaylist {
                name: "Road mix".into(),
                public: false,
                description: "For later".into(),
            },
        )
        .await;

        assert!(matches!(
            response,
            ApiResponse::PlaylistCreated(Ok(Playlist { ref id, .. })) if id == "created"
        ));
        let request = server.join().expect("server exits");
        assert_eq!(request.request_line, "POST /v1/me/playlists HTTP/1.1");
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer personal-token")
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request.body).expect("playlist JSON"),
            serde_json::json!({
                "name": "Road mix",
                "public": false,
                "description": "For later",
            })
        );
    }
}

#[cfg(test)]
mod web_token_migration_tests {
    use super::*;

    fn token(client_id: &str, access: &str) -> crate::auth::StoredToken {
        crate::auth::StoredToken {
            client_id: client_id.into(),
            access_token: access.into(),
            refresh_token: format!("refresh-{access}"),
            expires_at: u64::MAX,
            scope: crate::auth::WEB_SCOPES.join(" "),
        }
    }

    fn paths(name: &str) -> (std::path::PathBuf, crate::secrets::LegacySecret) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "fastpotify-dual-api-{name}-{}-{nonce}",
            std::process::id()
        ));
        let legacy = crate::secrets::LegacySecret::new(root.join("legacy.json"));
        (root, legacy)
    }

    #[test]
    fn previous_personal_grant_moves_only_after_verified_storage() {
        let (root, legacy_path) = paths("move");
        let store = PrivateFileStore::new(root.join("secrets"));
        let previous = token("0123456789abcdef0123456789abcdef", "old");
        crate::secrets::store_json(&store, SecretId::WebApi, &previous).unwrap();

        let (shared, personal) = Worker::load_saved_web_tokens(&store, &legacy_path).unwrap();

        assert!(shared.is_none());
        assert!(personal.as_ref() == Some(&previous));
        assert!(
            crate::secrets::load_json::<crate::auth::StoredToken>(&store, SecretId::WebApi)
                .unwrap()
                .is_none()
        );
        assert!(
            crate::secrets::load_json::<crate::auth::StoredToken>(&store, SecretId::PersonalWebApi)
                .unwrap()
                == Some(previous)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn conflicting_personal_grants_are_both_preserved() {
        let (root, legacy_path) = paths("conflict");
        let store = PrivateFileStore::new(root.join("secrets"));
        let previous = token("0123456789abcdef0123456789abcdef", "old");
        let current = token("fedcba9876543210fedcba9876543210", "new");
        crate::secrets::store_json(&store, SecretId::WebApi, &previous).unwrap();
        crate::secrets::store_json(&store, SecretId::PersonalWebApi, &current).unwrap();

        let result = Worker::load_saved_web_tokens(&store, &legacy_path);

        assert!(matches!(
            result,
            Err(crate::secrets::SecretError::MigrationConflict { .. })
        ));
        assert!(
            crate::secrets::load_json::<crate::auth::StoredToken>(&store, SecretId::WebApi)
                .unwrap()
                == Some(previous)
        );
        assert!(
            crate::secrets::load_json::<crate::auth::StoredToken>(&store, SecretId::PersonalWebApi)
                .unwrap()
                == Some(current)
        );
        let _ = std::fs::remove_dir_all(root);
    }
}

struct Worker {
    dirs: AppDirs,
    engine_config: EngineConfig,
    web_client_id: Option<String>,
    http: reqwest::Client,
    api: Arc<ApiGateway>,
    background_api: Arc<Semaphore>,
    api_mutations: Arc<ApiMutationScheduler>,
    art: ArtLoader,
    events: std::sync::mpsc::Sender<Event>,
    commands: mpsc::UnboundedSender<Command>,
    waker: Waker,
    engine_notifications: EngineNotificationGuard,
    engine_connection: EngineConnectionLifecycle,
    engine: Option<Arc<Engine>>,
    secrets: Arc<dyn SecretStore>,
    pending_personal_token: Option<crate::auth::StoredToken>,
    signed_in: bool,
    /// The plan, once the Web API has answered.
    premium: Option<bool>,
    authorization: AuthorizationLifecycle,
    reconnects: Vec<Instant>,
    pending_resume: Option<LoadSpec>,
    /// Invalidates browser flows that finish after cancellation by sign-out
    /// or application switching.
    auth_epoch: u64,
    /// Invalidates playback handshakes that finish after sign-out.
    credential_epoch: u64,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    fn new(
        dirs: AppDirs,
        engine_config: EngineConfig,
        web_client_id: Option<String>,
        http: reqwest::Client,
        art: ArtLoader,
        activity: Arc<NetActivity>,
        events: std::sync::mpsc::Sender<Event>,
        commands: mpsc::UnboundedSender<Command>,
        waker: Waker,
    ) -> Self {
        let secrets: Arc<dyn SecretStore> = Arc::new(PrivateFileStore::new(dirs.secrets_dir()));
        Self {
            dirs,
            engine_config,
            web_client_id,
            api: Arc::new(ApiGateway::new(http.clone(), activity)),
            background_api: Arc::new(Semaphore::new(4)),
            api_mutations: Arc::new(ApiMutationScheduler::default()),
            http,
            art,
            events,
            commands,
            waker,
            engine_notifications: EngineNotificationGuard::default(),
            engine_connection: EngineConnectionLifecycle::default(),
            engine: None,
            secrets,
            pending_personal_token: None,
            signed_in: false,
            premium: None,
            authorization: AuthorizationLifecycle::default(),
            reconnects: Vec::new(),
            pending_resume: None,
            auth_epoch: 0,
            credential_epoch: 0,
        }
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
        self.waker.wake();
    }

    async fn run(&mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        self.restore_session();
        while let Some(command) = commands.recv().await {
            match command {
                Command::Shutdown => break,
                Command::SignIn => self.sign_in(),
                Command::CancelSignIn => self.authorization.cancel(),
                Command::SignOut => self.sign_out(),
                Command::AuthorizePlayback => {
                    self.reconnects.clear();
                    self.authorize_playback();
                }
                Command::RestartEngine(config) => {
                    self.engine_config = config;
                    self.reconnects.clear();
                    self.reconnect_engine();
                }
                Command::Player(command) => match &self.engine {
                    Some(engine) => {
                        if let Err(error) = engine.command(command) {
                            self.emit(Event::Error(format!("Playback error: {error}")));
                        }
                    }
                    None => self.emit(Event::Error(
                        "Local playback isn't set up on this computer yet".into(),
                    )),
                },
                Command::Api(request) => self.dispatch(request),
                Command::Accent { url } => self.accent(url),
                Command::WebSignedIn {
                    source,
                    token,
                    epoch,
                } => self.on_web_signed_in(source, *token, epoch),
                Command::WebSignInFailed {
                    source,
                    message,
                    epoch,
                    interactive,
                    expired,
                    generation,
                } => self.on_web_sign_in_failed(
                    source,
                    message,
                    epoch,
                    interactive,
                    expired,
                    generation,
                ),
                Command::WebVerified {
                    source,
                    token,
                    user,
                    epoch,
                    interactive,
                    generation,
                } => self.on_web_verified(source, *token, *user, epoch, interactive, generation),
                Command::WebSessionExpired { source, generation } => {
                    self.on_web_session_expired(source, generation)
                }
                Command::PlaybackAuthorized {
                    access_token,
                    epoch,
                } => {
                    if !self.discard_stale_authorization(AuthorizationKind::Playback, epoch) {
                        self.connect_engine(Credentials::with_access_token(access_token));
                    }
                }
                Command::PlaybackAuthorizationFailed { message, epoch } => {
                    self.on_playback_authorization_failed(message, epoch)
                }
                Command::EngineConnected {
                    engine,
                    credential,
                    generation,
                    epoch,
                    error,
                } => self.on_engine_connected(*engine, credential, generation, epoch, error),
                Command::AccountChecked { premium } => self.on_account_checked(premium),
                Command::Reconnect => self.reconnect_engine(),
                Command::EngineEnded { generation } => self.on_engine_ended(generation),
                Command::CheckForUpdates => self.check_for_updates(),
                Command::Lyrics(request) => self.fetch_lyrics(*request),
                Command::LoadPlaylistCache { id } => self.load_playlist_cache(id),
                Command::StorePlaylistCache {
                    id,
                    snapshot,
                    items,
                } => self.store_playlist_cache(id, snapshot, items),
                Command::UserNames(ids) => self.fetch_user_names(ids),
                Command::ConfigurePersonalWebApp(client_id) => {
                    self.configure_personal_web_app(client_id)
                }
            }
            self.api_mutations.retire_stale(&self.api);
        }
        self.api_mutations.retire_all();
        self.engine_connection.reset();
        self.engine_notifications.retire();
        if let Some(engine) = self.engine.take() {
            engine.shutdown();
        }
    }

    // ---- Web API sign-in --------------------------------------------------

    fn token_secret(source: ApiSource) -> SecretId {
        match source {
            ApiSource::Shared => SecretId::WebApi,
            ApiSource::Personal => SecretId::PersonalWebApi,
        }
    }

    fn validate_saved_token(
        token: &crate::auth::StoredToken,
        id: SecretId,
    ) -> crate::secrets::Result<()> {
        token
            .validate()
            .map_err(|error| crate::secrets::SecretError::Corrupt {
                kind: id.label(),
                reason: error.to_string(),
            })
    }

    /// Split a pre-existing single Web grant by its application identity.
    /// The personal copy is durably written and read back before the old item
    /// is deleted, so an interrupted migration remains recoverable.
    fn load_saved_web_tokens(
        store: &dyn SecretStore,
        legacy: &crate::secrets::LegacySecret,
    ) -> crate::secrets::Result<(
        Option<crate::auth::StoredToken>,
        Option<crate::auth::StoredToken>,
    )> {
        let mut shared = crate::secrets::load_json_migrating_validated::<crate::auth::StoredToken>(
            store,
            SecretId::WebApi,
            legacy,
            |token| Self::validate_saved_token(token, SecretId::WebApi),
        )?;
        let mut personal =
            crate::secrets::load_json::<crate::auth::StoredToken>(store, SecretId::PersonalWebApi)?;
        if let Some(token) = &personal {
            Self::validate_saved_token(token, SecretId::PersonalWebApi)?;
        }

        if shared
            .as_ref()
            .is_some_and(|token| token.client_id != crate::auth::DEFAULT_WEB_CLIENT_ID)
        {
            let previous = shared.take().expect("non-shared grant exists");
            match &personal {
                Some(current) if current != &previous => {
                    return Err(crate::secrets::SecretError::MigrationConflict {
                        kind: SecretId::PersonalWebApi.label(),
                    });
                }
                Some(_) => {}
                None => {
                    crate::secrets::store_json(store, SecretId::PersonalWebApi, &previous)?;
                    let verified = crate::secrets::load_json::<crate::auth::StoredToken>(
                        store,
                        SecretId::PersonalWebApi,
                    )?
                    .ok_or(crate::secrets::SecretError::Verification {
                        kind: SecretId::PersonalWebApi.label(),
                    })?;
                    if verified != previous {
                        return Err(crate::secrets::SecretError::Verification {
                            kind: SecretId::PersonalWebApi.label(),
                        });
                    }
                }
            }
            store.delete(SecretId::WebApi)?;
            personal = Some(previous);
        }
        Ok((shared, personal))
    }

    /// On startup the shared grant is verified first because it establishes
    /// the canonical account. A saved personal grant waits for that result.
    fn restore_session(&mut self) {
        let (shared, personal) = match Self::load_saved_web_tokens(
            self.secrets.as_ref(),
            &self.dirs.legacy_web_secret(),
        ) {
            Ok(tokens) => tokens,
            Err(error) => {
                self.emit(Event::Auth(AuthStatus::Failed(format!(
                    "Stored Spotify sign-in could not be read safely: {error}"
                ))));
                return;
            }
        };
        self.pending_personal_token = personal.filter(|token| self.personal_token_matches(token));
        match shared {
            Some(token)
                if token.client_id == crate::auth::DEFAULT_WEB_CLIENT_ID
                    && token.has_scopes(crate::auth::WEB_SCOPES) =>
            {
                self.emit(Event::Auth(AuthStatus::Connecting));
                self.activate_web_token(ApiSource::Shared, token, self.auth_epoch, false);
            }
            Some(_) => self.emit(Event::Auth(AuthStatus::Failed(
                "Fastpotify needs one more Spotify permission. Please sign in again.".into(),
            ))),
            None => self.emit(Event::Auth(AuthStatus::SignedOut)),
        }
    }

    fn personal_token_matches(&self, token: &crate::auth::StoredToken) -> bool {
        self.web_client_id.as_deref() == Some(token.client_id.as_str())
            && crate::auth::Grant::personal_web_api(&token.client_id).is_ok()
            && token.has_scopes(crate::auth::WEB_SCOPES)
    }

    fn remember_saved_personal_token(&mut self) {
        match crate::secrets::load_json::<crate::auth::StoredToken>(
            self.secrets.as_ref(),
            SecretId::PersonalWebApi,
        ) {
            Ok(Some(token)) => {
                if let Err(error) = Self::validate_saved_token(&token, SecretId::PersonalWebApi) {
                    self.pending_personal_token = None;
                    self.emit(Event::Error(format!(
                        "Stored personal Spotify sign-in could not be read safely: {error}"
                    )));
                } else {
                    self.pending_personal_token =
                        self.personal_token_matches(&token).then_some(token);
                }
            }
            Ok(None) => self.pending_personal_token = None,
            Err(error) => {
                self.pending_personal_token = None;
                self.emit(Event::Error(format!(
                    "Stored personal Spotify sign-in could not be read safely: {error}"
                )));
            }
        }
    }

    fn activate_web_token(
        &mut self,
        source: ApiSource,
        token: crate::auth::StoredToken,
        epoch: u64,
        interactive: bool,
    ) {
        let tokens = WebTokens::new(
            self.http.clone(),
            token.clone(),
            Arc::clone(&self.secrets),
            Self::token_secret(source),
            source,
        );
        let generation = self.api.begin_verification(source, |generation| {
            tokens.attach_session(generation);
            TokenProvider::Web(Arc::clone(&tokens))
        });
        let client = self.api.verification_client(source);
        let gateway = Arc::clone(&self.api);
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let mut attempt = 0;
            let result = loop {
                attempt += 1;
                match client.me().await {
                    Ok(user) => break Ok(user),
                    Err(error)
                        if attempt < 3
                            && (matches!(error, ApiError::Network(_))
                                || error.status().is_some_and(|status| status >= 500))
                            && matches!(gateway.state(source), SessionState::Authorizing) =>
                    {
                        tokio::time::sleep(Duration::from_millis(250 * attempt)).await;
                    }
                    Err(error) => break Err(error),
                }
            };
            match result {
                Ok(user) => {
                    let _ = commands.send(Command::WebVerified {
                        source,
                        token: Box::new(token),
                        user: Box::new(user),
                        epoch,
                        interactive,
                        generation,
                    });
                }
                Err(error) => {
                    let expired = matches!(error, ApiError::SignInExpired { .. });
                    let _ = commands.send(Command::WebSignInFailed {
                        source,
                        message: Some(error.to_string()),
                        epoch,
                        interactive,
                        expired,
                        generation: Some(generation),
                    });
                }
            }
        });
    }

    fn current_web_authorization(&self, source: ApiSource, epoch: u64) -> bool {
        self.authorization.active.as_ref().is_some_and(|active| {
            active.kind == AuthorizationKind::Web(source)
                && active.epoch == epoch
                && !active.cancelled
                && epoch == self.auth_epoch
        })
    }

    fn on_web_signed_in(&mut self, source: ApiSource, token: crate::auth::StoredToken, epoch: u64) {
        if !self.current_web_authorization(source, epoch) {
            let _ = self.discard_stale_authorization(AuthorizationKind::Web(source), epoch);
            return;
        }
        let expected_client = match source {
            ApiSource::Shared => Some(crate::auth::DEFAULT_WEB_CLIENT_ID),
            ApiSource::Personal => self.web_client_id.as_deref(),
        };
        if expected_client != Some(token.client_id.as_str()) {
            let _ = self.discard_stale_authorization(AuthorizationKind::Web(source), epoch);
            self.emit(Event::Error(format!(
                "Spotify returned credentials for the wrong {source} application"
            )));
            return;
        }
        if let Err(error) =
            crate::secrets::store_json(self.secrets.as_ref(), Self::token_secret(source), &token)
        {
            let _ = self.discard_stale_authorization(AuthorizationKind::Web(source), epoch);
            if source == ApiSource::Shared {
                self.emit(Event::Auth(AuthStatus::Failed(
                    "Spotify approved the sign-in, but Fastpotify could not store it safely."
                        .into(),
                )));
            }
            self.emit(Event::Error(format!(
                "{source} sign-in was not activated because credential storage failed: {error}"
            )));
            return;
        }
        self.activate_web_token(source, token, epoch, true);
    }

    fn on_web_verified(
        &mut self,
        source: ApiSource,
        token: crate::auth::StoredToken,
        user: User,
        epoch: u64,
        interactive: bool,
        generation: u64,
    ) {
        if interactive && !self.current_web_authorization(source, epoch) {
            let _ = self.discard_stale_authorization(AuthorizationKind::Web(source), epoch);
            return;
        }
        if epoch != self.auth_epoch
            || !self.api.is_current(source, generation)
            || !matches!(self.api.state(source), SessionState::Authorizing)
            || source == ApiSource::Personal
                && self.web_client_id.as_deref() != Some(token.client_id.as_str())
        {
            return;
        }
        if let Err(error) = self
            .api
            .install(source, generation, AccountId::new(user.id.clone()))
        {
            self.api.clear_if_current(source, generation);
            if source == ApiSource::Personal {
                let _ = self.secrets.delete(SecretId::PersonalWebApi);
                self.emit(Event::WebApp { client_id: None });
            }
            self.emit(Event::Error(format!(
                "{source} Spotify authorization failed: {error}"
            )));
            if interactive {
                let _ = self.discard_stale_authorization(AuthorizationKind::Web(source), epoch);
            }
            return;
        }
        match source {
            ApiSource::Shared => {
                self.signed_in = true;
                self.emit(Event::Auth(AuthStatus::Connected {
                    username: user.name().to_string(),
                }));
                self.emit(Event::Api(Box::new(ApiResponse::Me(Ok(user.clone())))));
                let premium = user.product.as_deref().map(|product| product == "premium");
                self.on_account_checked(premium);
                if let Some(personal) = self.pending_personal_token.take() {
                    self.activate_web_token(ApiSource::Personal, personal, self.auth_epoch, false);
                }
            }
            ApiSource::Personal => self.emit(Event::WebApp {
                client_id: Some(token.client_id),
            }),
        }
        if interactive {
            let _ = self.discard_stale_authorization(AuthorizationKind::Web(source), epoch);
        }
    }

    fn on_web_sign_in_failed(
        &mut self,
        source: ApiSource,
        message: Option<String>,
        epoch: u64,
        interactive: bool,
        expired: bool,
        generation: Option<u64>,
    ) {
        if interactive && !self.current_web_authorization(source, epoch) {
            let _ = self.discard_stale_authorization(AuthorizationKind::Web(source), epoch);
            return;
        }
        if epoch != self.auth_epoch {
            return;
        }
        if let Some(generation) = generation
            && !self.api.clear_if_current(source, generation)
        {
            if interactive {
                let _ = self.discard_stale_authorization(AuthorizationKind::Web(source), epoch);
            }
            return;
        }
        if generation.is_none() {
            self.api.clear(source);
        }
        if expired && let Err(error) = self.secrets.delete(Self::token_secret(source)) {
            self.emit(Event::Error(format!(
                "The expired {source} credential could not be deleted: {error}"
            )));
        }
        if source == ApiSource::Shared {
            self.remember_saved_personal_token();
            self.signed_in = false;
            self.premium = None;
            self.emit(Event::WebApp { client_id: None });
            self.emit(Event::Auth(match &message {
                Some(message) => {
                    AuthStatus::Failed(format!("Shared Spotify sign-in failed: {message}"))
                }
                None => AuthStatus::SignedOut,
            }));
        } else {
            self.emit(Event::WebApp { client_id: None });
        }
        if let Some(message) = message {
            self.emit(Event::Error(format!(
                "{source} Spotify sign-in failed: {message}"
            )));
        }
        if interactive {
            let _ = self.discard_stale_authorization(AuthorizationKind::Web(source), epoch);
        }
    }

    fn on_web_session_expired(&mut self, source: ApiSource, generation: u64) {
        if !self.api.clear_if_current(source, generation) {
            return;
        }
        if let Err(error) = self.secrets.delete(Self::token_secret(source)) {
            self.emit(Event::Error(format!(
                "The expired {source} sign-in was cleared from memory, but its stored credential remains: {error}"
            )));
        }
        match source {
            ApiSource::Shared => {
                self.remember_saved_personal_token();
                self.signed_in = false;
                self.premium = None;
                self.emit(Event::WebApp { client_id: None });
                self.emit(Event::Auth(AuthStatus::Failed(
                    "Your shared Spotify sign-in expired. Please sign in again.".into(),
                )));
            }
            ApiSource::Personal => self.emit(Event::WebApp { client_id: None }),
        }
    }

    fn sign_in(&mut self) {
        self.sign_in_source(ApiSource::Shared);
    }

    fn sign_in_source(&mut self, source: ApiSource) {
        if matches!(self.api.state(source), SessionState::Ready { .. }) {
            return;
        }
        if self
            .authorization
            .defer_web_request(source, self.auth_epoch)
        {
            return;
        }
        if source == ApiSource::Personal
            && !matches!(
                self.api.state(ApiSource::Shared),
                SessionState::Ready { .. }
            )
        {
            return;
        }
        let grant = match source {
            ApiSource::Shared => crate::auth::Grant::shared_web_api(),
            ApiSource::Personal => {
                let Some(client_id) = self.web_client_id.as_deref() else {
                    return;
                };
                match crate::auth::Grant::personal_web_api(client_id) {
                    Ok(grant) => grant,
                    Err(error) => {
                        self.emit(Event::Error(error.to_string()));
                        return;
                    }
                }
            }
        };
        let session = match crate::auth::PreparedAuthorization::prepare(&grant) {
            Ok(session) => session,
            Err(error) => {
                let message = format!("Spotify sign-in could not start: {error}");
                if source == ApiSource::Shared {
                    self.emit(Event::Auth(AuthStatus::Failed(message)));
                } else {
                    self.emit(Event::Error(message));
                }
                return;
            }
        };
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let epoch = self.auth_epoch;
        self.authorization
            .begin(AuthorizationKind::Web(source), cancel_tx, epoch);
        let url = session.url().to_string();
        if source == ApiSource::Shared {
            self.emit(Event::Auth(AuthStatus::WaitingForBrowser {
                url: url.clone(),
            }));
        }
        if let Err(error) = open::that_detached(&url) {
            log::warn!("unable to open a browser: {error}");
        }
        let http = self.http.clone();
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let result = async {
                let authorized = session.wait(cancel_rx).await?;
                let response = crate::auth::exchange_code(
                    &http,
                    &grant,
                    &authorized.code,
                    &authorized.verifier,
                )
                .await?;
                crate::auth::StoredToken::from_response(&grant.client_id, response, None)
            }
            .await;
            match result {
                Ok(token) => {
                    let _ = commands.send(Command::WebSignedIn {
                        source,
                        token: Box::new(token),
                        epoch,
                    });
                }
                Err(error) => {
                    let message = error.to_string();
                    let _ = commands.send(Command::WebSignInFailed {
                        source,
                        message: (!message.contains("cancelled")).then_some(message),
                        epoch,
                        interactive: true,
                        expired: false,
                        generation: None,
                    });
                }
            }
        });
    }

    fn configure_personal_web_app(&mut self, client_id: Option<String>) {
        let replacing_personal_flow = self
            .authorization
            .active
            .as_ref()
            .is_some_and(|active| active.kind == AuthorizationKind::Web(ApiSource::Personal));
        if replacing_personal_flow {
            self.authorization.cancel();
            self.auth_epoch = self.auth_epoch.wrapping_add(1);
        }
        self.web_client_id = client_id.and_then(|value| {
            let value = value.trim().to_string();
            (!value.is_empty()).then_some(value)
        });
        self.pending_personal_token = None;
        self.api.clear(ApiSource::Personal);
        self.emit(Event::WebApp { client_id: None });
        if let Err(error) = self.secrets.delete(SecretId::PersonalWebApi) {
            self.emit(Event::Error(format!(
                "The personal Web sign-in was cleared from memory, but its stored credential remains: {error}"
            )));
        }
        if self.web_client_id.is_some() {
            self.sign_in_source(ApiSource::Personal);
        } else {
            self.authorization.pending_web = None;
        }
    }

    /// A replaced flow reports only after dropping its listener. Use that
    /// boundary to start the pending Web flow without a fixed-port bind race.
    fn discard_stale_authorization(&mut self, kind: AuthorizationKind, epoch: u64) -> bool {
        match self.authorization.finish(kind, epoch, self.auth_epoch) {
            AuthorizationCompletion::Current => false,
            AuthorizationCompletion::Ignore => true,
            AuthorizationCompletion::StartPendingWeb(source) => {
                self.sign_in_source(source);
                true
            }
        }
    }

    fn sign_out(&mut self) {
        let secrets = Arc::clone(&self.secrets);
        let web_legacy = self.dirs.legacy_web_secret();
        let playback_legacy = self.dirs.legacy_playback_secret();
        let cleared = crate::secrets::clear_all_secrets(
            secrets.as_ref(),
            &web_legacy,
            &playback_legacy,
            || {
                self.signed_in = false;
                self.auth_epoch = self.auth_epoch.wrapping_add(1);
                self.credential_epoch = self.credential_epoch.wrapping_add(1);
                self.engine_connection.reset();
                self.pending_resume = None;
                self.engine_notifications.retire();
                if let Some(engine) = self.engine.take() {
                    engine.shutdown();
                }
                self.authorization.cancel();
                self.pending_personal_token = None;
                self.api.clear_all();
            },
        );
        self.emit(Event::Playback(LocalPlayback::Unavailable));
        self.emit(Event::Auth(AuthStatus::SignedOut));
        if let Err(error) = cleared {
            self.emit(Event::Error(format!(
                "Signed out for this run, but some stored credentials could not be deleted: {error}"
            )));
        }
    }

    // ---- local playback engine -------------------------------------------

    fn engine_notify(&self) -> (u64, crate::player::Notify) {
        self.engine_notifications.notifier(
            self.events.clone(),
            self.commands.clone(),
            self.waker.clone(),
        )
    }

    /// Bring the engine up from a credential stored by a previous playback
    /// authorization, if there is one. Silent when there is nothing to resume.
    fn resume_engine(&mut self) {
        if self.engine.is_some() || !self.engine_connection.is_idle() || self.premium == Some(false)
        {
            return;
        }
        match self.saved_playback_credentials() {
            Ok(Some(credentials)) => self.connect_engine(credentials),
            Ok(None) => {}
            Err(error) => self.emit(Event::Playback(LocalPlayback::Failed(format!(
                "Stored playback credentials could not be read safely: {error}"
            )))),
        }
    }

    fn saved_playback_credentials(&self) -> crate::secrets::Result<Option<Credentials>> {
        crate::secrets::load_json_migrating_validated::<Credentials>(
            self.secrets.as_ref(),
            SecretId::Playback,
            &self.dirs.legacy_playback_secret(),
            |credentials| {
                let username_valid = credentials
                    .username
                    .as_deref()
                    .is_some_and(|username| !username.trim().is_empty() && username.len() <= 512);
                if !username_valid
                    || credentials.auth_data.is_empty()
                    || credentials.auth_data.len() > 512 * 1024
                    || credentials.auth_type
                        != AuthenticationType::AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS
                {
                    return Err(crate::secrets::SecretError::Corrupt {
                        kind: SecretId::Playback.label(),
                        reason: "credential shape or authentication type is invalid".into(),
                    });
                }
                Ok(())
            },
        )
    }

    /// Reconnect the engine after its session dropped or audio settings
    /// changed, restoring an interrupted track on the replacement session.
    fn reconnect_engine(&mut self) {
        if !self.signed_in {
            return;
        }
        match self.engine_connection.reconnect_requested() {
            RestartDisposition::Ignore | RestartDisposition::Deferred => {}
            RestartDisposition::Now => self.restart_engine_now(),
        }
    }

    fn on_engine_ended(&mut self, generation: u64) {
        if !self.signed_in {
            return;
        }
        match self.engine_connection.session_ended(generation) {
            RestartDisposition::Ignore => {}
            RestartDisposition::Deferred => self.engine_notifications.retire(),
            RestartDisposition::Now => self.restart_engine_now(),
        }
    }

    fn restart_engine_now(&mut self) {
        if let Some(engine) = self.engine.take() {
            self.pending_resume = engine.interrupted().map(|interrupted| LoadSpec {
                uris: vec![interrupted.uri],
                position_ms: interrupted.position_ms,
                play: interrupted.playing,
                ..LoadSpec::default()
            });
            self.engine_notifications.retire();
            engine.shutdown();
        }
        self.schedule_engine_reconnect();
    }

    fn schedule_engine_reconnect(&mut self) {
        let now = Instant::now();
        self.reconnects
            .retain(|attempt| now.duration_since(*attempt) < Duration::from_secs(600));
        if self.reconnects.len() >= 6 {
            self.pending_resume = None;
            self.emit(Event::Playback(LocalPlayback::Failed(
                "Local playback keeps dropping. Re-enable it from Settings.".into(),
            )));
            return;
        }
        self.reconnects.push(now);
        log::info!(
            "local playback session ended; reconnecting ({} of 6 in ten minutes)",
            self.reconnects.len()
        );
        self.resume_engine();
    }

    /// Start (or re-enter) the playback authorization in the browser. This is
    /// a distinct grant from the Web API sign-in: it uses Spotify's streaming
    /// client identity, the one librespot can play with.
    fn authorize_playback(&mut self) {
        if !self.engine_connection.is_idle() || self.authorization.is_active() {
            return;
        }
        if self.premium == Some(false) {
            self.emit(Event::Playback(LocalPlayback::Failed(
                PREMIUM_NEEDED.into(),
            )));
            return;
        }
        let grant = crate::auth::Grant::playback();
        let session = match crate::auth::PreparedAuthorization::prepare(&grant) {
            Ok(session) => session,
            Err(error) => {
                self.emit(Event::Playback(LocalPlayback::Failed(format!(
                    "Spotify playback approval could not start: {error}"
                ))));
                return;
            }
        };
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let epoch = self.auth_epoch;
        self.authorization
            .begin(AuthorizationKind::Playback, cancel_tx, epoch);
        self.emit(Event::Playback(LocalPlayback::Authorizing));
        if let Err(error) = open::that_detached(session.url()) {
            log::warn!("unable to open a browser: {error}");
        }
        let http = self.http.clone();
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let result = async {
                let authorized = session.wait(cancel_rx).await?;
                crate::auth::exchange_code(&http, &grant, &authorized.code, &authorized.verifier)
                    .await
            }
            .await;
            match result {
                Ok(token) => {
                    let _ = commands.send(Command::PlaybackAuthorized {
                        access_token: token.access_token,
                        epoch,
                    });
                }
                Err(error) => {
                    let message = error.to_string();
                    let _ = commands.send(Command::PlaybackAuthorizationFailed {
                        message: (!message.contains("cancelled")).then_some(message),
                        epoch,
                    });
                }
            }
        });
    }

    fn on_playback_authorization_failed(&mut self, message: Option<String>, epoch: u64) {
        if self.discard_stale_authorization(AuthorizationKind::Playback, epoch) {
            return;
        }
        match message {
            Some(message) => self.emit(Event::Playback(LocalPlayback::Failed(message))),
            None => self.emit(Event::Playback(LocalPlayback::Unavailable)),
        }
    }

    /// Spawn an engine connection so a slow or hung librespot handshake can
    /// never block the command loop (this was the cause of the app freezing
    /// on "Connecting to Spotify"). The reusable credential comes back to
    /// the command loop and must be durably stored before the engine is used.
    fn connect_engine(&mut self, credentials: Credentials) {
        if !self.engine_connection.is_idle() || !self.signed_in {
            return;
        }
        if self.premium == Some(false) {
            self.emit(Event::Playback(LocalPlayback::Failed(
                PREMIUM_NEEDED.into(),
            )));
            return;
        }
        self.emit(Event::Playback(LocalPlayback::Connecting));
        let config = self.engine_config.clone();
        let (generation, notify) = self.engine_notify();
        self.engine_connection.begin(generation);
        let commands = self.commands.clone();
        let waker = self.waker.clone();
        let epoch = self.credential_epoch;
        tokio::spawn(async move {
            let cache = match config.open_cache() {
                Ok(cache) => cache,
                Err(error) => {
                    let _ = commands.send(Command::EngineConnected {
                        engine: Box::new(None),
                        credential: None,
                        generation,
                        epoch,
                        error: Some(error.to_string()),
                    });
                    return;
                }
            };
            let attempt = tokio::time::timeout(
                Duration::from_secs(45),
                Engine::connect(&config, credentials, cache, notify),
            )
            .await;
            let outcome = match attempt {
                Ok(Ok((engine, credential))) => Command::EngineConnected {
                    engine: Box::new(Some(engine)),
                    credential: Some(credential),
                    generation,
                    epoch,
                    error: None,
                },
                Ok(Err(error)) => {
                    log::error!("engine connect failed: {error:#}");
                    Command::EngineConnected {
                        engine: Box::new(None),
                        credential: None,
                        generation,
                        epoch,
                        error: Some(friendly_connect_error(&error)),
                    }
                }
                Err(_) => Command::EngineConnected {
                    engine: Box::new(None),
                    credential: None,
                    generation,
                    epoch,
                    error: Some("Connecting to Spotify timed out".into()),
                },
            };
            let _ = commands.send(outcome);
            waker.wake();
        });
    }

    fn on_engine_connected(
        &mut self,
        engine: Option<Engine>,
        credential: Option<Credentials>,
        generation: u64,
        epoch: u64,
        error: Option<String>,
    ) {
        if epoch != self.credential_epoch || !self.signed_in {
            if let Some(engine) = engine {
                engine.shutdown();
            }
            return;
        }
        let completion = self.engine_connection.engine_connected(generation);
        if completion == CompletionDisposition::Ignore {
            if let Some(engine) = engine {
                engine.shutdown();
            }
            return;
        }
        match (engine, credential) {
            (Some(engine), Some(credential)) => {
                if let Err(error) = crate::secrets::store_json(
                    self.secrets.as_ref(),
                    SecretId::Playback,
                    &credential,
                ) {
                    self.pending_resume = None;
                    self.engine_connection.retire(generation);
                    self.engine_notifications.retire();
                    engine.shutdown();
                    self.emit(Event::Playback(LocalPlayback::Failed(format!(
                        "Spotify connected, but its playback credential could not be stored safely: {error}"
                    ))));
                    return;
                }
                if completion == CompletionDisposition::Restart {
                    if let Some(interrupted) = engine.interrupted() {
                        self.pending_resume = Some(LoadSpec {
                            uris: vec![interrupted.uri],
                            position_ms: interrupted.position_ms,
                            play: interrupted.playing,
                            ..LoadSpec::default()
                        });
                    }
                    engine.shutdown();
                    self.schedule_engine_reconnect();
                    return;
                }
                let device_id = engine.device_id().to_string();
                let engine = Arc::new(engine);
                if let Some(spec) = self.pending_resume.take()
                    && let Err(error) = engine.command(PlayerCommand::Load(spec))
                {
                    log::warn!("unable to resume playback after reconnecting: {error}");
                }
                self.engine = Some(engine);
                self.emit(Event::Playback(LocalPlayback::Ready { device_id }));
            }
            (None, _) => {
                self.pending_resume = None;
                self.engine_connection.retire(generation);
                self.engine_notifications.retire();
                let message = error.unwrap_or_else(|| "Local playback is unavailable".into());
                self.emit(Event::Playback(LocalPlayback::Failed(message)));
            }
            (Some(engine), None) => {
                self.pending_resume = None;
                self.engine_connection.retire(generation);
                self.engine_notifications.retire();
                engine.shutdown();
                self.emit(Event::Playback(LocalPlayback::Failed(
                    "Spotify did not return a reusable playback credential".into(),
                )));
            }
        }
    }

    /// The plan gates the engine because librespot 0.8 calls `exit(1)` from
    /// inside its session the moment Spotify tells it the account is not
    /// Premium; no error path of ours can catch that, so a Free account must
    /// never reach it. When the API cannot say, the engine comes back as it
    /// always did.
    fn on_account_checked(&mut self, premium: Option<bool>) {
        self.premium = premium;
        if premium == Some(false) {
            self.pending_resume = None;
            self.engine_connection.reset();
            self.engine_notifications.retire();
            if let Some(engine) = self.engine.take() {
                engine.shutdown();
            }
            match self.saved_playback_credentials() {
                Ok(Some(_)) => self.emit(Event::Playback(LocalPlayback::Failed(
                    PREMIUM_NEEDED.into(),
                ))),
                Ok(None) => {}
                Err(error) => self.emit(Event::Playback(LocalPlayback::Failed(format!(
                    "Stored playback credentials could not be read safely: {error}"
                )))),
            }
            return;
        }
        self.resume_engine();
    }

    fn check_for_updates(&self) {
        let http = self.http.clone();
        let events = self.events.clone();
        let waker = self.waker.clone();
        tokio::spawn(async move {
            match crate::updates::newer_release(&http).await {
                Ok(Some(release)) => {
                    let _ = events.send(Event::UpdateAvailable {
                        version: release.version,
                        url: release.url,
                    });
                    waker.wake();
                }
                Ok(None) => log::debug!("this is the newest release"),
                Err(error) => log::debug!("could not check for a newer release: {error:#}"),
            }
        });
    }

    fn fetch_lyrics(&self, request: LyricsRequest) {
        let http = self.http.clone();
        let events = self.events.clone();
        let waker = self.waker.clone();
        let cache_dir = self.dirs.lyrics_cache_dir();
        let engine = self.engine.clone();
        tokio::spawn(async move {
            // Spotify's own words go first: they follow the recording
            // exactly. LRCLIB is contacted only after the user opts in.
            let result = match spotify_lyrics(engine, &request.uri, &cache_dir).await {
                Some(found) => Ok(Some(found)),
                None if request.allow_lrclib => {
                    crate::lyrics::fetch(&http, &cache_dir, &request.query)
                        .await
                        .map_err(|error| format!("{error:#}"))
                }
                None => Ok(None),
            };
            let _ = events.send(Event::Lyrics {
                uri: request.uri,
                allow_lrclib: request.allow_lrclib,
                result,
            });
            waker.wake();
        });
    }

    /// Hand the interface a playlist's cached items, if any are on disk.
    /// Whether they are still true is the interface's call: it compares
    /// the snapshot against the live playlist before adopting them.
    fn load_playlist_cache(&self, id: String) {
        let events = self.events.clone();
        let waker = self.waker.clone();
        let path = self.dirs.playlist_cache_dir().join(format!("{id}.json"));
        tokio::spawn(async move {
            let Ok(text) = tokio::fs::read_to_string(&path).await else {
                return;
            };
            let Ok(cached) = serde_json::from_str::<CachedPlaylist>(&text) else {
                return;
            };
            let _ = events.send(Event::PlaylistCache {
                id,
                snapshot: cached.snapshot,
                items: cached.items,
            });
            waker.wake();
        });
    }

    fn store_playlist_cache(&self, id: String, snapshot: String, items: Vec<PlaylistItem>) {
        let path = self.dirs.playlist_cache_dir().join(format!("{id}.json"));
        tokio::spawn(async move {
            if let Some(parent) = path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            if let Ok(text) = serde_json::to_string(&CachedPlaylist { snapshot, items }) {
                let _ = tokio::fs::write(&path, text).await;
            }
        });
    }

    /// Ask Spotify who is behind each user id. Only the streaming session
    /// can ask; without one the interface shows the bare ids.
    fn fetch_user_names(&self, ids: Vec<String>) {
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let events = self.events.clone();
        let waker = self.waker.clone();
        tokio::spawn(async move {
            for id in ids {
                let name = engine.user_display_name(&id).await;
                let _ = events.send(Event::UserName { id, name });
                waker.wake();
            }
        });
    }

    // ---- api ----------------------------------------------------------------

    fn dispatch(&self, request: ApiRequest) {
        dispatch_api(
            ApiDispatchContext {
                api: Arc::clone(&self.api),
                background_api: Arc::clone(&self.background_api),
                events: self.events.clone(),
                commands: self.commands.clone(),
                waker: self.waker.clone(),
            },
            &self.api_mutations,
            request,
        );
    }

    fn accent(&self, url: String) {
        let art = self.art.clone();
        let events = self.events.clone();
        let waker = self.waker.clone();
        tokio::spawn(async move {
            if let Ok(bytes) = art.fetch(&url).await {
                let color = tokio::task::spawn_blocking(move || accent_color(&bytes))
                    .await
                    .ok()
                    .flatten();
                if let Some(color) = color {
                    let _ = events.send(Event::Accent { url, color });
                    waker.wake();
                }
            }
        });
    }
}

#[derive(Clone)]
struct ApiDispatchContext {
    api: Arc<ApiGateway>,
    background_api: Arc<Semaphore>,
    events: std::sync::mpsc::Sender<Event>,
    commands: mpsc::UnboundedSender<Command>,
    waker: Waker,
}

type RoutedClient = Result<(ApiSource, Arc<ApiClient>), ApiError>;

const MAX_RUNNING_PLAYBACK_MUTATIONS: usize = 6;
const MAX_PENDING_PLAYBACK_MUTATIONS: usize = 32;
const MAX_RUNNING_PLAYLIST_MUTATIONS: usize = 6;

/// Resources whose mutations can alias at Spotify's player endpoint. The
/// active-device lane is global, while an explicit device lane remains
/// independent. Transfer touches both because it changes the active alias and
/// names the device that becomes active.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaybackMutationLanes {
    active_device: bool,
    device_id: Option<String>,
}

impl PlaybackMutationLanes {
    fn active() -> Self {
        Self {
            active_device: true,
            device_id: None,
        }
    }

    fn explicit(device_id: String) -> Self {
        Self {
            active_device: false,
            device_id: Some(device_id),
        }
    }

    fn transfer(device_id: String) -> Self {
        Self {
            active_device: true,
            device_id: Some(device_id),
        }
    }

    fn conflicts_with(&self, other: &Self) -> bool {
        (self.active_device && other.active_device)
            || self
                .device_id
                .as_ref()
                .zip(other.device_id.as_ref())
                .is_some_and(|(left, right)| left == right)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct PlaybackSessionId {
    source: ApiSource,
    generation: u64,
    revision: u64,
}

impl From<&ApiRoute> for PlaybackSessionId {
    fn from(route: &ApiRoute) -> Self {
        Self {
            source: route.source,
            generation: route.generation,
            revision: route.revision,
        }
    }
}

struct PlaybackMutation {
    id: u64,
    lanes: PlaybackMutationLanes,
    request: ApiRequest,
}

struct RunningPlaybackMutation {
    lanes: PlaybackMutationLanes,
    abort: tokio::task::AbortHandle,
}

struct PlaybackSessionQueue {
    route: ApiRoute,
    pending: VecDeque<PlaybackMutation>,
    running: HashMap<u64, RunningPlaybackMutation>,
}

impl PlaybackSessionQueue {
    fn new(route: ApiRoute) -> Self {
        Self {
            route,
            pending: VecDeque::new(),
            running: HashMap::new(),
        }
    }

    /// Earlier blocked mutations reserve their lanes, preventing a later
    /// conflicting request from overtaking while still allowing unrelated
    /// explicit devices to run.
    fn next_ready(&self) -> Option<usize> {
        if self.running.len() >= MAX_RUNNING_PLAYBACK_MUTATIONS {
            return None;
        }
        let mut reserved: Vec<&PlaybackMutationLanes> = self
            .running
            .values()
            .map(|running| &running.lanes)
            .collect();
        for (index, mutation) in self.pending.iter().enumerate() {
            let blocked = reserved
                .iter()
                .any(|lanes| lanes.conflicts_with(&mutation.lanes));
            reserved.push(&mutation.lanes);
            if !blocked {
                return Some(index);
            }
        }
        None
    }
}

struct RunningPlaylistMutation {
    scheduler_id: u64,
    abort: tokio::task::AbortHandle,
}

#[derive(Default)]
struct ApiMutationSchedulerState {
    next_id: u64,
    sessions: HashMap<PlaybackSessionId, PlaybackSessionQueue>,
    playlists: HashMap<String, RunningPlaylistMutation>,
}

#[derive(Default)]
struct ApiMutationScheduler {
    state: Mutex<ApiMutationSchedulerState>,
}

struct PlaylistTaskCompletion {
    scheduler: Arc<ApiMutationScheduler>,
    data: Option<PlaylistTaskCompletionData>,
    interrupted: Option<ApiResponse>,
}

struct PlaylistTaskCompletionData {
    playlist_id: String,
    scheduler_id: u64,
    operation: Operation,
    route: ApiRoute,
    context: ApiDispatchContext,
}

impl PlaylistTaskCompletion {
    fn complete(mut self, response: ApiResponse) {
        self.interrupted = None;
        if let Some(data) = self.data.take() {
            self.scheduler.finish_playlist(data, response);
        }
    }
}

impl Drop for PlaylistTaskCompletion {
    fn drop(&mut self) {
        let Some(response) = self.interrupted.take() else {
            return;
        };
        if let Some(data) = self.data.take() {
            self.scheduler.finish_playlist(data, response);
        }
    }
}

impl ApiMutationScheduler {
    fn enqueue_playback(
        self: &Arc<Self>,
        context: ApiDispatchContext,
        request: ApiRequest,
        lanes: PlaybackMutationLanes,
    ) {
        let authoritative = request.authoritative_playback_mutation();
        let (mut state, route) = loop {
            let route = match context.api.route_for(Operation::Playback) {
                Ok(route) => route,
                Err(error) => {
                    tokio::spawn(async move {
                        dispatch_one(&context, request, Some(Err(error))).await;
                    });
                    return;
                }
            };
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            self.retire_stale_locked(&mut state, &context.api);
            if context.api.route_is_current(Operation::Playback, &route) {
                break (state, route);
            }
        };
        let identity = PlaybackSessionId::from(&route);
        let mut rejected = None;
        state.next_id = state
            .next_id
            .checked_add(1)
            .expect("playback mutation id exhausted");
        let mutation = PlaybackMutation {
            id: state.next_id,
            lanes,
            request,
        };
        let session = state
            .sessions
            .entry(identity)
            .or_insert_with(|| PlaybackSessionQueue::new(route.clone()));
        // At the bound, preserve a newly submitted absolute state by
        // replacing the oldest pending mutation. Relative commands are
        // rejected instead of being silently dropped or replayed later.
        if session.pending.len() < MAX_PENDING_PLAYBACK_MUTATIONS {
            session.pending.push_back(mutation);
        } else if authoritative {
            rejected = session.pending.pop_front();
            session.pending.push_back(mutation);
        } else {
            rejected = Some(mutation);
        }
        if let Some(rejected) = rejected {
            let response = playback_backpressure_response(rejected.request);
            context
                .api
                .with_current_route(Operation::Playback, &route, || {
                    publish_api_response(&context, response);
                });
        }
        self.pump_locked(&mut state, identity, &context);
    }

    fn complete(
        self: &Arc<Self>,
        identity: PlaybackSessionId,
        mutation_id: u64,
        context: ApiDispatchContext,
        response: ApiResponse,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(session) = state.sessions.get_mut(&identity) else {
            return;
        };
        if session.running.remove(&mutation_id).is_none() {
            return;
        }
        let route = session.route.clone();
        let session_expired = matches!(response.error(), Some(ApiError::SignInExpired { .. }));
        if !context
            .api
            .with_current_route(Operation::Playback, &route, || {
                publish_api_response(&context, response);
            })
        {
            Self::retire_session_locked(&mut state, identity);
            return;
        }
        if session_expired {
            Self::retire_session_locked(&mut state, identity);
            return;
        }
        self.pump_locked(&mut state, identity, &context);
        if state
            .sessions
            .get(&identity)
            .is_some_and(|session| session.pending.is_empty() && session.running.is_empty())
        {
            state.sessions.remove(&identity);
        }
    }

    /// Playlist item writes are single-flight per playlist and globally
    /// bounded before task creation. A replaced API generation cannot publish
    /// its completion even though the keyed lane stays reserved until the
    /// bounded wire attempt has actually ended.
    fn enqueue_playlist(self: &Arc<Self>, context: ApiDispatchContext, request: ApiRequest) {
        self.enqueue_playlist_with(context, request, |request, route| async move {
            let selected = Ok((route.source, Arc::clone(&route.client)));
            handle_routed(request, selected).await
        });
    }

    fn enqueue_playlist_with<Execute, Pending>(
        self: &Arc<Self>,
        context: ApiDispatchContext,
        request: ApiRequest,
        execute: Execute,
    ) where
        Execute: FnOnce(ApiRequest, ApiRoute) -> Pending + Send + 'static,
        Pending: Future<Output = ApiResponse> + Send + 'static,
    {
        let Some((playlist_id, mutation_id)) = request.playlist_mutation() else {
            return;
        };
        let playlist_id = playlist_id.to_owned();
        let operation = operation_for(&context.api, &request);
        let (mut state, route) = loop {
            let route = match context.api.route_for(operation) {
                Ok(route) => route,
                Err(error) => {
                    tokio::spawn(async move {
                        dispatch_one(&context, request, Some(Err(error))).await;
                    });
                    return;
                }
            };
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            self.retire_stale_locked(&mut state, &context.api);
            if context.api.route_is_current(operation, &route) {
                break (state, route);
            }
        };
        let rejected = if state.playlists.contains_key(&playlist_id) {
            Some(ApiError::PlaylistMutationPending)
        } else if state.playlists.len() >= MAX_RUNNING_PLAYLIST_MUTATIONS {
            Some(ApiError::PlaylistMutationBackpressure)
        } else {
            None
        };
        if let Some(error) = rejected {
            drop(state);
            let response = playlist_mutation_error_response(playlist_id, mutation_id, error);
            context.api.with_current_route(operation, &route, || {
                publish_api_response(&context, response);
            });
            return;
        }

        state.next_id = state
            .next_id
            .checked_add(1)
            .expect("API mutation scheduler id exhausted");
        let scheduler_id = state.next_id;
        let execution_route = route.clone();
        let completion = PlaylistTaskCompletion {
            scheduler: Arc::clone(self),
            data: Some(PlaylistTaskCompletionData {
                playlist_id: playlist_id.clone(),
                scheduler_id,
                operation,
                route,
                context,
            }),
            interrupted: Some(playlist_mutation_error_response(
                playlist_id.clone(),
                mutation_id,
                ApiError::PlaylistMutationInterrupted,
            )),
        };
        let task = tokio::spawn(async move {
            let response = execute(request, execution_route).await;
            completion.complete(response);
        });
        state.playlists.insert(
            playlist_id,
            RunningPlaylistMutation {
                scheduler_id,
                abort: task.abort_handle(),
            },
        );
    }

    fn finish_playlist(&self, data: PlaylistTaskCompletionData, response: ApiResponse) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !state
            .playlists
            .get(&data.playlist_id)
            .is_some_and(|running| running.scheduler_id == data.scheduler_id)
        {
            return;
        }
        state
            .playlists
            .remove(&data.playlist_id)
            .expect("matching playlist mutation exists");
        drop(state);
        data.context
            .api
            .with_current_route(data.operation, &data.route, || {
                observe_playlists(&data.context.api, &response);
                publish_api_response(&data.context, response);
            });
    }

    fn pump_locked(
        self: &Arc<Self>,
        state: &mut ApiMutationSchedulerState,
        identity: PlaybackSessionId,
        context: &ApiDispatchContext,
    ) {
        loop {
            let Some(index) = state
                .sessions
                .get(&identity)
                .and_then(PlaybackSessionQueue::next_ready)
            else {
                return;
            };
            let (route, mutation) = {
                let session = state
                    .sessions
                    .get_mut(&identity)
                    .expect("ready playback session exists");
                let mutation = session
                    .pending
                    .remove(index)
                    .expect("ready playback mutation exists");
                (session.route.clone(), mutation)
            };
            let mutation_id = mutation.id;
            let running_lanes = mutation.lanes.clone();
            let scheduler = Arc::clone(self);
            let task_context = context.clone();
            let task = tokio::spawn(async move {
                let selected = Ok((route.source, Arc::clone(&route.client)));
                let response = handle_routed(mutation.request, selected).await;
                scheduler.complete(identity, mutation_id, task_context, response);
            });
            state
                .sessions
                .get_mut(&identity)
                .expect("running playback session exists")
                .running
                .insert(
                    mutation_id,
                    RunningPlaybackMutation {
                        lanes: running_lanes,
                        abort: task.abort_handle(),
                    },
                );
        }
    }

    fn retire_stale(&self, api: &ApiGateway) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        self.retire_stale_locked(&mut state, api);
    }

    fn retire_stale_locked(&self, state: &mut ApiMutationSchedulerState, api: &ApiGateway) {
        let stale: Vec<_> = state
            .sessions
            .iter()
            .filter_map(|(identity, session)| {
                (!api.route_is_current(Operation::Playback, &session.route)).then_some(*identity)
            })
            .collect();
        for identity in stale {
            Self::retire_session_locked(state, identity);
        }
    }

    fn retire_all(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for (_, session) in state.sessions.drain() {
            for running in session.running.into_values() {
                running.abort.abort();
            }
        }
        for running in state.playlists.drain().map(|(_, running)| running) {
            running.abort.abort();
        }
    }

    fn retire_session_locked(state: &mut ApiMutationSchedulerState, identity: PlaybackSessionId) {
        if let Some(session) = state.sessions.remove(&identity) {
            for running in session.running.into_values() {
                running.abort.abort();
            }
        }
    }

    #[cfg(test)]
    fn counts(&self) -> (usize, usize) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.sessions.values().fold((0, 0), |counts, session| {
            (
                counts.0 + session.running.len(),
                counts.1 + session.pending.len(),
            )
        })
    }

    #[cfg(test)]
    fn playlist_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .playlists
            .len()
    }

    #[cfg(test)]
    fn abort_playlist(&self, playlist_id: &str) -> bool {
        let abort = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .playlists
            .get(playlist_id)
            .map(|running| running.abort.clone());
        if let Some(abort) = abort {
            abort.abort();
            true
        } else {
            false
        }
    }
}

fn dispatch_api(
    context: ApiDispatchContext,
    api_mutations: &Arc<ApiMutationScheduler>,
    request: ApiRequest,
) {
    if let Some(lanes) = request.playback_mutation_lanes() {
        api_mutations.enqueue_playback(context, request, lanes);
    } else if request.playlist_mutation().is_some() {
        api_mutations.enqueue_playlist(context, request);
    } else {
        tokio::spawn(async move {
            dispatch_one(&context, request, None).await;
        });
    }
}

async fn dispatch_one(
    context: &ApiDispatchContext,
    request: ApiRequest,
    selected: Option<RoutedClient>,
) {
    let background = request.background();
    let _background_permit = if background {
        Arc::clone(&context.background_api)
            .acquire_owned()
            .await
            .ok()
    } else {
        None
    };
    let response = match selected {
        Some(selected) => handle_routed(request, selected).await,
        None => handle(&context.api, request).await,
    };
    publish_api_response(context, response);
}

fn publish_api_response(context: &ApiDispatchContext, response: ApiResponse) {
    if let Some(ApiError::SignInExpired {
        api_source,
        session_generation,
    }) = response.error()
    {
        if !context.api.is_current(*api_source, *session_generation) {
            return;
        }
        let _ = context.commands.send(Command::WebSessionExpired {
            source: *api_source,
            generation: *session_generation,
        });
    }
    if let ApiResponse::Me(result) = &response {
        let premium = result
            .as_ref()
            .ok()
            .and_then(|user| user.product.as_deref())
            .map(|product| product == "premium");
        let _ = context.commands.send(Command::AccountChecked { premium });
    }
    let _ = context.events.send(Event::Api(Box::new(response)));
    context.waker.wake();
}

fn playback_backpressure_response(request: ApiRequest) -> ApiResponse {
    let error = ApiError::PlaybackBackpressure;
    match request {
        ApiRequest::Remote { action, .. } => ApiResponse::Remote {
            action,
            result: Err(error),
        },
        ApiRequest::Transfer { device_id, .. } => ApiResponse::Transferred {
            device_id,
            result: Err(error),
        },
        ApiRequest::PlayWithShuffle { .. } => ApiResponse::Remote {
            action: RemoteAction::Play,
            result: Err(error),
        },
        ApiRequest::AddToQueue { label, .. } => ApiResponse::QueueAdded {
            label,
            result: Err(error),
        },
        _ => unreachable!("only playback mutations enter the bounded scheduler"),
    }
}

fn playlist_mutation_error_response(
    playlist_id: String,
    mutation_id: u64,
    error: ApiError,
) -> ApiResponse {
    ApiResponse::PlaylistItemsChanged {
        mutation_id,
        id: playlist_id,
        message: String::new(),
        result: Err(error),
    }
}

fn friendly_connect_error(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    let lower = text.to_lowercase();
    if lower.contains("badcredentials") || lower.contains("bad credentials") {
        "Spotify rejected the saved sign-in. Please sign in again.".to_string()
    } else if lower.contains("premium") {
        PREMIUM_NEEDED.to_string()
    } else if lower.contains("dns") || lower.contains("connect") || lower.contains("resolve") {
        format!("Couldn't reach Spotify: {text}")
    } else {
        text
    }
}

fn operation_for(api: &ApiGateway, request: &ApiRequest) -> Operation {
    match request {
        ApiRequest::Me => Operation::CanonicalAccount,
        ApiRequest::Devices
        | ApiRequest::PlaybackState { .. }
        | ApiRequest::Queue
        | ApiRequest::Remote { .. }
        | ApiRequest::Transfer { .. }
        | ApiRequest::PlayWithShuffle { .. }
        | ApiRequest::AddToQueue { .. } => Operation::Playback,
        ApiRequest::RecentlyPlayed
        | ApiRequest::TopTracks { .. }
        | ApiRequest::TopArtists
        | ApiRequest::SavedTracks { .. }
        | ApiRequest::SavedAlbums { .. }
        | ApiRequest::FollowedArtists { .. }
        | ApiRequest::SavedShows { .. }
        | ApiRequest::SavedEpisodes { .. }
        | ApiRequest::SetSaved { .. } => Operation::UserData,
        ApiRequest::Contains { uris, .. }
            if uris.iter().any(|uri| uri.starts_with("spotify:playlist:")) =>
        {
            Operation::UnsupportedDevelopmentMode
        }
        ApiRequest::Contains { .. } => Operation::UserData,
        ApiRequest::MyPlaylists { .. } => Operation::PlaylistLibrary,
        ApiRequest::CreatePlaylist { .. } => Operation::PlaylistCreation,
        ApiRequest::Discover { .. } | ApiRequest::Search { .. } => Operation::PlaylistSearch,
        ApiRequest::Playlist { id } => Operation::PlaylistMetadata(api.playlist_access(id)),
        ApiRequest::PlaylistItems { id, .. } | ApiRequest::PlaylistSample { id, .. } => {
            Operation::PlaylistItems(api.playlist_access(id))
        }
        ApiRequest::UpdatePlaylist { id, .. } | ApiRequest::FollowPlaylist { id, .. } => {
            Operation::PlaylistMutation(api.playlist_access(id))
        }
        ApiRequest::AddToPlaylist { playlist_id, .. }
        | ApiRequest::RemoveFromPlaylist { playlist_id, .. }
        | ApiRequest::ReorderPlaylist { playlist_id, .. } => {
            Operation::PlaylistMutation(api.playlist_access(playlist_id))
        }
        ApiRequest::Recommendations { .. }
        | ApiRequest::ArtistTopTracks { .. }
        | ApiRequest::RelatedArtists { .. } => Operation::UnsupportedDevelopmentMode,
        ApiRequest::Artist { .. }
        | ApiRequest::ArtistAlbums { .. }
        | ApiRequest::Album { .. }
        | ApiRequest::AlbumTracks { .. }
        | ApiRequest::Show { .. }
        | ApiRequest::ShowEpisodes { .. }
        | ApiRequest::Track { .. } => Operation::Catalog,
    }
}

fn observe_playlists(api: &ApiGateway, response: &ApiResponse) {
    match response {
        ApiResponse::Discover {
            result: Ok(playlists),
            ..
        } => api.observe_playlists(playlists),
        ApiResponse::MyPlaylists {
            result: Ok(page), ..
        } => api.observe_playlists(&page.items),
        ApiResponse::Playlist {
            result: Ok(playlist),
            ..
        }
        | ApiResponse::PlaylistCreated(Ok(playlist)) => api.observe_playlist(playlist),
        ApiResponse::Search {
            result: Ok(results),
            ..
        } => {
            if let Some(playlists) = &results.playlists {
                api.observe_playlists(&playlists.items);
            }
        }
        ApiResponse::Playlist {
            id,
            result: Err(error),
        }
        | ApiResponse::PlaylistItems {
            id,
            result: Err(error),
            ..
        }
        | ApiResponse::PlaylistSample {
            id,
            result: Err(error),
        }
        | ApiResponse::PlaylistUpdated {
            id,
            result: Err(error),
        }
        | ApiResponse::PlaylistItemsChanged {
            id,
            result: Err(error),
            ..
        }
        | ApiResponse::PlaylistFollowChanged {
            id,
            result: Err(error),
            ..
        } if error.status() == Some(403) => {
            api.invalidate_playlist_access(&PlaylistId::new(id.clone()));
        }
        _ => {}
    }
}

async fn handle(api: &ApiGateway, request: ApiRequest) -> ApiResponse {
    let selected = api.client_for(operation_for(api, &request));
    let response = handle_routed(request, selected).await;
    observe_playlists(api, &response);
    response
}

async fn handle_routed(request: ApiRequest, selected: RoutedClient) -> ApiResponse {
    macro_rules! routed {
        ($method:ident($($argument:expr),* $(,)?)) => {
            match &selected {
                Ok((_, client)) => client.$method($($argument),*).await,
                Err(error) => Err(error.clone()),
            }
        };
    }

    match request {
        ApiRequest::Me => ApiResponse::Me(routed!(me())),
        ApiRequest::Devices => ApiResponse::Devices(routed!(devices())),
        ApiRequest::PlaybackState { seq } => ApiResponse::PlaybackState {
            seq,
            result: routed!(playback_state()),
        },
        ApiRequest::Queue => ApiResponse::Queue(routed!(queue())),
        ApiRequest::RecentlyPlayed => {
            ApiResponse::RecentlyPlayed(routed!(recently_played(50)).map(|page| page.items))
        }
        ApiRequest::TopTracks { offset, full } => ApiResponse::TopTracks {
            result: routed!(top_tracks("short_term", if full { 50 } else { 20 }, offset)),
            offset,
            full,
        },
        ApiRequest::TopArtists => {
            ApiResponse::TopArtists(routed!(top_artists("medium_term", 20)).map(|page| page.items))
        }
        ApiRequest::Recommendations {
            seed_tracks,
            seed_artists,
        } => {
            ApiResponse::Recommendations(routed!(recommendations(&seed_tracks, &seed_artists, 20)))
        }
        ApiRequest::Discover { term } => {
            let result = routed!(search(&term, &["playlist"]))
                .map(|results| results.playlists.map(|page| page.items).unwrap_or_default());
            ApiResponse::Discover { term, result }
        }
        ApiRequest::MyPlaylists { offset } => ApiResponse::MyPlaylists {
            offset,
            result: routed!(my_playlists(offset, 50)),
        },
        ApiRequest::Playlist { id } => ApiResponse::Playlist {
            result: routed!(playlist(&id)),
            id,
        },
        ApiRequest::PlaylistItems { id, offset } => ApiResponse::PlaylistItems {
            result: routed!(playlist_items(&id, offset, 100)),
            id,
            offset,
        },
        ApiRequest::PlaylistSample { id, offset } => ApiResponse::PlaylistSample {
            result: routed!(playlist_items(&id, offset, 100)),
            id,
        },
        ApiRequest::CreatePlaylist {
            name,
            public,
            description,
        } => ApiResponse::PlaylistCreated(routed!(create_playlist(&name, public, &description))),
        ApiRequest::UpdatePlaylist {
            id,
            name,
            description,
            public,
        } => ApiResponse::PlaylistUpdated {
            result: routed!(update_playlist(
                &id,
                name.as_deref(),
                description.as_deref(),
                public
            )),
            id,
        },
        ApiRequest::AddToPlaylist {
            mutation_id,
            playlist_id,
            playlist_name,
            uris,
        } => ApiResponse::PlaylistItemsChanged {
            mutation_id,
            result: routed!(add_playlist_items(&playlist_id, &uris, None)),
            id: playlist_id,
            message: format!("Added to {playlist_name}"),
        },
        ApiRequest::RemoveFromPlaylist {
            mutation_id,
            playlist_id,
            uris,
            snapshot_id,
        } => ApiResponse::PlaylistItemsChanged {
            mutation_id,
            result: routed!(remove_playlist_items(
                &playlist_id,
                &uris,
                snapshot_id.as_deref()
            )),
            id: playlist_id,
            message: "Removed from playlist".to_string(),
        },
        ApiRequest::ReorderPlaylist {
            mutation_id,
            playlist_id,
            range_start,
            insert_before,
            snapshot_id,
        } => ApiResponse::PlaylistItemsChanged {
            mutation_id,
            result: routed!(reorder_playlist(
                &playlist_id,
                range_start,
                insert_before,
                snapshot_id.as_deref()
            )),
            id: playlist_id,
            message: String::new(),
        },
        ApiRequest::FollowPlaylist { id, follow } => ApiResponse::PlaylistFollowChanged {
            result: if follow {
                routed!(follow_playlist(&id))
            } else {
                routed!(unfollow_playlist(&id))
            },
            id,
            followed: follow,
        },
        ApiRequest::SavedTracks { offset } => ApiResponse::SavedTracks {
            offset,
            result: routed!(saved_tracks(offset, 50)),
        },
        ApiRequest::SavedAlbums { offset } => ApiResponse::SavedAlbums {
            offset,
            result: routed!(saved_albums(offset, 50)),
        },
        ApiRequest::FollowedArtists { after } => ApiResponse::FollowedArtists {
            result: routed!(followed_artists(after.as_deref(), 50)),
            after,
        },
        ApiRequest::SavedShows { offset } => ApiResponse::SavedShows {
            offset,
            result: routed!(saved_shows(offset, 50)),
        },
        ApiRequest::SavedEpisodes { offset } => ApiResponse::SavedEpisodes {
            offset,
            result: routed!(saved_episodes(offset, 50)),
        },
        ApiRequest::SetSaved { uris, saved } => ApiResponse::SavedChanged {
            result: if saved {
                routed!(save(&uris))
            } else {
                routed!(unsave(&uris))
            },
            uris,
            saved,
        },
        ApiRequest::Contains { uris, user_id } => ApiResponse::Contains {
            result: routed!(contains(&uris, &user_id)),
            uris,
        },
        ApiRequest::Search { query, serial } => ApiResponse::Search {
            result: routed!(search(
                &query,
                &["track", "artist", "album", "playlist", "show", "episode"]
            )),
            query,
            serial,
        },
        ApiRequest::Artist { id } => ApiResponse::Artist {
            result: routed!(artist(&id)),
            id,
        },
        ApiRequest::ArtistTopTracks { id, name } => ApiResponse::ArtistTopTracks {
            result: routed!(artist_top_tracks(&id, &name)),
            id,
        },
        ApiRequest::ArtistAlbums { id, groups, offset } => ApiResponse::ArtistAlbums {
            result: routed!(artist_albums(&id, &groups, offset, 50)),
            id,
            groups,
            offset,
        },
        ApiRequest::RelatedArtists { id } => ApiResponse::RelatedArtists {
            result: routed!(related_artists(&id)),
            id,
        },
        ApiRequest::Album { id } => ApiResponse::Album {
            result: routed!(album(&id)),
            id,
        },
        ApiRequest::AlbumTracks { id, offset } => ApiResponse::AlbumTracks {
            result: routed!(album_tracks(&id, offset, 50)),
            id,
            offset,
        },
        ApiRequest::Show { id } => ApiResponse::Show {
            result: routed!(show(&id)),
            id,
        },
        ApiRequest::ShowEpisodes { id, offset } => ApiResponse::ShowEpisodes {
            result: routed!(show_episodes(&id, offset, 50)),
            id,
            offset,
        },
        ApiRequest::Track { id } => ApiResponse::Track {
            result: routed!(track(&id)),
            id,
        },
        ApiRequest::Remote {
            action,
            device_id,
            play,
            position_ms,
            percent,
            flag,
            repeat,
        } => {
            let device = device_id.as_deref();
            let result = match action {
                RemoteAction::Play => routed!(play(device, play.as_ref())),
                RemoteAction::Pause => routed!(pause(device)),
                RemoteAction::Next => routed!(next(device)),
                RemoteAction::Previous => routed!(previous(device)),
                RemoteAction::Seek => routed!(seek(position_ms, device)),
                RemoteAction::Volume => routed!(set_volume(percent, device)),
                RemoteAction::Shuffle => routed!(set_shuffle(flag, device)),
                RemoteAction::Repeat => routed!(set_repeat(&repeat, device)),
            };
            ApiResponse::Remote { action, result }
        }
        ApiRequest::PlayWithShuffle {
            device_id,
            play,
            shuffle,
        } => {
            let device = device_id.as_deref();
            let result = match routed!(set_shuffle(shuffle, device)) {
                Ok(()) => routed!(play(device, Some(&play))),
                Err(error) => Err(error),
            };
            ApiResponse::Remote {
                action: RemoteAction::Play,
                result,
            }
        }
        ApiRequest::Transfer { device_id, play } => ApiResponse::Transferred {
            result: routed!(transfer(&device_id, play)),
            device_id,
        },
        ApiRequest::AddToQueue {
            uri,
            device_id,
            label,
        } => ApiResponse::QueueAdded {
            result: routed!(add_to_queue(&uri, device_id.as_deref())),
            label,
        },
    }
}

#[cfg(test)]
pub(crate) async fn handle_for_transport_test(
    api: &ApiGateway,
    request: ApiRequest,
) -> ApiResponse {
    handle(api, request).await
}

/// Exercises the production dispatcher while a transport test controls when
/// each loopback response is released.
#[cfg(test)]
pub(crate) struct TransportDispatcher {
    context: ApiDispatchContext,
    api_mutations: Arc<ApiMutationScheduler>,
}

#[cfg(test)]
impl TransportDispatcher {
    pub(crate) fn new(api: Arc<ApiGateway>) -> (Self, std::sync::mpsc::Receiver<Event>) {
        let (events, replies) = std::sync::mpsc::channel();
        let (commands, _command_replies) = mpsc::unbounded_channel();
        let context = ApiDispatchContext {
            api,
            background_api: Arc::new(Semaphore::new(4)),
            events,
            commands,
            waker: Waker::default(),
        };
        (
            Self {
                context,
                api_mutations: Arc::new(ApiMutationScheduler::default()),
            },
            replies,
        )
    }

    pub(crate) fn dispatch(&self, request: ApiRequest) {
        dispatch_api(self.context.clone(), &self.api_mutations, request);
    }

    pub(crate) fn dispatch_panicking_playlist(&self, request: ApiRequest) {
        self.api_mutations.enqueue_playlist_with(
            self.context.clone(),
            request,
            |_request, _route| async move { panic!("injected playlist task panic") },
        );
    }

    pub(crate) fn dispatch_pending_playlist(&self, request: ApiRequest) {
        self.api_mutations.enqueue_playlist_with(
            self.context.clone(),
            request,
            |_request, _route| std::future::pending(),
        );
    }

    pub(crate) fn abort_playlist(&self, playlist_id: &str) -> bool {
        self.api_mutations.abort_playlist(playlist_id)
    }

    pub(crate) fn retire_stale(&self) {
        self.api_mutations.retire_stale(&self.context.api);
    }

    pub(crate) fn counts(&self) -> (usize, usize) {
        self.api_mutations.counts()
    }

    pub(crate) fn playlist_count(&self) -> usize {
        self.api_mutations.playlist_count()
    }

    pub(crate) fn shutdown(&self) {
        self.api_mutations.retire_all();
    }
}

#[cfg(test)]
pub(crate) fn dispatch_for_transport_test(
    api: Arc<ApiGateway>,
    requests: Vec<ApiRequest>,
) -> std::sync::mpsc::Receiver<Event> {
    let (dispatcher, replies) = TransportDispatcher::new(api);
    for request in requests {
        dispatcher.dispatch(request);
    }
    replies
}

#[cfg(test)]
mod playback_scheduler_tests {
    use super::*;
    use crate::api::test_support::{
        DelayedResponses, ObservedRequest, read_request, write_response,
    };
    use std::io::Write as _;
    use std::net::{Ipv4Addr, TcpListener};

    fn authorized_gateway(port: u16, personal: bool) -> Arc<ApiGateway> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("HTTP client");
        let api = Arc::new(ApiGateway::new_at(
            http,
            Arc::new(NetActivity::default()),
            &format!("http://127.0.0.1:{port}/v1"),
        ));
        let shared = api.begin_verification(ApiSource::Shared, |_| {
            TokenProvider::Fixed("shared-token".into())
        });
        api.install(ApiSource::Shared, shared, AccountId::new("same"))
            .expect("shared session");
        if personal {
            let generation = api.begin_verification(ApiSource::Personal, |_| {
                TokenProvider::Fixed("personal-token".into())
            });
            api.install(ApiSource::Personal, generation, AccountId::new("same"))
                .expect("personal session");
        }
        api
    }

    fn transfer(device_id: &str) -> ApiRequest {
        ApiRequest::Transfer {
            device_id: device_id.into(),
            play: true,
        }
    }

    fn remote(action: RemoteAction, device_id: Option<&str>) -> ApiRequest {
        ApiRequest::Remote {
            action,
            device_id: device_id.map(str::to_owned),
            play: None,
            position_ms: 0,
            percent: 0,
            flag: false,
            repeat: String::new(),
        }
    }

    fn add_to_playlist(playlist_id: &str, mutation_id: u64) -> ApiRequest {
        ApiRequest::AddToPlaylist {
            mutation_id,
            playlist_id: playlist_id.into(),
            playlist_name: playlist_id.into(),
            uris: vec!["spotify:track:one".into()],
        }
    }

    fn observe_owned_playlist(api: &ApiGateway, playlist_id: &str) {
        api.observe_playlist(&Playlist {
            id: playlist_id.into(),
            uri: format!("spotify:playlist:{playlist_id}"),
            owner: Owner {
                id: Some("same".into()),
                ..Owner::default()
            },
            ..Playlist::default()
        });
    }

    fn request_label(request: &ObservedRequest) -> String {
        if request.request_line == "PUT /v1/me/player HTTP/1.1" {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .expect("transfer body")["device_ids"][0]
                .as_str()
                .expect("transfer destination")
                .to_owned()
        } else {
            request.request_line.clone()
        }
    }

    async fn delayed_pair(first: ApiRequest, second: ApiRequest) -> (Vec<ObservedRequest>, usize) {
        let server = DelayedResponses::start(2);
        let replies =
            dispatch_for_transport_test(authorized_gateway(server.port, true), vec![first, second]);
        let observed = server.observe().await;
        tokio::task::spawn_blocking(move || {
            for _ in 0..2 {
                assert!(matches!(
                    replies.recv_timeout(Duration::from_secs(2)),
                    Ok(Event::Api(_))
                ));
            }
        })
        .await
        .expect("reply collector joins");
        observed
    }

    async fn next_reply(replies: &std::sync::mpsc::Receiver<Event>) -> Event {
        for _ in 0..200 {
            match replies.try_recv() {
                Ok(event) => return event,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    panic!("playlist reply channel disconnected")
                }
            }
        }
        panic!("playlist reply did not arrive")
    }

    #[tokio::test]
    async fn playlist_mutations_are_single_flight_per_playlist_at_the_wire() {
        let server = DelayedResponses::start(1);
        let api = authorized_gateway(server.port, true);
        observe_owned_playlist(&api, "mix");
        let (dispatcher, replies) = TransportDispatcher::new(api);
        dispatcher.dispatch(add_to_playlist("mix", 1));
        dispatcher.dispatch(add_to_playlist("mix", 2));
        assert_eq!(dispatcher.playlist_count(), 1);

        let (observed, arrived_early) = server.observe().await;
        assert_eq!(arrived_early, 0);
        assert_eq!(observed.len(), 1);
        assert_eq!(
            observed[0].request_line,
            "POST /v1/playlists/mix/items HTTP/1.1"
        );
        assert_eq!(
            observed[0].authorization.as_deref(),
            Some("Bearer personal-token")
        );

        let results = tokio::task::spawn_blocking(move || {
            (0..2)
                .map(|_| replies.recv_timeout(Duration::from_secs(2)))
                .collect::<Vec<_>>()
        })
        .await
        .expect("reply collector joins");
        assert!(results.iter().any(|event| {
            matches!(
                event,
                Ok(Event::Api(response))
                    if matches!(
                        response.as_ref(),
                        ApiResponse::PlaylistItemsChanged {
                            mutation_id: 1,
                            result: Ok(_),
                            ..
                        }
                    )
            )
        }));
        assert!(results.iter().any(|event| {
            matches!(
                event,
                Ok(Event::Api(response))
                    if matches!(
                        response.as_ref(),
                        ApiResponse::PlaylistItemsChanged {
                            mutation_id: 2,
                            result: Err(ApiError::PlaylistMutationPending),
                            ..
                        }
                    )
            )
        }));
        assert_eq!(dispatcher.playlist_count(), 0);
        dispatcher.shutdown();
    }

    #[tokio::test]
    async fn unrelated_playlist_mutations_remain_concurrent() {
        let server = DelayedResponses::start(2);
        let api = authorized_gateway(server.port, true);
        observe_owned_playlist(&api, "mix-a");
        observe_owned_playlist(&api, "mix-b");
        let replies = dispatch_for_transport_test(
            api,
            vec![add_to_playlist("mix-a", 1), add_to_playlist("mix-b", 2)],
        );
        let (observed, arrived_early) = server.observe().await;
        assert_eq!(arrived_early, 1);
        assert_eq!(observed.len(), 2);
        tokio::task::spawn_blocking(move || {
            for _ in 0..2 {
                assert!(matches!(
                    replies.recv_timeout(Duration::from_secs(2)),
                    Ok(Event::Api(_))
                ));
            }
        })
        .await
        .expect("reply collector joins");
    }

    #[tokio::test]
    async fn distinct_playlist_mutations_are_globally_bounded_before_task_spawn() {
        let server = DelayedResponses::start(MAX_RUNNING_PLAYLIST_MUTATIONS);
        let api = authorized_gateway(server.port, true);
        let (dispatcher, replies) = TransportDispatcher::new(Arc::clone(&api));
        for index in 0..=MAX_RUNNING_PLAYLIST_MUTATIONS {
            let playlist_id = format!("mix-{index}");
            observe_owned_playlist(&api, &playlist_id);
            dispatcher.dispatch(add_to_playlist(&playlist_id, index as u64 + 1));
        }
        assert_eq!(dispatcher.playlist_count(), MAX_RUNNING_PLAYLIST_MUTATIONS);

        assert!(matches!(
            next_reply(&replies).await,
            Event::Api(response)
                if matches!(
                    response.as_ref(),
                    ApiResponse::PlaylistItemsChanged {
                        mutation_id,
                        id,
                        result: Err(ApiError::PlaylistMutationBackpressure),
                        ..
                    } if *mutation_id == MAX_RUNNING_PLAYLIST_MUTATIONS as u64 + 1
                        && id == &format!("mix-{MAX_RUNNING_PLAYLIST_MUTATIONS}")
                )
        ));

        let (observed, arrived_early) = server.observe().await;
        assert_eq!(observed.len(), MAX_RUNNING_PLAYLIST_MUTATIONS);
        assert_eq!(arrived_early + 1, MAX_RUNNING_PLAYLIST_MUTATIONS);
        assert!(observed.iter().all(|request| {
            request.authorization.as_deref() == Some("Bearer personal-token")
                && !request
                    .request_line
                    .contains(&format!("mix-{MAX_RUNNING_PLAYLIST_MUTATIONS}"))
        }));
        for _ in 0..MAX_RUNNING_PLAYLIST_MUTATIONS {
            assert!(matches!(
                next_reply(&replies).await,
                Event::Api(response)
                    if matches!(
                        response.as_ref(),
                        ApiResponse::PlaylistItemsChanged { result: Ok(_), .. }
                    )
            ));
        }
        assert_eq!(dispatcher.playlist_count(), 0);
        dispatcher.shutdown();
    }

    #[tokio::test]
    async fn panic_and_abort_retire_playlist_gates_before_successful_retry() {
        let server = DelayedResponses::start(2);
        let api = authorized_gateway(server.port, true);
        observe_owned_playlist(&api, "panic-mix");
        observe_owned_playlist(&api, "abort-mix");
        let (dispatcher, replies) = TransportDispatcher::new(api);

        dispatcher.dispatch_panicking_playlist(add_to_playlist("panic-mix", 1));
        assert!(matches!(
            next_reply(&replies).await,
            Event::Api(response)
                if matches!(
                    response.as_ref(),
                    ApiResponse::PlaylistItemsChanged {
                        mutation_id: 1,
                        id,
                        result: Err(ApiError::PlaylistMutationInterrupted),
                        ..
                    } if id == "panic-mix"
                )
        ));
        assert_eq!(dispatcher.playlist_count(), 0);

        dispatcher.dispatch_pending_playlist(add_to_playlist("abort-mix", 2));
        assert_eq!(dispatcher.playlist_count(), 1);
        assert!(dispatcher.abort_playlist("abort-mix"));
        assert!(matches!(
            next_reply(&replies).await,
            Event::Api(response)
                if matches!(
                    response.as_ref(),
                    ApiResponse::PlaylistItemsChanged {
                        mutation_id: 2,
                        id,
                        result: Err(ApiError::PlaylistMutationInterrupted),
                        ..
                    } if id == "abort-mix"
                )
        ));
        assert_eq!(dispatcher.playlist_count(), 0);

        dispatcher.dispatch(add_to_playlist("panic-mix", 3));
        dispatcher.dispatch(add_to_playlist("abort-mix", 4));
        let (observed, arrived_early) = server.observe().await;
        assert_eq!(observed.len(), 2);
        assert_eq!(arrived_early, 1);
        let mut completed = Vec::new();
        for _ in 0..2 {
            let Event::Api(response) = next_reply(&replies).await else {
                panic!("playlist retry produced a non-API event");
            };
            let ApiResponse::PlaylistItemsChanged {
                mutation_id,
                result: Ok(_),
                ..
            } = response.as_ref()
            else {
                panic!("playlist retry failed: {response:?}");
            };
            completed.push(*mutation_id);
        }
        completed.sort_unstable();
        assert_eq!(completed, [3, 4]);
        assert_eq!(dispatcher.playlist_count(), 0);
        dispatcher.shutdown();
    }

    #[tokio::test]
    async fn shutdown_aborts_and_retires_every_supervised_playlist_task() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("loopback API");
        let api = authorized_gateway(listener.local_addr().expect("API address").port(), true);
        let (dispatcher, replies) = TransportDispatcher::new(Arc::clone(&api));
        for index in 0..MAX_RUNNING_PLAYLIST_MUTATIONS {
            let playlist_id = format!("pending-{index}");
            observe_owned_playlist(&api, &playlist_id);
            dispatcher.dispatch_pending_playlist(add_to_playlist(&playlist_id, index as u64));
        }
        assert_eq!(dispatcher.playlist_count(), MAX_RUNNING_PLAYLIST_MUTATIONS);

        dispatcher.shutdown();
        assert_eq!(dispatcher.playlist_count(), 0);
        tokio::task::yield_now().await;
        assert!(matches!(
            replies.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn stale_playlist_completion_cannot_reach_a_successor_generation() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("loopback API");
        let port = listener.local_addr().expect("API address").port();
        let (first_tx, first_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (second_tx, second_rx) = tokio::sync::oneshot::channel();
        let server = std::thread::spawn(move || {
            let (first, _) = listener.accept().expect("old playlist mutation");
            first_tx
                .send(read_request(&first))
                .expect("publish old request");
            release_rx.recv().expect("release old request");
            write_response(first, "200 OK", &[], r#"{"snapshot_id":"old"}"#);
            let (second, _) = listener.accept().expect("successor playlist mutation");
            second_tx
                .send(read_request(&second))
                .expect("publish successor request");
            write_response(second, "200 OK", &[], r#"{"snapshot_id":"new"}"#);
        });
        let api = authorized_gateway(port, true);
        observe_owned_playlist(&api, "mix");
        let (dispatcher, replies) = TransportDispatcher::new(Arc::clone(&api));
        dispatcher.dispatch(add_to_playlist("mix", 1));
        let first = first_rx.await.expect("old request arrives");
        assert_eq!(
            first.authorization.as_deref(),
            Some("Bearer personal-token")
        );

        let generation = api.begin_verification(ApiSource::Personal, |_| {
            TokenProvider::Fixed("personal-successor".into())
        });
        api.install(ApiSource::Personal, generation, AccountId::new("same"))
            .expect("successor session");
        dispatcher.retire_stale();
        assert_eq!(dispatcher.playlist_count(), 1);
        release_tx.send(()).expect("release old response");
        for _ in 0..100 {
            if dispatcher.playlist_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(dispatcher.playlist_count(), 0);
        assert!(matches!(
            replies.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        dispatcher.dispatch(add_to_playlist("mix", 2));
        let second = second_rx.await.expect("successor request arrives");
        assert_eq!(
            second.authorization.as_deref(),
            Some("Bearer personal-successor")
        );
        assert!(matches!(
            tokio::task::spawn_blocking(move || replies.recv_timeout(Duration::from_secs(2)))
                .await
                .expect("reply collector joins"),
            Ok(Event::Api(response))
                if matches!(
                    response.as_ref(),
                    ApiResponse::PlaylistItemsChanged {
                        mutation_id: 2,
                        result: Ok(Some(snapshot)),
                        ..
                    } if snapshot == "new"
                )
        ));
        server.join().expect("server joins");
        dispatcher.shutdown();
    }

    struct BlockedFirstServer {
        port: u16,
        first: Option<tokio::sync::oneshot::Receiver<ObservedRequest>>,
        second: Option<tokio::sync::oneshot::Receiver<ObservedRequest>>,
        release: std::sync::mpsc::Sender<()>,
        done: tokio::sync::oneshot::Receiver<usize>,
    }

    impl BlockedFirstServer {
        fn start() -> Self {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("loopback API");
            let port = listener.local_addr().expect("API address").port();
            let (first_tx, first) = tokio::sync::oneshot::channel();
            let (second_tx, second) = tokio::sync::oneshot::channel();
            let (release, release_rx) = std::sync::mpsc::channel();
            let (done_tx, done) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let (mut first_stream, _) = listener.accept().expect("old request");
                first_tx
                    .send(read_request(&first_stream))
                    .expect("publish old request");
                listener.set_nonblocking(true).expect("nonblocking accept");
                let deadline = Instant::now() + Duration::from_secs(3);
                let second_stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "successor request was blocked");
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("accept successor request: {error}"),
                    }
                };
                let second_request = read_request(&second_stream);
                second_tx
                    .send(second_request)
                    .expect("publish successor request");
                write_response(second_stream, "200 OK", &[], "{}");
                release_rx
                    .recv_timeout(Duration::from_secs(3))
                    .expect("release old request");
                let _ = first_stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                );
                let deadline = Instant::now() + Duration::from_millis(250);
                let mut unexpected = 0;
                while Instant::now() < deadline {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = read_request(&stream);
                            write_response(stream, "200 OK", &[], "{}");
                            unexpected += 1;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("probe retired queue: {error}"),
                    }
                }
                let _ = done_tx.send(unexpected);
            });
            Self {
                port,
                first: Some(first),
                second: Some(second),
                release,
                done,
            }
        }

        async fn first(&mut self) -> ObservedRequest {
            self.first
                .take()
                .expect("first request receiver")
                .await
                .expect("first request arrives")
        }

        async fn second(&mut self) -> ObservedRequest {
            self.second
                .take()
                .expect("second request receiver")
                .await
                .expect("second request arrives")
        }

        async fn finish(self) -> usize {
            self.release.send(()).expect("release old response");
            self.done.await.expect("server exits")
        }
    }

    async fn receive_one_api_reply(
        replies: std::sync::mpsc::Receiver<Event>,
    ) -> std::sync::mpsc::Receiver<Event> {
        tokio::task::spawn_blocking(move || {
            assert!(matches!(
                replies.recv_timeout(Duration::from_secs(2)),
                Ok(Event::Api(_))
            ));
            replies
        })
        .await
        .expect("reply collector joins")
    }

    #[tokio::test]
    async fn transfers_share_the_global_active_device_lane_in_both_orders() {
        for destinations in [["speaker-a", "speaker-b"], ["speaker-b", "speaker-a"]] {
            let (observed, arrived_early) =
                delayed_pair(transfer(destinations[0]), transfer(destinations[1])).await;
            assert_eq!(arrived_early, 0, "the second transfer must not race");
            assert_eq!(
                observed.iter().map(request_label).collect::<Vec<_>>(),
                destinations
            );
        }
    }

    #[tokio::test]
    async fn transfer_orders_active_alias_and_destination_mutations() {
        let cases = [
            (
                transfer("speaker-a"),
                remote(RemoteAction::Pause, None),
                [
                    "speaker-a".to_owned(),
                    "PUT /v1/me/player/pause HTTP/1.1".to_owned(),
                ],
            ),
            (
                remote(RemoteAction::Play, None),
                transfer("speaker-a"),
                [
                    "PUT /v1/me/player/play HTTP/1.1".to_owned(),
                    "speaker-a".to_owned(),
                ],
            ),
            (
                transfer("speaker-a"),
                remote(RemoteAction::Pause, Some("speaker-a")),
                [
                    "speaker-a".to_owned(),
                    "PUT /v1/me/player/pause?device_id=speaker-a HTTP/1.1".to_owned(),
                ],
            ),
            (
                remote(RemoteAction::Play, Some("speaker-a")),
                transfer("speaker-a"),
                [
                    "PUT /v1/me/player/play?device_id=speaker-a HTTP/1.1".to_owned(),
                    "speaker-a".to_owned(),
                ],
            ),
        ];
        for (first, second, expected) in cases {
            let (observed, arrived_early) = delayed_pair(first, second).await;
            assert_eq!(arrived_early, 0, "aliased mutations must not race");
            assert_eq!(
                observed.iter().map(request_label).collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn transfer_does_not_block_an_unrelated_explicit_device() {
        let (observed, arrived_early) = delayed_pair(
            transfer("speaker-a"),
            remote(RemoteAction::Pause, Some("speaker-b")),
        )
        .await;
        assert_eq!(arrived_early, 1);
        assert_eq!(observed.len(), 2);
        assert!(
            observed
                .iter()
                .any(|request| request_label(request) == "speaker-a")
        );
        assert!(observed.iter().any(|request| {
            request.request_line == "PUT /v1/me/player/pause?device_id=speaker-b HTTP/1.1"
        }));
    }

    #[tokio::test]
    async fn blocked_transfer_reserves_its_aliases_without_blocking_other_devices() {
        let server = DelayedResponses::start(4);
        let replies = dispatch_for_transport_test(
            authorized_gateway(server.port, true),
            vec![
                remote(RemoteAction::Play, Some("speaker-a")),
                transfer("speaker-a"),
                remote(RemoteAction::Pause, None),
                remote(RemoteAction::Pause, Some("speaker-b")),
            ],
        );
        let (observed, arrived_early) = server.observe().await;
        assert_eq!(arrived_early, 1, "only speaker-b may bypass the wait");
        assert_eq!(
            observed.iter().map(request_label).collect::<Vec<_>>(),
            [
                "PUT /v1/me/player/play?device_id=speaker-a HTTP/1.1",
                "PUT /v1/me/player/pause?device_id=speaker-b HTTP/1.1",
                "speaker-a",
                "PUT /v1/me/player/pause HTTP/1.1",
            ]
        );
        tokio::task::spawn_blocking(move || {
            for _ in 0..4 {
                assert!(matches!(
                    replies.recv_timeout(Duration::from_secs(2)),
                    Ok(Event::Api(_))
                ));
            }
        })
        .await
        .expect("reply collector joins");
    }

    #[tokio::test]
    async fn personal_replacement_retires_running_and_queued_mutations() {
        let mut server = BlockedFirstServer::start();
        let api = authorized_gateway(server.port, true);
        let (dispatcher, replies) = TransportDispatcher::new(Arc::clone(&api));
        dispatcher.dispatch(remote(RemoteAction::Pause, Some("speaker")));
        dispatcher.dispatch(remote(RemoteAction::Play, Some("speaker")));
        let first = server.first().await;
        assert_eq!(
            first.authorization.as_deref(),
            Some("Bearer personal-token")
        );
        assert_eq!(dispatcher.counts(), (1, 1));

        let generation = api.begin_verification(ApiSource::Personal, |_| {
            TokenProvider::Fixed("personal-successor".into())
        });
        api.install(ApiSource::Personal, generation, AccountId::new("same"))
            .expect("successor session");
        dispatcher.retire_stale();
        assert_eq!(dispatcher.counts(), (0, 0));
        dispatcher.dispatch(remote(RemoteAction::Pause, Some("speaker")));

        let second = server.second().await;
        assert_eq!(
            second.authorization.as_deref(),
            Some("Bearer personal-successor")
        );
        assert_eq!(
            second.request_line,
            "PUT /v1/me/player/pause?device_id=speaker HTTP/1.1"
        );
        let replies = receive_one_api_reply(replies).await;
        assert_eq!(
            server.finish().await,
            0,
            "old queued work must be discarded"
        );
        assert!(matches!(
            replies.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        dispatcher.shutdown();
    }

    #[tokio::test]
    async fn signout_retires_shared_work_before_a_new_shared_session() {
        let mut server = BlockedFirstServer::start();
        let api = authorized_gateway(server.port, false);
        let (dispatcher, replies) = TransportDispatcher::new(Arc::clone(&api));
        dispatcher.dispatch(remote(RemoteAction::Pause, Some("speaker")));
        dispatcher.dispatch(remote(RemoteAction::Play, Some("speaker")));
        let first = server.first().await;
        assert_eq!(first.authorization.as_deref(), Some("Bearer shared-token"));

        api.clear_all();
        dispatcher.retire_stale();
        assert_eq!(dispatcher.counts(), (0, 0));
        let generation = api.begin_verification(ApiSource::Shared, |_| {
            TokenProvider::Fixed("shared-successor".into())
        });
        api.install(ApiSource::Shared, generation, AccountId::new("same"))
            .expect("new shared session");
        dispatcher.dispatch(remote(RemoteAction::Pause, Some("speaker")));

        let second = server.second().await;
        assert_eq!(
            second.authorization.as_deref(),
            Some("Bearer shared-successor")
        );
        let replies = receive_one_api_reply(replies).await;
        assert_eq!(server.finish().await, 0);
        assert!(matches!(
            replies.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        dispatcher.shutdown();
    }

    #[tokio::test]
    async fn personal_install_retires_the_previous_shared_playback_route() {
        let mut server = BlockedFirstServer::start();
        let api = authorized_gateway(server.port, false);
        let (dispatcher, replies) = TransportDispatcher::new(Arc::clone(&api));
        dispatcher.dispatch(remote(RemoteAction::Pause, Some("speaker")));
        dispatcher.dispatch(remote(RemoteAction::Play, Some("speaker")));
        let first = server.first().await;
        assert_eq!(first.authorization.as_deref(), Some("Bearer shared-token"));

        let generation = api.begin_verification(ApiSource::Personal, |_| {
            TokenProvider::Fixed("personal-successor".into())
        });
        api.install(ApiSource::Personal, generation, AccountId::new("same"))
            .expect("personal session");
        dispatcher.retire_stale();
        assert_eq!(dispatcher.counts(), (0, 0));
        dispatcher.dispatch(remote(RemoteAction::Pause, Some("speaker")));

        let second = server.second().await;
        assert_eq!(
            second.authorization.as_deref(),
            Some("Bearer personal-successor")
        );
        let replies = receive_one_api_reply(replies).await;
        assert_eq!(server.finish().await, 0);
        assert!(matches!(
            replies.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        dispatcher.shutdown();
    }

    async fn assert_stale_completion_suppressed(status: &'static str, body: &'static str) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("loopback API");
        let port = listener.local_addr().expect("API address").port();
        let (seen_tx, seen) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (done_tx, done) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("old request");
            let request = read_request(&stream);
            seen_tx.send(request).expect("publish request");
            release_rx.recv().expect("release old request");
            write_response(stream, status, &[], body);
            let _ = done_tx.send(());
        });
        let api = authorized_gateway(port, true);
        let (dispatcher, replies) = TransportDispatcher::new(Arc::clone(&api));
        dispatcher.dispatch(remote(RemoteAction::Pause, Some("speaker")));
        let old = seen.await.expect("old request arrives");
        assert_eq!(old.authorization.as_deref(), Some("Bearer personal-token"));

        let generation = api.begin_verification(ApiSource::Personal, |_| {
            TokenProvider::Fixed("personal-successor".into())
        });
        api.install(ApiSource::Personal, generation, AccountId::new("same"))
            .expect("successor session");
        release_tx.send(()).expect("release response");
        done.await.expect("server exits");
        for _ in 0..100 {
            if dispatcher.counts() == (0, 0) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(matches!(
            replies.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert_eq!(dispatcher.counts(), (0, 0));
        dispatcher.shutdown();
    }

    #[tokio::test]
    async fn stale_success_and_ordinary_error_completions_are_both_suppressed() {
        assert_stale_completion_suppressed("200 OK", "{}").await;
        assert_stale_completion_suppressed(
            "500 Internal Server Error",
            r#"{"error":{"status":500,"message":"old failure"}}"#,
        )
        .await;
    }

    #[tokio::test]
    async fn playback_queue_is_bounded_and_retains_the_latest_absolute_mutation() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("loopback API");
        let port = listener.local_addr().expect("API address").port();
        let (first_tx, first) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (observed_tx, observed) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let (first_stream, _) = listener.accept().expect("first request");
            let first_request = read_request(&first_stream);
            first_tx.send(()).expect("first request arrived");
            release_rx.recv().expect("release lane");
            write_response(first_stream, "200 OK", &[], "{}");
            let mut requests = vec![first_request];
            while requests.len() < MAX_PENDING_PLAYBACK_MUTATIONS + 1 {
                let (stream, _) = listener.accept().expect("queued request");
                requests.push(read_request(&stream));
                write_response(stream, "200 OK", &[], "{}");
            }
            observed_tx.send(requests).expect("publish requests");
        });
        let api = authorized_gateway(port, true);
        let (dispatcher, replies) = TransportDispatcher::new(api);
        let total = MAX_PENDING_PLAYBACK_MUTATIONS + 3;
        for percent in 0..total {
            dispatcher.dispatch(ApiRequest::Remote {
                action: RemoteAction::Volume,
                device_id: Some("speaker".into()),
                play: None,
                position_ms: 0,
                percent: u8::try_from(percent).expect("test volume"),
                flag: false,
                repeat: String::new(),
            });
        }
        first.await.expect("first request reaches transport");
        assert_eq!(dispatcher.counts(), (1, MAX_PENDING_PLAYBACK_MUTATIONS));
        release_tx.send(()).expect("release lane");
        let observed = observed.await.expect("all retained requests finish");
        assert_eq!(observed.len(), MAX_PENDING_PLAYBACK_MUTATIONS + 1);
        assert_eq!(
            observed.last().expect("latest request").request_line,
            format!(
                "PUT /v1/me/player/volume?device_id=speaker&volume_percent={} HTTP/1.1",
                total - 1
            )
        );

        let (errors, successes) = tokio::task::spawn_blocking(move || {
            let mut errors = 0;
            let mut successes = 0;
            for _ in 0..total {
                match replies
                    .recv_timeout(Duration::from_secs(3))
                    .expect("one response per submitted mutation")
                {
                    Event::Api(response) => match *response {
                        ApiResponse::Remote {
                            result: Err(ApiError::PlaybackBackpressure),
                            ..
                        } => errors += 1,
                        ApiResponse::Remote { result: Ok(()), .. } => successes += 1,
                        _ => panic!("unexpected bounded scheduler response"),
                    },
                    _ => panic!("unexpected bounded scheduler event"),
                }
            }
            (errors, successes)
        })
        .await
        .expect("reply collector joins");
        assert_eq!(errors, 2);
        assert_eq!(successes, MAX_PENDING_PLAYBACK_MUTATIONS + 1);
        assert_eq!(dispatcher.counts(), (0, 0));
        dispatcher.shutdown();
    }
}

/// Spotify's transcription of the track, when the local session can ask for
/// one. Answers are cached like LRCLIB's, "none" included; `None` falls
/// back to LRCLIB.
async fn spotify_lyrics(
    engine: Option<Arc<Engine>>,
    uri: &str,
    cache_dir: &std::path::Path,
) -> Option<crate::lyrics::Lyrics> {
    let id = uri.strip_prefix("spotify:track:")?;
    let path = cache_dir.join(format!("spotify-{id}.json"));
    if let Some(cached) = crate::lyrics::cached(&path) {
        return cached;
    }
    match engine?.lyrics_json(uri).await {
        Ok(json) => {
            let found = json.as_ref().and_then(crate::lyrics::from_spotify);
            crate::lyrics::store(&path, &found);
            found
        }
        Err(error) => {
            log::debug!("spotify lyrics unavailable: {error:#}");
            None
        }
    }
}

/// A playlist's items on disk, valid for exactly one snapshot.
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedPlaylist {
    snapshot: String,
    items: Vec<PlaylistItem>,
}
