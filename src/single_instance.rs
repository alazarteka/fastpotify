//! One running instance at a time, and its remote-control channel.
//!
//! Two copies of Fastpotify fight over things a user notices: two Spotify
//! Connect devices with the same name, two MPRIS players for the media keys
//! to disagree about, two tray icons. So a second launch does not start a
//! second app; it asks the one already running to show itself and exits.
//!
//! Detection is a D-Bus well-known name, requested without queuing. The bus
//! grants it to exactly one process and releases it the moment that process
//! ends, crash included, so there is no stale lock file to clean up and no
//! race between two launches at once. Surfacing the running instance is the
//! MPRIS `Raise` method it already implements, which is also what a desktop's
//! own "jump to the running player" gesture calls.
//!
//! This uses zbus's blocking API deliberately. Both of zbus's executors are
//! compiled in here (the tray brings async-io, MPRIS brings tokio), so an
//! async connection awaited from an arbitrary runtime is not guaranteed to be
//! driven. The blocking API owns that problem, and a check that runs once
//! before the window exists has no reason to be asynchronous anyway.
//!
//! macOS and Windows have no session bus, so there the same two jobs are done
//! by a listening socket bound to loopback: binding is exclusive, so whoever
//! binds is the running instance, and a later launch connects to say "show
//! yourself" before exiting. It is bound to 127.0.0.1 so no firewall has an
//! opinion about it, it speaks only to itself, and the operating system
//! releases the port when the process ends.
//!
//! On those platforms the socket doubles as the remote-control channel:
//! `fastpotify next` (or a Raycast script running it) connects, sends one
//! authenticated request line, and reads one reply line. A random token in
//! the owner's private state directory prevents other local accounts from
//! reading snapshots or issuing commands. Playback verbs are acknowledged
//! with `fastpotify:ok` and land in the same action queue the
//! tray and the media keys feed; `nowplaying` and `devices` are answered from
//! snapshots the app keeps fresh, so the listener thread never touches app
//! state. Free-text arguments are bounded and validated before they enter
//! that queue. During startup, bind and authenticated activation are retried
//! together for a short handoff window. A port owner that remains
//! unauthenticated, or a token/listener setup failure, blocks a second full
//! application instead of falling back to an uncontrolled instance.
//! Linux needs none of this: MPRIS already gives `playerctl` the same verbs,
//! so the D-Bus name stays a pure instance guard there.

/// The name held for the lifetime of the running instance.
#[cfg(target_os = "linux")]
const INSTANCE_NAME: &str = "rocks.fastpotify.Instance";

/// The MPRIS player to ask when another instance already holds the name.
#[cfg(target_os = "linux")]
const MPRIS_NAME: &str = "org.mpris.MediaPlayer2.fastpotify";

pub enum Outcome {
    /// This process is the only instance. Hold the guard until it exits.
    Only(Guard),
    /// Another instance is running and has been asked to show its window.
    Surfaced,
    /// Exclusivity or authenticated control could not be established, so a
    /// competing full application must not start.
    Blocked(std::io::Error),
}

/// What a control client asked the running instance to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlCommand {
    /// Bring the window forward, creating it if the app lives in the tray.
    Show,
    PlayPause,
    Play,
    Pause,
    Next,
    Previous,
    /// Milliseconds; negative seeks backwards.
    SeekBy(i64),
    /// Percentage points; negative lowers the volume.
    VolumeBy(i8),
    /// Absolute percentage.
    SetVolume(u8),
    ToggleMute,
    ToggleShuffle,
    CycleRepeat,
    SetShuffle(bool),
    SetRepeat(crate::player::RepeatMode),
    /// Absolute position, in milliseconds.
    SeekTo(u32),
    /// Save the playing music track to the library, or take it back out.
    ToggleSaved,
    /// Play a validated music `spotify:` URI.
    PlayUri(String),
    /// Move playback to a Spotify Connect device, by id.
    Transfer(String),
    /// Refresh the device list after a control client reads its snapshot.
    RefreshDevices,
}

/// Holds whatever marks this process as the running instance. Dropping it
/// gives that up.
pub struct Guard {
    #[cfg(target_os = "linux")]
    _connection: Option<mpris_server::zbus::blocking::Connection>,
    #[cfg(any(test, not(target_os = "linux")))]
    _control_token: Option<ControlTokenLease>,
    /// Filled by control clients, drained by the app every frame. On Linux
    /// the same requests arrive through MPRIS instead and this stays empty.
    commands: std::sync::Arc<std::sync::Mutex<Vec<ControlCommand>>>,
    /// One line about the current track, kept fresh by the app so the
    /// listener can answer `nowplaying` without touching app state.
    now_playing: std::sync::Arc<std::sync::Mutex<String>>,
    /// The last Spotify Connect device response, encoded as JSON.
    devices: std::sync::Arc<std::sync::Mutex<String>>,
}

impl Guard {
    /// The queue a control client's commands land in. The app drains it.
    pub fn commands(&self) -> std::sync::Arc<std::sync::Mutex<Vec<ControlCommand>>> {
        std::sync::Arc::clone(&self.commands)
    }

    /// The slot the app writes the now-playing snapshot into.
    pub fn now_playing_slot(&self) -> std::sync::Arc<std::sync::Mutex<String>> {
        std::sync::Arc::clone(&self.now_playing)
    }

