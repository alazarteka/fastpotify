//! User preferences, stored as one readable JSON file.

use std::path::Path;

use serde::{Deserialize, Serialize};

const MAX_SETTINGS_BYTES: usize = 1024 * 1024;
const MAX_SESSION_BYTES: usize = 1024 * 1024;

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
    pub sidebar_width: f32,
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
            sidebar_width: 250.0,
            search_history: Vec::new(),
            show_shortcut_hints: true,
            web_client_id: None,
            playback_authorized: false,
            keep_playing_in_background: true,
            check_for_updates: false,
            lrclib_lyrics: false,
            external_services_disclosed: false,
            pinned_contexts: Vec::new(),
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        match crate::secrets::read_private_bounded(path, MAX_SETTINGS_BYTES) {
            Ok(Some(bytes)) => match serde_json::from_slice(&bytes) {
                Ok(mut settings) => {
                    Self::enforce_external_service_consent(&mut settings);
                    settings
                }
                Err(error) => {
                    log::warn!("settings at {} are unreadable: {error}", path.display());
                    Self::default()
                }
            },
            Ok(None) => Self::default(),
            Err(error) => {
                log::warn!("settings at {} could not be read safely: {error}", path.display());
                Self::default()
            }
        }
    }

    fn enforce_external_service_consent(settings: &mut Self) {
        if !settings.external_services_disclosed {
            settings.check_for_updates = false;
            settings.lrclib_lyrics = false;
        }
    }

    pub fn save(&self, path: &Path) {
        let text = match serde_json::to_string_pretty(self) {
            Ok(text) => text,
            Err(error) => {
                log::warn!("unable to encode settings: {error}");
                return;
            }
        };
        if let Err(error) = crate::secrets::write_private_atomic(path, text.as_bytes()) {
            log::warn!("unable to save settings to {}: {error}", path.display());
        }
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
}

impl SessionState {
    pub fn load(path: &Path) -> Self {
        crate::secrets::read_private_bounded(path, MAX_SESSION_BYTES)
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Ok(bytes) = serde_json::to_vec(self) {
            let _ = crate::secrets::write_private_atomic(path, &bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
