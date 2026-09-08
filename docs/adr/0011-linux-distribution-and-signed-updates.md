# ADR-0011: Cross-platform desktop distribution and signed updater handoff

* Status: Accepted
* Milestone: M8

## Context

M0-M7 created a local-first NVR whose correctness depends on process isolation, an
exact FFmpeg ABI, durable recording publication and an explicit desktop lifecycle.
Distribution therefore cannot mean copying only the desktop executable: the media
worker and its FFmpeg runtime are part of the compatibility boundary, and replacing
binaries while recording must not bypass M7 teardown.

M8 first proved this release model end to end on Linux. After that Linux slice and
the Forgejo/GitHub hybrid release topology passed review, Windows x86_64 became the
second required M8 platform. macOS remains deliberately deferred.

## Decision

### Linux AppImage and Windows NSIS are the M8 release formats

Linux targets `x86_64-unknown-linux-gnu` and ships an AppImage built against the
accepted Debian 12/glibc baseline. Windows targets `x86_64-pc-windows-msvc` and ships
a canonical NSIS `.exe` built on the explicit GitHub-hosted `windows-2022` runner.
`windows-latest` is not the release toolchain identity. MSI may be added later; macOS
is not part of this decision.

AppImage is the Linux updater artifact. The NSIS installer is the Windows updater
artifact. Both use the same Tauri updater trust root and the accepted M7 terminal
update lifecycle.

### The application owns FFmpeg on both platforms

Both platform jobs build FFmpeg 8.0.3 from the exact same SHA-256-pinned upstream
source archive. Shared libraries are enabled; static, GPL and nonfree builds are
rejected mechanically. The enabled protocol/demux/mux/parser surface remains the
existing recording/recovery/probe/playback contract rather than expanding into
transcoding merely for packaging. No FFmpeg CLI runtime is shipped.

On Linux, the worker resolves `libavformat.so.62`, `libavcodec.so.62` and
`libavutil.so.60` from application-owned files under `/usr/lib/nian-vision`.
Pre-bundle staging uses `$ORIGIN/../lib/nian-vision`. Tauri/linuxdeploy may rewrite
the packaged worker RUNPATH to `$ORIGIN/../lib` and duplicate those SONAMEs into
legacy `/usr/lib`, so release normalization restores the exact private RUNPATH and
removes only byte-identical legacy copies before the final AppImage smoke contract.

On Windows, FFmpeg is configured with `--toolchain=msvc`. The build must emit the
MSVC import libraries `avformat.lib`, `avcodec.lib` and `avutil.lib` for the Rust
`x86_64-pc-windows-msvc` link, while runtime DLLs are application-local. Staging
recursively inspects `nian-media-worker.exe` and the FFmpeg DLLs with
`dumpbin /dependents`. Classification is deliberately ordered: already-local files
recurse first; `VCRUNTIME*`, `MSVCP*` and `CONCRT*` are treated as VC redistributables
and copied from `VCToolsRedistDir` before generic System32 detection; only then may
Windows API-sets or actual OS dependencies remain system-provided. MSYS2, vcpkg,
developer PATH, repository target directories and `NIAN_FFMPEG_LIB_DIR` are never
installed runtime authorities. Global PATH, System32 copies and COM registration are
not used.

### Desktop/worker application versions remain a release pair

IPC protocol compatibility alone is insufficient. Worker HELLO carries
`application_version`; packaged hosts reject a missing or mismatched application
version even when the NDJSON protocol version is otherwise compatible. Both clean
platform staging smokes also require FFmpeg ABI 62/62/60 before fixture probe and
playback preparation.

### Windows uses NSIS current-user installation and normal WebView2 bootstrap

The Windows Tauri release config is public-only and targets NSIS. The installer uses
current-user mode and Tauri's normal WebView2 `downloadBootstrapper` strategy rather
than skipping WebView2 or bundling a fixed runtime without need. The worker and
application-owned DLL closure are deliberately bundled beside the installed desktop
layout expected by the Windows loader. React never launches the worker.

The actual NSIS installer is smoke-tested in a disposable install root. Installed
desktop, worker and application-local DLL bytes must match the exact bundle inputs.
The installed worker repeats the clean HELLO/ABI/probe/playback/shutdown smoke. The
installed desktop must reach backend readiness, successfully register the real
`nian-platform-windows` suspend/resume notification source, and preserve the accepted
Windows Job Object containment: after hard desktop death the exact installed sibling
worker must be reaped. Direct reinstall is also exercised while the desktop and
owned worker are running; NSIS/Tauri handles the desktop instance, Job Object closure
reaps only its worker, and no global worker-process-name kill is part of the installer.
Window focus/minimize events are not substitutes for power events.

### Upgrade and uninstall preserve authoritative user state

Windows installer smoke creates an M7 settings fixture through the actual
`nian-settings` API and proves camera configuration, credential reference, persisted
`recording_enabled`, launch-at-login preference, user-selected recording root and
footage bytes survive reinstall/upgrade. A deliberately stale Run entry must be
repaired by M7 startup reconciliation to the installed executable path. Fresh install
keeps launch-at-login disabled.

Silent uninstall removes application binaries and stale OS autostart registration but
does not delete settings SQLite, native credentials or recordings. No downgrade
migration is introduced. The recording index remains rebuildable under the existing
M4 rules.

### Signed update verification precedes lifecycle teardown

