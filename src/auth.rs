//! Spotify sign-in with the Authorization Code + PKCE flow.
//!
//! Three grants may exist because Spotify treats them differently:
//!
//! - The shared and optional personal **Web API grants** use independent
//!   registered application identities, refresh tokens, and request sessions.
//! - The **playback grant** uses Spotify's desktop client identity, the one
//!   librespot streams with. Its access token is exchanged once for a
//!   reusable credential that Fastpotify stores separately.
//!
//! The browser does the password entry; this process only ever sees the
//! one-time authorization code that Spotify sends back to a loopback
//! listener.

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// Spotify's own desktop client identity, the one librespot streams with.
pub const PLAYBACK_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
pub const PLAYBACK_REDIRECT_PORT: u16 = 8898;

/// The public Web API application shared by spotify-player, ncspot, and
/// Omarchy Spotify.
pub const DEFAULT_WEB_CLIENT_ID: &str = "d420a117a32841c2b3474932e49fb54b";
pub const WEB_REDIRECT_PORT: u16 = 8989;

pub const REDIRECT_PATH: &str = "/login";

/// Playback: what librespot needs to stream and join Spotify Connect.
pub const PLAYBACK_SCOPES: &[&str] = &[
    "app-remote-control",
    "streaming",
    "user-modify-playback-state",
    "user-read-currently-playing",
    "user-read-playback-state",
    "user-read-private",
];

/// Web API: what visible features use, plus `user-read-private` for the
/// plan (Free or Premium), which decides whether local playback is offered
/// at all; no email.
pub const WEB_SCOPES: &[&str] = &[
    "playlist-modify-private",
    "playlist-modify-public",
    "playlist-read-collaborative",
    "playlist-read-private",
    "user-follow-modify",
    "user-follow-read",
    "user-library-modify",
    "user-library-read",
    "user-modify-playback-state",
    "user-read-playback-position",
    "user-read-playback-state",
    "user-read-private",
    "user-read-recently-played",
    "user-top-read",
];

const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_REQUEST_LINE_BYTES: usize = 2048;
const MAX_REQUEST_HEAD_BYTES: usize = 16 * 1024;
const MAX_CALLBACK_BODY_BYTES: usize = 0;
const MAX_QUERY_VALUE_BYTES: usize = 4096;
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_ACCESS_TOKEN_BYTES: usize = 16 * 1024;
const MAX_REFRESH_TOKEN_BYTES: usize = 16 * 1024;
const MAX_SCOPE_BYTES: usize = 8 * 1024;
/// Refresh this long before the access token expires.
const REFRESH_MARGIN: Duration = Duration::from_secs(90);

/// One OAuth application identity and what it is asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub client_id: String,
    pub redirect_port: u16,
    pub scopes: &'static [&'static str],
}

impl Grant {
    pub fn playback() -> Self {
        Self {
            client_id: PLAYBACK_CLIENT_ID.to_string(),
            redirect_port: PLAYBACK_REDIRECT_PORT,
            scopes: PLAYBACK_SCOPES,
        }
    }

    pub fn shared_web_api() -> Self {
        Self {
            client_id: DEFAULT_WEB_CLIENT_ID.to_string(),
            redirect_port: WEB_REDIRECT_PORT,
            scopes: WEB_SCOPES,
        }
    }

    pub fn personal_web_api(client_id: &str) -> Result<Self> {
        let client_id = client_id.trim();
        if client_id.len() != 32 || !client_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("a Spotify Client ID must be 32 hexadecimal characters");
        }
        if client_id.eq_ignore_ascii_case(DEFAULT_WEB_CLIENT_ID) {
            bail!("the shared Spotify Client ID cannot be added as a personal app");
        }
        Ok(Self {
            client_id: client_id.to_string(),
            redirect_port: WEB_REDIRECT_PORT,
            scopes: WEB_SCOPES,
        })
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}{REDIRECT_PATH}", self.redirect_port)
    }
}

/// The one-time browser material held behind an already-bound listener.
struct Flow {
    verifier: String,
    state: String,
    url: String,
}

/// A browser authorization that cannot be exposed until its callback socket
/// is listening. The standard listener keeps accepting in the kernel while
/// the asynchronous task is scheduled.
pub struct PreparedAuthorization {
    listener: std::net::TcpListener,
    flow: Flow,
    port: u16,
}

