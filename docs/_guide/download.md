---
title: Download
description: Get Fastpotify for macOS, Windows, or Linux, with install instructions for each.
nav_order: 1
---

{% assign v = site.fastpotify_version %}
{% assign base = "https://github.com/crmne/fastpotify/releases/download/v" | append: v %}

The current version is **v{{ v }}**. Every packaged app and installer below is
listed in [SHA256SUMS]({{ base }}/SHA256SUMS). The release also includes an
[offline Sigstore attestation bundle]({{ base }}/fastpotify-v{{ v }}-attestation.json);
all versions live on the
[releases page](https://github.com/crmne/fastpotify/releases).

The release workflow verifies each build's checksum, consolidates the final
artifact digests into `SHA256SUMS`, and uses GitHub OIDC to attest exactly
those subjects. Before publication it checks the manifest again and compares
the uploaded assets' names, sizes, states, and GitHub-reported SHA-256 digests
with that attested set. You can calculate the SHA-256 of your download and
compare it with its line in `SHA256SUMS`; the JSON bundle makes the provenance
attestation available for offline verification tools.

## macOS

One download for both Apple Silicon and Intel:

- [fastpotify-v{{ v }}-macos-universal.dmg]({{ base }}/fastpotify-v{{ v }}-macos-universal.dmg)

Open it and drag **Fastpotify** to Applications. Or, with
[Homebrew](https://brew.sh):

```sh
brew install --cask crmne/tap/fastpotify
```

The published DMG is a mandatory output of the protected macOS signing job: the
app and disk image are Developer ID signed, the disk image is notarized by
Apple, and the notarization ticket is stapled and validated before the release
can be attested or published. Open the published app normally; these releases
do not require bypassing Gatekeeper or removing quarantine metadata.

### Locally built macOS bundles

`packaging/macos/bundle.sh` creates an ad-hoc-signed development bundle, not the
Developer ID signed and notarized GitHub release. If Gatekeeper blocks a local
bundle that you built and inspected yourself, remove quarantine recursively
from that exact bundle only (nested bundle files can carry the attribute):

```sh
xattr -dr com.apple.quarantine /path/to/Fastpotify.app
```

## Windows

The installer adds Fastpotify to the Start menu and needs no administrator
rights. Almost every PC wants the first one; the second is for Windows on
ARM:

- [fastpotify-v{{ v }}-x86_64-pc-windows-msvc-setup.exe]({{ base }}/fastpotify-v{{ v }}-x86_64-pc-windows-msvc-setup.exe)
- [fastpotify-v{{ v }}-aarch64-pc-windows-msvc-setup.exe]({{ base }}/fastpotify-v{{ v }}-aarch64-pc-windows-msvc-setup.exe)

If you would rather not install anything, the same program comes as a zip:
unpack it and run `fastpotify.exe`.

- [fastpotify-v{{ v }}-x86_64-pc-windows-msvc.zip]({{ base }}/fastpotify-v{{ v }}-x86_64-pc-windows-msvc.zip)
- [fastpotify-v{{ v }}-aarch64-pc-windows-msvc.zip]({{ base }}/fastpotify-v{{ v }}-aarch64-pc-windows-msvc.zip)

Either way, SmartScreen may warn about an unknown publisher on first run;
choose More info, then Run anyway.

## Linux

### Arch Linux

Fastpotify is in the AUR, with the desktop entry and icon installed for you:

```sh
yay -S fastpotify          # the released build
yay -S fastpotify-git      # built from the latest commit
```

### Flatpak

[FlatPark](https://flatpark.org/apps/rocks.fastpotify.Fastpotify) packages
each Linux release as a sandboxed Flatpak and follows every new version:

```sh
flatpak remote-add --if-not-exists flatpark https://dl.flatpark.org/flatpark.flatpakrepo
flatpak install flatpark rocks.fastpotify.Fastpotify
```

### Other distributions

- [fastpotify-v{{ v }}-x86_64-unknown-linux-gnu.tar.gz]({{ base }}/fastpotify-v{{ v }}-x86_64-unknown-linux-gnu.tar.gz)
- [fastpotify-v{{ v }}-aarch64-unknown-linux-gnu.tar.gz]({{ base }}/fastpotify-v{{ v }}-aarch64-unknown-linux-gnu.tar.gz)

Unpack, put `fastpotify` on your PATH, and copy the desktop entry and icon
from the bundled `packaging/` directory if you want it in your launcher.
Runtime needs are the ordinary desktop libraries: ALSA, PulseAudio or
PipeWire, and Wayland or X11.

Or build from source: see [Getting Started](/getting-started/).
