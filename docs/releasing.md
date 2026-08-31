# Releasing Nian Vision

## Authority model

Nian Vision uses an explicit hybrid CI model:

```text
Forgejo
  -> authoritative source repository
  -> normal push/PR/quality CI
  -> self-hosted DIND resources

GitHub
  -> one-way mirror of Forgejo
  -> release CI only
  -> GitHub-hosted platform runners
  -> GitHub Releases for public binaries
```

GitHub is not a second development source of truth. Ordinary commits, pull-request
quality gates, version changes and release tags originate on Forgejo. No GitHub
release workflow pushes source changes back to Forgejo, edits source, bumps a
version or creates a tag.

The mirror must synchronize **tags as well as branches**. A release therefore flows:

```text
Forgejo commit
-> Forgejo vX.Y.Z tag
-> mirror pushes the same tag/object to GitHub
-> GitHub Actions validates the mirrored identity
-> release candidate builds
-> draft GitHub Release
-> asset verification
-> publish
```

## GitHub mirror and tag protection

Configure the GitHub mirror repository with repository variable
`RELEASE_MIRROR_ACTOR` set to the GitHub identity used by the Forgejo mirror. The
release preflight rejects tag events delivered by another actor.

Also configure a GitHub tag ruleset for `v*` outside the repository:

- only the release/mirror identity may create or update release tags;
- normal developers and automation must not create `v*` tags directly on GitHub;
- release tags should not be force-updated or deleted as part of normal operation.

The workflow additionally proves that `GITHUB_REF` is an actual `refs/tags/v*`,
checked-out `HEAD == GITHUB_SHA`, the dereferenced tag commit equals `GITHUB_SHA`,
and the release commit is reachable from the mirrored default branch. These checks
are defense in depth and do not replace the tag ruleset.

## Version and release-note contract

The `vX.Y.Z` tag must exactly match all committed version surfaces:

- `[workspace.package].version` in `Cargo.toml`;
- `apps/nian-desktop/tauri.conf.json`;
- root `package.json`; and
- `ui/package.json`.

`scripts/release/version-check.mjs` rejects malformed SemVer, version drift, tag
mismatch and dirty production source. Release CI never modifies tags or versions.

`RELEASE_NOTES.md` is the single release-notes source. The finalized copy is used
for both Tauri `latest.json` notes and the GitHub Release body, preventing separate
Forgejo/GitHub release-note histories. Update it in the authoritative Forgejo
release commit before creating the tag.

## GitHub production-release environment

The Linux signing job, verification job and final publication job use the GitHub
Environment `production-release`. Configure these Environment secrets:

| Secret | Scope | Purpose |
|---|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | Linux AppImage signing step only | long-lived Tauri updater signing key |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Linux AppImage signing step only | required key passphrase |
| `NIAN_UPDATER_PUBLIC_KEY` | release-config generation and signature-verification steps | public updater verification key |
| `NIAN_RELEASE_SECRET_SENTINEL` | staging/extracted/final scan steps | optional secret-leak canary |

The private updater key/password never exist at workflow/job scope and are not
available to checkout, dependency installation, frontend builds, tests, FFmpeg,
ordinary Cargo commands, staging, metadata generation, artifact transfer or
publication. Vite is built before signing secrets enter the environment, and the
release Tauri config disables `beforeBuildCommand` so signing cannot spawn Vite.

The current GitHub-hosted updater endpoint is deterministic and needs no secret:

```text
https://github.com/<owner>/<repo>/releases/latest/download/latest.json
```

`latest` resolves only published GitHub Releases, so a draft release is never
advertised to clients. If updater metadata later moves to a separate HTTPS static
host, add `NIAN_UPDATER_ENDPOINT` to the Environment and preserve the same
non-placeholder HTTPS validation. Artifact URLs in `latest.json` always use the
exact tagged GitHub Release URL.

Future Windows Authenticode material belongs in the same protected Environment,
but only once Windows packaging enters M8. Apple signing/notarization material is
out of current scope.

## GitHub Actions permissions and dependency pins

`.github/workflows/release.yml` defaults to:

```yaml
permissions:
  contents: read
```

Build and verification jobs receive no repository write permission. Only
`publish-release` overrides this with `contents: write`. The workflow does not
grant actions/packages/issues/pull-request write authority. Signing secrets are
not repository write authority.

Every third-party `uses:` action is pinned to an exact 40-character commit SHA
with a human-readable version comment. Structural release tests reject floating
action tags. GitHub Release publication uses the GitHub-hosted `gh` CLI rather
than another third-party release action.

## Build topology

Current topology:

