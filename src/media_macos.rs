//! macOS Now Playing and remote commands.
//!
//! The Objective-C objects live in main-thread-local storage: `App` can still
//! move through eframe's process-level slot, while the native registration
//! survives closing the last window to the status item. Registration is
//! deferred until `App::attach`, after eframe has created `NSApplication`.
//!
//! Souvlaki remains the text-metadata publisher. The maintained
//! `objc2-media-player` bindings own the command handlers and playback
//! timeline because souvlaki 0.8 does not publish the playback-rate field
//! macOS needs to advance Now Playing's elapsed time.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::Duration;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSMutableDictionary, NSNumber, NSString};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPNowPlayingInfoCenter,
    MPNowPlayingInfoPropertyDefaultPlaybackRate, MPNowPlayingInfoPropertyElapsedPlaybackTime,
    MPNowPlayingInfoPropertyPlaybackRate, MPNowPlayingPlaybackState, MPRemoteCommand,
    MPRemoteCommandCenter, MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};
use souvlaki::{MediaControls, MediaMetadata, PlatformConfig};

use crate::media::{MediaCommand, MediaState};
use crate::player::Playback;

type Wake = Arc<dyn Fn() + Send + Sync>;

/// Enough bursts for normal button use without allowing an unbounded queue.
const COMMAND_QUEUE_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransportCommand {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
}

/// A callback-safe representation. Converting an `Arc<str>` to the app's
/// owned `String` is deliberately left to the main loop.
#[derive(Clone, Debug)]
enum NativeEvent {
    Transport(TransportCommand),
    SetPosition {
        track_uri: Arc<str>,
        position_ms: u32,
    },
}

impl NativeEvent {
    fn into_media_command(self) -> MediaCommand {
        match self {
            Self::Transport(TransportCommand::Play) => MediaCommand::Play,
            Self::Transport(TransportCommand::Pause) => MediaCommand::Pause,
            Self::Transport(TransportCommand::PlayPause) => MediaCommand::PlayPause,
            Self::Transport(TransportCommand::Stop) => MediaCommand::Stop,
            Self::Transport(TransportCommand::Next) => MediaCommand::Next,
            Self::Transport(TransportCommand::Previous) => MediaCommand::Previous,
            Self::SetPosition {
                track_uri,
                position_ms,
            } => MediaCommand::SetPosition {
                track_uri: track_uri.to_string(),
                position_ms,
            },
        }
    }
}

fn position_millis(seconds: f64) -> Option<u32> {
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some((seconds * 1000.0).round().min(f64::from(u32::MAX)) as u32)
}

fn position_event(seconds: f64, track_uri: Option<Arc<str>>) -> Option<NativeEvent> {
    Some(NativeEvent::SetPosition {
        track_uri: track_uri?,
        position_ms: position_millis(seconds)?,
    })
}

fn track_uri_snapshot(track_uri: &Mutex<Arc<str>>) -> Option<Arc<str>> {
    let uri = match track_uri.try_lock() {
        Ok(uri) => Arc::clone(&uri),
        Err(TryLockError::Poisoned(poisoned)) => {
            let uri = poisoned.into_inner();
            Arc::clone(&uri)
        }
        Err(TryLockError::WouldBlock) => return None,
    };
    (!uri.is_empty()).then_some(uri)
}

/// The Apple handler only calls this bounded send and schedules a coalesced
/// main-queue wake. It never waits for egui, Spirc, or network work.
fn enqueue(sender: &SyncSender<NativeEvent>, event: NativeEvent, wake: &Wake) -> bool {
    if sender.try_send(event).is_err() {
        return false;
    }
    wake();
    true
}

/// Apple may invoke a command handler on an arbitrary queue. Touch egui only
/// after dispatching to the main queue, and collapse a burst into one wake.
fn main_queue_waker(wake: impl Fn() + Send + Sync + 'static) -> Wake {
    let wake: Wake = Arc::new(wake);
    let scheduled = Arc::new(AtomicBool::new(false));
    Arc::new(move || {
        if scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let wake = Arc::clone(&wake);
        let scheduled = Arc::clone(&scheduled);
        dispatch2::DispatchQueue::main().exec_async(move || {
            // Clear first so a command arriving while this wake is handled
            // can schedule the next one.
            scheduled.store(false, Ordering::Release);
            wake();
        });
    })
}

