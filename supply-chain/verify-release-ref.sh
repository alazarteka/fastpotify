#!/usr/bin/env bash
# Verify that a release event names the same protected, GitHub-verified annotated
# tag locally and through the GitHub API. Later jobs pass EXPECTED_* to detect a
# tag deletion/replacement race before signing or publishing.
set -euo pipefail

: "${GITHUB_API_URL:?GITHUB_API_URL is required}"
: "${GITHUB_REF_NAME:?GITHUB_REF_NAME is required}"
: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
: "${GITHUB_SHA:?GITHUB_SHA is required}"
: "${GITHUB_TOKEN:?GITHUB_TOKEN is required}"

if [[ "${GITHUB_REF_TYPE:-}" != "tag" ]]; then
    echo "release verification requires a tag event" >&2
    exit 1
fi
if [[ "${GITHUB_REF_PROTECTED:-false}" != "true" ]]; then
    echo "release tag is not covered by a repository ruleset" >&2
    exit 1
fi
if [[ ! "$GITHUB_REF_NAME" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$ ]]; then
    echo "release tag is not a supported semantic version: $GITHUB_REF_NAME" >&2
    exit 1
fi

if [[ "$(git cat-file -t "$GITHUB_REF_NAME")" != "tag" ]]; then
    echo "release tag must be annotated, not lightweight" >&2
    exit 1
fi

tag_object="$(git rev-parse "$GITHUB_REF_NAME^{tag}")"
source_commit="$(git rev-parse "$GITHUB_REF_NAME^{}")"
if [[ "$source_commit" != "$GITHUB_SHA" ]]; then
    echo "tag target $source_commit does not match event commit $GITHUB_SHA" >&2
    exit 1
fi

api_headers=(
    --header "Accept: application/vnd.github+json"
    --header "Authorization: Bearer $GITHUB_TOKEN"
    --header "X-GitHub-Api-Version: 2022-11-28"
)
ref_json="$(curl --fail --silent --show-error --proto '=https' --tlsv1.2 \
    "${api_headers[@]}" \
    "$GITHUB_API_URL/repos/$GITHUB_REPOSITORY/git/ref/tags/$GITHUB_REF_NAME")"

api_type="$(jq -r '.object.type' <<< "$ref_json")"
api_tag_object="$(jq -r '.object.sha' <<< "$ref_json")"
if [[ "$api_type" != "tag" || "$api_tag_object" != "$tag_object" ]]; then
    echo "remote tag object changed or is not annotated" >&2
    exit 1
fi

tag_json="$(curl --fail --silent --show-error --proto '=https' --tlsv1.2 \
    "${api_headers[@]}" \
    "$GITHUB_API_URL/repos/$GITHUB_REPOSITORY/git/tags/$api_tag_object")"
api_commit="$(jq -r '.object.sha' <<< "$tag_json")"
verified="$(jq -r '.verification.verified' <<< "$tag_json")"
reason="$(jq -r '.verification.reason' <<< "$tag_json")"
if [[ "$api_commit" != "$source_commit" ]]; then
    echo "remote tag target changed from $source_commit to $api_commit" >&2
    exit 1
fi
if [[ "$verified" != "true" ]]; then
    echo "GitHub did not verify the tag signature (reason: $reason)" >&2
    exit 1
fi

if [[ -n "${EXPECTED_TAG_OBJECT:-}" && "$tag_object" != "$EXPECTED_TAG_OBJECT" ]]; then
    echo "tag object differs from the policy job" >&2
    exit 1
fi
if [[ -n "${EXPECTED_SOURCE_COMMIT:-}" && "$source_commit" != "$EXPECTED_SOURCE_COMMIT" ]]; then
    echo "source commit differs from the policy job" >&2
    exit 1
fi

version="${GITHUB_REF_NAME#v}"
manifest_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)"
if [[ "$manifest_version" != "$version" ]]; then
    echo "tag version $version does not match Cargo.toml $manifest_version" >&2
    exit 1
fi

source_date_epoch="$(git show -s --format=%ct "$source_commit")"
if [[ ! "$source_date_epoch" =~ ^[0-9]+$ ]]; then
    echo "could not derive SOURCE_DATE_EPOCH" >&2
    exit 1
fi

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    {
        printf 'tag_name=%s\n' "$GITHUB_REF_NAME"
        printf 'tag_object=%s\n' "$tag_object"
        printf 'source_commit=%s\n' "$source_commit"
        printf 'source_date_epoch=%s\n' "$source_date_epoch"
        printf 'version=%s\n' "$version"
    } >> "$GITHUB_OUTPUT"
fi

printf 'verified release %s: tag %s -> commit %s\n' \
    "$GITHUB_REF_NAME" "$tag_object" "$source_commit"