`tauri-plugin-updater` owns runtime transport/signature verification in Rust. The
frontend has only narrow check/install commands. The host downloads and verifies the
platform updater artifact before changing lifecycle state. Only after verification
does it close new admission and reuse M7's graceful shutdown ownership. Persisted
desired recording intent is preserved; a failed installer handoff cannot leave the
application stranded indefinitely in Quitting. Linux performs exactly one explicit
`app.restart()` after successful AppImage installation. Windows does not schedule a
second restart after successful `Update::install`; the NSIS updater handoff owns the
process exit/restart sequence.

### Compilation and signing are separate trust domains

`build-linux` and `build-windows` are independent peers after release preflight.
They run dependency installation, frontend lifecycle code, FFmpeg compilation and
ordinary test suites without protected signing secrets. They emit unsigned candidate
inputs through temporary GitHub Actions artifacts.

`sign-linux` and `sign-windows` run in the protected `production-release` GitHub
Environment. They do not run pnpm/Vite lifecycle commands. The updater private key
and password appear only in the exact Tauri updater-signing steps. Release-only Tauri
configuration contains only public updater data and resource mappings.

Windows Authenticode is a separate trust layer from the Tauri updater signature. If a
PFX/password is configured, Windows SDK `signtool` signs and then mechanically
verifies `nian-desktop.exe`, `nian-media-worker.exe` and the final NSIS installer.
The installer is Authenticode-signed before its mandatory Tauri updater signature so
the updater signature covers the exact published bytes. If Authenticode credentials
are absent, the Windows manifest explicitly records `authenticode_signed: false`; a
release policy variable can require Authenticode and fail closed. No certificate,
password or private key is committed or written into build metadata.

### Forgejo remains source/quality authority; GitHub is release-only

Forgejo remains the authoritative source repository and normal push/PR/quality CI
platform. GitHub is a one-way mirror used for release CI on hosted platform runners
and for public GitHub Releases. Release tags originate on Forgejo and must mirror to
the same Git object. Preflight proves the tag ref, `GITHUB_SHA`, mirror actor,
default-branch reachability and tag/version convergence. GitHub release automation
never creates tags, changes versions or pushes source back to Forgejo.

Workflow permissions default to `contents: read`. Build, signing and verification
jobs have no repository write authority. Only `publish-release` receives
`contents: write`. Signing authority and publication authority are distinct trust
domains.

### Public updater metadata is assembled only after both platforms pass

Each signing job emits one platform release candidate and a platform manifest
fragment. Neither platform job produces an authoritative public `latest.json` or
checksum manifest.

`verify-release` requires both Linux and Windows candidates. It re-verifies the exact
AppImage and NSIS updater signatures, platform manifests, shared release notes,
version/commit identity, FFmpeg source authority, Authenticode classification/policy,
required assets and secret-canary boundaries. It then generates exactly one public
`latest.json` containing the Tauri keys `linux-x86_64` and `windows-x86_64`, one
multi-platform `release-manifest.json`, and one global `SHA256SUMS.txt` covering all
public assets except itself. Artifact URLs point to the exact tagged GitHub Release.

### Publication remains draft-first and fail-closed

The final publication job creates a draft GitHub Release for the already-mirrored tag,
uploads every verified Linux, Windows and shared asset, downloads all assets again,
compares the exact filename set and bytes, verifies the global checksum manifest and
only then publishes the draft as Latest. An already-published release is never
overwritten. Drafts are not visible through the stable
`releases/latest/download/latest.json` updater endpoint.

## Consequences

* Linux keeps its already accepted AppImage/runtime/security invariants.
* Windows users do not install FFmpeg separately and the application remains on the
  MSVC target.
* A release cannot pair desktop and worker from different application versions.
* A required failure on either Linux or Windows prevents public production release.
* Private signing material is isolated from dependency/frontend/media build runners.
* Authenticode can be introduced or required independently without weakening the
  mandatory Tauri updater signature.
* Windows CI is more expensive because it builds FFmpeg from source and exercises the
  actual NSIS install/upgrade/uninstall boundary.
* Windows implementation is not called validated until the hosted `windows-2022` tag
  release path completes successfully.
* macOS remains deferred; M9 is unaffected by this release decision.

## Rejected alternatives

* **Ship system FFmpeg dependencies**: rejected because ABI/configuration would be
  outside application control.
* **Download a third-party prebuilt Windows FFmpeg**: rejected because provenance,
  features and licensing would no longer match the accepted source authority.
* **Switch Windows Rust to GNU**: rejected because the application target remains
  `x86_64-pc-windows-msvc`; packaging must solve MSVC compatibility rather than move
  the product target.
* **Static FFmpeg**: rejected by the existing dynamic/LGPL strategy.
* **Global PATH/System32 DLL installation**: rejected in favor of application-local
  Windows loading.
* **Skip WebView2 installation**: rejected because the installer must remain usable
  on systems without a preinstalled runtime.
* **Treat Authenticode as the updater signature**: rejected; they have different trust
  purposes and updater signature verification remains mandatory.
* **Generate competing Linux and Windows `latest.json` files**: rejected because
  public metadata must describe one release revision across all required platforms.
* **Unsigned updater downloads**: rejected because TLS alone does not establish
  artifact authenticity.
* **Stop recording before download/verification**: rejected because update checks and
  failed downloads must not cause avoidable recording downtime.
