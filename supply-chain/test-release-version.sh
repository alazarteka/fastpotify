#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=release-version.sh
source "$root/supply-chain/release-version.sh"

expect_metadata() {
    local tag="$1"
    local version="$2"
    local numeric="$3"
    local prerelease="$4"
    local latest="$5"
    release_version_parse "$tag"
    [ "$RELEASE_VERSION_TAG" = "$tag" ]
    [ "$RELEASE_VERSION" = "$version" ]
    [ "$RELEASE_NUMERIC_VERSION" = "$numeric" ]
    [ "$RELEASE_PRERELEASE" = "$prerelease" ]
    [ "$RELEASE_MAKE_LATEST" = "$latest" ]
}

expect_metadata v0.4.0 0.4.0 0.4.0 false true
expect_metadata v0.4.0-rc1 0.4.0-rc1 0.4.0 true false
expect_metadata v12.34.56-beta.2 12.34.56-beta.2 12.34.56 true false

for invalid in \
    0.4.0 v0.4 v0.4.0.1 v0.4.0- v0.4.0-rc_1 \
    v0.4.0--rc1 'v0.4.0 rc1' ' v0.4.0'; do
    if release_version_parse "$invalid"; then
        echo "accepted invalid release tag: $invalid" >&2
        exit 1
    fi
done

tmp="$(mktemp -d)"
cleanup() {
    rm -rf "$tmp"
}
trap cleanup EXIT INT TERM
printf '#!/usr/bin/env bash\nexit 0\n' > "$tmp/codesign"
chmod 755 "$tmp/codesign"
printf 'binary\n' > "$tmp/fastpotify"
PATH="$tmp:$PATH" bash "$root/packaging/macos/bundle.sh" \
    "$tmp/fastpotify" "$tmp/Fastpotify.app" 0.4.0-rc1 >/dev/null
grep -F '<key>CFBundleShortVersionString</key><string>0.4.0-rc1</string>' \
    "$tmp/Fastpotify.app/Contents/Info.plist" >/dev/null
grep -F '<key>CFBundleVersion</key><string>0.4.0</string>' \
    "$tmp/Fastpotify.app/Contents/Info.plist" >/dev/null

echo "release version contract passed"
