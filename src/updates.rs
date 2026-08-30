//! Whether a newer release exists, from GitHub's releases.
//!
//! This default-off check uses one of two fixed API endpoints and points only
//! at one fixed releases page. The API response cannot choose a browser
//! destination.

use std::cmp::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const STABLE_RELEASE_URL: &str = "https://api.github.com/repos/crmne/fastpotify/releases/latest";
const PRERELEASE_RELEASES_URL: &str =
    "https://api.github.com/repos/crmne/fastpotify/releases?per_page=20&page=1";
const RELEASE_PAGE_URL: &str = "https://github.com/crmne/fastpotify/releases/latest";
const MAX_RELEASE_BODY_BYTES: usize = 64 * 1024;
const MAX_RELEASE_LIST_ENTRIES: usize = 20;
// CFBundleVersion permits at most 4, 2, and 2 digits in its three components.
const MAX_VERSION_COMPONENTS: [u16; 3] = [9_999, 99, 99];

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
struct GitHubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReleaseChannel {
    Stable,
    Prerelease,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Discovery {
    channel: ReleaseChannel,
    endpoint: &'static str,
}

fn discovery(current: &str) -> Result<Discovery> {
    let current = parse(current).context("current build has an unsupported version")?;
    Ok(if current.prerelease.is_some() {
        Discovery {
            channel: ReleaseChannel::Prerelease,
            endpoint: PRERELEASE_RELEASES_URL,
        }
    } else {
        Discovery {
            channel: ReleaseChannel::Stable,
            endpoint: STABLE_RELEASE_URL,
        }
    })
}

/// The newest release, when it is newer than this build.
pub async fn newer_release(http: &reqwest::Client) -> Result<Option<Release>> {
    let current = env!("CARGO_PKG_VERSION");
    let discovery = discovery(current)?;
    let mut response = http
        .get(discovery.endpoint)
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
    select_release_body(&body, current, discovery.channel)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Version<'a> {
    numbers: [u16; 3],
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
    let mut parsed = [0u16; 3];
    for (index, (part, maximum)) in parts.into_iter().zip(MAX_VERSION_COMPONENTS).enumerate() {
        if part.is_empty()
            || !part.bytes().all(|byte| byte.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return None;
        }
        parsed[index] = part.parse().ok()?;
        if parsed[index] > maximum {
            return None;
        }
    }
    Some(Version {
        numbers: parsed,
        prerelease,
    })
}

fn valid_prerelease(suffix: &str) -> bool {
    !suffix.is_empty()
        && suffix.split('.').all(|part| {
            let bytes = part.as_bytes();
            let shaped = bytes.first().is_some_and(u8::is_ascii_alphanumeric)
                && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
                && bytes
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-');
            let numeric_leading_zero = part.len() > 1
                && part.starts_with('0')
                && part.bytes().all(|byte| byte.is_ascii_digit());
            let rc_leading_zero = part.strip_prefix("rc").is_some_and(|number| {
                number.len() > 1
                    && number.starts_with('0')
                    && number.bytes().all(|byte| byte.is_ascii_digit())
            });
            shaped && !numeric_leading_zero && !rc_leading_zero
        })
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

    let mut left = left.split('.');
    let mut right = right.split('.');
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

fn parse_tag(tag: &str) -> Option<(&str, Version<'_>)> {
    let version = tag.strip_prefix('v')?;
    Some((version, parse(version)?))
}

fn select_release_body(
    body: &[u8],
    current: &str,
    channel: ReleaseChannel,
) -> Result<Option<Release>> {
    match channel {
        ReleaseChannel::Stable => {
            let release: GitHubRelease =
                serde_json::from_slice(body).context("unexpected stable release listing")?;
            select_release(std::slice::from_ref(&release), current, channel)
        }
        ReleaseChannel::Prerelease => {
            let releases: Vec<GitHubRelease> =
                serde_json::from_slice(body).context("unexpected prerelease listing")?;
            if releases.len() > MAX_RELEASE_LIST_ENTRIES {
                bail!("prerelease listing exceeds the entry limit");
            }
            select_release(&releases, current, channel)
        }
    }
}

fn select_release(
    releases: &[GitHubRelease],
    current: &str,
    channel: ReleaseChannel,
) -> Result<Option<Release>> {
    let current = parse(current).context("current build has an unsupported version")?;
    if channel == ReleaseChannel::Prerelease && current.prerelease.is_none() {
        bail!("stable builds cannot use prerelease discovery");
    }

    let mut best: Option<(&str, Version<'_>)> = None;
    for release in releases {
        if release.draft {
            continue;
        }
        let Some((version_text, version)) = parse_tag(&release.tag_name) else {
            continue;
        };
        if release.prerelease != version.prerelease.is_some()
            || (channel == ReleaseChannel::Stable && release.prerelease)
            || !version_cmp(version, current).is_gt()
        {
            continue;
        }
        if best.is_none_or(|(_, best)| version_cmp(version, best).is_gt()) {
            best = Some((version_text, version));
        }
    }

    Ok(best.map(|(version, _)| Release {
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

    const VERSION_FIXTURES: &str = include_str!("../supply-chain/release-version-fixtures.tsv");

    fn release(tag: &str, draft: bool, prerelease: bool) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "draft": draft,
            "prerelease": prerelease,
        })
    }

    #[test]
    fn shared_release_policy_fixtures_match_rust() {
        for (line_number, line) in VERSION_FIXTURES.lines().enumerate() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<_> = line.split('\t').collect();
            match fields.as_slice() {
                ["valid", tag, numeric, prerelease, make_latest] => {
                    let (version_text, version) =
                        parse_tag(tag).unwrap_or_else(|| panic!("line {}: {tag}", line_number + 1));
                    assert_eq!(version_text, tag.strip_prefix('v').unwrap());
                    assert_eq!(
                        version.numbers,
                        numeric
                            .split('.')
                            .map(|part| part.parse::<u16>().unwrap())
                            .collect::<Vec<_>>()[..]
                    );
                    assert_eq!(
                        version.prerelease.is_some(),
                        prerelease.parse::<bool>().unwrap()
                    );
                    assert_eq!(
                        version.prerelease.is_none(),
                        make_latest.parse::<bool>().unwrap()
                    );
                }
                ["invalid", tag] => {
                    assert!(parse_tag(tag).is_none(), "line {}: {tag}", line_number + 1);
                }
                ["order", candidate, current, expected] => {
                    assert_eq!(
                        is_newer(candidate, current),
                        expected.parse::<bool>().unwrap(),
                        "line {}: {candidate} against {current}",
                        line_number + 1
                    );
                }
                ["channel", version, expected] => {
                    let actual = discovery(version).unwrap().channel;
                    assert_eq!(
                        actual,
                        match *expected {
                            "stable" => ReleaseChannel::Stable,
                            "prerelease" => ReleaseChannel::Prerelease,
                            _ => panic!("line {}: unknown channel {expected}", line_number + 1),
                        }
                    );
                }
                _ => panic!("line {}: malformed fixture", line_number + 1),
            }
        }
        assert!(parse(env!("CARGO_PKG_VERSION")).is_some());
    }

    #[test]
    fn production_discovery_uses_fixed_channel_endpoints() {
        assert_eq!(
            discovery("0.4.0").unwrap(),
            Discovery {
                channel: ReleaseChannel::Stable,
                endpoint: "https://api.github.com/repos/crmne/fastpotify/releases/latest",
            }
        );
        assert_eq!(
            discovery("0.4.0-rc1").unwrap(),
            Discovery {
                channel: ReleaseChannel::Prerelease,
                endpoint: "https://api.github.com/repos/crmne/fastpotify/releases?per_page=20&page=1",
            }
        );
        assert!(discovery("0.4.0-rc01").is_err());
    }

    #[test]
    fn stable_endpoint_body_selects_only_a_valid_newer_stable() {
        let body = br#"{
            "tag_name":"v9.9.9",
            "draft":false,
            "prerelease":false,
            "html_url":"http://127.0.0.1/secret"
        }"#;
        let release = select_release_body(body, "0.2.0", ReleaseChannel::Stable)
            .unwrap()
            .unwrap();
        assert_eq!(release.version, "9.9.9");
        assert_eq!(release.url, RELEASE_PAGE_URL);

        for body in [
            &br#"{"tag_name":"v0.2.0","draft":false,"prerelease":false}"#[..],
            &br#"{"tag_name":"v9.9.9","draft":true,"prerelease":false}"#[..],
            &br#"{"tag_name":"v09.9.9","draft":false,"prerelease":false}"#[..],
        ] {
            assert!(
                select_release_body(body, "0.2.0", ReleaseChannel::Stable)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn prerelease_list_selects_newest_valid_nondraft_release() {
        let body = serde_json::to_vec(&vec![
            release("v0.4.0-rc1", false, true),
            release("v0.4.0-rc2", false, true),
            release("v0.4.0-rc2", false, true),
            release("v0.4.0-rc99", true, true),
            release("v00.4.0-rc3", false, true),
            release("v0.4.0", false, true),
            release("v0.4.0-rc50", false, false),
        ])
        .unwrap();
        let release = select_release_body(&body, "0.4.0-rc1", ReleaseChannel::Prerelease)
            .unwrap()
            .unwrap();
        assert_eq!(release.version, "0.4.0-rc2");
        assert_eq!(release.url, RELEASE_PAGE_URL);
    }

    #[test]
    fn prerelease_list_orders_rcs_and_final_independently_of_body_order() {
        let body = serde_json::to_vec(&vec![
            release("v0.4.0-rc2", false, true),
            release("v0.4.0", false, false),
            release("v0.4.0-rc10", false, true),
        ])
        .unwrap();
        let selected = select_release_body(&body, "0.4.0-rc1", ReleaseChannel::Prerelease)
            .unwrap()
            .unwrap();
        assert_eq!(selected.version, "0.4.0");

        let rc_body = serde_json::to_vec(&vec![
            release("v0.4.0-rc2", false, true),
            release("v0.4.0-rc10", false, true),
        ])
        .unwrap();
        let selected = select_release_body(&rc_body, "0.4.0-rc1", ReleaseChannel::Prerelease)
            .unwrap()
            .unwrap();
        assert_eq!(selected.version, "0.4.0-rc10");
    }

    #[test]
    fn prerelease_list_handles_empty_no_newer_and_malformed_bodies() {
        assert!(
            select_release_body(b"[]", "0.4.0-rc1", ReleaseChannel::Prerelease)
                .unwrap()
                .is_none()
        );
        let body = serde_json::to_vec(&vec![
            release("v0.4.0-rc1", false, true),
            release("v0.3.9", false, false),
        ])
        .unwrap();
        assert!(
            select_release_body(&body, "0.4.0-rc1", ReleaseChannel::Prerelease)
                .unwrap()
                .is_none()
        );
        assert!(select_release_body(b"{}", "0.4.0-rc1", ReleaseChannel::Prerelease).is_err());
        assert!(select_release_body(&body, "0.4.0", ReleaseChannel::Prerelease).is_err());
    }

    #[test]
    fn prerelease_list_entry_count_is_bounded() {
        let at_limit = vec![release("v0.3.9", false, false); MAX_RELEASE_LIST_ENTRIES];
        assert!(
            select_release_body(
                &serde_json::to_vec(&at_limit).unwrap(),
                "0.4.0-rc1",
                ReleaseChannel::Prerelease,
            )
            .unwrap()
            .is_none()
        );
        let over_limit = vec![release("v0.3.9", false, false); MAX_RELEASE_LIST_ENTRIES + 1];
        assert!(
            select_release_body(
                &serde_json::to_vec(&over_limit).unwrap(),
                "0.4.0-rc1",
                ReleaseChannel::Prerelease,
            )
            .is_err()
        );
    }

    #[test]
    fn chunked_release_body_is_bounded() {
        let mut body = vec![b'x'; MAX_RELEASE_BODY_BYTES - 1];
        append_bounded(&mut body, b"y", MAX_RELEASE_BODY_BYTES).unwrap();
        assert!(append_bounded(&mut body, b"z", MAX_RELEASE_BODY_BYTES).is_err());
    }
}