fn handler_status(accepted: bool) -> MPRemoteCommandHandlerStatus {
    if accepted {
        MPRemoteCommandHandlerStatus::Success
    } else {
        MPRemoteCommandHandlerStatus::CommandFailed
    }
}

/// The opaque target returned by MediaPlayer must be retained for exactly as
/// long as its handler is registered.
struct RemoteTarget {
    command: Retained<MPRemoteCommand>,
    target: Retained<AnyObject>,
}

impl RemoteTarget {
    fn register(
        command: Retained<MPRemoteCommand>,
        handler: RcBlock<
            dyn Fn(NonNull<MPRemoteCommandEvent>) -> MPRemoteCommandHandlerStatus + 'static,
        >,
    ) -> Self {
        // Safety: the generated binding gives this block the exact event and
        // return ABI required by `addTargetWithHandler`. MediaPlayer retains
        // the block behind the returned opaque target.
        let target = unsafe {
            command.setEnabled(true);
            command.addTargetWithHandler(&handler)
        };
        Self { command, target }
    }

    fn transport(
        command: Retained<MPRemoteCommand>,
        kind: TransportCommand,
        sender: SyncSender<NativeEvent>,
        wake: Wake,
    ) -> Self {
        // Every captured value is Send + Sync. MediaPlayer may invoke this
        // block on a queue other than the one that registered it.
        let handler: RcBlock<
            dyn Fn(NonNull<MPRemoteCommandEvent>) -> MPRemoteCommandHandlerStatus + 'static,
        > = RcBlock::new(move |_event| {
            handler_status(enqueue(&sender, NativeEvent::Transport(kind), &wake))
        });
        Self::register(command, handler)
    }

    fn position(
        command: Retained<MPRemoteCommand>,
        sender: SyncSender<NativeEvent>,
        wake: Wake,
        track_uri: Arc<Mutex<Arc<str>>>,
    ) -> Self {
        // Every captured value is Send + Sync. The URI lock is only tried,
        // never waited on, from this Apple callback.
        let handler: RcBlock<
            dyn Fn(NonNull<MPRemoteCommandEvent>) -> MPRemoteCommandHandlerStatus + 'static,
        > = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
            // Safety: MediaPlayer invokes this block with a valid retained
            // MPRemoteCommandEvent for the duration of the call.
            let event = unsafe { event.as_ref() };
            // The runtime downcast verifies the position-event subclass
            // before calling its getter.
            let Some(event) = event.downcast_ref::<MPChangePlaybackPositionCommandEvent>() else {
                return MPRemoteCommandHandlerStatus::CommandFailed;
            };
            let seconds = unsafe { event.positionTime() };
            let event = position_event(seconds, track_uri_snapshot(&track_uri));
            handler_status(event.is_some_and(|event| enqueue(&sender, event, &wake)))
        });
        Self::register(command, handler)
    }
}

