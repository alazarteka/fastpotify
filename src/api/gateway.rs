//! Authoritative routing across independent shared and personal Web API sessions.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use super::ApiSource;
use super::client::{ApiClient, ApiError, BASE_URL, NetActivity, TokenProvider};
use super::models::Playlist;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AccountId(String);

impl AccountId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PlaylistId(String);

impl PlaylistId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl std::borrow::Borrow<str> for PlaylistId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    Unavailable,
    Authorizing,
    Ready { account: AccountId },
}

impl SessionState {
    pub fn account(&self) -> Option<&AccountId> {
        match self {
            Self::Ready { account } => Some(account),
            Self::Unavailable | Self::Authorizing => None,
        }
    }
}

#[derive(Clone, Copy)]
struct ApiProfile {
    source: ApiSource,
    search_limit: u32,
    artist_albums_limit: u32,
}

impl ApiProfile {
    const SHARED: Self = Self {
        source: ApiSource::Shared,
        search_limit: 20,
        artist_albums_limit: 50,
    };
    const PERSONAL: Self = Self {
        source: ApiSource::Personal,
        search_limit: 10,
        artist_albums_limit: 10,
    };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlaylistAccess {
    Owned,
    Collaborative,
    External,
    #[default]
    Unknown,
}

/// A logical API capability, selected before any request leaves the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    CanonicalAccount,
    Playback,
    UserData,
    PlaylistLibrary,
    PlaylistCreation,
    PlaylistSearch,
    Catalog,
    PlaylistMetadata(PlaylistAccess),
    PlaylistItems(PlaylistAccess),
    PlaylistMutation(PlaylistAccess),
    UnsupportedDevelopmentMode,
}

/// Unknown playlists stay on the shared app until a response proves that the
/// listener owns or collaborates on them. A request is never retried through
/// the other application after this decision has been made.
fn plan(operation: Operation, personal_ready: bool) -> ApiSource {
    use Operation::*;
    match operation {
        CanonicalAccount | PlaylistLibrary | PlaylistSearch | UnsupportedDevelopmentMode => {
            ApiSource::Shared
        }
        PlaylistMetadata(PlaylistAccess::External | PlaylistAccess::Unknown)
        | PlaylistItems(PlaylistAccess::External | PlaylistAccess::Unknown)
        | PlaylistMutation(PlaylistAccess::External | PlaylistAccess::Unknown) => ApiSource::Shared,
        PlaylistMetadata(_) | PlaylistItems(_) | PlaylistMutation(_) if personal_ready => {
            ApiSource::Personal
        }
        Playback | UserData | PlaylistCreation | Catalog if personal_ready => ApiSource::Personal,
        Playback | UserData | PlaylistCreation | Catalog | PlaylistMetadata(_)
        | PlaylistItems(_) | PlaylistMutation(_) => ApiSource::Shared,
    }
}

fn classify_playlist(account: &AccountId, playlist: &Playlist) -> PlaylistAccess {
    if playlist.owner.id.as_deref() == Some(account.as_str()) {
        PlaylistAccess::Owned
    } else if playlist.collaborative {
        PlaylistAccess::Collaborative
    } else if playlist.owner.id.is_some() {
        PlaylistAccess::External
    } else {
        PlaylistAccess::Unknown
    }
}

struct Session {
    live: RwLock<LiveSession>,
    http: reqwest::Client,
    activity: Arc<NetActivity>,
    profile: ApiProfile,
    base_url: String,
}

struct LiveSession {
    generation: u64,
    state: SessionState,
    client: Arc<ApiClient>,
}

impl Session {
    fn new(
        http: reqwest::Client,
        activity: Arc<NetActivity>,
        profile: ApiProfile,
        base_url: &str,
    ) -> Self {
        let client = Self::make_client(&http, &activity, profile, base_url);
        Self {
            live: RwLock::new(LiveSession {
                generation: 0,
                state: SessionState::Unavailable,
                client,
            }),
            http,
            activity,
            profile,
            base_url: base_url.to_owned(),
        }
    }

    fn make_client(
        http: &reqwest::Client,
        activity: &Arc<NetActivity>,
        profile: ApiProfile,
        base_url: &str,
    ) -> Arc<ApiClient> {
        Arc::new(ApiClient::new_at(
            http.clone(),
            Arc::clone(activity),
            profile.search_limit,
            profile.artist_albums_limit,
            profile.source,
            base_url,
        ))
    }

