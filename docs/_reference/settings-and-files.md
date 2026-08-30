---
title: Settings & Files
description: Where Fastpotify keeps configuration, credentials, and caches, and what is safe to delete.
nav_order: 0
---

## Where things live

Fastpotify follows each platform's conventions. On Linux:

| What | Where | Safe to delete? |
| --- | --- | --- |
| Settings | `~/.config/fastpotify/settings.json` | Yes, you lose preferences |
| Shared Web API sign-in | `~/.local/state/fastpotify/secrets-v1/web-api.secret` | Yes, you sign in again |
| Personal Web API sign-in | `~/.local/state/fastpotify/secrets-v1/personal-web-api.secret` | Yes, you authorize your app again |
| Playback credential | `~/.local/state/fastpotify/secrets-v1/playback.secret` | Yes, you approve playback again |
| Last session | `~/.local/state/fastpotify/session.json` | Yes |
| Control IPC token (macOS/Windows) | `<state>/control-ipc.secret` (not used on Linux) | Only while Fastpotify is stopped |
| Audio cache | `~/.cache/fastpotify/audio/` | Always |
| Artwork cache | `~/.cache/fastpotify/art/` | Always |
| Lyrics cache | `~/.cache/fastpotify/lyrics/` | Always |
| Last run's log | `~/.local/state/fastpotify/fastpotify.log` | Always |
| Crash log | `~/.local/state/fastpotify/panic.log` | Always |

Clearing caches never signs you out; credentials live in *state*, not
*cache*, precisely so cleanup tools cannot log you out. Shared Web, personal
Web, and playback credentials are separate versioned items. On Unix, Fastpotify makes their
directories mode 0700 and files mode 0600, validates ownership and modes on
read, and replaces files atomically. Windows uses the account ACL inherited
from the local application-data directory; private-file opens do not follow
the final reparse point, reject handles with multiple links, lock the name
against replacement while it is open, and use the validated handle for atomic
replacement. A legacy credential is deleted only after its parent and file
have been made private and the new item was written, read back, and compared
successfully. Migration retains the validated legacy handle through that
transaction. Unix captures the legacy pathname entry before unlinking it;
Windows marks the retained handle itself for deletion. A concurrent legacy
writer's replacement is therefore preserved rather than mistaken for the
value that was migrated.
Signing out clears every live provider and the playback engine first, attempts
to delete all new and legacy credential locations, and reports any partial
failure. Removing a personal app clears only its provider and credential.

On macOS and Windows, `control-ipc.secret` authenticates later command-line or
application launches to the one primary process's loopback control socket. It
is a random process-lifetime token, separate from Spotify credentials, so
Spotify sign-out does not remove it or break activation of the still-running
application. Each primary replaces it through the bounded owner-private atomic
writer. On orderly shutdown, the process removes the pathname only when it
still contains that process's token; a successor value is retained. Private
state writers and conditional deletion share a bounded advisory mutation lock,
which the operating system releases if a process exits. An abrupt exit may
leave the token file itself, and the next primary replaces it after winning the
control port.

The Windows test suite exercises hard-link rejection, filename locking before
truncation, and handle-based replacement. The remaining platform validation is
a manual NTFS stress test that repeatedly substitutes a symlink or junction at
the log path while it opens and confirms that neither the link target nor a
different file is ever truncated or replaced.

This backend is deliberately prompt-free local storage, not a Keychain and
not application-level encryption. A key beside an encrypted file would not
improve this threat model, so Fastpotify does not add one. FileVault, BitLocker,
or another full-disk encryption system protects credentials while the disk is
offline. Malware or another process already running as your user can read the
files while you are logged in.

On macOS, settings, state, and the logs are in
`~/Library/Application Support/me.paolino.fastpotify` and the caches in
`~/Library/Caches/me.paolino.fastpotify`. On Windows, settings are in
`%APPDATA%\paolino\fastpotify\config`, state and the logs in
`%LOCALAPPDATA%\paolino\fastpotify\data`, and the caches in
`%LOCALAPPDATA%\paolino\fastpotify\cache`.

## settings.json

One readable JSON file, written atomically. The interesting fields:

| Field | Default | Meaning |
| --- | --- | --- |
| `device_name` | `Fastpotify` | Name on Spotify Connect |
| `bitrate` | `320` | 96, 160, or 320 kbps |
| `normalisation` | `false` | Volume normalisation |
| `autoplay` | `true` | Keep playing similar music at the end |
| `gapless` | `true` | Gapless playback |
| `audio_backend` | platform | `pulseaudio` or `rodio` on Linux |
| `audio_cache_mb` | `1024` | On-disk audio cache budget |
| `theme` | `dark` | `dark`, `light`, or `system` |
| `accent_from_art` | `true` | Tint pages with album art |
| `sidebar_visible` | `true` | Show the library sidebar |
| `sidebar_width` | `250` | Library sidebar width |
| `lyrics_width` | `360` | Lyrics panel width |
| `queue_width` | `360` | Queue panel width |
| `zoom` | `1.0` | Interface zoom from 50% to 250% |
| `keep_playing_in_background` | `true` | Keep playing after the window closes |
| `check_for_updates` | `false` | Ask GitHub at most once a day for a newer release |
| `lrclib_lyrics` | `false` | Ask LRCLIB when Spotify lyrics are unavailable |
| `external_services_disclosed` | `false` | Records that the external-service choices were made after their disclosure was shown |
| `web_client_id` | none | Optional personal Spotify app id used alongside the shared app |

Both external-service options are off by default. Older settings written
before the disclosure marker existed are migrated with both options off, even
if an old build had enabled release checks by default. Enabling either switch
in Settings records that the disclosure was shown.

`session.json` carries restorable runtime state separately from preferences:
the current page, recent and resumed playback, shuffle and table sorting,
window geometry, and whether the queue panel was open. Older files remain
valid; missing fields use their defaults.

## Command line

```
fastpotify [OPTIONS]

  --device-name <NAME>  Spotify Connect name for this session
  -v, --verbose         More logs from librespot and the API client
```

The log directory is owner-only and both log files are owner-only on Unix.
`fastpotify.log` in the state directory is what to attach to a bug report:
it holds the last run's output, the same lines `fastpotify -v` prints, so a
run with `-v` says the most. If the app vanished, `panic.log` next to it
says where it died; attach that too. The panic log contains only the most
recent bounded crash record rather than growing indefinitely.

## Demo mode

Builds made with `cargo build --features demo` accept `--demo`, which fills
the interface with sample data, useful for screenshots, theming, and
interface work. Demo mode never writes settings.

`--demo-page` opens a page, such as `home`, `playlist:pl1`, or `artist:art0`,
and `--demo-show` adds surfaces on top of it: a comma separated list of
`queue`, `devices`, `shortcuts`, `create`, `light`, and `focus`.

`--demo-shot <PATH>` writes the window to a PNG and exits, which is how the
screenshots in these pages are made:

```
cargo run --release --features demo -- \
  --demo-shot docs/screenshot.png --demo-page playlist:pl1 --demo-show queue
```

The shot is the window's own frame buffer, so it comes out at whatever size
the window is. `--demo-shot-delay <MS>` sets how long cover art has to arrive
before the frame is taken.
