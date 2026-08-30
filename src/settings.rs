//! User preferences, stored as one readable JSON file.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_SETTINGS_BYTES: usize = 1024 * 1024;
const MAX_SESSION_BYTES: usize = 1024 * 1024;

pub const SIDEBAR_WIDTH_MIN: f32 = 210.0;
pub const SIDEBAR_WIDTH_MAX: f32 = 440.0;
pub const LYRICS_WIDTH_MIN: f32 = 280.0;
pub const LYRICS_WIDTH_MAX: f32 = 640.0;
pub const QUEUE_WIDTH_MIN: f32 = 280.0;
pub const QUEUE_WIDTH_MAX: f32 = 560.0;
pub const ZOOM_MIN: f32 = 0.5;
pub const ZOOM_MAX: f32 = 2.5;

const SIDEBAR_WIDTH_DEFAULT: f32 = 250.0;
const PANEL_WIDTH_DEFAULT: f32 = 360.0;
const ZOOM_DEFAULT: f32 = 1.0;

fn load_private_json<T>(path: &Path, max_bytes: usize, kind: &str) -> T
where
    T: DeserializeOwned + Default,
{
    match crate::secrets::read_private_bounded(path, max_bytes) {
        Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            log::warn!("{kind} at {} is unreadable: {error}", path.display());
            T::default()
        }),
        Ok(None) => T::default(),
        Err(error) => {
            log::warn!(
                "{kind} at {} could not be read safely: {error}",
                path.display()
            );
            T::default()
        }
    }
}

#[derive(Debug, Error)]
pub enum SaveError {
    #[error("unable to encode {kind}: {source}")]
    Encode {
        kind: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("refusing to save {kind}: encoded data is {actual} bytes (limit {limit})")]
    TooLarge {
        kind: &'static str,
        actual: usize,
        limit: usize,
    },
    #[error("unable to save {kind} to {path}: {source}")]
    Write {
        kind: &'static str,
        path: PathBuf,
        #[source]
        source: crate::secrets::SecretError,
    },
}