```text
release-preflight
      |
      v
build-linux
      |
      v
verify-release
      |
      v
publish-release
```

When Windows packaging is implemented, `build-windows` becomes an independent
peer of `build-linux`, both feeding `verify-release`. Independent platform builds
should not be serialized without a reason.

### Linux build

`build-linux` runs on an explicit GitHub-hosted `ubuntu-24.04` runner with the
actual build inside `rust:1.98.0-bookworm`. The container preserves the accepted
Debian 12/glibc 2.36 release baseline rather than inheriting the host runner's
glibc. The job also pins Node 26.7.0 and pnpm 11.22.0.

It preserves all accepted Linux M8 gates:

1. exact source/tag/version validation;
2. SHA-256-pinned FFmpeg 8.0.3 source;
3. shared LGPL runtime with GPL/nonfree rejection;
4. media fixture integration against the release FFmpeg candidate;
5. Rust fmt/clippy/workspace tests;
6. release worker build and application-local FFmpeg closure;
7. clean worker HELLO/version/ABI 62/62/60 probe/playback smoke;
8. public-only Tauri release config;
9. private signing key/password injected only for AppImage signing;
10. post-build minisign-compatible public/private key verification;
11. extracted AppImage worker smoke and real desktop Xvfb/D-Bus startup smoke;
12. staged/extracted/final secret-canary scans;
13. finalized `latest.json`, release-manifest and SHA-256 consistency; and
14. upload of `linux-release-candidate` as a temporary GitHub Actions artifact.

Temporary Actions artifacts are build-transfer objects, not the public release.

### Verification job

`verify-release` downloads the Linux candidate and independently revalidates:

- exact tag/HEAD identity;
- application version;
- exact GitHub tagged asset URL;
- release-manifest commit/hash relationships;
- `latest.json` signature and release-notes consistency;
- every `SHA256SUMS.txt` entry;
- finalized AppImage signature using `NIAN_UPDATER_PUBLIC_KEY`; and
- final secret sentinel boundary.

Only this validated directory is uploaded as `verified-release` for publication.
Any required platform build or verification failure means there is no public
release.

## Draft-first GitHub publication

`publish-release` alone receives `contents: write`. It consumes only
`verified-release` and never source-build artifacts directly.

Publication ordering is:

```text
create draft release for existing mirrored tag
-> upload every finalized asset
-> download the draft assets back from GitHub
-> compare filenames and bytes to verified-release
-> verify SHA256SUMS again
-> publish the draft as the latest release
```

An already-published release is never overwritten. A failed attempt may leave a
draft, which a retry can delete and recreate. This remains compatible with GitHub
immutable releases because mutability is required only while the release is a
draft; verified publication is the terminal transition.

The expected public assets currently include:

- `Nian-Vision_X.Y.Z_linux-x86_64.AppImage`;
- matching `.AppImage.sig`;
- `latest.json`;
- `release-manifest.json`;
- `SHA256SUMS.txt`;
- `RELEASE_NOTES.md`;
- `THIRD_PARTY_NOTICES.txt`; and
- FFmpeg build/license/provenance files.

No generic `app.AppImage` or `setup.exe` filename is a public release contract.

## Updater behavior

Runtime verification remains Tauri-owned:

```text
check/update selection
-> download AppImage
-> Tauri verifies updater signature with embedded public key
-> verified bytes exist
-> enter terminal M7 update lifecycle / close new admission
-> gracefully stop RecordingController
-> close playback sessions and pins
-> cancel/reap probe
-> stop tray/power workers
-> install verified update
-> restart through normal startup
-> persisted desired recording restores
```

Release-time signature verification only catches signing-secret/public-key
misconfiguration before publication. It does not replace Tauri runtime
verification. `recording_enabled` is never cleared because an update installs.

## Platform status

- **Linux x86_64 AppImage**: current M8 CI-validated release target.
- **Windows x86_64**: next primary product release target. Packaging/signing
  validation is not implemented or marked tested yet. It will use an explicitly
  selected GitHub-hosted Windows runner such as `windows-2022`, not Forgejo DIND.
- **macOS**: distribution remains out of current scope.

The hybrid migration intentionally happens before Windows packaging so the next
M8 slice can use a real hosted Windows runner without weakening the already
validated Linux path.

## Local validation

```bash
pnpm release:test
cargo test -p nian-release-verifier
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
pnpm lint
pnpm typecheck
pnpm test
pnpm build
```

A full local AppImage proof additionally needs Linux desktop packaging
prerequisites (Xvfb, D-Bus, FUSE helper) and a disposable Tauri signing key. Never
substitute a local disposable key for the production updater trust root.