    pub fn devices_slot(&self) -> std::sync::Arc<std::sync::Mutex<String>> {
        std::sync::Arc::clone(&self.devices)
    }
}

/// What the app writes into the snapshot slot before anything plays, and
/// what `nowplaying` reports when nothing does.
pub const NOTHING_PLAYING: &str = "stopped";

/// A stable JSON shape before Spotify has returned any devices.
pub const NO_DEVICES: &str = "[]";

/// Replaces terminal and wire control characters in one externally sourced
/// field without changing its printable Unicode text.
pub fn sanitize_control_field(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// Sanitizes a complete now-playing wire line while retaining tabs as its
/// field separators. Snapshot production sanitizes fields before joining;
/// this second boundary also protects raw CLI output from a malformed peer.
pub fn sanitize_snapshot_line(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character != '\t' && character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// Loopback port that marks a running instance on platforms without a bus.
/// Registered to nothing; chosen high and out of the ephemeral range.
#[cfg(not(target_os = "linux"))]
const INSTANCE_PORT: u16 = 47_113;

/// Frames replies and separates the request token from its verb.
#[cfg(any(test, not(target_os = "linux")))]
const PREFIX: &str = "fastpotify:";
#[cfg(any(test, not(target_os = "linux")))]
const OK_REPLY: &str = "fastpotify:ok";
#[cfg(any(test, not(target_os = "linux")))]
const NOW_REPLY: &str = "fastpotify:now ";
#[cfg(any(test, not(target_os = "linux")))]
const DEVICES_REPLY: &str = "fastpotify:devices ";
#[cfg(any(test, not(target_os = "linux")))]
const MAX_CONTROL_LINE_BYTES: usize = 256;
#[cfg(any(test, not(target_os = "linux")))]
const MAX_CONTROL_REPLY_BYTES: u64 = 64 * 1024;
#[cfg(any(test, not(target_os = "linux")))]
const CONTROL_TOKEN_BYTES: usize = 32;
#[cfg(any(test, not(target_os = "linux")))]
const CONTROL_TOKEN_HEX_BYTES: usize = CONTROL_TOKEN_BYTES * 2;
#[cfg(not(target_os = "linux"))]
const ACQUIRE_ATTEMPTS: usize = 10;
#[cfg(any(test, not(target_os = "linux")))]
const ACQUIRE_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

/// Removes only the token this primary instance wrote. The Spotify sign-out
/// path intentionally does not own this lease.
#[cfg(any(test, not(target_os = "linux")))]
struct ControlTokenLease {
    path: std::path::PathBuf,
    token: String,
}

#[cfg(any(test, not(target_os = "linux")))]
impl ControlTokenLease {
    fn issue(dirs: &crate::paths::AppDirs) -> std::io::Result<Self> {
        use rand::RngCore as _;
        use std::fmt::Write as _;

        let mut random = [0u8; CONTROL_TOKEN_BYTES];
        rand::rng().fill_bytes(&mut random);
        let mut token = String::with_capacity(CONTROL_TOKEN_HEX_BYTES);
        for byte in random {
            write!(&mut token, "{byte:02x}").expect("writing to a String cannot fail");
        }
        let path = dirs.control_token_file();
        crate::secrets::write_private_atomic(&path, token.as_bytes())
            .map_err(std::io::Error::other)?;
        Ok(Self { path, token })
    }
}

#[cfg(any(test, not(target_os = "linux")))]
impl Drop for ControlTokenLease {
    fn drop(&mut self) {
        if let Err(error) = crate::secrets::delete_private_if_matches(
            &self.path,
            self.token.as_bytes(),
            CONTROL_TOKEN_HEX_BYTES,
        ) {
            log::warn!("cannot clear the control socket token: {error}");
        }
    }
}

#[cfg(any(test, not(target_os = "linux")))]
fn load_control_token(dirs: &crate::paths::AppDirs) -> std::io::Result<String> {
    let Some(bytes) =
        crate::secrets::read_private_bounded(&dirs.control_token_file(), CONTROL_TOKEN_HEX_BYTES)
            .map_err(std::io::Error::other)?
    else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "the running Fastpotify control token is missing",
        ));
    };
    if bytes.len() != CONTROL_TOKEN_HEX_BYTES
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the Fastpotify control token is invalid",
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the Fastpotify control token is invalid",
        )
    })
}

/// What the running instance said back.
#[cfg(any(test, not(target_os = "linux")))]
pub enum Reply {
    /// The command was accepted.
    Ok,
    /// The `nowplaying` snapshot: [`NOTHING_PLAYING`], or tab-separated
    /// `state, title, artists, album, position_ms, duration_ms, volume,
    /// shuffle, repeat, art_url, saved, device`.
    NowPlaying(String),
    /// A JSON array of device objects.
    Devices(String),
}

/// Sends one verb to the running instance and reads its reply.
#[cfg(not(target_os = "linux"))]
pub fn send(dirs: &crate::paths::AppDirs, verb: &str) -> std::io::Result<Reply> {
    let token = load_control_token(dirs)?;
    send_to(INSTANCE_PORT, &token, verb)
}

