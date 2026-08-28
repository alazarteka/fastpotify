//! Album art: fetched once, kept on disk, decoded by egui on demand.
//!
//! [`ArtLoader`] plugs into egui's image pipeline as a bytes loader for
//! approved HTTPS Spotify CDN URIs, so every view simply asks for
//! `ui.image(url)`. The first
//! request for a URL starts a background download (or a disk-cache read);
//! until it lands egui shows a placeholder. Entries that no view has drawn
//! for a while are evicted so a long browsing session does not accumulate
//! textures without bound.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::load::{Bytes, BytesLoadResult, BytesLoader, BytesPoll, LoadError};
use sha1::{Digest, Sha1};

const STALE_AFTER: Duration = Duration::from_secs(150);
const MAX_ART_BYTES: usize = 8 * 1024 * 1024;
const MAX_ART_REDIRECTS: usize = 3;
const MAX_ART_DIMENSION: u32 = 8192;
const MAX_ART_PIXELS: u64 = 16_777_216;

enum Entry {
    Pending,
    Ready {
        bytes: Arc<[u8]>,
        last_used: Instant,
    },
    Failed(String),
}

struct Inner {
    entries: Mutex<HashMap<String, Entry>>,
    http: reqwest::Client,
    runtime: tokio::runtime::Handle,
    cache_dir: PathBuf,
}

#[derive(Clone)]
pub struct ArtLoader {
    inner: Arc<Inner>,
}

impl ArtLoader {
    pub fn new(http: reqwest::Client, runtime: tokio::runtime::Handle, cache_dir: PathBuf) -> Self {
        let _ = crate::secrets::ensure_private_dir(&cache_dir);
        Self {
            inner: Arc::new(Inner {
                entries: Mutex::new(HashMap::new()),
                http,
                runtime,
                cache_dir,
            }),
        }
    }

    /// Bytes for `url`, from memory, disk, or the network.
    pub async fn fetch(&self, url: &str) -> Result<Arc<[u8]>, String> {
        self.inner.fetch(url).await
    }

    /// Drops artwork no view has drawn recently, freeing bytes and textures.
    pub fn evict_stale(&self, ctx: &egui::Context) {
        let stale: Vec<String> = {
            let entries = self.inner.entries.lock().unwrap_or_else(|p| p.into_inner());
            entries
                .iter()
                .filter_map(|(url, entry)| match entry {
                    Entry::Ready { last_used, .. } if last_used.elapsed() > STALE_AFTER => {
                        Some(url.clone())
                    }
                    Entry::Failed(_) => Some(url.clone()),
                    _ => None,
                })
                .collect()
        };
        for url in stale {
            ctx.forget_image(&url);
        }
    }

    pub fn clear_disk_cache(&self) -> std::io::Result<u64> {
        let mut removed = 0;
        for entry in std::fs::read_dir(&self.inner.cache_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                removed += entry.metadata().map(|m| m.len()).unwrap_or(0);
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(removed)
    }
}

impl Inner {
    fn cache_path(&self, url: &str) -> PathBuf {
        let digest = Sha1::digest(url.as_bytes());
        let mut name = String::with_capacity(40);
        for byte in digest {
            use std::fmt::Write;
            let _ = write!(name, "{byte:02x}");
        }
        self.cache_dir.join(name)
    }

    async fn fetch(self: &Arc<Self>, url: &str) -> Result<Arc<[u8]>, String> {
        let requested = validate_art_url(url).map_err(|error| {
            log::warn!(
                "blocked artwork from host {}: {error}",
                safe_art_host(url)
            );
            error
        })?;
        if let Some(Entry::Ready { bytes, .. }) = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(url)
        {
            return Ok(Arc::clone(bytes));
        }
        let path = self.cache_path(url);
        let cached = tokio::task::spawn_blocking({
            let path = path.clone();
            move || {
                crate::secrets::read_private_bounded(&path, MAX_ART_BYTES)
                    .ok()
                    .flatten()
            }
        })
        .await
        .ok()
        .flatten();
        let bytes: Vec<u8> = if let Some(bytes) = cached {
            match validate_art_bytes(&bytes, None) {
                Ok(()) => bytes,
                Err(_) => {
                    let _ = std::fs::remove_file(&path);
                    self.download_art(requested, &path).await?
                }
            }
        } else {
            self.download_art(requested, &path).await?
        };
        Ok(Arc::from(bytes))
    }

