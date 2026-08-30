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
    # The numeric core shares Apple's 4/2/2-digit CFBundleVersion limit.
    local pattern='^v(0|[1-9][0-9]{0,3})\.(0|[1-9][0-9]?)\.(0|[1-9][0-9]?)(-[0-9A-Za-z]([0-9A-Za-z-]*[0-9A-Za-z])?(\.[0-9A-Za-z]([0-9A-Za-z-]*[0-9A-Za-z])?)*)?$'
    if [ "${#tag}" -gt 65 ] || [[ ! "$tag" =~ $pattern ]]; then
        return 1
    fi

    local version="${tag#v}"
    local numeric="${version%%-*}"
    if [[ "$version" == *-* ]]; then
        local suffix="${version#*-}"
        local identifier
        local -a identifiers
        IFS='.' read -r -a identifiers <<< "$suffix"
        for identifier in "${identifiers[@]}"; do
            if [[ "$identifier" =~ ^[0-9]+$ ]] &&
                [ "${#identifier}" -gt 1 ] && [[ "$identifier" == 0* ]]; then
                return 1
            fi
            if [[ "$identifier" =~ ^rc([0-9]+)$ ]]; then
                local rc_number="${BASH_REMATCH[1]}"
                if [ "${#rc_number}" -gt 1 ] && [[ "$rc_number" == 0* ]]; then
                    return 1
                fi
            fi
        done
        RELEASE_PRERELEASE=true
        RELEASE_MAKE_LATEST=false
    else
        RELEASE_PRERELEASE=false
        RELEASE_MAKE_LATEST=true
    fi
    RELEASE_VERSION_TAG="$tag"
    RELEASE_VERSION="$version"
    RELEASE_NUMERIC_VERSION="$numeric"
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