#[cfg(any(test, not(target_os = "linux")))]
fn send_to(port: u16, token: &str, verb: &str) -> std::io::Result<Reply> {
    send_to_with_timeout(port, token, verb, std::time::Duration::from_secs(2))
}

#[cfg(any(test, not(target_os = "linux")))]
fn send_to_with_timeout(
    port: u16,
    token: &str,
    verb: &str,
    timeout: std::time::Duration,
) -> std::io::Result<Reply> {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpStream};

    if PREFIX.len() + token.len() + 1 + verb.len() + 1 > MAX_CONTROL_LINE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the Fastpotify control request is too long",
        ));
    }
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.write_all(format!("{PREFIX}{token}:{verb}\n").as_bytes())?;
    // The listener writes one line and closes, so read to end and keep the
    // line. An instance predating the authenticated protocol ignores this
    // frame without replying; the read times out and surfaces as an error.
    let mut reply = String::new();
    stream
        .take(MAX_CONTROL_REPLY_BYTES + 1)
        .read_to_string(&mut reply)?;
    if reply.len() as u64 > MAX_CONTROL_REPLY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the Fastpotify control reply is too long",
        ));
    }
    let line = reply.lines().next().unwrap_or("");
    if line == OK_REPLY {
        Ok(Reply::Ok)
    } else if let Some(snapshot) = line.strip_prefix(NOW_REPLY) {
        Ok(Reply::NowPlaying(sanitize_snapshot_line(snapshot)))
    } else if let Some(snapshot) = line.strip_prefix(DEVICES_REPLY) {
        Ok(Reply::Devices(snapshot.to_owned()))
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the port is held by something other than Fastpotify",
        ))
    }
}

#[cfg(any(test, not(target_os = "linux")))]
enum PortClaim {
    Primary(std::net::TcpListener),
    Surfaced,
}

/// Recheck both ownership and authenticated activation during the bounded
/// handoff window. A missing or stale token may be ordinary publication or
/// shutdown timing, but an owner that remains unauthenticated is never a
/// reason to start a competing application.
#[cfg(any(test, not(target_os = "linux")))]
fn claim_port_with(
    dirs: &crate::paths::AppDirs,
    port: u16,
    attempts: usize,
    mut wait: impl FnMut(),
) -> std::io::Result<PortClaim> {
    use std::net::{Ipv4Addr, TcpListener};

    let mut activation_error = None;
    for attempt in 0..attempts.max(1) {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            Ok(listener) => return Ok(PortClaim::Primary(listener)),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("cannot claim the Fastpotify control port {port}: {error}"),
                ));
            }
        }

        let activated = load_control_token(dirs)
            .and_then(|token| send_to_with_timeout(port, &token, "show", ACQUIRE_REPLY_TIMEOUT));
        match activated {
            Ok(Reply::Ok) => return Ok(PortClaim::Surfaced),
            Ok(Reply::NowPlaying(_) | Reply::Devices(_)) => {
                activation_error = Some("the owner returned the wrong control reply".to_owned());
            }
            Err(error) => activation_error = Some(error.to_string()),
        }
        if attempt + 1 < attempts.max(1) {
            wait();
        }
    }

    let detail = activation_error.unwrap_or_else(|| "the owner did not answer".to_owned());
    Err(std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        format!(
            "Fastpotify control port {port} remains occupied, but its owner could not be authenticated ({detail}); refusing to start a second instance"
        ),
    ))
}

#[cfg(any(test, not(target_os = "linux")))]
fn start_primary_with<F>(
    dirs: &crate::paths::AppDirs,
    waker: &crate::backend::Waker,
    listener: std::net::TcpListener,
    spawn: F,
) -> std::io::Result<Guard>
where
    F: FnOnce(
        std::net::TcpListener,
        String,
        std::sync::Arc<std::sync::Mutex<Vec<ControlCommand>>>,
        std::sync::Arc<std::sync::Mutex<String>>,
        std::sync::Arc<std::sync::Mutex<String>>,
        crate::backend::Waker,
    ) -> std::io::Result<()>,
{
    use std::sync::{Arc, Mutex};

    // The token is durable before the listener can answer. Any setup error
    // drops both the bound listener and this matching-token lease.
    let lease = ControlTokenLease::issue(dirs)?;
    let token = lease.token.clone();
    let guard = Guard {
        #[cfg(target_os = "linux")]
        _connection: None,
        _control_token: Some(lease),
        commands: Default::default(),
        now_playing: Arc::new(Mutex::new(NOTHING_PLAYING.to_owned())),
        devices: Arc::new(Mutex::new(NO_DEVICES.to_owned())),
    };
    let commands = Arc::clone(&guard.commands);
    let now_playing = Arc::clone(&guard.now_playing);
    let devices = Arc::clone(&guard.devices);
    spawn(
        listener,
        token,
        commands,
        now_playing,
        devices,
        waker.clone(),
    )?;
    Ok(guard)
}