    fn fresh_client(&self) -> Arc<ApiClient> {
        Self::make_client(&self.http, &self.activity, self.profile, &self.base_url)
    }

    fn client(&self) -> Arc<ApiClient> {
        Arc::clone(
            &self
                .live
                .read()
                .unwrap_or_else(|lock| lock.into_inner())
                .client,
        )
    }

    fn ready_client(&self) -> Option<(u64, Arc<ApiClient>)> {
        let live = self.live.read().unwrap_or_else(|lock| lock.into_inner());
        matches!(live.state, SessionState::Ready { .. })
            .then(|| (live.generation, Arc::clone(&live.client)))
    }

    fn begin(&self, provider: impl FnOnce(u64) -> TokenProvider) -> u64 {
        let mut live = self.live.write().unwrap_or_else(|lock| lock.into_inner());
        let generation = live
            .generation
            .checked_add(1)
            .expect("Spotify session generation exhausted");
        let client = self.fresh_client();
        client.set_token_provider(Some(provider(generation)));
        let previous = std::mem::replace(&mut live.client, client);
        live.generation = generation;
        live.state = SessionState::Authorizing;
        // Route selection cannot observe the replacement until the old
        // provider has been synchronously retired.
        previous.set_token_provider(None);
        generation
    }

    fn clear(&self) {
        let client = self.fresh_client();
        let mut live = self.live.write().unwrap_or_else(|lock| lock.into_inner());
        let previous = std::mem::replace(&mut live.client, client);
        live.generation = live
            .generation
            .checked_add(1)
            .expect("Spotify session generation exhausted");
        live.state = SessionState::Unavailable;
        previous.set_token_provider(None);
    }

    fn clear_if_generation(&self, generation: u64) -> bool {
        let client = self.fresh_client();
        let mut live = self.live.write().unwrap_or_else(|lock| lock.into_inner());
        if live.generation != generation {
            return false;
        }
        let previous = std::mem::replace(&mut live.client, client);
        live.generation = live
            .generation
            .checked_add(1)
            .expect("Spotify session generation exhausted");
        live.state = SessionState::Unavailable;
        previous.set_token_provider(None);
        true
    }

    fn state(&self) -> SessionState {
        self.live
            .read()
            .unwrap_or_else(|lock| lock.into_inner())
            .state
            .clone()
    }

    #[cfg(test)]
    fn set_state(&self, state: SessionState) {
        self.live
            .write()
            .unwrap_or_else(|lock| lock.into_inner())
            .state = state;
    }

    fn set_ready_if_current(&self, generation: u64, account: AccountId) -> bool {
        let mut live = self.live.write().unwrap_or_else(|lock| lock.into_inner());
        if live.generation != generation || !matches!(live.state, SessionState::Authorizing) {
            return false;
        }
        live.state = SessionState::Ready { account };
        true
    }

    fn generation(&self) -> u64 {
        self.live
            .read()
            .unwrap_or_else(|lock| lock.into_inner())
            .generation
    }

    fn is_ready_generation(&self, generation: u64) -> bool {
        let live = self.live.read().unwrap_or_else(|lock| lock.into_inner());
        live.generation == generation && matches!(live.state, SessionState::Ready { .. })
    }

    #[cfg(test)]
    fn next_generation(&self) -> u64 {
        let mut live = self.live.write().unwrap_or_else(|lock| lock.into_inner());
        live.generation = live
            .generation
            .checked_add(1)
            .expect("Spotify session generation exhausted");
        live.generation
    }
}

/// One atomic routing decision, including the client whose mutable transport
/// state belongs only to this authorization generation.
#[derive(Clone)]
pub(crate) struct ApiRoute {
    pub source: ApiSource,
    pub generation: u64,
    pub revision: u64,
    pub client: Arc<ApiClient>,
}

pub struct ApiGateway {
    shared: Session,
    personal: Session,
    /// Linearizes session transitions with route selection and guarded event
    /// publication. The revision gives queued work a compact route identity.
    routing: RwLock<()>,
    route_revision: AtomicU64,
    playlist_access: Mutex<HashMap<PlaylistId, PlaylistAccess>>,
}

impl ApiGateway {
    pub fn new(http: reqwest::Client, activity: Arc<NetActivity>) -> Self {
        Self::new_at(http, activity, BASE_URL)
    }

