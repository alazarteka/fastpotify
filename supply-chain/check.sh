#!/usr/bin/env bash
# Run dependency policy in a disposable environment that contains no signing,
# Spotify, or release credentials. CI enters the locked Nix policy shell first.
set -euo pipefail

for secret_name in \
    APPLE_CERTIFICATE_P12 \
    APPLE_CERTIFICATE_PASSWORD \
    APPLE_SIGNING_IDENTITY \
    APPLE_ID \
    APPLE_TEAM_ID \
    APPLE_APP_PASSWORD \
    CARGO_REGISTRIES_CRATES_IO_TOKEN \
    GH_TOKEN \
    GITHUB_TOKEN \
    SPOTIFY_CLIENT_ID \
    SPOTIFY_CLIENT_SECRET; do
    if [[ -v "$secret_name" ]]; then
        echo "refusing dependency policy with $secret_name in the environment" >&2
        exit 1
    fi
done

metadata="$(mktemp)"
cleanup() {
    rm -f "$metadata"
}
trap cleanup EXIT INT TERM

# Cargo metadata resolves but does not compile the crate graph. Its output pins
# cargo-deny to Cargo.lock while still allowing a current RustSec database.
cargo metadata --locked --all-features --format-version 1 > "$metadata"
cargo deny --metadata-path "$metadata" check

# --locked makes cargo-vet use frozen Cargo metadata and the committed import
# lock. With no exemptions, an unaudited crate is a hard failure.
cargo vet --locked --no-registry-suggestions

# Policy checks must be observational. Catch tracked changes and newly-created
# store files, not only changes visible to `git diff`.
policy_changes="$(
    git status --porcelain --untracked-files=all -- \
        Cargo.lock flake.lock deny.toml supply-chain
)"
if [[ -n "$policy_changes" ]]; then
    printf 'dependency policy changed committed inputs:\n%s\n' "$policy_changes" >&2
    exit 1
fi