pub struct AuthorizationCode {
    pub code: String,
    pub verifier: String,
}

impl PreparedAuthorization {
    pub fn prepare(grant: &Grant) -> Result<Self> {
        let listener = bind_callback_listener(grant.redirect_port)?;
        let port = listener
            .local_addr()
            .context("unable to inspect the prepared Spotify redirect listener")?
            .port();
        let mut bound_grant = grant.clone();
        bound_grant.redirect_port = port;
        Ok(Self {
            listener,
            flow: begin(&bound_grant),
            port,
        })
    }

    pub fn url(&self) -> &str {
        &self.flow.url
    }

    pub async fn wait(self, cancel: watch::Receiver<bool>) -> Result<AuthorizationCode> {
        let Self {
            listener,
            flow,
            port,
        } = self;
        let code = wait_for_code(listener, port, &flow.state, cancel).await?;
        Ok(AuthorizationCode {
            code,
            verifier: flow.verifier,
        })
    }
}

fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);
    URL_SAFE_NO_PAD.encode(buffer)
}

fn begin(grant: &Grant) -> Flow {
    let verifier = random_token(48);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_token(18);
    let url = format!(
        "{AUTHORIZE_URL}?client_id={}&response_type=code&redirect_uri={}&code_challenge_method=S256&code_challenge={challenge}&state={state}&scope={}",
        urlencoding::encode(&grant.client_id),
        urlencoding::encode(&grant.redirect_uri()),
        urlencoding::encode(&grant.scopes.join(" "))
    );
    Flow {
        verifier,
        state,
        url,
    }
}

fn bind_callback_listener(port: u16) -> Result<std::net::TcpListener> {
    let address: SocketAddr = ([127, 0, 0, 1], port).into();
    let listener = std::net::TcpListener::bind(address)
        .with_context(|| format!("unable to listen on {address} for the Spotify redirect"))?;
    listener
        .set_nonblocking(true)
        .with_context(|| format!("unable to prepare the listener on {address}"))?;
    Ok(listener)
}

#[derive(Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    #[serde(default)]
    pub scope: Option<String>,
}

impl TokenResponse {
    fn validate(&self) -> Result<()> {
        validate_token_value("access token", &self.access_token, MAX_ACCESS_TOKEN_BYTES)?;
        if !self.token_type.eq_ignore_ascii_case("bearer") {
            bail!("Spotify returned an unsupported token type");
        }
        if !(1..=7 * 24 * 60 * 60).contains(&self.expires_in) {
            bail!("Spotify returned an invalid token lifetime");
        }
        if let Some(refresh) = &self.refresh_token {
            validate_token_value("refresh token", refresh, MAX_REFRESH_TOKEN_BYTES)?;
        }
        if let Some(scope) = &self.scope {
            validate_scope(scope)?;
        }
        Ok(())
    }
}

fn validate_token_value(label: &str, value: &str, limit: usize) -> Result<()> {
    if value.is_empty() || value.len() > limit || !value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        bail!("Spotify returned an invalid {label}");
    }
    Ok(())
}

fn validate_scope(scope: &str) -> Result<()> {
    if scope.len() > MAX_SCOPE_BYTES
        || !scope
            .bytes()
            .all(|byte| byte == b' ' || (byte.is_ascii_graphic() && byte != b'"'))
    {
        bail!("Spotify returned an invalid scope list");
    }
    Ok(())
}

/// Waits on an already-bound listener. Malformed, unrelated, and stale local
/// traffic is retryable; a valid-state provider result is terminal.
async fn wait_for_code(
    listener: std::net::TcpListener,
    port: u16,
    expected_state: &str,
    mut cancel: watch::Receiver<bool>,
) -> Result<String> {
    let address: SocketAddr = ([127, 0, 0, 1], port).into();
    let listener = TcpListener::from_std(listener)
        .with_context(|| format!("unable to attach the listener on {address} to the runtime"))?;
    let deadline = tokio::time::Instant::now() + LOGIN_TIMEOUT;

    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => accepted.context("redirect listener failed")?,
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() { bail!("sign-in cancelled"); }
                continue;
            }
            _ = tokio::time::sleep_until(deadline) => bail!("sign-in timed out; try again"),
        };
        if !peer.ip().is_loopback() {
            continue;
        }
        let connection_deadline = deadline.min(tokio::time::Instant::now() + CONNECTION_TIMEOUT);
        let disposition = tokio::select! {
            biased;
            handled = serve_callback(stream, expected_state, port, connection_deadline) => handled,
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() { bail!("sign-in cancelled"); }
                continue;
            }
            _ = tokio::time::sleep_until(deadline) => bail!("sign-in timed out; try again"),
        };
        match disposition {
            CallbackDisposition::Finish(result) => return result,
            CallbackDisposition::Retry(error) => {
                log::debug!("ignored request on the OAuth callback listener: {error}");
            }
        }
    }
}

