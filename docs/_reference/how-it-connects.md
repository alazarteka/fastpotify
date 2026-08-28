---
title: How It Connects
description: The two Spotify grants, why they are separate, what is stored, and what the client does when Spotify pushes back.
nav_order: 1
---

## Two grants, once each

Fastpotify talks to Spotify in two distinct ways, and Spotify issues
credentials for them separately:

1. **The Web API** covers your library, search, playlists, and devices. Fastpotify
   uses the standard Authorization Code + PKCE flow in your browser, as a
   registered Spotify application. The refresh token is stored locally and
   renewed automatically; your password never touches the app.
2. **Streaming** is actually playing audio on this computer, through
   [librespot](https://github.com/librespot-org/librespot). This runs the
   same browser flow once against Spotify's streaming client identity, after
   which Fastpotify reconstructs and stores librespot's reusable credential.
   Premium is required, because that is what Spotify's streaming protocol
   requires.

Why not one grant? Because Spotify throttles Web API calls made with
streaming-identity tokens. Measured during development, every endpoint
answers `429` within the first request. Two narrow grants are what actually
works, and each one happens exactly once per machine.

Each browser redirect returns to a short-lived listener bound only to
`127.0.0.1`. It accepts one bounded HTTP GET on the registered path and port,
requires the unpredictable PKCE state before interpreting Spotify's result,
and times out slow local connections. Its success and failure pages are
static, non-reflecting, non-cacheable, and carry a restrictive browser content
policy. Token responses from Spotify are also read under a fixed size limit
and validated before they can be stored or used.

By default the Web API uses the shared public application also used by
spotify-player, ncspot, and Omarchy Spotify, whose allowance Spotify
divides among everyone running any of them. An application of your own
gets one to itself; [Make It Even Faster](/make-it-even-faster/) shows
how, in five minutes.

## What the client stores

- The Web API refresh token and librespot's reusable credential, as separate
  owner-private files in the state directory
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
  User-Agent. Responses are not redirected and are capped at 2 MiB.
- **Release checks:** contacts the fixed GitHub API endpoint at most once a
  day. GitHub receives the connection's IP address, time, and Fastpotify
  version in the User-Agent. The response is capped at 64 KiB, must name a
  strict `major.minor.patch` version, cannot redirect, and cannot choose the
  release page Fastpotify opens.

Artwork is accepted only over HTTPS on port 443 from `scdn.co` or
`spotifycdn.com` and their subdomains. Every redirect is rechecked against the
same policy, downloads are capped at 8 MiB, and the JPEG/PNG signature, MIME
type, dimensions, and pixel budget must agree before decoding. No real-account
authentication was used while establishing this policy. A gated live check
still needs to confirm the complete set of hosts currently returned by
Spotify; a future legitimate host will be rejected until it is evaluated and
explicitly added. A rejection log records only the host and policy reason,
never the URL path, query, fragment, or user information.

## When Spotify pushes back

The Web API rate-limits bursts. Fastpotify bounds its concurrency, honours
`Retry-After`, retries quietly, and shows a small spinner in the top bar
when a conversation with Spotify takes longer than a moment. Spotify also
reshapes endpoints over time; the client detects several of these shapes at
runtime and falls back to the older form where one still exists.

## The engine

Playback runs on a dedicated runtime: librespot maintains the Spotify
Connect session, so this computer appears as a device to every other Spotify
client you own, receives transfers, and reports its position back. If the
session drops, the engine reconnects with the stored credential; the
interface never blocks on any of it.
