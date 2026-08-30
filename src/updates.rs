//! Whether a newer release exists, from GitHub's releases.
//!
//! This default-off check uses one fixed API endpoint and points only at one
//! fixed releases page. The API response cannot choose a browser destination.

use std::cmp::Ordering;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Version<'a> {
    numbers: [u64; 3],
    prerelease: Option<&'a str>,
}

/// The release-policy version grammar without its leading `v`.
fn parse(version: &str) -> Option<Version<'_>> {
    if version.is_empty() || version.len() > 64 {
        return None;
    }
    let (numbers, prerelease) = match version.split_once('-') {
        Some((numbers, suffix)) if valid_prerelease(suffix) => (numbers, Some(suffix)),
        Some(_) => return None,
        None => (version, None),
    };
    let parts: Vec<&str> = numbers.split('.').collect();
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
    Some(Version {
        numbers: parsed,
        prerelease,
    })
}

fn valid_prerelease(suffix: &str) -> bool {
    !suffix.is_empty()
        && suffix
            .split(['.', '-'])
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric()))
}

fn numeric_text_cmp(left: &str, right: &str) -> Ordering {
    let left = left.trim_start_matches('0');
    let right = right.trim_start_matches('0');
    let left = if left.is_empty() { "0" } else { left };
    let right = if right.is_empty() { "0" } else { right };
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

fn rc_number(value: &str) -> Option<&str> {
    let number = value.strip_prefix("rc")?;
    (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())).then_some(number)
}

fn prerelease_cmp(left: &str, right: &str) -> Ordering {
    if let (Some(left), Some(right)) = (rc_number(left), rc_number(right)) {
        return numeric_text_cmp(left, right);
    }

    let mut left = left.split(['.', '-']);
    let mut right = right.split(['.', '-']);
    loop {
        match (left.next(), right.next()) {
            (Some(left), Some(right)) => {
                let left_numeric = left.bytes().all(|byte| byte.is_ascii_digit());
                let right_numeric = right.bytes().all(|byte| byte.is_ascii_digit());
                let order = match (left_numeric, right_numeric) {
                    (true, true) => numeric_text_cmp(left, right),
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    (false, false) => left.cmp(right),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(_), None) => return Ordering::Greater,
            (None, Some(_)) => return Ordering::Less,
            (None, None) => return Ordering::Equal,
        }
    }
}

fn version_cmp(left: Version<'_>, right: Version<'_>) -> Ordering {
    left.numbers
        .cmp(&right.numbers)
        .then_with(|| match (left.prerelease, right.prerelease) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(left), Some(right)) => prerelease_cmp(left, right),
        })
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

/// Whether `candidate` is a later release-policy version than `current`.
/// Stable builds stay on the stable channel; prerelease builds may advance
/// through later prereleases and then to the final release. Unparseable
/// versions are never newer.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse(candidate), parse(current)) {
        (Some(candidate), Some(current))
            if candidate.prerelease.is_some() && current.prerelease.is_none() =>
        {
            false
        }
        (Some(candidate), Some(current)) => version_cmp(candidate, current).is_gt(),
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
        assert!(!is_newer("0.2.0-rc1", "0.1.3"));
        assert!(is_newer("0.4.0-rc2", "0.4.0-rc1"));
        assert!(is_newer("0.4.0-rc10", "0.4.0-rc2"));
        assert!(is_newer("0.5.0-rc1", "0.4.0-rc10"));
        assert!(!is_newer("0.4.0-rc1", "0.4.0-rc2"));
        assert!(is_newer("0.4.0", "0.4.0-rc10"));
        assert!(!is_newer("0.4.0-rc10", "0.4.0"));
        assert!(is_newer("0.4.0-beta.10", "0.4.0-beta.2"));
        assert!(!is_newer("nightly", "0.1.3"));
        assert!(!is_newer("0.2.0.1", "0.1.3"));
        assert!(!is_newer(" 0.2.0", "0.1.3"));
        assert!(!is_newer("01.2.0", "0.1.3"));
        assert!(!is_newer("0.02.0", "0.1.3"));
        assert!(!is_newer("0.2.0-", "0.1.3"));
        assert!(!is_newer("0.2.0-rc_1", "0.1.3"));
        assert!(!is_newer("0.2.0-rc..1", "0.1.3"));
    }

    #[test]
    fn stable_and_prerelease_boundaries_parse_under_one_policy() {
        assert_eq!(
            parse("0.4.0"),
            Some(Version {
                numbers: [0, 4, 0],
                prerelease: None,
            })
        );
        assert_eq!(
            parse("0.4.0-rc1"),
            Some(Version {
                numbers: [0, 4, 0],
                prerelease: Some("rc1"),
            })
        );
        for invalid in [
            "v0.4.0",
            "0.4",
            "0.4.0.1",
            "0.4.0-",
            "0.4.0--rc1",
            "0.4.0 rc1",
            " 0.4.0",
        ] {
            assert!(parse(invalid).is_none(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn release_page_never_comes_from_the_api() {
        let body = br#"{"tag_name":"v9.9.9","html_url":"http://127.0.0.1/secret"}"#;
        let release = parse_release_body(body, "0.2.0").unwrap().unwrap();
        assert_eq!(release.version, "9.9.9");
        assert_eq!(release.url, RELEASE_PAGE_URL);
    }

    #[test]
    fn release_body_preserves_prerelease_versions() {
        let body = br#"{"tag_name":"v0.4.0-rc2"}"#;
        let release = parse_release_body(body, "0.4.0-rc1").unwrap().unwrap();
        assert_eq!(release.version, "0.4.0-rc2");
        assert_eq!(release.url, RELEASE_PAGE_URL);

        let final_body = br#"{"tag_name":"v0.4.0"}"#;
        assert!(
            parse_release_body(final_body, "0.4.0-rc2")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn chunked_release_body_is_bounded() {
        let mut body = Vec::new();
        append_bounded(&mut body, b"1234", 8).unwrap();
        append_bounded(&mut body, b"5678", 8).unwrap();
        assert!(append_bounded(&mut body, b"9", 8).is_err());
    }
}
