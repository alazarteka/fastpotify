# Supply-chain and release boundary

Fastpotify treats Rust compilation as arbitrary code execution. A dependency
build script, proc macro, native build helper, linker wrapper, or the
repository's own `build.rs` can read every value available to Cargo. Therefore
no Cargo command, Nix evaluation of this repository, project script, or binary
may run in an environment that contains Apple signing/notary material, Spotify
credentials, or a long-lived publishing token.

## Bootstrap status

This foundation was prepared by static inspection only. At bootstrap:

- `nix`, `cargo-deny`, `cargo-vet`, `actionlint`, and `shellcheck` were not
  installed on the review host;
- no Cargo metadata, dependency tool, Nix evaluation, build, test, proc macro,
  build script, or project binary was run;
- `supply-chain/audits.toml` intentionally contains no audits and
  `supply-chain/config.toml` contains no exemptions; and
- consequently, `cargo vet --locked` is expected to fail until reviewers add
  sufficient real audits, explicitly trusted imports, or narrowly justified
  exemptions.

That failure is the gate working as designed. An exemption records an explicit
risk acceptance; it is not an audit. Never describe exemptions produced by
`cargo vet init` or `cargo vet regenerate` as reviewed code.

## Differential static review

The pre-edit comparison used annotated tag `v0.1.3` (tag object
`4a7f5fe77d2ea0d0c16dc97fb2a0a6fe025888af`, commit
`d605cb03c0b847b85d4fc874f8559e6c9f20be0e`) and current fork commit
`7d9d29ddf8343c561bbf5f3586826ba6183dfe08`.

Relative to `v0.1.3`, the current branch had added `build.rs`, the Windows
`winresource` build dependency, 230 Cargo lockfile lines, a Nix flake, native
macOS/Windows integration dependencies, and an Inno Setup packaging step. The
release workflow otherwise preserved the baseline security design:

- all six Apple secrets were job-scoped before checkout, toolchain/cache
  Actions, and two Cargo builds;
- every job inherited `contents: write`;
- Actions used mutable major/channel references and checkout persisted a token;
- Cargo builds did not use `--locked` or `--frozen`;
- macOS used `codesign --deep`, a fixed keychain password, and no unconditional
  keychain cleanup; and
- artifacts were hashed only in the publishing job, with no pre-sign handoff
  digest, attestation, protected tag check, or tag-replacement check.

The added `flake.lock` did pin Nix inputs and `buildRustPackage` did use
`Cargo.lock`, but the release artifacts did not use that sandboxed path and the
flake did not expose dependency-policy tools or enforce a deterministic epoch.

## Safe gate

Run policy only on a disposable machine or hosted runner with no credentials in
its environment. CI uses a job with `contents: read`, checkout credentials
disabled, and no secrets. The locked Nix shell supplies the tools without a
separate `cargo install` graph:

```sh
nix flake metadata --no-write-lock-file
nix develop .#policy --no-write-lock-file --command bash supply-chain/check.sh
```

`check.sh` rejects known Apple, Spotify, GitHub, and Cargo token variables,
obtains Cargo metadata with `--locked`, gives that immutable graph to
cargo-deny, and runs
cargo-vet with `--locked` (which makes cargo-vet use frozen metadata and the
committed imports lock) while disabling live registry suggestions.
Cargo-deny's advisory database is intentionally a current network input; the
dependency graph it evaluates is the committed metadata. The script fails if
policy activity changes a lock or policy file.

The canonical sandboxed package path is:

```sh
nix build .#fastpotify --no-write-lock-file --option sandbox true
```

The flake fixes the Rust toolchain, Nix inputs, Cargo graph, and
`SOURCE_DATE_EPOCH`; `buildRustPackage` vendors the locked Cargo inputs before
the sandboxed build. Do not disable the Nix sandbox to make a build pass.

## Review order

Audit the current graph in this order because compromise has different reach:

1. Build-time execution: the repository `build.rs`, `winresource`, proc-macro
   crates (`proc-macro2`, `quote`, `syn`, and derive implementations), and
   native helpers such as `cc`. These run before the application and must meet
   cargo-vet's `safe-to-deploy` criterion.
2. Unsafe/native platform surface: `objc2*`, `windows-sys`,
   `security-framework`, `cpal`, `rodio`, `tray-icon`, `souvlaki`, `memmap2`,
   and their system-library edges.
3. Authentication, cryptography, and network input: `librespot-*`, `reqwest`,
   `rustls*`, `ring`, token persistence, and redirect/listener code. The direct
   `mdns-sd` LAN-provisioning stack has been removed; its reappearance is a
   regression. The locked graph currently contains two `rustls-webpki`
   versions, including the legacy line discussed in `Cargo.toml`; cargo-deny
   has no blanket advisory exception for it.
