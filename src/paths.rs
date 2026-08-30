//! Where Fastpotify keeps its files.
//!
//! Configuration, durable state (Spotify credentials), and disposable caches
//! (audio, artwork) live in the platform's conventional directories, so
//! clearing a cache never signs the user out and a config backup never
//! contains a credential.

use std::path::PathBuf;

use directories::ProjectDirs;

#[derive(Clone, Debug)]
pub struct AppDirs {
    pub config: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
}

impl AppDirs {
    pub fn discover() -> Self {
        let project = ProjectDirs::from("me", "paolino", "fastpotify");
        match project {
            Some(project) => Self {
                config: project.config_dir().to_path_buf(),
                state: project
                    .state_dir()
                    .map(|path| path.to_path_buf())
                    .unwrap_or_else(|| project.data_local_dir().to_path_buf()),
                cache: project.cache_dir().to_path_buf(),
            },
            None => {
                let fallback = std::env::current_dir().unwrap_or_default();
                Self {
                    config: fallback.join("fastpotify-config"),
                    state: fallback.join("fastpotify-state"),
                    cache: fallback.join("fastpotify-cache"),
                }
            }
        }
    }

    pub fn settings_file(&self) -> PathBuf {
        self.config.join("settings.json")
    }

    pub fn session_file(&self) -> PathBuf {
        self.state.join("session.json")
    }

    /// Process-lifetime authentication for the non-Linux control socket.
    /// This is deliberately outside the Spotify credential store: signing
    /// out must not break activation of the still-running application.
    pub fn control_token_file(&self) -> PathBuf {
        self.state.join("control-ipc.secret")
    }

    /// Versioned owner-private Web and playback credential files.
    pub fn secrets_dir(&self) -> PathBuf {
        self.state.join("secrets-v1")
    }

    /// The legacy Web API OAuth grant, migrated on first safe read.
    pub fn legacy_web_token_file(&self) -> PathBuf {
        self.state.join("web_api_token.json")
    }

    pub fn legacy_web_secret(&self) -> crate::secrets::LegacySecret {
        let primary = self.legacy_web_token_file();
        crate::secrets::LegacySecret::new(primary.clone())
            .with_stale(primary.with_extension("json.tmp"))
    }

    /// The log of the current run, replaced at every start.
    pub fn log_file(&self) -> PathBuf {
        self.state.join("fastpotify.log")
    }

    /// Where a panic is recorded before the process dies of it.
    pub fn panic_log(&self) -> PathBuf {
        self.state.join("panic.log")
    }

    /// The directory librespot versions before 0.8 integration used directly.
    pub fn legacy_credentials_dir(&self) -> PathBuf {
        self.state.join("credentials")
    }

    pub fn legacy_playback_secret(&self) -> crate::secrets::LegacySecret {
        crate::secrets::LegacySecret::new(self.legacy_credentials_dir().join("credentials.json"))
    }

    pub fn volume_dir(&self) -> PathBuf {
        self.state.join("volume")
    }

    pub fn audio_cache_dir(&self) -> PathBuf {
        self.cache.join("audio")
    }

    pub fn art_cache_dir(&self) -> PathBuf {
        self.cache.join("art")
    }

    pub fn lyrics_cache_dir(&self) -> PathBuf {
        self.cache.join("lyrics")
    }

    pub fn playlist_cache_dir(&self) -> PathBuf {
        self.cache.join("playlists")
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        for dir in [&self.config, &self.state, &self.cache] {
            crate::secrets::ensure_private_dir(dir).map_err(std::io::Error::other)?;
        }
        crate::secrets::ensure_private_dir(&self.secrets_dir()).map_err(std::io::Error::other)?;
        Ok(())
    }
}
