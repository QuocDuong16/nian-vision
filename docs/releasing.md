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
Forgejo versioned commit (X.Y.Z or X.Y.Z-rc.N)
-> Forgejo v<exact-source-version> tag
-> mirror pushes the same tag/object to GitHub
-> GitHub Actions validates the mirrored identity
-> exact-version platform builds
-> draft GitHub Release
-> asset verification
-> publish as prerelease/latest=false or final/latest
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

The release tag must be exactly `v<authoritative-source-version>`, including any SemVer prerelease suffix. All committed version surfaces must match:

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

Only jobs that need protected signing material, release verification canaries or
publication approval use the GitHub Environment `production-release`:

- `sign-linux`;
- `sign-windows`;
- `verify-release`; and
- `publish-release`.

`build-linux` and `build-windows` are deliberately outside this Environment. They
run dependency installation, frontend lifecycle scripts, FFmpeg compilation and
ordinary tests without protected signing material.

Configure these Environment secrets:

| Secret | Scope | Purpose |
|---|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | updater-signing step in `sign-linux` and `sign-windows` only | shared Tauri updater signing key |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | same updater-signing steps only | updater-key passphrase |
| `WINDOWS_SIGNING_PFX_BASE64` | Windows Authenticode steps only | optional production code-signing certificate/PFX bytes |
| `WINDOWS_SIGNING_PFX_PASSWORD` | Windows Authenticode steps only | optional PFX password |
| `NIAN_RELEASE_SECRET_SENTINEL` | staged/installed/final scan steps | optional binary-safe secret-leak canary |

Configure these non-secret repository/environment variables:

| Variable | Purpose |
|---|---|
| `NIAN_UPDATER_PUBLIC_KEY` | public verification key embedded in both platform builds and used by release verification |
| `WINDOWS_SIGNING_TIMESTAMP_URL` | HTTPS Authenticode timestamp authority when Windows signing is configured |
| `REQUIRE_WINDOWS_AUTHENTICODE` | set to `true` to reject public publication of an explicitly unsigned Windows candidate |

The private updater key/password never exist at workflow/job scope. Authenticode
material likewise appears only in the exact Windows signing steps. Signing jobs do
not run pnpm/Vite or frontend lifecycle scripts; they install the exact Tauri CLI
version through Cargo solely for bundling/signing. Generated Tauri release configs
contain only public updater configuration and resource/sidecar mapping.

The stable updater endpoint needs no secret:

```text
https://github.com/<owner>/<repo>/releases/latest/download/latest.json
```

`latest` resolves only the production release channel, so a draft is never advertised. An RC commit must itself use a prerelease source version such as `1.0.0-rc.1`, and its tag must be exactly `v1.0.0-rc.1`. That tag is published as a GitHub prerelease with `latest=false`; source version, tag, artifacts, updater metadata and release manifest therefore retain one identity. Every platform URL inside `latest.json` uses the exact tagged GitHub Release asset.

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

Current release topology keeps platform compilation independent and signing isolated:

```text
release-preflight
      |
      +------------------------+
      |                        |
      v                        v
build-linux                build-windows
      |                        |
      v                        v
sign-linux                 sign-windows
      |                        |
      +-----------+------------+
                  |
                  v
           verify-release
                  |
                  v
          publish-release
```

A required Linux or Windows failure blocks the release. Windows is not serialized
after Linux. Temporary GitHub Actions artifacts are transfer objects between these
jobs, not public release authority.

### Linux build and signing

`build-linux` runs on `ubuntu-24.04` with the actual compilation inside
`rust:1.98.0-bookworm`, preserving the accepted Debian 12/glibc 2.36 baseline. It
pins Node 26.7.0 and pnpm 11.22.0 and preserves the existing Linux M8 gates: exact
source/tag/version, SHA-256-pinned FFmpeg 8.0.3, LGPL/shared checks, media fixtures,
Rust fmt/clippy/workspace tests, clean staged worker smoke and actual AppImage
worker/desktop smoke. It produces an unsigned AppImage input and the staged runtime
without receiving private release secrets.

`sign-linux` runs separately in `production-release`. It scans the staged runtime
and built frontend with the configured sentinel, signs the exact AppImage with the
Tauri updater key, independently verifies the resulting signature with
`nian-release-verifier`, repeats the signed AppImage boundary smoke, creates the
Linux platform manifest fragment and scans the finalized Linux candidate. The
signing runner does not run pnpm or Vite.