    pub(crate) fn new_at(
        http: reqwest::Client,
        activity: Arc<NetActivity>,
        base_url: &str,
    ) -> Self {
        Self {
            shared: Session::new(
                http.clone(),
                Arc::clone(&activity),
                ApiProfile::SHARED,
                base_url,
            ),
            personal: Session::new(http, activity, ApiProfile::PERSONAL, base_url),
            routing: RwLock::new(()),
            route_revision: AtomicU64::new(0),
            playlist_access: Mutex::new(HashMap::new()),
        }
    }

    fn session(&self, source: ApiSource) -> &Session {
        match source {
            ApiSource::Shared => &self.shared,
            ApiSource::Personal => &self.personal,
        }
    }

    pub fn state(&self, source: ApiSource) -> SessionState {
        self.session(source).state()
    }

    pub fn begin_verification(
        &self,
        source: ApiSource,
        provider: impl FnOnce(u64) -> TokenProvider,
    ) -> u64 {
        let _routing = self
            .routing
            .write()
            .unwrap_or_else(|lock| lock.into_inner());
        if source == ApiSource::Shared {
            self.clear_session_locked(ApiSource::Personal);
        }
        let session = self.session(source);
        let generation = session.begin(provider);
        self.advance_route_revision();
        generation
    }

    pub fn verification_client(&self, source: ApiSource) -> Arc<ApiClient> {
        self.session(source).client()
    }

    /// Marks a verified session ready. The shared session is the canonical
    /// account, so a personal grant cannot become ready before it.
    pub fn install(
        &self,
        source: ApiSource,
        generation: u64,
        account: AccountId,
    ) -> Result<(), ApiError> {
        let _routing = self
            .routing
            .write()
            .unwrap_or_else(|lock| lock.into_inner());
        if self.session(source).generation() != generation
            || !matches!(self.state(source), SessionState::Authorizing)
        {
            return Err(ApiError::NotSignedIn);
        }
        match source {
            ApiSource::Shared => {
                if self
                    .state(ApiSource::Personal)
                    .account()
                    .is_some_and(|active| active != &account)
                {
                    return Err(ApiError::Status {
                        status: 403,
                        message: "The Spotify grants belong to different accounts".into(),
                    });
                }
            }
            ApiSource::Personal => match self.state(ApiSource::Shared).account() {
                Some(active) if active != &account => {
                    return Err(ApiError::Status {
                        status: 403,
                        message: "The Spotify grants belong to different accounts".into(),
                    });
                }
                Some(_) => {}
                None => {
                    return Err(ApiError::Status {
                        status: 409,
                        message: "The shared Spotify grant must be verified first".into(),
                    });
                }
            },
        }
        let installed = self
            .session(source)
            .set_ready_if_current(generation, account)
            .then_some(())
            .ok_or(ApiError::NotSignedIn);
        if installed.is_ok() {
            self.advance_route_revision();
        }
        installed
    }

    pub fn clear(&self, source: ApiSource) {
        let _routing = self
            .routing
            .write()
            .unwrap_or_else(|lock| lock.into_inner());
        self.clear_session_locked(source);
        if source == ApiSource::Shared {
            // The personal identity is meaningful only relative to the
            // canonical shared account. It must never remain routable alone.
            self.clear_session_locked(ApiSource::Personal);
        }
    }

    fn clear_session_locked(&self, source: ApiSource) {
        let session = self.session(source);
        session.clear();
        self.advance_route_revision();
        if source == ApiSource::Shared {
            self.playlist_access
                .lock()
                .unwrap_or_else(|lock| lock.into_inner())
                .clear();
        }
    }

    pub fn clear_all(&self) {
        let _routing = self
            .routing
            .write()
            .unwrap_or_else(|lock| lock.into_inner());
        self.clear_session_locked(ApiSource::Shared);
        self.clear_session_locked(ApiSource::Personal);
        self.playlist_access
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .clear();
    }

    /// Clears only the session that dispatched a failing request. A late
    /// response from a replaced provider cannot retire its successor.
    pub fn clear_if_current(&self, source: ApiSource, generation: u64) -> bool {
        let _routing = self
            .routing
            .write()
            .unwrap_or_else(|lock| lock.into_inner());
        if !self.session(source).clear_if_generation(generation) {
            return false;
        }
        self.advance_route_revision();
        if source == ApiSource::Shared {
            self.playlist_access
                .lock()
                .unwrap_or_else(|lock| lock.into_inner())
                .clear();
            self.clear_session_locked(ApiSource::Personal);
        }
        true
    }