    async fn download_art(
        &self,
        mut current: reqwest::Url,
        cache_path: &std::path::Path,
    ) -> Result<Vec<u8>, String> {
        for redirects in 0..=MAX_ART_REDIRECTS {
            let mut response = self
                .http
                .get(current.clone())
                .header("Accept", "image/jpeg, image/png")
                .send()
                .await
                .map_err(|_| "artwork request failed".to_string())?;
            if response.status().is_redirection() {
                if redirects == MAX_ART_REDIRECTS {
                    return Err("artwork redirected too many times".into());
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| "artwork redirect has no valid Location".to_string())?;
                current = redirect_target(&current, location)?;
                continue;
            }
            if !response.status().is_success() {
                return Err(format!("artwork request failed: {}", response.status()));
            }
            if response
                .content_length()
                .is_some_and(|length| length > MAX_ART_BYTES as u64)
            {
                return Err("artwork is too large".into());
            }
            let mime = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(allowed_mime)
                .ok_or_else(|| "artwork has an unsupported content type".to_string())?;
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| "artwork body failed".to_string())?
            {
                append_art_chunk(&mut bytes, &chunk)?;
            }
            validate_art_bytes(&bytes, Some(mime))?;
            let write_path = cache_path.to_path_buf();
            let payload = bytes.clone();
            self.runtime.spawn_blocking(move || {
                let _ = crate::secrets::write_private_atomic(&write_path, &payload);
            });
            return Ok(bytes);
        }
        Err("artwork redirected too many times".into())
    }

    fn start(self: &Arc<Self>, ctx: &egui::Context, url: String) {
        let loader = Arc::clone(self);
        let ctx = ctx.clone();
        self.runtime.spawn(async move {
            let result = loader.fetch(&url).await;
            let entry = match result {
                Ok(bytes) => Entry::Ready {
                    bytes,
                    last_used: Instant::now(),
                },
                Err(error) => Entry::Failed(error),
            };
            loader
                .entries
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(url, entry);
            ctx.request_repaint();
        });
    }
}

impl BytesLoader for ArtLoader {
    fn id(&self) -> &'static str {
        "fastpotify::ArtLoader"
    }

    fn load(&self, ctx: &egui::Context, uri: &str) -> BytesLoadResult {
        let Ok(parsed) = reqwest::Url::parse(uri) else {
            return Err(LoadError::NotSupported);
        };
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(LoadError::NotSupported);
        }
        let mut entries = self.inner.entries.lock().unwrap_or_else(|p| p.into_inner());
        match entries.get_mut(uri) {
            Some(Entry::Ready { bytes, last_used }) => {
                *last_used = Instant::now();
                Ok(BytesPoll::Ready {
                    size: None,
                    bytes: Bytes::Shared(Arc::clone(bytes)),
                    mime: None,
                })
            }
            Some(Entry::Pending) => Ok(BytesPoll::Pending { size: None }),
            Some(Entry::Failed(error)) => Err(LoadError::Loading(error.clone())),
            None => {
                entries.insert(uri.to_string(), Entry::Pending);
                drop(entries);
                self.inner.start(ctx, uri.to_string());
                Ok(BytesPoll::Pending { size: None })
            }
        }
    }

    fn forget(&self, uri: &str) {
        self.inner
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(uri);
    }

    fn forget_all(&self) {
        self.inner
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    fn byte_size(&self) -> usize {
        self.inner
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .map(|entry| match entry {
                Entry::Ready { bytes, .. } => bytes.len(),
                _ => 0,
            })
            .sum()
    }
}