#[cfg(any(test, not(target_os = "linux")))]
fn acquire_with<F>(
    dirs: &crate::paths::AppDirs,
    waker: &crate::backend::Waker,
    port: u16,
    attempts: usize,
    wait: impl FnMut(),
    spawn: F,
) -> Outcome
where
    F: FnOnce(
        std::net::TcpListener,
        String,
        std::sync::Arc<std::sync::Mutex<Vec<ControlCommand>>>,
        std::sync::Arc<std::sync::Mutex<String>>,
        std::sync::Arc<std::sync::Mutex<String>>,
        crate::backend::Waker,
    ) -> std::io::Result<()>,
{
    match claim_port_with(dirs, port, attempts, wait) {
        Ok(PortClaim::Surfaced) => Outcome::Surfaced,
        Ok(PortClaim::Primary(listener)) => {
            match start_primary_with(dirs, waker, listener, spawn) {
                Ok(guard) => Outcome::Only(guard),
                Err(error) => Outcome::Blocked(std::io::Error::new(
                    error.kind(),
                    format!("cannot establish authenticated Fastpotify control: {error}"),
                )),
            }
        }
        Err(error) => Outcome::Blocked(error),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn acquire(dirs: &crate::paths::AppDirs, waker: &crate::backend::Waker) -> Outcome {
    acquire_with(
        dirs,
        waker,
        INSTANCE_PORT,
        ACQUIRE_ATTEMPTS,
        || std::thread::sleep(std::time::Duration::from_millis(50)),
        |listener, token, commands, now_playing, devices, waker| {
            std::thread::Builder::new()
                .name("fastpotify-instance".to_owned())
                .spawn(move || serve(listener, &token, &commands, &now_playing, &devices, &waker))
                .map(|_| ())
        },
    )
}

/// Answers control clients until the listener closes. One request line and
/// one reply line per connection.
#[cfg(any(test, not(target_os = "linux")))]
fn serve(
    listener: std::net::TcpListener,
    token: &str,
    commands: &std::sync::Mutex<Vec<ControlCommand>>,
    now_playing: &std::sync::Mutex<String>,
    devices: &std::sync::Mutex<String>,
    waker: &crate::backend::Waker,
) {
    use std::io::Write;
    use std::time::Duration;

    let queue = |command| {
        commands
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(command);
        waker.wake();
    };

    for mut stream in listener.incoming().flatten() {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let Some(line) = read_line(&mut stream) else {
            continue;
        };
        let Some(verb) = authenticate(&line, token) else {
            continue;
        };
        match parse(verb) {
            Some(Request::Command(command)) => {
                let _ = stream.write_all(format!("{OK_REPLY}\n").as_bytes());
                queue(command);
            }
            Some(Request::NowPlaying) => {
                let snapshot = now_playing
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                let _ = stream.write_all(format!("{NOW_REPLY}{snapshot}\n").as_bytes());
            }
            Some(Request::Devices) => {
                let snapshot = devices
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .clone();
                let _ = stream.write_all(format!("{DEVICES_REPLY}{snapshot}\n").as_bytes());
                queue(ControlCommand::RefreshDevices);
            }
            // Not our client; say nothing and hang up.
            None => {}
        }
    }
}

/// A parsed request line: a command for the app, or a read the listener
/// answers itself.
#[cfg(any(test, not(target_os = "linux")))]
enum Request {
    Command(ControlCommand),
    NowPlaying,
    Devices,
}

#[cfg(any(test, not(target_os = "linux")))]
fn parse(line: &str) -> Option<Request> {
    let verb = line.trim_end_matches('\r');
    let (verb, argument) = match verb.split_once(' ') {
        Some((verb, argument)) => (verb, Some(argument.trim())),
        None => (verb, None),
    };
    let command = match (verb, argument) {
        ("show", None) => ControlCommand::Show,
        ("playpause", None) => ControlCommand::PlayPause,
        ("play", None) => ControlCommand::Play,
        ("pause", None) => ControlCommand::Pause,
        ("next", None) => ControlCommand::Next,
        ("previous", None) => ControlCommand::Previous,
        ("seek-by", Some(ms)) => ControlCommand::SeekBy(ms.parse().ok()?),
        ("seek-to", Some(ms)) => ControlCommand::SeekTo(ms.parse().ok()?),
        ("volume-by", Some(delta)) => ControlCommand::VolumeBy(delta.parse().ok()?),
        ("volume-set", Some(volume)) => ControlCommand::SetVolume(volume.parse().ok()?),
        ("mute", None) => ControlCommand::ToggleMute,
        ("shuffle", None) => ControlCommand::ToggleShuffle,
        ("shuffle-set", Some("on")) => ControlCommand::SetShuffle(true),
        ("shuffle-set", Some("off")) => ControlCommand::SetShuffle(false),
        ("repeat", None) => ControlCommand::CycleRepeat,
        ("repeat-set", Some("off")) => ControlCommand::SetRepeat(crate::player::RepeatMode::Off),
        ("repeat-set", Some("context")) => {
            ControlCommand::SetRepeat(crate::player::RepeatMode::Context)
        }
        ("repeat-set", Some("track")) => {
            ControlCommand::SetRepeat(crate::player::RepeatMode::Track)
        }
        ("save-toggle", None) => ControlCommand::ToggleSaved,
        ("play-uri", Some(uri)) => ControlCommand::PlayUri(spotify_music_uri(uri)?),
        ("transfer", Some(id)) => ControlCommand::Transfer(device_id(id)?),
        ("nowplaying", None) => return Some(Request::NowPlaying),
        ("devices", None) => return Some(Request::Devices),
        _ => return None,
    };
    Some(Request::Command(command))
}

/// Authenticate the bounded frame before the verb is parsed or dispatched.
#[cfg(any(test, not(target_os = "linux")))]
fn authenticate<'a>(line: &'a str, expected: &str) -> Option<&'a str> {
    let frame = line.strip_prefix(PREFIX)?;
    let (provided, verb) = frame.split_once(':')?;
    let same = provided.len() == expected.len()
        && provided
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0;
    same.then_some(verb)
}