    pub fn is_current(&self, source: ApiSource, generation: u64) -> bool {
        self.session(source).generation() == generation
    }

    pub fn account(&self) -> Option<AccountId> {
        self.state(ApiSource::Shared).account().cloned()
    }

    pub fn personal_ready(&self) -> bool {
        matches!(self.state(ApiSource::Personal), SessionState::Ready { .. })
    }

    pub fn client_for(
        &self,
        operation: Operation,
    ) -> Result<(ApiSource, Arc<ApiClient>), ApiError> {
        self.route_for(operation)
            .map(|route| (route.source, route.client))
    }

    pub(crate) fn route_for(&self, operation: Operation) -> Result<ApiRoute, ApiError> {
        let _routing = self.routing.read().unwrap_or_else(|lock| lock.into_inner());
        let revision = self.route_revision.load(Ordering::Acquire);
        let source = plan(operation, self.personal_ready());
        let (generation, client) = self
            .session(source)
            .ready_client()
            .ok_or(ApiError::NotSignedIn)?;
        log::debug!("Spotify route operation={operation:?} source={source}");
        Ok(ApiRoute {
            source,
            generation,
            revision,
            client,
        })
    }

    pub(crate) fn route_is_current(&self, operation: Operation, route: &ApiRoute) -> bool {
        let _routing = self.routing.read().unwrap_or_else(|lock| lock.into_inner());
        self.route_is_current_locked(operation, route)
    }

    pub(crate) fn with_current_route(
        &self,
        operation: Operation,
        route: &ApiRoute,
        action: impl FnOnce(),
    ) -> bool {
        let _routing = self.routing.read().unwrap_or_else(|lock| lock.into_inner());
        if !self.route_is_current_locked(operation, route) {
            return false;
        }
        action();
        true
    }

    fn route_is_current_locked(&self, operation: Operation, route: &ApiRoute) -> bool {
        self.route_revision.load(Ordering::Acquire) == route.revision
            && plan(operation, self.personal_ready()) == route.source
            && self
                .session(route.source)
                .is_ready_generation(route.generation)
    }

