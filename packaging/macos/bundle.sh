#!/bin/bash
# Build Fastpotify.app from a GUI binary, on a macOS machine.
#
#   packaging/macos/bundle.sh <binary> <output.app> <version>
#
# Set CODESIGN_IDENTITY to sign with a Developer ID; the default is an ad-hoc
# signature, which arm64 requires before the app will launch at all.
#
# The committed .icns is generated from icon-1024.png; its 1024px representation
# is pixel-identical to that source image. Keeping it beside the source makes
# local packaging independent of iconutil, which is broken on some macOS
# releases. The Info.plist template lives next to this script.
set -euo pipefail

binary="$1"
app="$2"
version="$3"
here="$(cd "$(dirname "$0")" && pwd)"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

cp "$binary" "$app/Contents/MacOS/fastpotify"
chmod 755 "$app/Contents/MacOS/fastpotify"
sed "s/__VERSION__/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"

cp "$here/fastpotify.icns" "$app/Contents/Resources/fastpotify.icns"

# arm64 refuses to launch an unsigned bundle, so sign one way or another.
if [ -n "${CODESIGN_IDENTITY:-}" ]; then
    codesign --force --timestamp --options runtime \
        --sign "$CODESIGN_IDENTITY" "$app"
else
    codesign --force --sign - "$app"
fi
codesign --verify --strict "$app"

echo "$app"