#[derive(Debug)]
enum ProviderCallback {
    Accepted(String),
    Denied,
    Invalid(&'static str),
}

impl ProviderCallback {
    fn response(&self) -> (&'static str, &'static str) {
        match self {
            Self::Accepted(_) => ("200 OK", success_page()),
            Self::Denied | Self::Invalid(_) => ("400 Bad Request", failure_page()),
        }
    }

    fn into_result(self) -> Result<String> {
        match self {
            Self::Accepted(code) => Ok(code),
            Self::Denied => bail!("Spotify declined the sign-in"),
            Self::Invalid(message) => bail!("{message}"),
        }
    }
}

enum CallbackDisposition {
    Retry(anyhow::Error),
    Finish(Result<String>),
}

async fn serve_callback(
    mut stream: TcpStream,
    expected_state: &str,
    port: u16,
    deadline: tokio::time::Instant,
) -> CallbackDisposition {
    let request = match tokio::time::timeout_at(deadline, read_request_head(&mut stream)).await {
        Ok(Ok(request)) => request,
        Ok(Err(error)) => return CallbackDisposition::Retry(error),
        Err(_) => {
            return CallbackDisposition::Retry(anyhow!(
                "callback connection timed out before a complete request"
            ));
        }
    };
    let parsed = parse_callback_request(&request, expected_state, port);
    respond_and_classify(parsed, |status, body| {
        spawn_callback_response(stream, status, body, deadline);
        Ok(())
    })
}

/// Browser delivery is deliberately outside the terminal-result decision.
/// Once state authenticates a code or denial, no write or disconnect may turn
/// it back into retryable traffic.
fn respond_and_classify(
    parsed: Result<ProviderCallback>,
    respond: impl FnOnce(&'static str, &'static str) -> Result<()>,
) -> CallbackDisposition {
    let (status, body) = match &parsed {
        Ok(provider) => provider.response(),
        Err(_) => ("400 Bad Request", failure_page()),
    };
    if let Err(error) = respond(status, body) {
        log::debug!("OAuth callback response could not be scheduled: {error}");
    }
    match parsed {
        Ok(provider) => CallbackDisposition::Finish(provider.into_result()),
        Err(error) => CallbackDisposition::Retry(error),
    }
}

fn spawn_callback_response(
    mut stream: TcpStream,
    status: &'static str,
    body: &'static str,
    deadline: tokio::time::Instant,
) {
    tokio::spawn(async move {
        match tokio::time::timeout_at(deadline, write_callback_response(&mut stream, status, body))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                log::debug!("OAuth callback response delivery failed: {error}")
            }
            Err(_) => log::debug!("OAuth callback response delivery timed out"),
        }
    });
}