    fn advance_route_revision(&self) {
        self.route_revision
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |revision| {
                revision.checked_add(1)
            })
            .expect("Spotify route revision exhausted");
    }

    pub fn playlist_access(&self, id: &str) -> PlaylistAccess {
        self.playlist_access
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .get(id)
            .copied()
            .unwrap_or_default()
    }

    pub fn observe_playlist(&self, playlist: &Playlist) {
        let Some(account) = self.account() else {
            return;
        };
        let access = classify_playlist(&account, playlist);
        self.playlist_access
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .insert(PlaylistId::new(playlist.id.clone()), access);
    }

    pub fn observe_playlists<'a>(&self, playlists: impl IntoIterator<Item = &'a Playlist>) {
        for playlist in playlists {
            self.observe_playlist(playlist);
        }
    }

    pub fn invalidate_playlist_access(&self, id: &PlaylistId) {
        self.playlist_access
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .insert(id.clone(), PlaylistAccess::Unknown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_matrix_selects_one_authoritative_source() {
        for operation in [
            Operation::Playback,
            Operation::UserData,
            Operation::PlaylistCreation,
            Operation::Catalog,
            Operation::PlaylistMetadata(PlaylistAccess::Owned),
            Operation::PlaylistMetadata(PlaylistAccess::Collaborative),
            Operation::PlaylistItems(PlaylistAccess::Owned),
            Operation::PlaylistItems(PlaylistAccess::Collaborative),
            Operation::PlaylistMutation(PlaylistAccess::Owned),
            Operation::PlaylistMutation(PlaylistAccess::Collaborative),
        ] {
            assert_eq!(plan(operation, true), ApiSource::Personal);
        }
        for operation in [
            Operation::CanonicalAccount,
            Operation::PlaylistLibrary,
            Operation::PlaylistSearch,
            Operation::UnsupportedDevelopmentMode,
            Operation::PlaylistMetadata(PlaylistAccess::External),
            Operation::PlaylistMetadata(PlaylistAccess::Unknown),
            Operation::PlaylistItems(PlaylistAccess::External),
            Operation::PlaylistItems(PlaylistAccess::Unknown),
            Operation::PlaylistMutation(PlaylistAccess::External),
            Operation::PlaylistMutation(PlaylistAccess::Unknown),
        ] {
            assert_eq!(plan(operation, true), ApiSource::Shared);
        }
        for operation in [
            Operation::Playback,
            Operation::UserData,
            Operation::PlaylistCreation,
            Operation::Catalog,
            Operation::PlaylistMetadata(PlaylistAccess::Owned),
            Operation::PlaylistItems(PlaylistAccess::Collaborative),
            Operation::PlaylistMutation(PlaylistAccess::Owned),
        ] {
            assert_eq!(plan(operation, false), ApiSource::Shared);
        }
    }

    #[test]
    fn playlist_access_is_learned_only_from_canonical_account_data() {
        let account = AccountId::new("me");
        let mut playlist = Playlist {
            id: "p".into(),
            ..Playlist::default()
        };
        assert_eq!(
            classify_playlist(&account, &playlist),
            PlaylistAccess::Unknown
        );
        playlist.owner.id = Some("other".into());
        assert_eq!(
            classify_playlist(&account, &playlist),
            PlaylistAccess::External
        );
        playlist.collaborative = true;
        assert_eq!(
            classify_playlist(&account, &playlist),
            PlaylistAccess::Collaborative
        );
        playlist.owner.id = Some("me".into());
        assert_eq!(
            classify_playlist(&account, &playlist),
            PlaylistAccess::Owned
        );
    }

    #[test]
    fn personal_session_requires_the_canonical_account() {
        let gateway = ApiGateway::new(reqwest::Client::new(), Arc::new(NetActivity::default()));
        gateway
            .session(ApiSource::Personal)
            .set_state(SessionState::Authorizing);
        assert!(
            gateway
                .install(
                    ApiSource::Personal,
                    gateway.session(ApiSource::Personal).generation(),
                    AccountId::new("same"),
                )
                .is_err()
        );

        gateway
            .session(ApiSource::Shared)
            .set_state(SessionState::Authorizing);
        gateway
            .install(
                ApiSource::Shared,
                gateway.session(ApiSource::Shared).generation(),
                AccountId::new("same"),
            )
            .unwrap();
        gateway
            .session(ApiSource::Personal)
            .set_state(SessionState::Authorizing);
        gateway
            .install(
                ApiSource::Personal,
                gateway.session(ApiSource::Personal).generation(),
                AccountId::new("same"),
            )
            .unwrap();
        assert!(gateway.personal_ready());

        gateway.clear(ApiSource::Personal);
        gateway
            .session(ApiSource::Personal)
            .set_state(SessionState::Authorizing);
        assert!(
            gateway
                .install(
                    ApiSource::Personal,
                    gateway.session(ApiSource::Personal).generation(),
                    AccountId::new("other"),
                )
                .is_err()
        );
        assert!(matches!(
            gateway.state(ApiSource::Shared),
            SessionState::Ready { .. }
        ));

        gateway.clear(ApiSource::Shared);
        assert_eq!(
            gateway.state(ApiSource::Personal),
            SessionState::Unavailable
        );
    }

    #[test]
    fn ready_sessions_follow_the_matrix_without_cross_app_fallback() {
        let gateway = ApiGateway::new(reqwest::Client::new(), Arc::new(NetActivity::default()));
        gateway
            .session(ApiSource::Shared)
            .set_state(SessionState::Ready {
                account: AccountId::new("same"),
            });
        gateway
            .session(ApiSource::Personal)
            .set_state(SessionState::Ready {
                account: AccountId::new("same"),
            });

        assert_eq!(
            gateway.client_for(Operation::Playback).unwrap().0,
            ApiSource::Personal
        );
        assert_eq!(
            gateway
                .client_for(Operation::PlaylistItems(PlaylistAccess::External))
                .unwrap()
                .0,
            ApiSource::Shared
        );

        gateway.clear(ApiSource::Personal);
        assert_eq!(
            gateway.client_for(Operation::Playback).unwrap().0,
            ApiSource::Shared
        );
    }

    #[test]
    fn stale_session_completion_cannot_clear_its_successor() {
        let gateway = ApiGateway::new(reqwest::Client::new(), Arc::new(NetActivity::default()));
        let session = gateway.session(ApiSource::Shared);
        session.set_state(SessionState::Ready {
            account: AccountId::new("same"),
        });
        let stale = session.generation();
        let current = session.next_generation();

        assert!(!gateway.clear_if_current(ApiSource::Shared, stale));
        assert!(matches!(
            gateway.state(ApiSource::Shared),
            SessionState::Ready { .. }
        ));
        assert!(gateway.clear_if_current(ApiSource::Shared, current));
        assert_eq!(gateway.state(ApiSource::Shared), SessionState::Unavailable);
    }

    #[tokio::test]
    async fn replaced_personal_generation_cannot_mutate_successor_transport_state() {
        use crate::api::test_support::{read_request, write_response};
        use std::net::{Ipv4Addr, TcpListener};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("loopback listener");
        let port = listener.local_addr().expect("listener address").port();
        let (observed_tx, mut observed_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            for index in 0..5 {
                let (stream, _) = listener.accept().expect("API request");
                let request = read_request(&stream);
                observed_tx.send(request).expect("test receives request");
                match index {
                    0 => write_response(
                        stream,
                        "404 Not Found",
                        &[],
                        r#"{"error":{"status":404,"message":"gone"}}"#,
                    ),
                    1 => {
                        release_rx.recv().expect("release old response");
                        write_response(
                            stream,
                            "429 Too Many Requests",
                            &[("retry-after", "1")],
                            r#"{"error":{"status":429,"message":"slow"}}"#,
                        );
                    }
                    2..=4 => write_response(stream, "200 OK", &[], "{}"),
                    _ => unreachable!(),
                }
            }
        });
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("HTTP client");
        let gateway = Arc::new(ApiGateway::new_at(
            http,
            Arc::new(NetActivity::default()),
            &format!("http://127.0.0.1:{port}/v1"),
        ));
        let shared = gateway
            .begin_verification(ApiSource::Shared, |_| TokenProvider::Fixed("shared".into()));
        gateway
            .install(ApiSource::Shared, shared, AccountId::new("same"))
            .unwrap();
        let old_generation =
            gateway.begin_verification(ApiSource::Personal, |_| TokenProvider::Fixed("old".into()));
        gateway
            .install(ApiSource::Personal, old_generation, AccountId::new("same"))
            .unwrap();
        let old_client = gateway.verification_client(ApiSource::Personal);
        let old_request_client = Arc::clone(&old_client);
        let old_request =
            tokio::spawn(async move { old_request_client.playlist_items("old", 0, 100).await });

        let first = observed_rx.recv().await.expect("old modern request");
        assert!(
            first
                .request_line
                .starts_with("GET /v1/playlists/old/items?")
        );
        assert_eq!(first.authorization.as_deref(), Some("Bearer old"));
        let old_fallback = observed_rx.recv().await.expect("old fallback request");
        assert!(
            old_fallback
                .request_line
                .starts_with("GET /v1/playlists/old/tracks?")
        );
        assert_eq!(old_fallback.authorization.as_deref(), Some("Bearer old"));

        let new_generation =
            gateway.begin_verification(ApiSource::Personal, |_| TokenProvider::Fixed("new".into()));
        gateway
            .install(ApiSource::Personal, new_generation, AccountId::new("same"))
            .unwrap();
        let new_client = gateway.verification_client(ApiSource::Personal);
        release_tx.send(()).expect("release old fallback 429");
        for _ in 0..100 {
            if old_client.cooldown_active().await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(old_client.cooldown_active().await);

        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            new_client.playlist_items("new", 0, 100),
        )
        .await
        .expect("successor does not inherit the old cooldown")
        .expect("successor request succeeds");
        let second = observed_rx.recv().await.expect("new modern request");
        assert!(
            second
                .request_line
                .starts_with("GET /v1/playlists/new/items?")
        );
        assert_eq!(second.authorization.as_deref(), Some("Bearer new"));

        old_request
            .await
            .expect("old task joins")
            .expect("old fallback succeeds");
        let old_retry = observed_rx.recv().await.expect("old fallback retry");
        assert!(
            old_retry
                .request_line
                .starts_with("GET /v1/playlists/old/tracks?")
        );

        new_client
            .playlist_items("new-again", 0, 100)
            .await
            .expect("successor keeps modern endpoints");
        let final_request = observed_rx.recv().await.expect("final modern request");
        assert!(
            final_request
                .request_line
                .starts_with("GET /v1/playlists/new-again/items?")
        );
        assert_eq!(final_request.authorization.as_deref(), Some("Bearer new"));
        server.join().expect("server exits");
    }
}
