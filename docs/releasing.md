# Releasing Nian Vision on Linux

M8 currently has one CI-validated production release target: **Linux x86_64
AppImage**. Windows x86_64 remains the primary future desktop product target, but
its packaging/signing validation is explicitly deferred until a Forgejo Windows
runner is available. macOS distribution is also deferred.

## Release contract

Production releases are tag-driven. The Forgejo `Release Linux` workflow requires
a `v<SemVer>` tag whose version exactly matches all committed version surfaces:

* `[workspace.package].version` in `Cargo.toml`;
* `apps/nian-desktop/tauri.conf.json`;
* root `package.json`; and
* `ui/package.json`.

`scripts/release/version-check.mjs` rejects malformed SemVer, version drift, tag
mismatch and a dirty source tree in production mode. Release CI never creates or
modifies tags.

The release runner is Debian 12 with Rust 1.98.0, Node 26.7.0 and pnpm 11.22.0.
This deliberately constrains the Linux glibc baseline rather than silently
inheriting requirements from a newer developer workstation.

## Release secrets and scope

The release job has **no release secrets at job scope**. Each value is injected
only into the step that consumes it:

| Forgejo secret | Scope | Purpose |
|---|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | signed AppImage build step only | long-lived Tauri updater signing key |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | signed AppImage build step only | required passphrase for that key |
| `NIAN_UPDATER_PUBLIC_KEY` | release-config generation and post-build signature verification | public key embedded in the application and used to verify the generated AppImage |
| `NIAN_UPDATER_ENDPOINT` | release-config generation only | HTTPS URL returning updater metadata |
| `NIAN_RELEASE_DOWNLOAD_BASE_URL` | finalization only | HTTPS base URL written into finalized `latest.json` |
| `NIAN_RELEASE_SECRET_SENTINEL` | staging, extracted-AppImage smoke and final artifact scan | optional canary used to detect secret leakage |

The frontend is fully built **before** private updater signing material enters the
environment. The generated Tauri release config explicitly sets
`beforeBuildCommand` to an empty command, so the signed Tauri bundle step does not
spawn Vite while `TAURI_SIGNING_*` exists. Vite keeps its default `VITE_*` exposure
model; no broad `TAURI_*` `envPrefix` is configured.

`apps/nian-desktop/tauri.release.generated.conf.json` contains only public updater
configuration plus bundle/resource mapping. It never receives or serializes the
private signing key/password, is gitignored, and is removed after cryptographic
post-build verification in successful CI. A failed job still runs in an ephemeral
CI workspace, so the non-secret generated config is not a persistence boundary.

Production updater/download authorities must use HTTPS and must not be localhost,
loopback, RFC example/invalid/test placeholder hosts, or equivalent subdomains.
The metadata endpoint and artifact/CDN host may intentionally be different.

## Release security gates

All production `uses:` actions in `.forgejo/workflows/release.yml` are pinned to
immutable commit SHAs. Mutable action tags are not accepted by the release workflow.

Tauri signs the generated AppImage, then `nian-release-verifier` independently
verifies the **exact AppImage bytes** against the generated `.AppImage.sig` and the
public key stored in the generated Tauri config. The verifier uses the same
`base64` + `minisign-verify` representation used by `tauri-plugin-updater`; it is a
misconfiguration defense, not a replacement for Tauri runtime verification.
Release tests include fixed offline vectors proving:

* key A signature + public key A succeeds;
* key A signature + public key B fails;
* a mutated artifact fails; and
* a mutated signature fails.

The configured secret sentinel is scanned at three boundaries: staged runtime,
actual extracted AppImage tree, and finalized release output. The final scan also
covers built frontend assets. The scanner is binary-safe and reports only the file
path on failure, never the sentinel value.

## Pipeline

`.forgejo/workflows/release.yml` executes the following sequence:

