#!/usr/bin/env bash
# One release-tag contract for policy, packaging, and GitHub metadata.

release_version_parse() {
    RELEASE_VERSION_TAG=
    RELEASE_VERSION=
    RELEASE_NUMERIC_VERSION=
    RELEASE_PRERELEASE=
    RELEASE_MAKE_LATEST=
    if [ "$#" -ne 1 ]; then
        return 1
    fi

    local tag="$1"
    if [ "${#tag}" -gt 65 ] ||
        [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$ ]]; then
        return 1
    fi

    RELEASE_VERSION_TAG="$tag"
    RELEASE_VERSION="${tag#v}"
    RELEASE_NUMERIC_VERSION="${RELEASE_VERSION%%-*}"
    if [[ "$RELEASE_VERSION" == *-* ]]; then
        RELEASE_PRERELEASE=true
        RELEASE_MAKE_LATEST=false
    else
        RELEASE_PRERELEASE=false
        RELEASE_MAKE_LATEST=true
    fi
}

release_version_emit() {
    release_version_parse "$1" || {
        printf 'unsupported release tag: %s\n' "$1" >&2
        return 1
    }
    printf 'tag_name=%s\n' "$RELEASE_VERSION_TAG"
    printf 'version=%s\n' "$RELEASE_VERSION"
    printf 'numeric_version=%s\n' "$RELEASE_NUMERIC_VERSION"
    printf 'prerelease=%s\n' "$RELEASE_PRERELEASE"
    printf 'make_latest=%s\n' "$RELEASE_MAKE_LATEST"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    if [ "$#" -ne 1 ]; then
        echo "usage: $0 vMAJOR.MINOR.PATCH[-PRERELEASE]" >&2
        exit 2
    fi
    release_version_emit "$1"
fi