### Windows build and signing

`build-windows` runs on the explicit GitHub-hosted `windows-2022` runner and targets
`x86_64-pc-windows-msvc`. Node 26.7.0, pnpm 11.22.0 and Rust 1.98.0 are pinned. The
job builds FFmpeg 8.0.3 from the same source archive/SHA used by Linux with
`--toolchain=msvc`, `--enable-shared`, `--disable-static`, `--disable-gpl` and
`--disable-nonfree`. Required MSVC import libraries are emitted by the FFmpeg MSVC
build and consumed through `NIAN_FFMPEG_LIB_DIR` only while compiling Rust.

The Windows stage contains `nian-media-worker.exe`, the FFmpeg DLL closure, any
required application-local Visual C++ redistributable DLLs and release evidence.
`dumpbin /dependents` recursively validates the worker, final desktop and staged DLLs.
Dependency classification is ordered: already-local files recurse first; VC runtime
names (`VCRUNTIME*`, `MSVCP*`, `CONCRT*`) resolve from `VCToolsRedistDir` and are copied
application-local before generic System32 detection; only then may Windows API-set or
actual OS dependencies remain system-provided. MSYS2, vcpkg, developer PATH and
repository build directories are not runtime authorities. The clean worker smoke
removes the FFmpeg development override and verifies HELLO, application version, ABI
62/62/60, fixture probe/playback and shutdown.

The unsigned desktop is then built with a public-only Tauri configuration. NSIS is
the canonical Windows bundle and WebView2 uses `downloadBootstrapper`. Runtime DLLs
are application-local beside the installed executable/worker, never global PATH or
System32 copies.

`sign-windows` runs separately in `production-release`. Optional Authenticode is
applied first to the exact desktop and worker bytes. The NSIS installer is then
built from those inputs, optionally Authenticode-signed and mechanically verified,
and finally receives the mandatory Tauri updater signature. This order matters: the
updater signature covers the exact final installer bytes that will be published.

When Authenticode credentials are unavailable the candidate is explicitly classified
`authenticode_signed: false`. Setting `REQUIRE_WINDOWS_AUTHENTICODE=true` makes final
verification reject that state. Tauri updater signing remains mandatory regardless
of Authenticode.

### Windows installed-runtime gates

The actual NSIS installer is silently installed into a disposable runner-local
directory. CI proves:

- the installed desktop, worker and complete application-local DLL closure match the
  exact signed/bundled input bytes;
- third-party notices, FFmpeg build/license evidence and `BUILD_METADATA.json` exist;
- the installed worker passes the clean media fixture smoke without system FFmpeg;
- the installed desktop reaches backend readiness and successfully registers the
  native `nian-platform-windows` power subscription;
- hard desktop termination causes the accepted Windows Job Object to reap the exact
  installed sibling media worker;
- direct NSIS reinstall while the installed desktop is running succeeds through the
  installer/Tauri app-running path; the owning desktop exits, its Job Object reaps the
  worker, no global worker-name kill is used, and the new desktop starts afterward;
- a fresh install leaves `launch_at_login=false`;
- authoritative camera settings, camera/PTZ/Event credential refs and ownership, Recording Desired, Event Desired/binding, notification preference, selected recording root and footage bytes survive reinstall/upgrade;
- M7 startup reconciliation repairs a deliberately stale Windows Run entry to the
  actual installed executable path; and
- silent uninstall removes application binaries and stale autostart registration
  without deleting settings SQLite or recording footage.

No physical camera and no downgrade migration are involved.

### Platform candidates and final verification

`sign-linux` and `sign-windows` each emit one platform candidate with a
`platform-manifest.json`. Platform jobs do **not** generate independent public
`latest.json` or checksum authorities.

`verify-release` requires both signed candidates and independently revalidates:

- mirrored tag/version/commit identity;
- matching FFmpeg 8.0.3 source version/SHA across platforms;
- Linux AppImage updater signature;
- Windows NSIS updater signature;
- Windows Authenticode classification/policy;
- platform artifact and runtime hashes;
- the exact tagged GitHub Release URLs;
- shared `RELEASE_NOTES.md`;
- the required asset set; and
- final secret-sentinel boundaries.