fn save_private_json<T>(
    value: &T,
    path: &Path,
    max_bytes: usize,
    kind: &'static str,
    pretty: bool,
) -> Result<(), SaveError>
where
    T: Serialize,
{
    let bytes = if pretty {
        serde_json::to_vec_pretty(value)
    } else {
        serde_json::to_vec(value)
    }
    .map_err(|source| SaveError::Encode { kind, source })?;
    if bytes.len() > max_bytes {
        return Err(SaveError::TooLarge {
            kind,
            actual: bytes.len(),
            limit: max_bytes,
        });
    }
    crate::secrets::write_private_atomic(path, &bytes).map_err(|source| SaveError::Write {
        kind,
        path: path.to_path_buf(),
        source,
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
    System,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [Self::Dark, Self::Light, Self::System];

    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
            Self::System => "Follow system",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The Spotify Connect name other devices see.
    pub device_name: String,
    /// 96, 160, or 320 kbps.
    pub bitrate: u16,
    pub normalisation: bool,
    pub autoplay: bool,
    pub gapless: bool,
    /// librespot backend name; `None` picks the platform default.
    pub audio_backend: Option<String>,
    pub audio_device: Option<String>,
    pub audio_cache: bool,
    pub audio_cache_mb: u64,
    pub theme: ThemeChoice,
    /// Tint the interface with the colour of the playing album's art.
    pub accent_from_art: bool,
    /// Last local volume, 0..=65535.
    pub volume: u16,
    /// Whether the library sidebar is visible.
    pub sidebar_visible: bool,
    pub sidebar_width: f32,
    pub lyrics_width: f32,
    pub queue_width: f32,
    pub search_history: Vec<String>,
    pub show_shortcut_hints: bool,
    /// A personal Spotify Web API application id, if the user registered one.
    /// `None` uses the shared public application.
    pub web_client_id: Option<String>,
    /// Local playback has been authorized at least once on this machine, so
    /// the app can resume it silently instead of prompting.
    pub playback_authorized: bool,
    /// Closing the window hides to the tray and keeps the music playing.
    pub keep_playing_in_background: bool,
    /// Ask GitHub once a day whether a newer release exists.
    pub check_for_updates: bool,
    /// Ask LRCLIB when Spotify itself has no lyrics for a track.
    pub lrclib_lyrics: bool,
    /// The external-service toggles were chosen after their data disclosure
    /// was added. Missing/false forces both opt-ins off on load.
    pub external_services_disclosed: bool,
    /// Context URIs pinned to the top of the sidebar, in pin order.
    pub pinned_contexts: Vec<String>,
    /// Interface zoom, egui's zoom factor; Ctrl+plus/minus changes it.
    pub zoom: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            device_name: "Fastpotify".to_string(),
            bitrate: 320,
            normalisation: false,
            autoplay: true,
            gapless: true,
            audio_backend: None,
            audio_device: None,
            audio_cache: true,
            audio_cache_mb: 1024,
            theme: ThemeChoice::Dark,
            accent_from_art: true,
            volume: (u16::MAX as u32 * 70 / 100) as u16,
            sidebar_visible: true,
            sidebar_width: SIDEBAR_WIDTH_DEFAULT,
            lyrics_width: PANEL_WIDTH_DEFAULT,
            queue_width: PANEL_WIDTH_DEFAULT,
            search_history: Vec::new(),
            show_shortcut_hints: true,
            web_client_id: None,
            playback_authorized: false,
            keep_playing_in_background: true,
            check_for_updates: false,
            lrclib_lyrics: false,
            external_services_disclosed: false,
            pinned_contexts: Vec::new(),
            zoom: ZOOM_DEFAULT,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        let mut settings = load_private_json(path, MAX_SETTINGS_BYTES, "settings");
        Self::enforce_external_service_consent(&mut settings);
        settings.normalize_layout();
        settings
    }

    pub(crate) fn normalize_layout(&mut self) {
        self.sidebar_width = bounded_f32(
            self.sidebar_width,
            SIDEBAR_WIDTH_MIN,
            SIDEBAR_WIDTH_MAX,
            SIDEBAR_WIDTH_DEFAULT,
        );
        self.lyrics_width = bounded_f32(
            self.lyrics_width,
            LYRICS_WIDTH_MIN,
            LYRICS_WIDTH_MAX,
            PANEL_WIDTH_DEFAULT,
        );
        self.queue_width = bounded_f32(
            self.queue_width,
            QUEUE_WIDTH_MIN,
            QUEUE_WIDTH_MAX,
            PANEL_WIDTH_DEFAULT,
        );
        self.zoom = bounded_f32(self.zoom, ZOOM_MIN, ZOOM_MAX, ZOOM_DEFAULT);
    }

    fn enforce_external_service_consent(settings: &mut Self) {
        if !settings.external_services_disclosed {
            settings.check_for_updates = false;
            settings.lrclib_lyrics = false;
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), SaveError> {
        save_private_json(self, path, MAX_SETTINGS_BYTES, "settings", true)
    }

    pub fn platform_backend(&self) -> Option<String> {
        self.audio_backend.clone().or_else(|| {
            if cfg!(target_os = "linux") {
                Some("pulseaudio".to_string())
            } else {
                None
            }
        })
    }

    pub fn remember_search(&mut self, query: &str) {
        let query = query.trim();
        if query.is_empty() {
            return;
        }
        self.search_history.retain(|entry| entry != query);
        self.search_history.insert(0, query.to_string());
        self.search_history.truncate(12);
    }
}

fn bounded_f32(value: f32, min: f32, max: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        fallback
    }
}

/// Restorable UI session: what was open when the app last closed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionState {
    pub last_page: Option<String>,
    /// Context URIs most recently played, newest first.
    pub recent_contexts: Vec<String>,
    /// What was playing when the app closed, to resume from a cold start.
    pub last_context: Option<String>,
    pub last_track: Option<String>,
    pub last_position_ms: u32,
    /// Shuffle is a listener mode that carries across playback contexts.
    pub shuffle_on: bool,
    /// Each table's chosen sort, keyed by its encoded page.
    pub sorts: Vec<(String, crate::model::TableSort)>,
    /// Last window inner size, to restore on next launch.
    pub window_size: Option<[f32; 2]>,
    /// Last window outer position, to restore on next launch.
    pub window_pos: Option<[f32; 2]>,
    /// Whether the queue panel was open.
    pub queue_open: Option<bool>,
}

impl SessionState {
    pub fn load(path: &Path) -> Self {
        load_private_json(path, MAX_SESSION_BYTES, "session")
    }