impl Drop for RemoteTarget {
    fn drop(&mut self) {
        // Safety: `target` is the opaque object this same command returned.
        unsafe {
            self.command.removeTarget(Some(&self.target));
            self.command.setEnabled(false);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Timeline {
    elapsed_seconds: f64,
    playback_rate: f64,
}

fn timeline(playback: Playback, position_ms: u32) -> Timeline {
    Timeline {
        elapsed_seconds: f64::from(position_ms) / 1000.0,
        playback_rate: if playback == Playback::Playing {
            1.0
        } else {
            0.0
        },
    }
}

fn now_playing_state(playback: Playback) -> MPNowPlayingPlaybackState {
    match playback {
        Playback::Playing => MPNowPlayingPlaybackState::Playing,
        Playback::Paused | Playback::Loading => MPNowPlayingPlaybackState::Paused,
        Playback::Stopped => MPNowPlayingPlaybackState::Stopped,
    }
}

fn number(value: f64) -> Retained<AnyObject> {
    NSNumber::new_f64(value)
        .into_super()
        .into_super()
        .into_super()
}

/// Native state, used only from the main-thread-local host below.
struct Bridge {
    // Drop our precise targets before souvlaki's metadata handle, whose Drop
    // performs its legacy remove-all cleanup at final process shutdown.
    _targets: Vec<RemoteTarget>,
    metadata: MediaControls,
    now_playing: Retained<MPNowPlayingInfoCenter>,
    last: MediaState,
    track_uri: Arc<Mutex<Arc<str>>>,
}

impl Bridge {
    fn new(sender: SyncSender<NativeEvent>, wake: Wake) -> Result<Self, String> {
        let metadata = MediaControls::new(PlatformConfig {
            display_name: "Fastpotify",
            dbus_name: "fastpotify",
            hwnd: None,
        })
        .map_err(|error| error.to_string())?;
        let track_uri = Arc::new(Mutex::new(Arc::<str>::from("")));

        // Safety: `App::attach` establishes the macOS main thread after
        // NSApplication exists. These are process-wide MediaPlayer objects.
        let center = unsafe { MPRemoteCommandCenter::sharedCommandCenter() };
        let now_playing = unsafe { MPNowPlayingInfoCenter::defaultCenter() };
        let mut targets = Vec::with_capacity(7);
        targets.push(RemoteTarget::transport(
            unsafe { center.playCommand() },
            TransportCommand::Play,
            sender.clone(),
            Arc::clone(&wake),
        ));
        targets.push(RemoteTarget::transport(
            unsafe { center.pauseCommand() },
            TransportCommand::Pause,
            sender.clone(),
            Arc::clone(&wake),
        ));
        targets.push(RemoteTarget::transport(
            unsafe { center.togglePlayPauseCommand() },
            TransportCommand::PlayPause,
            sender.clone(),
            Arc::clone(&wake),
        ));
        targets.push(RemoteTarget::transport(
            unsafe { center.stopCommand() },
            TransportCommand::Stop,
            sender.clone(),
            Arc::clone(&wake),
        ));
        targets.push(RemoteTarget::transport(
            unsafe { center.nextTrackCommand() },
            TransportCommand::Next,
            sender.clone(),
            Arc::clone(&wake),
        ));
        targets.push(RemoteTarget::transport(
            unsafe { center.previousTrackCommand() },
            TransportCommand::Previous,
            sender.clone(),
            Arc::clone(&wake),
        ));
        targets.push(RemoteTarget::position(
            unsafe { center.changePlaybackPositionCommand() }.into_super(),
            sender,
            wake,
            Arc::clone(&track_uri),
        ));

        Ok(Self {
            _targets: targets,
            metadata,
            now_playing,
            last: MediaState::default(),
            track_uri,
        })
    }

    fn apply(&mut self, state: MediaState) {
        let track_changed = state.track != self.last.track;
        if track_changed {
            // A scrub during the tiny metadata transition is rejected rather
            // than being attached to either the old or new track by accident.
            self.set_track_uri("");
            self.publish_metadata(&state);
            self.set_track_uri(
                state
                    .track
                    .as_ref()
                    .map(|track| track.uri.as_str())
                    .unwrap_or_default(),
            );
        }
        if track_changed || state.playback != self.last.playback {
            self.publish_timeline(state.playback, state.position_ms);
        }
        self.last = state;
    }

    fn seeked(&mut self, position_ms: u32) {
        self.last.position_ms = position_ms;
        self.publish_timeline(self.last.playback, position_ms);
    }

    fn set_track_uri(&self, uri: &str) {
        let uri: Arc<str> = Arc::from(uri);
        *self
            .track_uri
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = uri;
    }

    fn publish_metadata(&mut self, state: &MediaState) {
        let artist = state
            .track
            .as_ref()
            .map(|track| track.artists.join(", "))
            .unwrap_or_default();
        let metadata = match &state.track {
            Some(track) => MediaMetadata {
                title: Some(track.title.as_str()),
                album: Some(track.album.as_str()),
                artist: Some(artist.as_str()),
                // Souvlaki 0.8 accepts only a URL and fetches it outside the
                // validated ArtLoader path; omitting it is the safe boundary.
                cover_url: None,
                duration: Some(Duration::from_millis(u64::from(track.duration_ms))),
            },
            None => MediaMetadata::default(),
        };
        if let Err(error) = self.metadata.set_metadata(metadata) {
            log::debug!("media controls refused the metadata: {error}");
        }
    }

    fn publish_timeline(&self, playback: Playback, position_ms: u32) {
        let timeline = timeline(playback, position_ms);
        // Preserve souvlaki's title/album dictionary and change only the
        // fields this bridge owns.
        let info: Retained<NSMutableDictionary<NSString, AnyObject>> =
            match unsafe { self.now_playing.nowPlayingInfo() } {
                Some(info) => NSMutableDictionary::dictionaryWithDictionary(&info),
                None => NSMutableDictionary::new(),
            };
        let elapsed = number(timeline.elapsed_seconds);
        let rate = number(timeline.playback_rate);
        let default_rate = number(1.0);
        // Safety: these extern keys come from MediaPlayer.framework, each
        // value has the documented NSNumber type, and the dictionary has the
        // exact NSString/AnyObject generic expected by the generated binding.
        unsafe {
            info.insert(MPNowPlayingInfoPropertyElapsedPlaybackTime, &elapsed);
            info.insert(MPNowPlayingInfoPropertyPlaybackRate, &rate);
            info.insert(MPNowPlayingInfoPropertyDefaultPlaybackRate, &default_rate);
            self.now_playing.setNowPlayingInfo(Some(&info));
            self.now_playing
                .setPlaybackState(now_playing_state(playback));
        }
    }
}

mod host {
    use std::cell::RefCell;

    use super::*;

    thread_local! {
        /// MediaPlayer objects are not Send. They live for the process on the
        /// macOS main thread, independently of any individual eframe window.
        static BRIDGE: RefCell<Option<Bridge>> = const { RefCell::new(None) };
    }

    pub fn create(sender: SyncSender<NativeEvent>, wake: Wake) -> Result<(), String> {
        if MainThreadMarker::new().is_none() {
            return Err("media controls require the macOS main thread".to_owned());
        }
        BRIDGE.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                *slot = Some(Bridge::new(sender, wake)?);
            }
            Ok(())
        })
    }