Only this stage assembles the public metadata. `latest.json` contains exactly the
Tauri updater keys `linux-x86_64` and `windows-x86_64`; `release-manifest.json`
contains both platform fragments plus the shared source/commit authority; and one
global `SHA256SUMS.txt` covers every public asset except itself. The verified
directory is then uploaded as `verified-release`.

## Draft-first GitHub publication

`publish-release` alone receives `contents: write`. It consumes only
`verified-release` and never platform build inputs directly. Publication ordering is:

```text
create draft release for existing mirrored tag
-> upload every Linux + Windows + shared asset
-> download every draft asset back from GitHub
-> compare the exact filename set and bytes
-> verify the global SHA256SUMS.txt again
-> publish final SemVer as latest, or publish prerelease SemVer with latest=false
```

An already-published release is never overwritten. A failed attempt may leave a
draft, which a retry can delete and recreate; publication is the terminal transition.
An RC is never promoted by moving its tag. If RC validation finds a blocker, create a new commit whose synchronized source version advances to the next prerelease (for example `1.0.0-rc.2`), run Forgejo CI again, and create a new immutable matching tag. After RC acceptance, create a minimal final version-only commit changing every authoritative surface from `1.0.0-rc.N` to `1.0.0`; run Forgejo CI and final review on that new commit, then create `v1.0.0`. Final artifacts are rebuilt from the final commit rather than reusing RC binaries.

Canonical public names include:

- `Nian-Vision_X.Y.Z_linux-x86_64.AppImage`;
- `Nian-Vision_X.Y.Z_linux-x86_64.AppImage.sig`;
- `Nian-Vision_X.Y.Z_windows-x86_64-setup.exe`;
- `Nian-Vision_X.Y.Z_windows-x86_64-setup.exe.sig`;
- `latest.json`;
- `release-manifest.json`;
- `SHA256SUMS.txt`;
- `RELEASE_NOTES.md`;
- `THIRD_PARTY_NOTICES.txt`; and
- platform FFmpeg build/license/provenance evidence.

Generic names such as `setup.exe` are not a public release contract.

## Updater behavior

Runtime verification remains Tauri-owned on both platforms:

```text
check/update selection
-> download the platform artifact
-> Tauri verifies the updater signature with the embedded public key
-> verified bytes exist
-> enter terminal M7 update lifecycle / close new admission
-> gracefully stop RecordingController
-> close playback sessions and pins
-> cancel/reap probe
-> stop tray/power workers
-> install verified update
-> Linux: app.restart() exactly once after AppImage install
-> Windows: NSIS handoff owns process exit/restart; no app.restart() is scheduled
-> persisted desired recording restores through normal M7 startup
```

Release-time signature verification catches signing-secret/public-key
misconfiguration before publication; it does not replace runtime verification.
Windows Authenticode is an independent publisher-identity layer and does not replace
the Tauri updater signature. `recording_enabled` is never cleared merely because an
update installs.

## Platform status

- **Linux x86_64 AppImage**: required v1 release target. The release workflow builds, signs, validates bundled runtime paths and performs headless package startup smoke; clean-machine/manual RC evidence remains required for final v1 acceptance.
- **Windows x86_64 NSIS**: required v1 release target on explicit `windows-2022`, including bundled FFmpeg, installed-package upgrade/uninstall smoke, updater signing and optional Authenticode. Clean-machine/manual RC evidence remains required for final v1 acceptance.
- **macOS**: outside v1.

## Local validation

```bash
pnpm release:test
cargo test -p nian-release-verifier
cargo fmt --all --check
cargo check --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
pnpm lint
pnpm typecheck
pnpm test
pnpm build
```

A full local Linux AppImage proof additionally needs Xvfb, D-Bus/FUSE helpers and a
disposable updater signing key. `docs/release-checklist.md` is the authority for the
manual/clean-machine RC evidence that automation cannot honestly manufacture. The authoritative Windows packaging proof requires a
real Windows/MSVC environment equivalent to the explicit `windows-2022` release
runner; a Linux cross-check cannot prove MSVC linking, NSIS behavior, WebView2
bootstrap, Authenticode or Windows loader/Job Object/power-event behavior. Never use
a disposable local key as the production updater trust root.
