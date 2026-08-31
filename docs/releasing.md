# Releasing Nian Vision on Linux

M8 currently supports one production release target: **Linux x86_64 AppImage**.
Windows and macOS distribution are intentionally deferred.

## Release contract

Production releases are tag-driven. The Forgejo `Release Linux` workflow requires
a `v<SemVer>` tag whose version exactly matches all committed version surfaces:

* `[workspace.package].version` in `Cargo.toml`;
* `apps/nian-desktop/tauri.conf.json`;
* root `package.json`; and
* `ui/package.json`.

`scripts/release/version-check.mjs` rejects malformed SemVer, version drift, tag
mismatch and a dirty source tree in production mode.

The release runner is Debian 12 with Rust 1.98.0, Node 26.7.0 and pnpm 11.22.0.
This deliberately keeps the Linux glibc baseline at Debian 12 rather than building
on a newer workstation and silently increasing runtime requirements.

## Required Forgejo secrets

Production release jobs require:

| Secret | Purpose |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | private Tauri updater signing key |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | required passphrase for the production updater signing key |
| `NIAN_UPDATER_PUBLIC_KEY` | public key embedded in the release build for updater verification |
| `NIAN_UPDATER_ENDPOINT` | HTTPS URL returning the updater metadata document |
| `NIAN_RELEASE_DOWNLOAD_BASE_URL` | HTTPS base URL used when generating artifact URLs in `latest.json` |
| `NIAN_RELEASE_SECRET_SENTINEL` | optional canary value used to prove release outputs do not contain injected secrets |

The workflow fails closed when mandatory signing/updater values are absent.
Release configuration generation also rejects HTTP, localhost and example-domain
updater authorities. Private signing material is never written into generated
metadata or application resources.

## Pipeline

`.forgejo/workflows/release.yml` executes the following sequence:

1. validate release tag/version/source cleanliness;
2. require production signing/updater configuration;
3. run release-script unit tests and frontend lint/typecheck/tests/build;
4. download and SHA-256 verify FFmpeg 8.0.3;
5. build an LGPL, shared-only FFmpeg runtime;
6. mechanically validate FFmpeg configuration;
7. run the real `nian-media-ffmpeg` fixture integration suite against that build;
8. run Rust fmt/clippy/workspace tests against the candidate FFmpeg ABI;
9. build the release media worker;
10. stage the worker, app-owned FFmpeg `.so` files, notices and build metadata;
11. run a clean staged-runtime smoke without development library overrides;
12. generate release-only Tauri configuration from secrets/environment;
13. build the AppImage and signed updater artifact;
14. extract the actual AppImage and smoke its installed layout;
15. emit updater metadata, release manifest and SHA-256 checksums; and
16. upload the resulting release directory as the CI artifact.

The clean runtime smoke verifies worker HELLO/application version, FFmpeg ABI
62/62/60, fixture `camera.probe` and fixture `playback.prepare`. Dynamic loader
checks ensure the worker resolves the bundled FFmpeg libraries from the staged
application runtime, not from system FFmpeg or a developer path.

## Updater behavior

The Settings screen exposes a manual Check for updates action. If an update is
available, the user must explicitly confirm installation. The application does
not stop recording merely to check for an update.

On installation, the updater package is downloaded and signature-verified first.
Only then does the desktop close new lifecycle admission and reuse the M7 graceful
shutdown path for recording, playback, probe and desktop-owned background runtime.
Persisted Desired recording intent remains authoritative and is never cleared by
the update handoff. Restart of the updated application therefore restores the
recording through the normal startup path.

## Release outputs

The finalized release directory contains the AppImage and updater signature plus:

* `latest.json` for Tauri updater discovery;
* `release-manifest.json` describing the release artifact set;
* `BUILD_METADATA.json` with non-secret build provenance;
* `SHA256SUMS.txt` covering finalized release outputs; and
* bundled FFmpeg license/build-notice evidence inside the application image.

Always verify `SHA256SUMS.txt` before publishing artifacts. Do not publish an
unsigned AppImage as an updater release and do not bypass a failed clean-runtime or
AppImage smoke test.

## Local validation

Release-independent checks can be run without production signing secrets:

```bash
pnpm release:test
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
pnpm lint
pnpm typecheck
pnpm test
pnpm build
```

The FFmpeg candidate/runtime staging scripts can also be run locally on a suitable
Linux x86_64 machine. Building the production updater AppImage additionally
requires the signing/updater environment listed above.