    pub fn update(state: MediaState) {
        BRIDGE.with(|slot| {
            if let Some(bridge) = slot.borrow_mut().as_mut() {
                bridge.apply(state);
            }
        });
    }

    pub fn seeked(position_ms: u32) {
        BRIDGE.with(|slot| {
            if let Some(bridge) = slot.borrow_mut().as_mut() {
                bridge.seeked(position_ms);
            }
        });
    }

    pub fn destroy() {
        if MainThreadMarker::new().is_some() {
            BRIDGE.with(|slot| {
                let bridge = slot.borrow_mut().take();
                drop(bridge);
            });
        }
    }
}

enum Deferred<P> {
    Pending(P),
    Ready,
    Unavailable,
}

impl<P> Deferred<P> {
    /// Initializes at most once. Failure is terminal so reopening a window
    /// cannot build up duplicate platform handlers.
    fn initialize<E>(&mut self, initialize: impl FnOnce(P) -> Result<(), E>) -> Result<bool, E> {
        match std::mem::replace(self, Self::Unavailable) {
            Self::Pending(pending) => {
                initialize(pending)?;
                *self = Self::Ready;
                Ok(true)
            }
            ready @ Self::Ready => {
                *self = ready;
                Ok(false)
            }
            Self::Unavailable => Ok(false),
        }
    }
}

pub struct MediaService {
    commands: Receiver<NativeEvent>,
    bridge: Deferred<(SyncSender<NativeEvent>, Wake)>,
}

impl MediaService {
    /// Creates only the bounded event path. Native registration waits for
    /// `attach`, after eframe has established AppKit.
    pub fn spawn(wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (sender, commands) = std::sync::mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        Self {
            commands,
            bridge: Deferred::Pending((sender, main_queue_waker(wake))),
        }
    }

    /// Registers once for the process lifetime. Recreating an eframe window
    /// leaves the existing command center and metadata publisher untouched.
    pub fn attach(&mut self) {
        if MainThreadMarker::new().is_none() {
            log::warn!("media controls can only be initialized on the macOS main thread");
            return;
        }
        if let Err(error) = self
            .bridge
            .initialize(|(sender, wake)| host::create(sender, wake))
        {
            log::warn!("no media controls: {error}");
        }
    }

    pub fn drain_commands(&self) -> Vec<MediaCommand> {
        self.commands
            .try_iter()
            .map(NativeEvent::into_media_command)
            .collect()
    }

    pub fn update(&mut self, state: MediaState) {
        host::update(state);
    }

    pub fn seeked(&self, position_ms: u32) {
        host::seeked(position_ms);
    }
}