4. Remaining parsing, media, UI, and utility dependencies.

Imports from another cargo-vet project are direct trust decisions. Review the
publisher, URL, criteria mappings, and locked imported subset. Local audit
entries must identify a reviewer and the exact full version or delta reviewed.
Any exemption needs an owner, rationale, scope, and removal condition in its
notes.

## Release boundary

The release workflow separates five trust zones:

1. **Policy** validates a protected, GitHub-verified annotated tag, fixes the
   source commit and epoch, and runs dependency policy without secrets.
2. **Build** fetches only the locked Cargo graph, then compiles with `--frozen`.
   Build jobs have read-only permissions and no release secrets. Every output is
   hashed before upload. The macOS handoff contains an explicitly unsigned app.
3. **Signing/notarization** is gated by the `macos-signing` environment. It
   verifies the pre-sign hash and tag again, then exposes Apple values only to
   one shell step. That step runs no project script or Cargo command, signs
   nested code explicitly (never `--deep`), and deletes the temporary keychain
   and certificate both by trap and an `always()` cleanup step.
4. **Attestation** downloads only final release artifacts, verifies their
   per-job hashes, creates a consolidated SHA-256 manifest, and uses GitHub OIDC
   to attest exactly those checksums.
5. **Publishing** is separately gated by `release-publishing`, rechecks that the
   tag object has not changed, and is the only job with `contents: write`.

Before setting repository variable `FASTPOTIFY_RELEASE_POLICY` to `1`, an owner
must configure all of the following:

- a tag ruleset for `v*` that prevents update/deletion, restricts creation, and
  makes `GITHUB_REF_PROTECTED` true;
- signed annotated tags whose signature GitHub reports as verified;
- `macos-signing` with required reviewers, tag restrictions, and only the six
  Apple environment secrets used by the workflow;
- `release-publishing` with required reviewers and tag restrictions;
- immutable GitHub Releases for the repository; and
- default workflow token permissions of read-only.

The workflow intentionally refuses to release when the acknowledgement
variable, protected ref, verified signature, version match, cargo-deny policy,
or cargo-vet policy is missing.

## Reproducibility claims and limits

For a release, `SOURCE_DATE_EPOCH` is the tagged commit timestamp. Source,
Cargo, Rust, Nix, workflow Action code, and the pre-sign/final handoff digests
are fixed. Linux tar metadata is normalized, and Cargo executes offline after a
locked fetch.

This does **not** yet claim byte-for-byte reproducible release artifacts.
GitHub-hosted runner images, Ubuntu packages, Xcode/SDK tools, Visual Studio,
the bundled Inno Setup installation, GitHub artifact transport, Apple signing
timestamps, notarization, and DMG compression are outside the flake lock.
Developer-ID signatures and notarization are intentionally nondeterministic.
The Nix package is the stronger hermetic foundation; cross-platform release
packaging remains a documented boundary until those toolchains are pinned or
independently reproduced.

## Immutable Action pins

The following upstream refs were resolved with public `git ls-remote --refs`
on 2026-08-28. Workflow comments retain the human-readable release associated
with each immutable commit.

| Action | Verified release/ref | Commit |
| --- | --- | --- |
| `actions/checkout` | `v7.0.1` | `3d3c42e5aac5ba805825da76410c181273ba90b1` |
| `dtolnay/rust-toolchain` | `v1` | `6c977a6ca4077a0ceb28ffbe03f59d46e9ac8772` |
| `cachix/install-nix-action` | `v31.10.2` | `51f3067b56fe8ae331890c77d4e454f6d60615ff` |
| `actions/upload-artifact` | `v7.0.1` | `043fb46d1a93c77aae656e7c1c64a875d1fc6a0a` |
| `actions/download-artifact` | `v7.0.0` | `37930b1c2abaa49bbe596cd826c3c89aef350131` |
| `actions/attest` | `v4.0.0` | `c32b4b8b198b65d0bd9d63490e847ff7b53989d4` |
| `softprops/action-gh-release` | `v2.5.0` | `a06a81a03ee405af7f2048a818ed3f03bbf83c7b` |
| `actions/configure-pages` | `v6.0.0` | `45bfe0192ca1faeb007ade9deae92b16b8254a0d` |
| `ruby/setup-ruby` | `v1.321.0` | `95ef2b042f9d7a56d8268cba8559e2842e2ad01b` |
| `actions/upload-pages-artifact` | `v5.0.0` | `fc324d3547104276b827a68afc52ff2a11cc49c9` |
| `actions/deploy-pages` | `v5.0.0` | `cd2ce8fcbc39b97be8ca5fce6e763baed58fa128` |

Updating a pin requires reviewing the upstream release notes and bundled Action
diff, resolving the exact release tag again, and updating this table in the
same change. A moving major tag, branch, or channel is not an acceptable pin.