    pub fn save(&self, path: &Path) -> Result<(), SaveError> {
        save_private_json(self, path, MAX_SESSION_BYTES, "session", false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_test_file(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "fastpotify-settings-test-{}-{started}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ))
            .join(name)
    }

    #[test]
    fn external_services_are_off_by_default() {
        let settings = Settings::default();
        assert!(!settings.check_for_updates);
        assert!(!settings.lrclib_lyrics);
        assert!(!settings.external_services_disclosed);
    }

    #[test]
    fn pre_disclosure_settings_cannot_preserve_implicit_network_opt_ins() {
        let mut settings = Settings {
            check_for_updates: true,
            lrclib_lyrics: true,
            external_services_disclosed: false,
            ..Settings::default()
        };
        Settings::enforce_external_service_consent(&mut settings);
        assert!(!settings.check_for_updates);
        assert!(!settings.lrclib_lyrics);
    }

    #[test]
    fn disclosed_opt_ins_are_preserved() {
        let mut settings = Settings {
            check_for_updates: true,
            lrclib_lyrics: true,
            external_services_disclosed: true,
            ..Settings::default()
        };
        Settings::enforce_external_service_consent(&mut settings);
        assert!(settings.check_for_updates);
        assert!(settings.lrclib_lyrics);
    }

    #[test]
    fn older_settings_keep_desktop_layout_defaults() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert!(settings.sidebar_visible);
        assert_eq!(settings.sidebar_width, SIDEBAR_WIDTH_DEFAULT);
        assert_eq!(settings.lyrics_width, PANEL_WIDTH_DEFAULT);
        assert_eq!(settings.queue_width, PANEL_WIDTH_DEFAULT);
        assert_eq!(settings.zoom, ZOOM_DEFAULT);
    }

    #[test]
    fn desktop_layout_values_are_normalized_before_use() {
        let mut settings = Settings {
            sidebar_width: 10.0,
            lyrics_width: 5_000.0,
            queue_width: f32::INFINITY,
            zoom: 10.0,
            ..Settings::default()
        };
        settings.normalize_layout();
        assert_eq!(settings.sidebar_width, SIDEBAR_WIDTH_MIN);
        assert_eq!(settings.lyrics_width, LYRICS_WIDTH_MAX);
        assert_eq!(settings.queue_width, PANEL_WIDTH_DEFAULT);
        assert_eq!(settings.zoom, ZOOM_MAX);
    }

    #[test]
    fn older_session_files_default_new_persistence_fields() {
        let session: SessionState =
            serde_json::from_str(r#"{"last_page":"home","last_position_ms":1200}"#).unwrap();
        assert!(!session.shuffle_on);
        assert!(session.sorts.is_empty());
        assert_eq!(session.window_size, None);
        assert_eq!(session.window_pos, None);
        assert_eq!(session.queue_open, None);
    }

    #[test]
    fn playback_and_desktop_session_state_round_trip_together() {
        let session = SessionState {
            shuffle_on: true,
            sorts: vec![(
                "liked".into(),
                crate::model::TableSort {
                    column: crate::model::SortColumn::Index,
                    ascending: false,
                },
            )],
            window_size: Some([1280.0, 800.0]),
            window_pos: Some([120.0, 80.0]),
            queue_open: Some(true),
            ..SessionState::default()
        };
        let restored: SessionState =
            serde_json::from_slice(&serde_json::to_vec(&session).unwrap()).unwrap();
        assert_eq!(restored, session);
    }

    #[test]
    fn oversized_session_is_rejected_without_replacing_valid_state() {
        let path = private_test_file("session.json");
        let valid = SessionState {
            last_page: Some("home".into()),
            ..SessionState::default()
        };
        valid.save(&path).expect("write valid session");
        let before = std::fs::read(&path).expect("read valid session");

        let oversized = SessionState {
            recent_contexts: vec!["x".repeat(MAX_SESSION_BYTES + 1)],
            ..valid
        };
        let error = oversized.save(&path).expect_err("reject oversized state");

        assert!(matches!(
            error,
            SaveError::TooLarge {
                kind: "session",
                limit: MAX_SESSION_BYTES,
                ..
            }
        ));
        assert_eq!(
            std::fs::read(&path).expect("read preserved session"),
            before
        );
        let root = path.parent().expect("test directory");
        std::fs::remove_dir_all(root).expect("remove test directory");
    }
}