impl Drop for MediaService {
    fn drop(&mut self) {
        host::destroy();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn media_keys_and_ear_detection_use_the_ordinary_transport_path() {
        assert_eq!(
            NativeEvent::Transport(TransportCommand::Play).into_media_command(),
            MediaCommand::Play
        );
        assert_eq!(
            NativeEvent::Transport(TransportCommand::Pause).into_media_command(),
            MediaCommand::Pause
        );
        assert_eq!(
            NativeEvent::Transport(TransportCommand::PlayPause).into_media_command(),
            MediaCommand::PlayPause
        );
        assert_eq!(
            NativeEvent::Transport(TransportCommand::Next).into_media_command(),
            MediaCommand::Next
        );
        assert_eq!(
            NativeEvent::Transport(TransportCommand::Previous).into_media_command(),
            MediaCommand::Previous
        );
        assert_eq!(
            NativeEvent::Transport(TransportCommand::Stop).into_media_command(),
            MediaCommand::Stop
        );
    }

    #[test]
    fn position_event_keeps_the_displayed_track_identity() {
        let uri: Arc<str> = Arc::from("spotify:track:shown");
        assert_eq!(
            position_event(12.345, Some(uri)).map(NativeEvent::into_media_command),
            Some(MediaCommand::SetPosition {
                track_uri: "spotify:track:shown".to_owned(),
                position_ms: 12_345,
            })
        );
        assert!(position_event(12.0, None).is_none());
        assert!(position_event(-1.0, Some(Arc::from("track"))).is_none());
        assert!(position_event(f64::NAN, Some(Arc::from("track"))).is_none());
    }

    #[test]
    fn command_queue_is_bounded_and_only_accepted_work_wakes_the_app() {
        let (sender, commands) = std::sync::mpsc::sync_channel(1);
        let wake_count = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&wake_count);
        let wake: Wake = Arc::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
        });

        assert!(enqueue(
            &sender,
            NativeEvent::Transport(TransportCommand::Play),
            &wake,
        ));
        assert!(!enqueue(
            &sender,
            NativeEvent::Transport(TransportCommand::Pause),
            &wake,
        ));
        assert_eq!(wake_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            commands.try_recv().map(NativeEvent::into_media_command),
            Ok(MediaCommand::Play)
        );
        assert!(enqueue(
            &sender,
            NativeEvent::Transport(TransportCommand::Pause),
            &wake,
        ));
        assert_eq!(wake_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn timeline_callback_drops_instead_of_waiting_for_track_update() {
        let track_uri = Mutex::new(Arc::<str>::from("spotify:track:old"));
        let held = track_uri.lock().expect("test lock");
        assert!(track_uri_snapshot(&track_uri).is_none());
        drop(held);
        assert_eq!(
            track_uri_snapshot(&track_uri).as_deref(),
            Some("spotify:track:old")
        );
    }

    #[test]
    fn playing_advances_timeline_while_paused_and_loading_do_not() {
        assert_eq!(timeline(Playback::Playing, 12_345).playback_rate, 1.0);
        assert_eq!(timeline(Playback::Paused, 12_345).playback_rate, 0.0);
        assert_eq!(timeline(Playback::Loading, 12_345).playback_rate, 0.0);
        assert_eq!(timeline(Playback::Playing, 12_345).elapsed_seconds, 12.345);
        assert_eq!(
            now_playing_state(Playback::Playing),
            MPNowPlayingPlaybackState::Playing
        );
        assert_eq!(
            now_playing_state(Playback::Paused),
            MPNowPlayingPlaybackState::Paused
        );
    }

    #[test]
    fn deferred_bridge_initializes_once_and_survives_reattach() {
        let attempts = AtomicUsize::new(0);
        let mut bridge = Deferred::Pending(41usize);
        assert_eq!(
            bridge.initialize(|pending| {
                attempts.fetch_add(1, Ordering::Relaxed);
                assert_eq!(pending, 41);
                Ok::<_, ()>(())
            }),
            Ok(true)
        );
        assert_eq!(
            bridge.initialize(|_| -> Result<(), ()> {
                panic!("a recreated window must not register duplicate handlers")
            }),
            Ok(false)
        );
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn failed_bridge_initialization_is_not_retried() {
        let mut bridge = Deferred::Pending(());
        assert_eq!(
            bridge.initialize(|()| Err::<(), _>("unavailable")),
            Err("unavailable")
        );
        assert_eq!(
            bridge.initialize(|()| -> Result<(), &str> {
                panic!("a failed platform bridge must degrade once")
            }),
            Ok(false)
        );
    }
}
