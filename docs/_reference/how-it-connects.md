---
title: How It Connects
description: Fastpotify's Spotify grants, why they are separate, what is stored, and what the client does when Spotify pushes back.
nav_order: 1
---

## Two kinds of grant

Fastpotify talks to Spotify in two distinct ways, and Spotify issues
credentials for them separately:

1. **The Web API** covers your library, search, playlists, and devices.
   Fastpotify always has a shared application grant and can also have an
   optional personal application grant. They request the same visible-feature
   scopes, but have independent refresh tokens, rate limits, and sessions.
2. **Streaming** is actually playing audio on this computer, through
   [librespot](https://github.com/librespot-org/librespot). This runs the
   same browser flow once against Spotify's streaming client identity, after
   which Fastpotify reconstructs and stores librespot's reusable credential.
   Premium is required, because that is what Spotify's streaming protocol
   requires.

Why not use the streaming grant for everything? Spotify throttles Web API
calls made with streaming-identity tokens. Measured during development, every endpoint
answers `429` within the first request. Separating Web access from streaming
is what actually works.

Before the browser opens, each flow binds a short-lived listener only to
`127.0.0.1`. It accepts bounded HTTP GETs on the registered path and port,
ignores malformed or unrelated local traffic, and requires the unpredictable
PKCE state before interpreting a terminal Spotify result. An authenticated
denial ends immediately, and an accepted code survives failure to deliver the
best-effort browser page. Slow connections time out. Success and failure pages
are static, non-reflecting, non-cacheable, and carry a restrictive browser
content policy. Token responses from Spotify are also read under a fixed size
limit and validated before they can be stored or used.

The shared Web application is authoritative for account identity, the full
playlist library, playlist-bearing search, external playlists, and endpoints
Spotify withholds from Development Mode. An optional personal application
accelerates playback and device commands, your library data, supported catalog
lookups, playlist creation, and playlists that the shared response proves you
own or collaborate on. Both grants must resolve to the same Spotify account.
A request is routed once before dispatch and is never replayed through the
other application after an error.

Each Web session owns its token-refresh lock, six-request concurrency bound,
rate-limit cooldown, and endpoint compatibility state. A small shared
background-work bound prevents the two sessions from flooding the interface,
while a `429` on one application does not stall the other. The shared public
application is also used by spotify-player, ncspot, and Omarchy Spotify; [Make
It Even Faster](/make-it-even-faster/) shows how to add a personal one.

## What the client stores

- The shared Web API refresh token, optional personal Web API refresh token,
  and librespot's reusable credential, as separate owner-private files
  ([where and threat model](/settings-and-files/)).
- Downloaded audio and artwork, in the cache directory, within the budget
  you set.
- Lyrics, in the cache directory, for a month.
- Nothing else. There is no telemetry, no analytics, and no Fastpotify server.

Two optional external services are disclosed in Settings and are off by
default:

- **LRCLIB fallback:** when Spotify lyrics are unavailable (including when
  local playback is not signed in), sends the track's artist, title, album,
  and duration to the fixed `https://lrclib.net` API.
  LRCLIB also receives the connection's IP address, time, and Fastpotify
  User-Agent. Responses are not redirected and are capped at 2 MiB. Decoded
  lyrics also have per-input-line timestamp, total-line, and cumulative-text
  limits so compact LRC input cannot multiply into an unbounded result.
- **Release checks:** contacts the fixed GitHub API endpoint at most once a
  day. GitHub receives the connection's IP address, time, and Fastpotify
  version in the User-Agent. The response is capped at 64 KiB, must name a
  release-policy `major.minor.patch` version with an optional prerelease
  suffix, cannot redirect, and cannot choose the release page Fastpotify
  opens.

Every artwork request made and decoded by Fastpotify goes through one loader.
It accepts only HTTPS on port 443 from `scdn.co` or `spotifycdn.com` and their
subdomains. Every redirect is rechecked against the same policy, downloads are
capped at 8 MiB, and the JPEG/PNG signature, MIME type, dimensions, and pixel
budget must agree before decoding. macOS Now Playing receives no artwork
because Souvlaki 0.8 accepts only a URL, which would create a second fetch
outside that policy. Windows keeps its established system-media URL behavior;
Linux exposes the provider URL as MPRIS metadata, so those operating-system
surfaces may fetch independently of Fastpotify. No real-account authentication
was used while establishing the in-app policy. A gated live check still needs
to confirm the complete set of hosts currently returned by Spotify; a future
legitimate host will be rejected until it is evaluated and explicitly added.
A rejection log records only the host and policy reason, never the URL path,
query, fragment, or user information.

## When Spotify pushes back

The Web API rate-limits bursts. Each session bounds its own concurrency,
honours `Retry-After`, retries safe reads quietly, and shows a small spinner in
the top bar when a conversation with Spotify takes longer than a moment. Spotify also
reshapes endpoints over time; the client detects several of these shapes at
runtime and falls back to the older form where one still exists.

## The engine

Playback runs on a dedicated runtime: librespot maintains the Spotify
Connect session, so this computer appears as a device to every other Spotify
client you own, receives transfers, and reports its position back. If the
session drops, the engine reconnects with the stored credential; the
interface never blocks on any of it.