1. validate release tag/version/source cleanliness;
2. install pinned toolchain/runtime prerequisites;
3. run release-script, workflow-structure and updater-verifier tests;
4. run frontend lint/typecheck/tests and build final frontend assets without signing secrets;
5. download and SHA-256 verify FFmpeg 8.0.3;
6. build the minimal LGPL, shared-only FFmpeg runtime;
7. mechanically validate FFmpeg GPL/nonfree/shared configuration;
8. run the real `nian-media-ffmpeg` fixture integration suite against that build;
9. run Rust fmt/clippy/workspace tests against the candidate FFmpeg ABI;
10. build the release media worker;
11. stage worker, app-owned FFmpeg libraries, notices and build metadata;
12. run clean staged worker/media smoke and the staging secret scan;
13. generate public-only release Tauri configuration;
14. inject the private updater key/password only for the signed AppImage build;
15. cryptographically verify the generated AppImage/signature against the configured public key;
16. dispose the generated Tauri release config;
17. extract the actual AppImage and repeat installed worker/media checks;
18. launch the **actual AppImage** under isolated Xvfb + D-Bus, wait for the backend startup-ready marker, prove a bounded post-ready stability interval, then terminate the smoke session;
19. scan the extracted application tree for the configured secret sentinel;
20. finalize `latest.json`, release manifest and SHA-256 manifest;
21. validate finalized version/platform/URL/signature/hash relationships;
22. verify `SHA256SUMS.txt` against actual finalized bytes;
23. scan frontend assets and the complete finalized release directory for the secret sentinel; and
24. upload the finalized directory as a Forgejo CI artifact.

The worker checks remain unchanged: application/protocol HELLO, exact application
version, FFmpeg ABI 62/62/60, fixture `camera.probe`, fixture `playback.prepare`,
application-local RUNPATH and no dependency on `NIAN_FFMPEG_LIB_DIR` or
`LD_LIBRARY_PATH`.

## Updater behavior

The Settings screen exposes a manual Check for updates action. Checking does not
stop recording. Installation requires explicit confirmation and re-checks the
expected version.

Runtime ordering remains:

```text
check/update selection
-> download AppImage
-> Tauri verifies updater signature with the embedded public key
-> verified bytes exist
-> enter terminal update lifecycle / close new admission
-> gracefully stop RecordingController
-> close playback sessions and pins
-> cancel/reap probe
-> stop tray/power workers
-> install verified update
-> restart through normal startup
-> persisted desired recording restores
```

Release-time signature verification only catches a misconfigured public/private
key pair before publication. It does **not** replace Tauri's built-in runtime
verification, and updater installation never begins while M7 teardown is incomplete.
`recording_enabled` is never cleared merely because an update is installed.

## Release outputs and metadata consistency

The finalized release directory contains:

* `Nian-Vision_<version>_linux-x86_64.AppImage`;
* the matching `.AppImage.sig` Tauri v2 updater signature;
* `latest.json`;
* `release-manifest.json`;
* `BUILD_METADATA.json`;
* `SHA256SUMS.txt`; and
* FFmpeg license/build-notice evidence.

`validate-release-output.mjs` proves that `latest.json` version/platform/URL/signature,
release-manifest filenames/hashes and `SHA256SUMS.txt` all describe the exact
finalized AppImage and signature. No updater metadata may point back to Tauri's
pre-finalization filename.

## Publication is a separate deployment step

The Forgejo workflow **generates, validates and uploads a CI artifact**. That
artifact upload does not update the production updater endpoint and must not be
described as publication.

A deployment process must publish only a release directory that passed every gate
above. Publish immutable payloads first (AppImage, `.sig`, checksums/manifests),
then make `latest.json` visible **last** so the production updater endpoint never
advertises an artifact before its bytes/signature are available. A metadata/CDN
split is valid as long as both authorities remain HTTPS.

Rollback normally means publishing a newer fixed release; the updater does not
silently downgrade clients.

## Windows status

* **Linux x86_64 AppImage**: current M8 CI-validated release target.
* **Windows x86_64**: primary future product target; application/runtime source
  contracts remain intact, but installer/build/signing validation is deferred
  until an appropriate Forgejo Windows runner exists. Do not mark Windows release
  packaging or signing as tested yet.
* **macOS**: distribution remains out of current scope.

The lack of a Windows runner is an infrastructure constraint, not a Linux M8
implementation failure, and no Windows-first runtime architecture should be
removed because release CI is Linux-only today.

## Local validation

Release-independent checks can be run without production signing secrets:

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

A full local AppImage proof additionally needs Linux desktop packaging prerequisites
(Xvfb, D-Bus, FUSE helper) and a disposable Tauri signing key. Never substitute a
local disposable key for the production updater trust root.