/// Only music contexts cross the local control socket; podcast and audiobook
/// playback are not part of this protocol.
#[cfg(any(test, not(target_os = "linux")))]
fn spotify_music_uri(text: &str) -> Option<String> {
    let mut parts = text.split(':');
    let shaped = matches!(parts.next(), Some("spotify"))
        && matches!(
            parts.next(),
            Some("track" | "album" | "playlist" | "artist")
        );
    let id = parts.next()?;
    let valid_id = !id.is_empty()
        && parts.next().is_none()
        && text.len() <= 128
        && id.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '%' | '+')
        });
    (shaped && valid_id).then(|| text.to_owned())
}

/// Spotify Connect device ids are opaque, bounded tokens supplied by the API.
#[cfg(any(test, not(target_os = "linux")))]
fn device_id(text: &str) -> Option<String> {
    let shaped = !text.is_empty()
        && text.len() <= 64
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    shaped.then(|| text.to_owned())
}

/// Reads up to the first newline. A line too long to be one of ours, or any
/// read error, disqualifies the client.
#[cfg(any(test, not(target_os = "linux")))]
fn read_line(stream: &mut std::net::TcpStream) -> Option<String> {
    use std::io::Read;
    let mut buffer = [0u8; MAX_CONTROL_LINE_BYTES];
    let mut filled = 0;
    loop {
        if filled == buffer.len() {
            return None;
        }
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => {
                filled += read;
                if buffer[..filled].contains(&b'\n') {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    let line = buffer[..filled].split(|&byte| byte == b'\n').next()?;
    String::from_utf8(line.to_vec()).ok()
}

#[cfg(target_os = "linux")]
pub fn acquire(_dirs: &crate::paths::AppDirs, _waker: &crate::backend::Waker) -> Outcome {
    use mpris_server::zbus::blocking::Connection;
    use mpris_server::zbus::fdo::{RequestNameFlags, RequestNameReply};

    let guard = |connection: Option<Connection>| Guard {
        _connection: connection,
        #[cfg(test)]
        _control_token: None,
        commands: Default::default(),
        now_playing: std::sync::Arc::new(std::sync::Mutex::new(NOTHING_PLAYING.to_owned())),
        devices: std::sync::Arc::new(std::sync::Mutex::new(NO_DEVICES.to_owned())),
    };

    let connection = match Connection::session() {
        Ok(connection) => connection,
        Err(error) => {
            // No session bus at all: nothing to coordinate through, so run.
            log::debug!("no session bus, running unguarded: {error}");
            return Outcome::Only(guard(None));
        }
    };

    // Holding the name is how this process says it is the one running.
    // zbus reports a name another peer already owns as `NameTaken` rather
    // than as a reply, so that error is the ordinary second-launch path.
    match connection.request_name_with_flags(INSTANCE_NAME, RequestNameFlags::DoNotQueue.into()) {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
            Outcome::Only(guard(Some(connection)))
        }
        Ok(_) | Err(mpris_server::zbus::Error::NameTaken) => {
            if !raise_running_instance(&connection) {
                log::warn!(
                    "Fastpotify is already running but did not answer; not starting a second copy"
                );
            }
            Outcome::Surfaced
        }
        Err(error) => {
            log::warn!("cannot check for a running instance, starting anyway: {error}");
            Outcome::Only(guard(None))
        }
    }
}

/// Asks the running instance to show its window, retrying briefly because it
/// may still be registering MPRIS when this launch arrives.
#[cfg(target_os = "linux")]
fn raise_running_instance(connection: &mpris_server::zbus::blocking::Connection) -> bool {
    for attempt in 0..10 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        let raised = connection.call_method(
            Some(MPRIS_NAME),
            "/org/mpris/MediaPlayer2",
            Some("org.mpris.MediaPlayer2"),
            "Raise",
            &(),
        );
        if raised.is_ok() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::RepeatMode;

    fn command(verb: &str) -> Option<ControlCommand> {
        match parse(verb) {
            Some(Request::Command(command)) => Some(command),
            _ => None,
        }
    }

    fn test_dirs(label: &str) -> (crate::paths::AppDirs, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "fastpotify-control-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let dirs = crate::paths::AppDirs {
            config: root.join("config"),
            state: root.join("state"),
            cache: root.join("cache"),
        };
        dirs.ensure().expect("private application directories");
        (dirs, root)
    }

    #[test]
    fn parses_every_control_verb() {
        // #given / #when / #then
        assert_eq!(command("show\r"), Some(ControlCommand::Show));
        assert_eq!(command("playpause"), Some(ControlCommand::PlayPause));
        assert_eq!(command("play"), Some(ControlCommand::Play));
        assert_eq!(command("pause"), Some(ControlCommand::Pause));
        assert_eq!(command("next"), Some(ControlCommand::Next));
        assert_eq!(command("previous"), Some(ControlCommand::Previous));
        assert_eq!(
            command("seek-by -10000"),
            Some(ControlCommand::SeekBy(-10_000))
        );
        assert_eq!(command("volume-by +5"), Some(ControlCommand::VolumeBy(5)));
        assert_eq!(
            command("volume-set 40"),
            Some(ControlCommand::SetVolume(40))
        );
        assert_eq!(command("mute"), Some(ControlCommand::ToggleMute));
        assert_eq!(command("shuffle"), Some(ControlCommand::ToggleShuffle));
        assert_eq!(command("repeat"), Some(ControlCommand::CycleRepeat));
        assert_eq!(
            command("seek-to 90000"),
            Some(ControlCommand::SeekTo(90_000))
        );
        assert_eq!(
            command("shuffle-set on"),
            Some(ControlCommand::SetShuffle(true))
        );
        assert_eq!(
            command("shuffle-set off"),
            Some(ControlCommand::SetShuffle(false))
        );
        assert_eq!(
            command("repeat-set track"),
            Some(ControlCommand::SetRepeat(RepeatMode::Track))
        );
        assert_eq!(
            command("repeat-set context"),
            Some(ControlCommand::SetRepeat(RepeatMode::Context))
        );
        assert_eq!(
            command("repeat-set off"),
            Some(ControlCommand::SetRepeat(RepeatMode::Off))
        );
        assert_eq!(command("save-toggle"), Some(ControlCommand::ToggleSaved));
        assert_eq!(
            command("play-uri spotify:playlist:37i9dQZF1DXcBWIGoYBM5M"),
            Some(ControlCommand::PlayUri(
                "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M".to_owned()
            ))
        );
        assert_eq!(
            command("transfer a1b2c3d4e5"),
            Some(ControlCommand::Transfer("a1b2c3d4e5".to_owned()))
        );
        assert!(matches!(parse("nowplaying"), Some(Request::NowPlaying)));
        assert!(matches!(parse("devices"), Some(Request::Devices)));
    }

    #[test]
    fn rejects_invalid_control_verbs() {
        assert!(parse("GET / HTTP/1.1").is_none());
        assert!(parse("frobnicate").is_none());
        assert!(parse("seek-by soon").is_none());
        assert!(parse("volume-set 999").is_none());
        assert!(parse("next please").is_none());
        assert!(parse("shuffle-set maybe").is_none());
        assert!(parse("repeat-set all").is_none());
        assert!(parse("play-uri https://open.spotify.com/track/x").is_none());
        assert!(parse("play-uri spotify:show:x").is_none());
        assert!(parse("play-uri spotify:episode:x").is_none());
        assert!(parse("play-uri spotify:audiobook:x").is_none());
        assert!(parse("play-uri spotify:collection:tracks").is_none());
        assert!(parse("play-uri spotify:track:x:extra").is_none());
        assert!(parse("transfer ../speaker").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn a_control_client_bounds_requests_and_replies() {
        use std::io::Write as _;
        use std::net::{Ipv4Addr, TcpListener};

        let token = "a".repeat(CONTROL_TOKEN_HEX_BYTES);
        let request_error = send_to(0, &token, &"x".repeat(256))
            .err()
            .expect("an oversized request is rejected before connecting");
        assert_eq!(request_error.kind(), std::io::ErrorKind::InvalidInput);

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback port");
        let port = listener.local_addr().expect("a bound address").port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one client");
            let _ = stream.write_all(
                format!(
                    "{NOW_REPLY}{}\n",
                    "x".repeat(MAX_CONTROL_REPLY_BYTES as usize)
                )
                .as_bytes(),
            );
        });

        let reply_error = send_to(port, &token, "nowplaying")
            .err()
            .expect("an oversized reply is rejected");
        assert_eq!(reply_error.kind(), std::io::ErrorKind::InvalidData);
        server.join().expect("server exits");
    }

    #[test]
    fn raw_snapshot_reply_sanitizes_terminal_control_characters() {
        use std::io::Write as _;
        use std::net::{Ipv4Addr, TcpListener};

        let token = "a".repeat(CONTROL_TOKEN_HEX_BYTES);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback port");
        let port = listener.local_addr().expect("a bound address").port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one client");
            read_line(&mut stream).expect("control request");
            stream
                .write_all(format!("{NOW_REPLY}playing\tGo\u{1b}[31m\u{7}\tBand\n").as_bytes())
                .expect("snapshot reply");
        });

        let Reply::NowPlaying(snapshot) = send_to(port, &token, "nowplaying").expect("reply")
        else {
            panic!("unexpected reply type");
        };
        assert_eq!(snapshot, "playing\tGo [31m \tBand");
        assert!(
            snapshot
                .chars()
                .all(|character| character == '\t' || !character.is_control())
        );
        server.join().expect("server exits");
    }

    /// The whole channel over a real socket: commands reach the app queue and
    /// read verbs return the snapshots the app published.
    #[test]
    fn a_client_reaches_the_command_queue_and_the_snapshot() {
        use std::io::{Read as _, Write as _};
        use std::net::{Ipv4Addr, TcpListener};
        use std::sync::{Arc, Mutex};

        // #given
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback port");
        let port = listener.local_addr().expect("a bound address").port();
        let commands: Arc<Mutex<Vec<ControlCommand>>> = Default::default();
        let now_playing = Arc::new(Mutex::new("playing\tGo\tThe Band".to_owned()));
        let devices = Arc::new(Mutex::new(r#"[{"id":"speaker"}]"#.to_owned()));
        let token = "a".repeat(CONTROL_TOKEN_HEX_BYTES);
        let served = {
            let commands = Arc::clone(&commands);
            let now_playing = Arc::clone(&now_playing);
            let devices = Arc::clone(&devices);
            let waker = crate::backend::Waker::default();
            let token = token.clone();
            std::thread::spawn(move || {
                serve(listener, &token, &commands, &now_playing, &devices, &waker)
            })
        };

        let raw = |line: &str| {
            let mut stream = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .expect("connect to test server");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                .expect("set timeout");
            stream.write_all(line.as_bytes()).expect("write request");
            stream
                .shutdown(std::net::Shutdown::Write)
                .expect("finish request");
            let mut reply = String::new();
            stream.read_to_string(&mut reply).expect("read reply");
            reply
        };

        // #when
        let missing = raw("fastpotify:next\n");
        let wrong = raw(&format!(
            "fastpotify:{}:next\n",
            "b".repeat(CONTROL_TOKEN_HEX_BYTES)
        ));
        assert!(commands.lock().expect("the queue").is_empty());
        let accepted = send_to(port, &token, "next").expect("a reply");
        let volume = send_to(port, &token, "volume-by -5").expect("a reply");
        let snapshot = send_to(port, &token, "nowplaying").expect("a reply");
        let device_snapshot = send_to(port, &token, "devices").expect("a reply");
        let refused = send_to(port, &token, "frobnicate");

        // #then
        assert!(missing.is_empty());
        assert!(wrong.is_empty());
        assert!(matches!(accepted, Reply::Ok));
        assert!(matches!(volume, Reply::Ok));
        match snapshot {
            Reply::NowPlaying(line) => assert_eq!(line, "playing\tGo\tThe Band"),
            Reply::Ok | Reply::Devices(_) => {
                panic!("nowplaying answered with the wrong reply type")
            }
        }
        match device_snapshot {
            Reply::Devices(json) => assert_eq!(json, r#"[{"id":"speaker"}]"#),
            Reply::Ok | Reply::NowPlaying(_) => {
                panic!("devices answered with the wrong reply type")
            }
        }
        // An unknown verb gets no reply at all, so the client sees a closed
        // connection rather than a command it never sent being obeyed.
        assert!(refused.is_err());
        assert_eq!(
            *commands.lock().expect("the queue"),
            vec![
                ControlCommand::Next,
                ControlCommand::VolumeBy(-5),
                ControlCommand::RefreshDevices,
            ]
        );

        drop(served);
    }

    #[test]
    fn acquire_surfaces_an_authenticated_current_primary() {
        use std::io::Write as _;
        use std::net::{Ipv4Addr, TcpListener};

        let (dirs, root) = test_dirs("acquire-current");
        let token = "a".repeat(CONTROL_TOKEN_HEX_BYTES);
        crate::secrets::write_private_atomic(&dirs.control_token_file(), token.as_bytes()).unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let expected = token.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("activation client");
            let line = read_line(&mut stream).expect("authenticated activation frame");
            assert_eq!(authenticate(&line, &expected), Some("show"));
            stream
                .write_all(format!("{OK_REPLY}\n").as_bytes())
                .expect("activation reply");
        });

        let outcome = acquire_with(
            &dirs,
            &crate::backend::Waker::default(),
            port,
            1,
            || {},
            |_, _, _, _, _, _| panic!("an authenticated primary must be surfaced"),
        );

        assert!(matches!(outcome, Outcome::Surfaced));
        server.join().expect("primary exits");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn acquire_blocks_on_an_occupied_foreign_or_old_port() {
        use std::net::{Ipv4Addr, TcpListener};

        let (dirs, root) = test_dirs("acquire-foreign");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let outcome = acquire_with(
            &dirs,
            &crate::backend::Waker::default(),
            port,
            1,
            || {},
            |_, _, _, _, _, _| panic!("an occupied port must not become primary"),
        );

        let Outcome::Blocked(error) = outcome else {
            panic!("an unauthenticated owner must fail closed");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        assert!(
            error
                .to_string()
                .contains("refusing to start a second instance")
        );
        drop(listener);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn acquire_blocks_when_an_old_protocol_owner_rejects_the_authenticated_frame() {
        use std::net::{Ipv4Addr, TcpListener};

        let (dirs, root) = test_dirs("acquire-old");
        let token = "b".repeat(CONTROL_TOKEN_HEX_BYTES);
        crate::secrets::write_private_atomic(&dirs.control_token_file(), token.as_bytes()).unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mixed-version client");
            read_line(&mut stream).expect("new authenticated frame");
            // An older owner does not understand the token-bearing verb and
            // closes without the authenticated acknowledgement.
        });

        let outcome = acquire_with(
            &dirs,
            &crate::backend::Waker::default(),
            port,
            1,
            || {},
            |_, _, _, _, _, _| panic!("an old owner must not permit a rival"),
        );

        assert!(matches!(outcome, Outcome::Blocked(_)));
        server.join().expect("old owner exits");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn acquire_retries_bind_and_wins_a_shutdown_handoff_after_token_read_failure() {
        use std::net::{Ipv4Addr, TcpListener};

        let (dirs, root) = test_dirs("acquire-handoff");
        let mut previous = Some(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap());
        let port = previous.as_ref().unwrap().local_addr().unwrap().port();
        let published = std::cell::Cell::new(false);
        let outcome = acquire_with(
            &dirs,
            &crate::backend::Waker::default(),
            port,
            2,
            || drop(previous.take()),
            |listener, token, _, _, _, _| {
                assert_eq!(listener.local_addr()?.port(), port);
                assert_eq!(load_control_token(&dirs)?, token);
                published.set(true);
                drop(listener);
                Ok(())
            },
        );

        let Outcome::Only(guard) = outcome else {
            panic!("the released port must be reclaimed");
        };
        assert!(published.get(), "the token precedes listener startup");
        assert!(load_control_token(&dirs).is_ok());
        drop(guard);
        assert_eq!(
            load_control_token(&dirs).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn acquire_reports_invalid_owner_token_without_starting() {
        use std::net::{Ipv4Addr, TcpListener};

        let (dirs, root) = test_dirs("acquire-invalid-token");
        crate::secrets::write_private_atomic(&dirs.control_token_file(), b"not-a-token").unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let outcome = acquire_with(
            &dirs,
            &crate::backend::Waker::default(),
            port,
            1,
            || {},
            |_, _, _, _, _, _| panic!("invalid owner token must fail closed"),
        );

        let Outcome::Blocked(error) = outcome else {
            panic!("invalid owner token must block startup");
        };
        assert!(error.to_string().contains("control token is invalid"));
        drop(listener);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn primary_setup_failure_releases_port_and_matching_token() {
        use std::net::{Ipv4Addr, TcpListener};

        let (dirs, root) = test_dirs("acquire-listener-failure");
        let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let outcome = acquire_with(
            &dirs,
            &crate::backend::Waker::default(),
            port,
            1,
            || {},
            |listener, token, _, _, _, _| {
                assert_eq!(load_control_token(&dirs)?, token);
                drop(listener);
                Err(std::io::Error::other("injected listener setup failure"))
            },
        );

        let Outcome::Blocked(error) = outcome else {
            panic!("listener setup failure must block startup");
        };
        assert!(
            error
                .to_string()
                .contains("injected listener setup failure")
        );
        assert_eq!(
            load_control_token(&dirs).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        drop(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).expect("port was released"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn token_publication_failure_releases_the_won_port() {
        use std::net::{Ipv4Addr, TcpListener};

        let (dirs, root) = test_dirs("acquire-token-failure");
        std::fs::create_dir(dirs.control_token_file()).expect("block token publication");
        let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let outcome = acquire_with(
            &dirs,
            &crate::backend::Waker::default(),
            port,
            1,
            || {},
            |_, _, _, _, _, _| panic!("listener cannot start before token publication"),
        );

        assert!(matches!(outcome, Outcome::Blocked(_)));
        drop(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).expect("port was released"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn control_token_rotates_and_a_lease_clears_only_its_own_file() {
        let (dirs, root) = test_dirs("token-lifecycle");
        let stale = "b".repeat(CONTROL_TOKEN_HEX_BYTES);
        crate::secrets::write_private_atomic(&dirs.control_token_file(), stale.as_bytes())
            .expect("stale token");

        let lease = ControlTokenLease::issue(&dirs).expect("fresh token");
        assert_ne!(lease.token, stale);
        assert_eq!(load_control_token(&dirs).expect("read token"), lease.token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dirs.control_token_file())
                .expect("token metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        let successor = "c".repeat(CONTROL_TOKEN_HEX_BYTES);
        crate::secrets::write_private_atomic(&dirs.control_token_file(), successor.as_bytes())
            .expect("successor token");
        drop(lease);
        assert_eq!(
            load_control_token(&dirs).expect("successor survives"),
            successor
        );
        drop(ControlTokenLease {
            path: dirs.control_token_file(),
            token: successor,
        });
        assert_eq!(
            load_control_token(&dirs)
                .expect_err("lease clears token")
                .kind(),
            std::io::ErrorKind::NotFound
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn spotify_sign_out_does_not_remove_the_process_control_token() {
        use crate::secrets::{LegacySecret, PrivateFileStore};

        let (dirs, root) = test_dirs("token-sign-out");
        let lease = ControlTokenLease::issue(&dirs).expect("control token");
        let store = PrivateFileStore::new(dirs.secrets_dir());
        let web_legacy = LegacySecret::new(root.join("legacy-web.json"));
        let playback_legacy = LegacySecret::new(root.join("legacy-playback.json"));
        let mut memory_cleared = false;

        crate::secrets::clear_all_secrets(&store, &web_legacy, &playback_legacy, || {
            memory_cleared = true
        })
        .expect("Spotify sign-out storage clear");

        assert!(memory_cleared);
        assert_eq!(load_control_token(&dirs).unwrap(), lease.token);
        drop(lease);
        let _ = std::fs::remove_dir_all(root);
    }
}
