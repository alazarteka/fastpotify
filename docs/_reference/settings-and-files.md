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
| Web API sign-in | `~/.local/state/fastpotify/secrets-v1/web-api.secret` | Yes, you sign in again |
| Playback credential | `~/.local/state/fastpotify/secrets-v1/playback.secret` | Yes, you approve playback again |
| Last session | `~/.local/state/fastpotify/session.json` | Yes |
| Audio cache | `~/.cache/fastpotify/audio/` | Always |
| Artwork cache | `~/.cache/fastpotify/art/` | Always |
| Lyrics cache | `~/.cache/fastpotify/lyrics/` | Always |
| Last run's log | `~/.local/state/fastpotify/fastpotify.log` | Always |
| Crash log | `~/.local/state/fastpotify/panic.log` | Always |

Clearing caches never signs you out; credentials live in *state*, not
*cache*, precisely so cleanup tools cannot log you out. Web and playback
credentials are separate versioned items. On Unix, Fastpotify makes their
directories mode 0700 and files mode 0600, validates ownership and modes on
read, and replaces files atomically. Windows uses the account ACL inherited
from the local application-data directory. A legacy credential is deleted
only after the new item was written, read back, and compared successfully.
Signing out clears the live providers and playback engine first, attempts to
delete both new and legacy locations, and reports any partial failure.

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
| `keep_playing_in_background` | `true` | Close to tray |
| `check_for_updates` | `true` | Ask GitHub once a day for a newer release |
| `web_client_id` | none | Your own Spotify app id, if you set one |

## Command line

```
fastpotify [OPTIONS]

  --device-name <NAME>  Spotify Connect name for this session
  -v, --verbose         More logs from librespot and the API client
```

`fastpotify.log` in the state directory is what to attach to a bug report:
it holds the last run's output, the same lines `fastpotify -v` prints, so a
run with `-v` says the most. If the app vanished, `panic.log` next to it
says where it died; attach that too.

## Demo mode

Builds made with `cargo build --features demo` accept `--demo`, which fills
the interface with sample data, useful for screenshots, theming, and
interface work. Demo mode never writes settings.

`--demo-page` opens a page, such as `home`, `playlist:pl1`, or `artist:art0`,
and `--demo-show` adds surfaces on top of it: a comma separated list of
`queue`, `devices`, `shortcuts`, `create`, and `light`.

`--demo-shot <PATH>` writes the window to a PNG and exits, which is how the
screenshots in these pages are made:

```
cargo run --release --features demo -- \
  --demo-shot docs/screenshot.png --demo-page playlist:pl1 --demo-show queue
```

The shot is the window's own frame buffer, so it comes out at whatever size
the window is. `--demo-shot-delay <MS>` sets how long cover art has to arrive
before the frame is taken.
