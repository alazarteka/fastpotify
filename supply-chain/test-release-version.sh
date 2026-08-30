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

fixtures="$root/supply-chain/release-version-fixtures.tsv"
while IFS=$'\t' read -r kind first second third fourth extra; do
    if [[ -z "$kind" || "$kind" == \#* ]]; then
        continue
    fi
    case "$kind" in
        valid)
            [ -z "${extra:-}" ]
            expect_metadata "$first" "${first#v}" "$second" "$third" "$fourth"
            ;;
        invalid)
            [ -z "${second:-}${third:-}${fourth:-}${extra:-}" ]
            if release_version_parse "$first"; then
                echo "accepted invalid release tag: $first" >&2
                exit 1
            fi
            ;;
        order)
            [ -n "$first" ] && [ -n "$second" ] && [ -n "$third" ]
            [ -z "${fourth:-}${extra:-}" ]
            release_version_parse "v$first"
            release_version_parse "v$second"
            ;;
        channel)
            [ -n "$first" ] && [ -n "$second" ]
            [ -z "${third:-}${fourth:-}${extra:-}" ]
            release_version_parse "v$first"
            case "$second" in
                stable) [ "$RELEASE_PRERELEASE" = false ] ;;
                prerelease) [ "$RELEASE_PRERELEASE" = true ] ;;
                *) echo "unknown release channel: $second" >&2; exit 1 ;;
            esac
            ;;
        *)
            echo "unknown release fixture kind: $kind" >&2
            exit 1
            ;;
    esac
done < "$fixtures"

manifest_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml" | head -n 1)"
release_version_parse "v$manifest_version"

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
grep -F '<key>CFBundleShortVersionString</key><string>0.4.0</string>' \
    "$tmp/Fastpotify.app/Contents/Info.plist" >/dev/null
grep -F '<key>CFBundleVersion</key><string>0.4.0</string>' \
    "$tmp/Fastpotify.app/Contents/Info.plist" >/dev/null
if grep -F '0.4.0-rc1' "$tmp/Fastpotify.app/Contents/Info.plist" >/dev/null; then
    echo "prerelease label leaked into numeric Apple bundle versions" >&2
    exit 1
fi

PATH="$tmp:$PATH" bash "$root/packaging/macos/bundle.sh" \
    "$tmp/fastpotify" "$tmp/Fastpotify-stable.app" 12.34.56 >/dev/null
grep -F '<key>CFBundleShortVersionString</key><string>12.34.56</string>' \
    "$tmp/Fastpotify-stable.app/Contents/Info.plist" >/dev/null
grep -F '<key>CFBundleVersion</key><string>12.34.56</string>' \
    "$tmp/Fastpotify-stable.app/Contents/Info.plist" >/dev/null

echo "release version contract passed"