fn validate_art_url(value: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(value).map_err(|_| "artwork URL is invalid".to_string())?;
    if url.scheme() != "https" {
        return Err("artwork URL is not HTTPS".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("artwork URL contains user information".into());
    }
    if url.port_or_known_default() != Some(443) {
        return Err("artwork URL uses a nonstandard port".into());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "artwork URL has no host".to_string())?;
    if !allowed_art_host(host) {
        return Err("artwork URL host is not an approved Spotify CDN".into());
    }
    if url.fragment().is_some() {
        return Err("artwork URL contains a fragment".into());
    }
    Ok(url)
}

/// Enough origin information to diagnose a newly used Spotify CDN without
/// ever putting a path, query string, fragment, or user information in logs.
fn safe_art_host(value: &str) -> String {
    reqwest::Url::parse(value)
        .ok()
        .and_then(|url| {
            url.host_str()
                .map(|host| host.chars().take(255).collect::<String>())
        })
        .unwrap_or_else(|| "<invalid>".to_string())
}

fn allowed_art_host(host: &str) -> bool {
    fn under(host: &str, suffix: &str) -> bool {
        host == suffix
            || host
                .strip_suffix(suffix)
                .is_some_and(|prefix| prefix.ends_with('.'))
    }
    if under(host, "scdn.co") || under(host, "spotifycdn.com") {
        return true;
    }
    #[cfg(feature = "demo")]
    if host == "picsum.photos" || host == "fastly.picsum.photos" {
        return true;
    }
    false
}

fn redirect_target(current: &reqwest::Url, location: &str) -> Result<reqwest::Url, String> {
    let next = current
        .join(location)
        .map_err(|_| "artwork redirect Location is invalid".to_string())?;
    validate_art_url(next.as_str())
}

fn append_art_chunk(output: &mut Vec<u8>, chunk: &[u8]) -> Result<(), String> {
    if chunk.len() > MAX_ART_BYTES.saturating_sub(output.len()) {
        return Err("artwork is too large".into());
    }
    output.extend_from_slice(chunk);
    Ok(())
}

fn allowed_mime(value: &str) -> Option<&'static str> {
    let mime = value.split(';').next()?.trim();
    if mime.eq_ignore_ascii_case("image/jpeg") || mime.eq_ignore_ascii_case("image/jpg") {
        Some("image/jpeg")
    } else if mime.eq_ignore_ascii_case("image/png") {
        Some("image/png")
    } else {
        None
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ArtFormat {
    Jpeg,
    Png,
}

fn art_format(bytes: &[u8]) -> Option<ArtFormat> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ArtFormat::Jpeg)
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ArtFormat::Png)
    } else {
        None
    }
}

fn validate_art_bytes(bytes: &[u8], mime: Option<&str>) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() > MAX_ART_BYTES {
        return Err("artwork is empty or too large".into());
    }
    let format = art_format(bytes)
        .ok_or_else(|| "artwork signature is not JPEG or PNG".to_string())?;
    if let Some(mime) = mime {
        let matches = (mime == "image/jpeg" && format == ArtFormat::Jpeg)
            || (mime == "image/png" && format == ArtFormat::Png);
        if !matches {
            return Err("artwork content type does not match its signature".into());
        }
    }
    let (width, height) = match format {
        ArtFormat::Png if bytes.len() >= 24 && &bytes[12..16] == b"IHDR" => (
            u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
            u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
        ),
        ArtFormat::Png => return Err("PNG artwork has no valid IHDR".into()),
        ArtFormat::Jpeg => image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|_| "JPEG artwork header is invalid".to_string())?
            .into_dimensions()
            .map_err(|_| "JPEG artwork dimensions are invalid".to_string())?,
    };
    if width == 0
        || height == 0
        || width > MAX_ART_DIMENSION
        || height > MAX_ART_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_ART_PIXELS
    {
        return Err("artwork dimensions exceed the pixel budget".into());
    }
    Ok(())
}

