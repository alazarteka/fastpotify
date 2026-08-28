# macOS media controls manual test

Run this checklist on a Mac build of the exact commit under test. It validates
Fastpotify's `MPRemoteCommandCenter`/Now Playing lifecycle; it does not test a
private AirPods sensor API. Automatic Ear Detection reaches the active player
as ordinary system Play/Pause commands.

## Record the setup

- Fastpotify commit:
- macOS version and Mac model:
- AirPods model and firmware:
- “Automatic Ear Detection” enabled: yes / no
- “Keep playing in background” enabled: yes / no
- Selected Spotify device: **This Mac** (local playback)
- Selected macOS sound output:

Quit Apple Music, Spotify, browsers playing media, and other apps that could
take ownership of Now Playing. Connect the AirPods and start a track in
Fastpotify on **This Mac**. Do not run the ear-detection section against a
remote Spotify speaker: this checklist is for the Mac's local player.

## 1. Control Center state and commands

1. Open Control Center → Now Playing.
2. Confirm the title, artist, album, artwork, duration, and play state match
   Fastpotify.
3. Let the track play for 10 seconds. Confirm the Control Center position
   advances by roughly 10 seconds.
4. Press Pause in Control Center. Confirm audio and both UIs pause, then wait
   5 seconds and confirm the displayed position does not continue advancing.
5. Press Play in Control Center. Confirm playback resumes from that position.
6. Drag the Control Center scrubber to a clearly different position. Confirm
   Fastpotify seeks the same track once and its position agrees.
7. Press Next once and Previous once. Each press must move exactly one track.

Pass: state and timeline remain coherent and every command is applied once.

## 2. Keyboard media keys

1. With Fastpotify playing, press the hardware Play/Pause key once; confirm a
   single pause.
2. Press it once more; confirm a single resume.
3. Press Next and Previous once each; confirm one track transition per press.
4. Repeat with Control Center closed.

Pass: media keys do not depend on Control Center being open and never double
toggle or skip.

## 3. AirPods stem controls

Use the stem gestures configured in macOS/Bluetooth settings.

1. Press the Play/Pause gesture once; confirm one pause.
2. Press it again; confirm one resume.
3. If configured, invoke Next and Previous once each; confirm one transition
   per gesture.

Pass: stem gestures follow the same command path as the keyboard media keys.

## 4. Automatic Ear Detection

System settings, AirPods generation, and reinsertion timing can change whether
macOS emits Play on reinsertion. Before judging Fastpotify, perform the same
sequence once in Apple Music and record the system baseline.

1. With both AirPods inserted and Fastpotify playing locally, remove one
   AirPod. Confirm Fastpotify follows the system's Pause action and Control
   Center becomes paused.
2. Reinsert that AirPod within the same interval used for the Apple Music
   baseline. If macOS resumed Apple Music, confirm it emits the equivalent Play
   behavior for Fastpotify; if the baseline did not resume, Fastpotify must not
   invent a resume.
3. Start playback again, then remove both AirPods. Confirm Fastpotify follows
   the system's pause/stop behavior and does not continue inaudibly.
4. Reinsert both AirPods. Confirm Fastpotify resumes only when the equivalent
   Apple Music baseline resumes.

Pass: Fastpotify follows ordinary system Play/Pause commands. It adds no
AirPods-specific auto-resume policy of its own.

## 5. Closed-window/status-item lifecycle

1. Start local playback, then close the Fastpotify window with the red close
   button. Confirm the window disappears, the status item remains, and audio
   continues.
2. While no window exists, repeat one Control Center Play/Pause cycle, one
   keyboard Play/Pause cycle, and one AirPods stem Play/Pause cycle.
3. While still windowless, press Next once. Confirm Control Center advances by
   exactly one track and its metadata/timeline update.
4. Use the status item to show Fastpotify. Confirm the reopened window shows
   the same track, position, and state.
5. Close and reopen the window three times. Then press Play/Pause once and Next
   once. Confirm neither command is duplicated; this detects duplicate native
   handler registration across window recreation.
6. Close the window once more and choose Quit from the status item. Confirm the
   process exits and Fastpotify disappears from Now Playing ownership.

Pass: controls remain registered for the process lifetime, survive the
windowless interval, and are registered only once.

## Record failures

For each failure, record the numbered step, local/remote Spotify target,
whether another media app was open, the Fastpotify and Control Center states,
and the relevant lines from Fastpotify's log. A successful Linux build does
not replace this Mac runtime test.

Default-output route changes, physical output disappearance, and system
sleep/wake are separate from Automatic Ear Detection. This lifecycle patch
does not claim a CoreAudio route observer or an `NSWorkspace` sleep/wake
observer; validate such a policy separately if one is added later.
