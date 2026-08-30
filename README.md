# Fastpotify

**Spotify, native and fast.** A lightweight Spotify client written in Rust with
[egui](https://github.com/emilk/egui), playing music through
[librespot](https://github.com/librespot-org/librespot). It runs on Linux,
macOS, and Windows, starts in well under a second, and stays small while it
runs. There is no browser engine anywhere in the process.

Fastpotify follows in the footsteps of
[Omarchy Spotify](https://github.com/stappmus/Omarchy-Spotify) and
[spotify-tui](https://github.com/Rigellute/spotify-tui): the familiar Spotify
layout, the whole library, and a Spotify Connect receiver on your computer,
as one ordinary desktop application rather than a shell plugin.

![Fastpotify showing a playlist, with the queue open and a track playing on a remote speaker](docs/screenshot.png)

**Documentation:** [fastpotify.rocks](https://fastpotify.rocks/): what it is, getting started, everyday use, and how it connects to Spotify.

## What it does

- **Plays music on this computer.** Fastpotify is a Spotify Connect device.
  Pick it from your phone, or press play here. Gapless, up to 320 kbps, with
  optional volume normalisation and an on-disk audio cache.
- **Controls every other device.** Move playback to a speaker, a phone, or
  another computer from the device picker, and keep controlling it: play,
  pause, skip, seek, shuffle, repeat, volume.
- **Your whole library.** Playlists, Liked Songs, saved albums, followed
  artists, podcasts, and saved episodes, filterable in the sidebar and as
  full pages.
- **Search** across songs, artists, albums, playlists, podcasts, and episodes,
  with a top result and per-type views.
- **Home** with Made for you, Recently played, your top artists and songs, and
  recommendations.
- **Artist pages** with popular songs, a filterable discography, and related
  artists. **Album**, **playlist**, and **podcast** pages with everything
  playable from any row.
- **Playlists you own** can be created, renamed, described, reordered, and
  edited: add from any row's menu, remove from the playlist page.
- **Queue** as a side panel or a page; add anything to it from a row menu.
- **Album-art colour.** Pages and the player bar take a tint from the cover
  of what you are looking at or listening to. Turn it off in Settings.
- **Light and dark**, or follow the system.
- **Keyboard-first.** Every common action has a shortcut (`Ctrl+/` or `?` lists
  them).
- **Keeps playing when you close the window.** The window closes for real
  while the music and process stay alive. Reopen it from the Dock on macOS or
  the system tray on Linux and Windows. Quit with `Cmd+Q` on macOS or from the
  tray/`Ctrl+Q` elsewhere; Settings can switch to close-to-quit.
- **Honest about the network.** Pages show spinners while they load, a
  quiet indicator appears in the top bar whenever the app is talking to
  Spotify for more than a moment, and if Spotify asks the app to back off
  you see that it is waiting, instead of an unexplained pause.
- **One instance.** Launching it again surfaces the window that is already
  open instead of starting a rival copy, on every platform.
- **Desktop integration.** MPRIS on Linux, so media keys, the shell, and
  `playerctl` see Fastpotify like any other player. On macOS and Windows,
  `fastpotify next` and its siblings drive the running app from a terminal,
  a launcher, or a hotkey.

## Install

On Arch Linux, Fastpotify is in the AUR:

```bash
yay -S fastpotify          # the released build
yay -S fastpotify-git      # built from the latest commit
```

On macOS, with [Homebrew](https://brew.sh):

```sh
brew install --cask crmne/tap/fastpotify
```

Everywhere else it is a single binary. Build it with a stable Rust toolchain
(1.95 or newer):

```bash
cargo install --path .
```

On Linux you also need the development packages for ALSA, PulseAudio (which
covers PipeWire), and the usual windowing libraries, for example on Arch:

```bash
sudo pacman -S --needed alsa-lib libpulse libxkbcommon wayland
```

and on Debian or Ubuntu:

```bash
sudo apt install libasound2-dev libpulse-dev libxkbcommon-dev libwayland-dev
```

With [Nix](https://nixos.org), `nix develop` provides all of it, along with
the exact toolchain `rust-toolchain.toml` pins.

Titles in a script the interface font does not cover -- Chinese, Japanese,
Korean, Arabic, Hebrew, Thai, the Indic scripts and a dozen more -- are drawn
with a face borrowed from the system rather than bundled, which would cost
more than ten megabytes for Chinese alone. macOS and Windows carry faces for
the common ones; on Linux install the Noto families for the scripts you
listen to, for example `noto-fonts` and `noto-fonts-cjk` (Arch) or
`fonts-noto` and `fonts-noto-cjk` (Debian or Ubuntu). A script with no face
installed still shows as empty boxes.

A desktop entry is provided in `packaging/applications/fastpotify.desktop`.

## Sign in

Press **Sign in with Spotify**. Your browser opens Spotify's own consent
page (Authorization Code with PKCE); Fastpotify never sees your password.
When Spotify redirects back to the app, your library, search, and control
of other devices work immediately. The refresh token is stored in the
platform's state directory as an owner-private file, so the browser is needed
once per machine.

Playing music **on this computer** is one more one-time browser approval.
Spotify treats streaming as a separate grant for its own client identity,
which is what librespot plays with. Take it from the device menu ("Play
here, set up once") or Settings; it needs Spotify Premium, and librespot
returns a reusable credential that Fastpotify stores separately from the Web
grant, so it also never asks again. Browsing and remote control work on any
account without this step.

These files avoid Keychain prompts and use the conventional desktop/Linux
threat model: directories are owner-only (0700) and credential files are
owner-only (0600) on Unix. They are not encrypted with a key stored beside
them. FileVault or full-disk encryption protects a powered-off disk, but
malware already running as your account can read the credentials.

The Web API always keeps a shared public application connected for complete
catalog and playlist coverage. If you hit rate limits, you can also authorize
your own free Spotify application in Settings → Account. Eligible playback,
library, catalog, and owned-playlist requests then use its independent session;
operations limited by Spotify's Development Mode stay on the shared session.

## Keyboard shortcuts

| Shortcut | What it does |
| --- | --- |
| `Space` | Play or pause |
| `Ctrl+←` / `Ctrl+→` | Previous or next |
| `Shift+←` / `Shift+→` | Seek 10 seconds |
| `Ctrl+↑` / `Ctrl+↓` | Volume |
| `M` | Mute |
| `S` / `R` | Shuffle / cycle repeat |
| `Q` | Queue panel |
| `Ctrl+F` or `/` | Search |
| `Ctrl+B` | Show or hide the sidebar |
| `Alt+←` / `Alt+→` | Back or forward |
| `Ctrl+H` / `Ctrl+L` | Home / Liked Songs |
| `Ctrl+Shift+A` / `Ctrl+Shift+B` | Playing artist / album |
| `Ctrl+,` | Settings |
| `Ctrl+/` or `?` | All shortcuts |
| `Ctrl+Q` | Quit |

On macOS, `Cmd` replaces `Ctrl`.

## Controlling it from outside

On Linux, Fastpotify is an MPRIS player, so `playerctl --player=fastpotify
play-pause` already works.

macOS and Windows have no such bus, so the same verbs are subcommands. They
talk to the instance already running and print nothing on success:

```
fastpotify play-pause          fastpotify volume 40
fastpotify play                fastpotify volume-up [percent]
fastpotify pause               fastpotify volume-down [percent]
fastpotify next                fastpotify mute
fastpotify previous            fastpotify shuffle [on|off]
fastpotify seek 15             fastpotify repeat [off|context|track]
fastpotify seek -- -15         fastpotify like
fastpotify seek-to 90          fastpotify play-uri spotify:playlist:37i9…
fastpotify show                fastpotify transfer <device-id>
fastpotify now-playing [--raw] fastpotify devices [--raw]
```

`shuffle` and `repeat` toggle without an argument and set an explicit state
with one. `like` changes the saved state of the playing music track;
`play-uri` accepts music track, album, playlist, and artist URIs only.

`now-playing` prints one readable line; `--raw` prints the fields
tab-separated — state, title, artists, album, position_ms, duration_ms,
volume, shuffle, repeat, art_url, saved, device — for a script that wants one
of them. The final three fields are appended so scripts written for the
original nine retain their field positions. `saved` is `yes`, `no`, or
`unknown` while Spotify's answer is pending.

`devices` lists Spotify Connect devices with the id first and the active one
marked `*`; its final column says `restricted` when Spotify disallows remote
control or `fixed volume` when only volume is unavailable. `--raw` emits JSON,
including those capability flags. Reading the list also requests a refresh,
so a cold first read can be empty and the next one current. Unsupported target
controls are refused by the running app with a visible warning instead of
being sent to the wrong device. A verb exits non-zero when Fastpotify is not
running or the control reply is invalid.

That is enough for a launcher such as Raycast or Alfred to drive playback
through its own script commands.

## Settings

Preferences live in one readable JSON file (`~/.config/fastpotify/settings.json`
on Linux): the Connect device name, bitrate, normalisation, autoplay, gapless
playback, the audio backend (PulseAudio/PipeWire or ALSA on Linux), audio
cache size, theme, interface layout and zoom, and whether pages take colour
from artwork. Playback settings apply when you press **Apply and restart
playback**.

Caches (audio, artwork) live under the cache directory and can be deleted at
any time without signing you out.

## How it is built

- `src/player.rs`: the librespot session, player, mixer, and Spirc (Spotify
  Connect) wrapped into one engine that folds player events into a state
  snapshot for the interface.
- `src/api/`: a gateway over independent shared and personal Web API sessions,
  each with bounded concurrency, refresh and `Retry-After` state, plus
  automatic fallback between the 2026 endpoint shapes (`/me/library`,
  `/playlists/{id}/items`) and the classic ones.
- `src/backend.rs`: a tokio runtime on its own thread; the interface talks to
  it through channels and is woken with `request_repaint`, so the app is idle
  when nothing happens.
- `src/images.rs`: album art as an egui bytes loader with a disk cache and
  time-based eviction, plus the accent-colour extraction.
- `src/app.rs`, `src/model.rs`, `src/ui/`: state, navigation, and the views.
  Views collect `Action`s while drawing and the app applies them afterwards.
- `src/mpris.rs`: Linux media controls on a dedicated thread.

Fastpotify pins its Rust toolchain in `rust-toolchain.toml`; `cargo test`
covers the API models, the endpoint fallbacks, PKCE, the player state
machine, and a headless render of every page, panel, and dialog.

To look at the interface without a Spotify account, build with the `demo`
feature and start it with sample data:

```bash
cargo run --features demo -- --demo --demo-page playlist:pl1 --demo-show queue
```

Demo mode never writes settings. `--demo-shot <PATH>` writes the window to a
PNG and exits, which is how the screenshot above is made.

## Acknowledgements

Fastpotify stands on [librespot](https://github.com/librespot-org/librespot),
[egui](https://github.com/emilk/egui), the [Inter](https://rsms.me/inter/)
typeface (OFL), and [Lucide](https://lucide.dev) icons (ISC).

Fastpotify is an independent project and is not affiliated with Spotify.
Spotify is a trademark of Spotify AB.

Licensed under the [MIT License](LICENSE).