/// A colour that represents an album cover, suitable for tinting a dark or
/// light surface: the most common saturated hue, with its lightness pulled
/// into a range that still reads as a background.
pub fn accent_color(bytes: &[u8]) -> Option<[u8; 3]> {
    validate_art_bytes(bytes, None).ok()?;
    let decoded = image::load_from_memory(bytes).ok()?;
    let small = decoded.thumbnail(48, 48).to_rgb8();
    let mut buckets: HashMap<(u8, u8, u8), (u64, [u64; 3])> = HashMap::new();
    for pixel in small.pixels() {
        let [r, g, b] = pixel.0;
        let (max, min) = (r.max(g).max(b) as f32, r.min(g).min(b) as f32);
        let saturation = if max == 0.0 { 0.0 } else { (max - min) / max };
        let lightness = (max + min) / 510.0;
        // Weight toward vivid mid-tones so black borders and white text lose.
        let weight = (1.0 + saturation * 6.0) * (1.0 - (lightness - 0.5).abs() * 1.4).max(0.05);
        let weight = (weight * 100.0) as u64;
        let key = (r >> 4, g >> 4, b >> 4);
        let bucket = buckets.entry(key).or_insert((0, [0, 0, 0]));
        bucket.0 += weight;
        bucket.1[0] += r as u64 * weight;
        bucket.1[1] += g as u64 * weight;
        bucket.1[2] += b as u64 * weight;
    }
    let (_, (weight, sum)) = buckets.into_iter().max_by_key(|(_, (weight, _))| *weight)?;
    if weight == 0 {
        return None;
    }
    Some([
        (sum[0] / weight) as u8,
        (sum[1] / weight) as u8,
        (sum[2] / weight) as u8,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artwork_urls_are_https_and_limited_to_spotify_cdns() {
        for valid in [
            "https://i.scdn.co/image/abc",
            "https://mosaic.scdn.co/640/abc",
            "https://image-cdn-ak.spotifycdn.com/image/abc",
            "https://i.scdn.co:443/image/abc?size=640",
        ] {
            assert!(validate_art_url(valid).is_ok(), "expected {valid} to pass");
        }
        assert_eq!(
            safe_art_host("https://user:secret@example.test/private?token=secret"),
            "example.test"
        );
        for invalid in [
            "http://i.scdn.co/image/abc",
            "https://127.0.0.1/image/abc",
            "https://[::1]/image/abc",
            "https://localhost/image/abc",
            "https://user@i.scdn.co/image/abc",
            "https://user:password@i.scdn.co/image/abc",
            "https://i.scdn.co:8443/image/abc",
            "https://evilscdn.co/image/abc",
            "https://scdn.co.example.test/image/abc",
        ] {
            assert!(
                validate_art_url(invalid).is_err(),
                "expected {invalid} to fail"
            );
        }
    }

    #[test]
    fn artwork_redirects_revalidate_every_destination() {
        let current = validate_art_url("https://i.scdn.co/image/abc").unwrap();
        assert!(redirect_target(&current, "/image/next").is_ok());
        assert!(
            redirect_target(&current, "https://image-cdn-ak.spotifycdn.com/image/next").is_ok()
        );
        assert!(redirect_target(&current, "https://localhost/secret").is_err());
        assert!(redirect_target(&current, "http://i.scdn.co/image/next").is_err());
        assert!(redirect_target(&current, "https://i.scdn.co:444/image/next").is_err());
    }

    #[test]
    fn chunked_artwork_cannot_cross_the_byte_cap() {
        let mut bytes = vec![0u8; MAX_ART_BYTES - 2];
        append_art_chunk(&mut bytes, &[1, 2]).unwrap();
        assert!(append_art_chunk(&mut bytes, &[3]).is_err());
        assert_eq!(bytes.len(), MAX_ART_BYTES);
    }

    #[test]
    fn artwork_mime_magic_and_pixel_budget_must_agree() {
        let mut image = image::RgbImage::new(2, 2);
        for pixel in image.pixels_mut() {
            *pixel = image::Rgb([42, 42, 42]);
        }
        let mut png = Vec::new();
        image
            .write_to(
                &mut std::io::Cursor::new(&mut png),
                image::ImageFormat::Png,
            )
            .unwrap();
        assert!(validate_art_bytes(&png, Some("image/png")).is_ok());
        assert!(validate_art_bytes(&png, Some("image/jpeg")).is_err());
        assert!(validate_art_bytes(b"not an image", Some("image/png")).is_err());

        let mut oversized = vec![0u8; 24];
        oversized[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        oversized[12..16].copy_from_slice(b"IHDR");
        oversized[16..20].copy_from_slice(&10_000u32.to_be_bytes());
        oversized[20..24].copy_from_slice(&10_000u32.to_be_bytes());
        assert!(validate_art_bytes(&oversized, Some("image/png")).is_err());
    }

    #[test]
    fn accent_color_finds_dominant_hue() {
        let mut image = image::RgbImage::new(16, 16);
        for (x, _, pixel) in image.enumerate_pixels_mut() {
            *pixel = if x < 12 {
                image::Rgb([20, 120, 200])
            } else {
                image::Rgb([255, 255, 255])
            };
        }
        let mut bytes = Vec::new();
        image
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        let color = accent_color(&bytes).unwrap();
        assert!(
            color[2] > color[0],
            "expected the blue field, got {color:?}"
        );
    }
}
