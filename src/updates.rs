//! Whether a newer release exists, from GitHub's releases.
//!
//! This default-off check uses one fixed API endpoint and points only at one
//! fixed releases page. The API response cannot choose a browser destination.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/crmne/fastpotify/releases/latest";
const RELEASE_PAGE_URL: &str = "https://github.com/crmne/fastpotify/releases/latest";
const MAX_RELEASE_BODY_BYTES: usize = 64 * 1024;

/// How often a running app asks again.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// The version number, without a leading `v`.
    pub version: String,
    /// The release page, with every download.
    pub url: String,
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
}

/// The newest release, when it is newer than this build.
pub async fn newer_release(http: &reqwest::Client) -> Result<Option<Release>> {
    let mut response = http
        .get(LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("release check failed")?;
    if response.status().is_redirection() {
        bail!("release check refused an HTTP redirect");
    }
    if !response.status().is_success() {
        bail!("release check answered {}", response.status());
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RELEASE_BODY_BYTES as u64)
    {
        bail!("release listing is too large");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.context("release body failed")? {
        append_bounded(&mut body, &chunk, MAX_RELEASE_BODY_BYTES)?;
    }
    parse_release_body(&body, env!("CARGO_PKG_VERSION"))
}

/// `major.minor.patch`; anything else (a pre-release tag, say) is `None`.
fn parse(version: &str) -> Option<[u64; 3]> {
    if version.is_empty() || version.len() > 64 {
        return None;
    }
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut parsed = [0u64; 3];
    for (index, part) in parts.into_iter().enumerate() {
        if part.is_empty()
            || !part.bytes().all(|byte| byte.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return None;
        }
        parsed[index] = part.parse().ok()?;
    }
    Some(parsed)
}

fn parse_release_body(body: &[u8], current: &str) -> Result<Option<Release>> {
    let latest: LatestRelease =
        serde_json::from_slice(body).context("unexpected release listing")?;
    let version = latest
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&latest.tag_name);
    parse(version).context("release tag is not a strict semantic version")?;
    Ok(is_newer(version, current).then(|| Release {
        version: version.to_string(),
        url: RELEASE_PAGE_URL.to_string(),
    }))
}

fn append_bounded(output: &mut Vec<u8>, chunk: &[u8], limit: usize) -> Result<()> {
    if chunk.len() > limit.saturating_sub(output.len()) {
        bail!("release listing exceeds the {limit}-byte limit");
    }
    output.extend_from_slice(chunk);
    Ok(())
}

/// Whether `candidate` is a later version than `current`. Unparseable
/// versions are never newer, so a pre-release is never announced.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse(candidate), parse(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.1.4", "0.1.3"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.3", "0.1.3"));
        assert!(!is_newer("0.1.2", "0.1.3"));
        assert!(
            !is_newer("0.2.0-rc1", "0.1.3"),
            "pre-releases are not announced"
        );
        assert!(!is_newer("nightly", "0.1.3"));
        assert!(!is_newer("0.2.0.1", "0.1.3"));
        assert!(!is_newer(" 0.2.0", "0.1.3"));
        assert!(!is_newer("01.2.0", "0.1.3"));
        assert!(!is_newer("0.02.0", "0.1.3"));
    }

    #[test]
    fn release_page_never_comes_from_the_api() {
        let body = br#"{"tag_name":"v9.9.9","html_url":"http://127.0.0.1/secret"}"#;
        let release = parse_release_body(body, "0.2.0").unwrap().unwrap();
        assert_eq!(release.version, "9.9.9");
        assert_eq!(release.url, RELEASE_PAGE_URL);
    }

    #[test]
    fn chunked_release_body_is_bounded() {
        let mut body = Vec::new();
        append_bounded(&mut body, b"1234", 8).unwrap();
        append_bounded(&mut body, b"5678", 8).unwrap();
        assert!(append_bounded(&mut body, b"9", 8).is_err());
    }
}