async fn read_request_head(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let count = stream
            .read(&mut chunk)
            .await
            .context("callback read failed")?;
        if count == 0 {
            bail!("callback connection closed before its headers");
        }
        if request.len() + count > MAX_REQUEST_HEAD_BYTES {
            bail!("callback request headers are too large");
        }
        request.extend_from_slice(&chunk[..count]);
        if let Some(line_end) = find_bytes(&request, b"\r\n") {
            if line_end > MAX_REQUEST_LINE_BYTES {
                bail!("callback request line is too large");
            }
        } else if request.len() > MAX_REQUEST_LINE_BYTES {
            bail!("callback request line is too large");
        }
        if find_bytes(&request, b"\r\n\r\n").is_some() {
            return Ok(request);
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_callback_request(
    request: &[u8],
    expected_state: &str,
    port: u16,
) -> Result<ProviderCallback> {
    if request.len() > MAX_REQUEST_HEAD_BYTES {
        bail!("callback request headers are too large");
    }
    let end = find_bytes(request, b"\r\n\r\n")
        .ok_or_else(|| anyhow!("callback request headers are incomplete"))?;
    if request.len() - end - 4 > MAX_CALLBACK_BODY_BYTES {
        bail!("callback request bodies are not accepted");
    }
    let head =
        std::str::from_utf8(&request[..end]).context("callback request headers are not UTF-8")?;
    if head
        .bytes()
        .any(|byte| byte == 0 || (byte < 0x20 && byte != b'\r' && byte != b'\n' && byte != b'\t'))
    {
        bail!("callback request contains control characters");
    }
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow!("callback request line is missing"))?;
    if request_line.len() > MAX_REQUEST_LINE_BYTES {
        bail!("callback request line is too large");
    }
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if parts.next().is_some() || method != "GET" || version != "HTTP/1.1" || target.is_empty() {
        bail!("callback must be an exact HTTP/1.1 GET");
    }
    if target.len() > MAX_REQUEST_LINE_BYTES || target.contains('#') || !target.starts_with('/') {
        bail!("callback request target is invalid");
    }

    let mut host = None;
    let mut content_length = None;
    for (index, line) in lines.enumerate() {
        if index >= 64 || line.is_empty() || line.starts_with(' ') || line.starts_with('\t') {
            bail!("callback request headers are malformed");
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow!("callback request header is malformed"))?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
        {
            bail!("callback request header name is invalid");
        }
        let value = value.trim();
        if value.bytes().any(|byte| byte < 0x20 && byte != b'\t') {
            bail!("callback request header value is invalid");
        }
        if name.eq_ignore_ascii_case("host") {
            if host.replace(value).is_some() {
                bail!("callback request has duplicate Host headers");
            }
        } else if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                bail!("callback request has duplicate Content-Length headers");
            }
            content_length = Some(
                value
                    .parse::<usize>()
                    .context("callback Content-Length is invalid")?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("expect")
        {
            bail!("callback request body framing is not accepted");
        }
    }
    let expected_host = format!("127.0.0.1:{port}");
    if host != Some(expected_host.as_str()) {
        bail!("callback Host does not match the loopback listener");
    }
    if content_length.unwrap_or(0) != MAX_CALLBACK_BODY_BYTES {
        bail!("callback request bodies are not accepted");
    }

    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != REDIRECT_PATH {
        bail!("callback path is not the registered redirect path");
    }
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&') {
        if pair.is_empty() {
            bail!("callback query is malformed");
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if value.len() > MAX_QUERY_VALUE_BYTES {
            bail!("callback query value is too large");
        }
        if !valid_percent_encoding(value.as_bytes()) {
            bail!("callback query encoding is invalid");
        }
        let value = urlencoding::decode(value)
            .context("callback query encoding is invalid")?
            .into_owned();
        if value.len() > MAX_QUERY_VALUE_BYTES
            || value
                .bytes()
                .any(|byte| byte == 0 || byte == b'\r' || byte == b'\n')
        {
            bail!("callback query value is invalid");
        }
        match key {
            "code" => set_once(&mut code, value, "code")?,
            "state" => set_once(&mut state, value, "state")?,
            "error" => set_once(&mut error, value, "error")?,
            _ => {}
        }
    }
    if !state
        .as_deref()
        .is_some_and(|received| constant_time_eq(received.as_bytes(), expected_state.as_bytes()))
    {
        bail!("callback state mismatch");
    }
    match (code, error) {
        (Some(code), None)
            if !code.is_empty() && code.bytes().all(|byte| byte.is_ascii_graphic()) =>
        {
            Ok(ProviderCallback::Accepted(code))
        }
        (Some(_), None) => Ok(ProviderCallback::Invalid(
            "Spotify returned an invalid authorization code",
        )),
        (None, Some(_)) => Ok(ProviderCallback::Denied),
        (Some(_), Some(_)) => Ok(ProviderCallback::Invalid(
            "Spotify returned conflicting sign-in results",
        )),
        (None, None) => Ok(ProviderCallback::Invalid(
            "Spotify did not return an authorization result",
        )),
    }
}

fn valid_percent_encoding(value: &[u8]) -> bool {
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'%' {
            if index + 2 >= value.len()
                || !value[index + 1].is_ascii_hexdigit()
                || !value[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

fn set_once(slot: &mut Option<String>, value: String, label: &str) -> Result<()> {
    if slot.replace(value).is_some() {
        bail!("callback has a duplicate {label} parameter");
    }
    Ok(())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| {
            difference | (*left ^ *right)
        })
        == 0
}

async fn write_callback_response(stream: &mut TcpStream, status: &str, body: &str) -> Result<()> {
    let response = callback_response(status, body);
    stream
        .write_all(response.as_bytes())
        .await
        .context("callback response write failed")?;
    stream
        .shutdown()
        .await
        .context("callback response shutdown failed")
}

fn callback_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n\
X-Content-Type-Options: nosniff\r\n\
Referrer-Policy: no-referrer\r\n\
Cache-Control: no-store, max-age=0\r\n\
Pragma: no-cache\r\n\
Cross-Origin-Opener-Policy: same-origin\r\n\
Connection: close\r\n\
Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

pub async fn exchange_code(
    http: &reqwest::Client,
    grant: &Grant,
    code: &str,
    verifier: &str,
) -> Result<TokenResponse> {
    token_request(
        http,
        &[
            ("client_id", grant.client_id.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", grant.redirect_uri().as_str()),
            ("code_verifier", verifier),
        ],
    )
    .await
    .map_err(anyhow::Error::new)
}

pub async fn refresh(
    http: &reqwest::Client,
    client_id: &str,
    refresh_token: &str,
) -> std::result::Result<TokenResponse, TokenEndpointError> {
    token_request(
        http,
        &[
            ("client_id", client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ],
    )
    .await
}

#[derive(Debug, thiserror::Error)]
pub enum TokenEndpointError {
    /// Spotify refused the grant itself; only a browser flow can replace it.
    #[error("Spotify rejected the token request ({status}): {detail}")]
    Rejected { status: u16, detail: String },
    /// The endpoint or response failed without proving that the grant is bad.
    #[error("token request failed: {0}")]
    Unreachable(String),
}

async fn token_request(
    http: &reqwest::Client,
    form: &[(&str, &str)],
) -> std::result::Result<TokenResponse, TokenEndpointError> {
    let mut response = http
        .post(TOKEN_URL)
        .form(form)
        .send()
        .await
        .map_err(|error| TokenEndpointError::Unreachable(error.to_string()))?;
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_TOKEN_RESPONSE_BYTES as u64)
    {
        return Err(TokenEndpointError::Unreachable(
            "Spotify token response is too large".into(),
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| TokenEndpointError::Unreachable(error.to_string()))?
    {
        append_bounded(&mut body, &chunk, MAX_TOKEN_RESPONSE_BYTES)
            .map_err(|error| TokenEndpointError::Unreachable(error.to_string()))?;
    }
    if !status.is_success() {
        let detail = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value["error_description"]
                    .as_str()
                    .or(value["error"].as_str())
                    .and_then(safe_provider_detail)
            })
            .unwrap_or_else(|| "request was rejected".into());
        if matches!(status.as_u16(), 400 | 401 | 403) {
            return Err(TokenEndpointError::Rejected {
                status: status.as_u16(),
                detail,
            });
        }
        return Err(TokenEndpointError::Unreachable(format!(
            "Spotify answered {status}: {detail}"
        )));
    }
    let token: TokenResponse = serde_json::from_slice(&body).map_err(|error| {
        TokenEndpointError::Unreachable(format!("unexpected token response: {error}"))
    })?;
    token
        .validate()
        .map_err(|error| TokenEndpointError::Unreachable(error.to_string()))?;
    Ok(token)
}

fn append_bounded(output: &mut Vec<u8>, chunk: &[u8], limit: usize) -> Result<()> {
    if chunk.len() > limit.saturating_sub(output.len()) {
        bail!("response body exceeds the {limit}-byte limit");
    }
    output.extend_from_slice(chunk);
    Ok(())
}

fn safe_provider_detail(value: &str) -> Option<String> {
    let detail: String = value
        .chars()
        .filter(|character| character.is_ascii_graphic() || *character == ' ')
        .take(160)
        .collect();
    (!detail.trim().is_empty()).then(|| detail.trim().to_string())
}

/// The Web API grant as kept on disk between runs.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StoredToken {
    pub client_id: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: u64,
    #[serde(default)]
    pub scope: String,
}

impl StoredToken {
    pub fn from_response(
        client_id: &str,
        response: TokenResponse,
        previous_refresh: Option<&str>,
    ) -> Result<Self> {
        response.validate()?;
        let refresh_token = response
            .refresh_token
            .or_else(|| previous_refresh.map(str::to_string))
            .ok_or_else(|| anyhow!("Spotify did not return a refresh token"))?;
        validate_token_value("refresh token", &refresh_token, MAX_REFRESH_TOKEN_BYTES)?;
        Ok(Self {
            client_id: client_id.to_string(),
            access_token: response.access_token,
            refresh_token,
            expires_at: now_secs().saturating_add(response.expires_in),
            scope: response.scope.unwrap_or_default(),
        })
    }

    pub fn needs_refresh(&self) -> bool {
        now_secs() + REFRESH_MARGIN.as_secs() >= self.expires_at
    }

    pub fn expired(&self) -> bool {
        now_secs() >= self.expires_at
    }

    pub fn validate(&self) -> Result<()> {
        if self.client_id.is_empty()
            || self.client_id.len() > 512
            || !self.client_id.bytes().all(|byte| byte.is_ascii_graphic())
        {
            bail!("stored Spotify client id is invalid");
        }
        validate_token_value(
            "stored access token",
            &self.access_token,
            MAX_ACCESS_TOKEN_BYTES,
        )?;
        validate_token_value(
            "stored refresh token",
            &self.refresh_token,
            MAX_REFRESH_TOKEN_BYTES,
        )?;
        validate_scope(&self.scope)
    }

    /// Whether the grant covers every scope in `scopes`. A grant cannot be
    /// widened by a refresh, only by the browser.
    pub fn has_scopes(&self, scopes: &[&str]) -> bool {
        let granted: Vec<&str> = self.scope.split_whitespace().collect();
        scopes.iter().all(|scope| granted.contains(scope))
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

const SUCCESS_PAGE: &str = "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Signed in to Fastpotify</title>\
<style>:root{color-scheme:dark}body{margin:0;min-height:100vh;display:grid;place-items:center;background:#0f1114;color:#e8eaed;font-family:system-ui,sans-serif}main{max-width:28rem;padding:2.5rem;border-radius:1.25rem;background:#181b20;text-align:center}.mark{width:64px;height:64px;border-radius:50%;background:#1ed760;display:grid;place-items:center;margin:0 auto 1.25rem}.mark svg{width:30px;height:30px;fill:#0f1114}h1{font-size:1.4rem;margin:.25rem 0 .5rem}p{color:#a5adba;line-height:1.5;margin:0}</style>\
<main><div class=\"mark\"><svg viewBox=\"0 0 24 24\"><path d=\"M5 5a2 2 0 0 1 3.008-1.728l11.997 6.998a2 2 0 0 1 .003 3.458l-12 7A2 2 0 0 1 5 19z\"/></svg></div><h1>You are signed in</h1><p>You can close this tab and return to Fastpotify.</p></main></html>";

const FAILURE_PAGE: &str = "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Fastpotify sign-in did not complete</title>\
<style>:root{color-scheme:dark}body{margin:0;min-height:100vh;display:grid;place-items:center;background:#0f1114;color:#e8eaed;font-family:system-ui,sans-serif}main{max-width:28rem;padding:2.5rem;border-radius:1.25rem;background:#181b20;text-align:center}.mark{width:64px;height:64px;border-radius:50%;background:#f5717f;display:grid;place-items:center;margin:0 auto 1.25rem}.mark svg{width:30px;height:30px;fill:#0f1114}h1{font-size:1.4rem;margin:.25rem 0 .5rem}p{color:#a5adba;line-height:1.5;margin:0}</style>\
<main><div class=\"mark\"><svg viewBox=\"0 0 24 24\"><path d=\"M5 5a2 2 0 0 1 3.008-1.728l11.997 6.998a2 2 0 0 1 .003 3.458l-12 7A2 2 0 0 1 5 19z\"/></svg></div><h1>Sign-in did not complete</h1><p>Return to Fastpotify and try again.</p></main></html>";

fn success_page() -> &'static str {
    SUCCESS_PAGE
}

fn failure_page() -> &'static str {
    FAILURE_PAGE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_produces_valid_pkce_material() {
        let flow = begin(&Grant::shared_web_api());
        assert!(flow.verifier.len() >= 43);
        assert!(flow.url.contains("code_challenge_method=S256"));
        assert!(flow.url.contains(&format!("state={}", flow.state)));
        assert!(
            flow.url
                .contains(&format!("client_id={DEFAULT_WEB_CLIENT_ID}"))
        );
        assert!(flow.url.contains("8989"));
        let playback = begin(&Grant::playback());
        assert!(
            playback
                .url
                .contains(&format!("client_id={PLAYBACK_CLIENT_ID}"))
        );
        assert!(playback.url.contains("8898"));
    }

    #[test]
    fn personal_client_id_is_validated_at_the_boundary() {
        let client_id = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            Grant::personal_web_api(&format!("  {client_id} "))
                .unwrap()
                .client_id,
            client_id
        );
        assert!(Grant::personal_web_api("  ").is_err());
        assert!(Grant::personal_web_api("not&a=spotify-client-id-value!!").is_err());
        assert!(Grant::personal_web_api(DEFAULT_WEB_CLIENT_ID).is_err());
    }

    #[test]
    fn prepared_authorization_owns_the_listener_before_exposing_its_url() {
        let mut grant = Grant::shared_web_api();
        grant.redirect_port = 0;
        let session = PreparedAuthorization::prepare(&grant).unwrap();
        let address = session.listener.local_addr().unwrap();
        assert!(session.url().starts_with(AUTHORIZE_URL));
        assert!(std::net::TcpListener::bind(address).is_err());
    }

    fn callback(target: &str) -> Vec<u8> {
        format!(
            "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{WEB_REDIRECT_PORT}\r\nConnection: close\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn callback_accepts_only_the_exact_get_and_loopback_host() {
        let request = callback("/login?code=abc%2Dd&state=s1");
        let parsed = parse_callback_request(&request, "s1", WEB_REDIRECT_PORT).unwrap();
        let ProviderCallback::Accepted(code) = parsed else {
            panic!("a valid callback must accept its code");
        };
        assert_eq!(code, "abc-d");

        for invalid in [
            "POST /login?code=abc&state=s1 HTTP/1.1\r\nHost: 127.0.0.1:8989\r\n\r\n",
            "GET /login?code=abc&state=s1 HTTP/1.0\r\nHost: 127.0.0.1:8989\r\n\r\n",
            "GET http://127.0.0.1:8989/login?code=abc&state=s1 HTTP/1.1\r\nHost: 127.0.0.1:8989\r\n\r\n",
            "GET /favicon.ico?code=abc&state=s1 HTTP/1.1\r\nHost: 127.0.0.1:8989\r\n\r\n",
            "GET /login?code=abc&state=s1 HTTP/1.1\r\nHost: localhost:8989\r\n\r\n",
        ] {
            assert!(parse_callback_request(invalid.as_bytes(), "s1", WEB_REDIRECT_PORT).is_err());
        }
    }

    #[test]
    fn callback_validates_state_before_terminal_provider_results() {
        let wrong_state = callback("/login?error=%3Cscript%3E&state=wrong");
        let error = parse_callback_request(&wrong_state, "s1", WEB_REDIRECT_PORT)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "callback state mismatch");

        let denied = callback("/login?error=%3Cscript%3E&state=s1");
        assert!(matches!(
            parse_callback_request(&denied, "s1", WEB_REDIRECT_PORT),
            Ok(ProviderCallback::Denied)
        ));
        assert!(!failure_page().contains("script"));
        assert!(!failure_page().contains("wrong"));
    }

    #[test]
    fn authenticated_results_are_terminal_even_if_the_browser_response_fails() {
        let accepted = parse_callback_request(
            &callback("/login?code=accepted&state=s1"),
            "s1",
            WEB_REDIRECT_PORT,
        );
        match respond_and_classify(accepted, |status, _| {
            assert_eq!(status, "200 OK");
            Err(anyhow!("simulated browser disconnect"))
        }) {
            CallbackDisposition::Finish(result) => assert_eq!(result.unwrap(), "accepted"),
            CallbackDisposition::Retry(_) => panic!("an accepted code must be terminal"),
        }

        let denied = parse_callback_request(
            &callback("/login?error=access_denied&state=s1"),
            "s1",
            WEB_REDIRECT_PORT,
        );
        match respond_and_classify(denied, |status, _| {
            assert_eq!(status, "400 Bad Request");
            Err(anyhow!("simulated browser disconnect"))
        }) {
            CallbackDisposition::Finish(result) => {
                assert_eq!(
                    result.unwrap_err().to_string(),
                    "Spotify declined the sign-in"
                )
            }
            CallbackDisposition::Retry(_) => panic!("an authenticated denial must be terminal"),
        }
    }

    #[test]
    fn callback_rejects_duplicates_bodies_and_oversize_inputs() {
        for target in [
            "/login?code=a&code=b&state=s1",
            "/login?code=a&state=s1&state=s1",
            "/login?code=a&state=s1&error=denied&error=again",
            "/login?code=%GG&state=s1",
        ] {
            assert!(parse_callback_request(&callback(target), "s1", WEB_REDIRECT_PORT).is_err());
        }

        let length = b"GET /login?code=a&state=s1 HTTP/1.1\r\nHost: 127.0.0.1:8989\r\nContent-Length: 1\r\n\r\n";
        let chunked = b"GET /login?code=a&state=s1 HTTP/1.1\r\nHost: 127.0.0.1:8989\r\nTransfer-Encoding: chunked\r\n\r\n";
        let trailing = b"GET /login?code=a&state=s1 HTTP/1.1\r\nHost: 127.0.0.1:8989\r\n\r\nx";
        assert!(parse_callback_request(length, "s1", WEB_REDIRECT_PORT).is_err());
        assert!(parse_callback_request(chunked, "s1", WEB_REDIRECT_PORT).is_err());
        assert!(parse_callback_request(trailing, "s1", WEB_REDIRECT_PORT).is_err());

        let oversized = format!(
            "GET /login?code={}&state=s1 HTTP/1.1\r\nHost: 127.0.0.1:8989\r\n\r\n",
            "a".repeat(MAX_REQUEST_LINE_BYTES)
        );
        assert!(parse_callback_request(oversized.as_bytes(), "s1", WEB_REDIRECT_PORT).is_err());
    }

    #[test]
    fn callback_pages_have_non_reflecting_browser_defences() {
        let response = callback_response("400 Bad Request", failure_page());
        for header in [
            "Content-Security-Policy: default-src 'none'",
            "X-Content-Type-Options: nosniff",
            "Referrer-Policy: no-referrer",
            "Cache-Control: no-store, max-age=0",
            "Pragma: no-cache",
            "Cross-Origin-Opener-Policy: same-origin",
        ] {
            assert!(response.contains(header));
        }
        assert!(!response.contains("<script"));
    }

    #[test]
    fn bounded_response_assembly_rejects_chunked_oversize() {
        let mut output = Vec::new();
        append_bounded(&mut output, b"1234", 8).unwrap();
        append_bounded(&mut output, b"5678", 8).unwrap();
        assert!(append_bounded(&mut output, b"9", 8).is_err());
        assert_eq!(output.len(), 8);
    }

    fn token_response() -> TokenResponse {
        TokenResponse {
            access_token: "access".into(),
            token_type: "Bearer".into(),
            refresh_token: Some("refresh".into()),
            expires_in: 3600,
            scope: Some("user-read-private".into()),
        }
    }

    #[test]
    fn token_response_is_strictly_bounded_and_typed() {
        assert!(token_response().validate().is_ok());
        let mut invalid = token_response();
        invalid.access_token = "has whitespace".into();
        assert!(invalid.validate().is_err());
        let mut invalid = token_response();
        invalid.token_type = "MAC".into();
        assert!(invalid.validate().is_err());
        let mut invalid = token_response();
        invalid.expires_in = 0;
        assert!(invalid.validate().is_err());
        let mut invalid = token_response();
        invalid.refresh_token = Some("r".repeat(MAX_REFRESH_TOKEN_BYTES + 1));
        assert!(invalid.validate().is_err());
        let extended = serde_json::from_str::<TokenResponse>(
            r#"{"access_token":"a","token_type":"Bearer","expires_in":3600,"provider_metadata":1}"#,
        )
        .unwrap();
        assert!(extended.validate().is_ok());
    }

    #[test]
    fn stored_token_round_trips_and_tracks_expiry() {
        let mut response = token_response();
        response.scope = Some("x".into());
        let token = StoredToken::from_response("id", response, None).unwrap();
        assert!(!token.needs_refresh());
        assert!(token.has_scopes(&["x"]));
        assert!(!token.has_scopes(&["x", "y"]));
        let expired = StoredToken {
            expires_at: now_secs() + 10,
            ..token.clone()
        };
        assert!(expired.needs_refresh());
    }
}
