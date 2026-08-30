//! Authoritative routing across independent shared and personal Web API sessions.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use super::ApiSource;
use super::client::{ApiClient, ApiError, NetActivity, TokenProvider};
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
    state: RwLock<SessionState>,
    generation: AtomicU64,
    client: Arc<ApiClient>,
}

impl Session {
    fn new(http: reqwest::Client, activity: Arc<NetActivity>, profile: ApiProfile) -> Self {
        Self {
            state: RwLock::new(SessionState::Unavailable),
            generation: AtomicU64::new(0),
            client: Arc::new(ApiClient::new(
                http,
                activity,
                profile.search_limit,
                profile.artist_albums_limit,
                profile.source,
            )),
        }
    }

    fn state(&self) -> SessionState {
        self.state
            .read()
            .unwrap_or_else(|lock| lock.into_inner())
            .clone()
    }

    fn set_state(&self, state: SessionState) {
        *self.state.write().unwrap_or_else(|lock| lock.into_inner()) = state;
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }
}

pub struct ApiGateway {
    shared: Session,
    personal: Session,
    playlist_access: Mutex<HashMap<PlaylistId, PlaylistAccess>>,
}

impl ApiGateway {
    pub fn new(http: reqwest::Client, activity: Arc<NetActivity>) -> Self {
        Self {
            shared: Session::new(http.clone(), Arc::clone(&activity), ApiProfile::SHARED),
            personal: Session::new(http, activity, ApiProfile::PERSONAL),
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
        if source == ApiSource::Shared {
            self.clear_session(ApiSource::Personal);
        }
        let session = self.session(source);
        let generation = session.next_generation();
        session
            .client
            .set_token_provider(Some(provider(generation)));
        session.set_state(SessionState::Authorizing);
        generation
    }

    pub fn verification_client(&self, source: ApiSource) -> Arc<ApiClient> {
        Arc::clone(&self.session(source).client)
    }

    /// Marks a verified session ready. The shared session is the canonical
    /// account, so a personal grant cannot become ready before it.
    pub fn install(
        &self,
        source: ApiSource,
        generation: u64,
        account: AccountId,
    ) -> Result<(), ApiError> {
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
        self.session(source)
            .set_state(SessionState::Ready { account });
        Ok(())
    }

    pub fn clear(&self, source: ApiSource) {
        self.clear_session(source);
        if source == ApiSource::Shared {
            // The personal identity is meaningful only relative to the
            // canonical shared account. It must never remain routable alone.
            self.clear_session(ApiSource::Personal);
        }
    }

    fn clear_session(&self, source: ApiSource) {
        let session = self.session(source);
        session.next_generation();
        session.client.set_token_provider(None);
        session.set_state(SessionState::Unavailable);
        if source == ApiSource::Shared {
            self.playlist_access
                .lock()
                .unwrap_or_else(|lock| lock.into_inner())
                .clear();
        }
    }

    pub fn clear_all(&self) {
        self.clear(ApiSource::Shared);
        self.playlist_access
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .clear();
    }

    /// Clears only the session that dispatched a failing request. A late
    /// response from a replaced provider cannot retire its successor.
    pub fn clear_if_current(&self, source: ApiSource, generation: u64) -> bool {
        if !self.is_current(source, generation) {
            return false;
        }
        self.clear(source);
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
        let source = plan(operation, self.personal_ready());
        let session = self.session(source);
        if !matches!(session.state(), SessionState::Ready { .. }) {
            return Err(ApiError::NotSignedIn);
        }
        log::debug!("Spotify route operation={operation:?} source={source}");
        Ok((source, Arc::clone(&session.client)))
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
}
