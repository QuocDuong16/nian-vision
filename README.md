# Nian Vision

Local-first desktop NVR (network video recorder) for IP cameras. The first
supported camera is the TP-Link Tapo C200 over RTSP, with a camera-agnostic
domain so other RTSP/ONVIF cameras can follow.

**Status**: milestone **M9 (simultaneous multi-camera recording)** is implemented on top
of M0–M8. The desktop persists camera definitions, recorder/storage settings,
launch-at-login preference and independent per-camera desired recording intent in
authoritative platform app-data `settings.sqlite3`, while camera passwords remain
in the operating system credential store. The M4 recording catalog at
`<storage_root>/.nian/recordings.sqlite3` remains disposable and rebuildable from
footage. Users can add/edit/delete saved RTSP cameras, test a connection through
the media worker, start/stop multiple cameras independently, and observe Desired
and Runtime recording state per camera. M6 provides recording-day/range queries,
filesystem-revalidated normal/recovered playback, lazy duration enrichment,
seekable packet-copy H.264/AAC fragmented-MP4 playback over tokenized loopback
HTTP, and retention playback pins. M7 adds single-instance activation,
close-to-tray, explicit coordinated Quit, launch-at-login with hidden startup,
persisted recording restoration, Windows suspend/resume handling, and Windows
Job Object containment so hard desktop termination cannot orphan media workers.
M8 provides validated Linux x86_64 AppImage and Windows x86_64 NSIS release paths
with the same pinned LGPL FFmpeg 8.0.3 source authority, signed Tauri updater trust
root, deterministic lifecycle handoff, provenance/checksums and installed-runtime
smoke. M9 replaces the one-slot recording controller with independent per-camera
slots: one WorkerSupervisor, media-worker process and CameraLease per active camera,
with a conservative eight-recording safety cap. Startup, suspend/resume, Quit and
update teardown operate over all owned slots without clearing desired intent.
macOS distribution remains deferred. Live camera viewing, ONVIF discovery, motion
analysis, AI, cloud and remote streaming remain outside M9.

## What it does today

* Tauri 2 desktop application (React/TypeScript/Vite UI) with managed M9 state:
  camera CRUD, pre-save connection testing, per-camera Start/Stop controls, typed recording
  status, delete confirmation, persisted storage/retention settings, recording-day
  navigation, a gap-aware timeline, native video playback controls and adjacent
  recording navigation, tray controls and launch-at-login preference. Recording
  cards show persisted Desired state independently from transient Runtime state and
  a small aggregate summary without manufacturing a fake global RecordingState.
  Closing the main window hides it; explicit Quit owns backend teardown. The
  webview never receives an absolute recording path;
  privileged work flows through narrow Tauri commands.
* Playback sessions bind only an ephemeral `127.0.0.1` port. An unguessable
  per-session token maps to one already-validated finalized recording; HTTP Range
  requests provide browser seeking without a directory listing, arbitrary path
  parameter or LAN listener.
* Authoritative application settings (`nian-settings`) live in the platform
  app-data directory, separate from recording storage. Camera credentials live
  in the native OS credential store behind `CredentialStore`; neither SQLite
  database contains passwords.
* An isolated `nian-media-worker` process that talks FFmpeg through FFI:
  * `nian-media-worker probe <file|credential-free-rtsp-url>` prints stream
    information (codec, resolution, duration);
  * `nian-media-worker run` serves a versioned NDJSON IPC protocol on
    stdin/stdout with the `recording.*` namespace (one supervised job per
    worker; start/stop/status plus typed reconnect events) and bounded
    `camera.probe` source-only connection tests. M6 adds `playback.prepare`, which
    inspects a host-validated finalized MKV and packet-copy remuxes supported H.264
    plus AAC into fragmented MP4 under the application playback cache;
  * `nian-media-worker record --storage <DIR> --camera <ID> ...` is the
    manual smoke path with explicit stop modes (`--duration N`,
    `--until-stdin-eof`, or Ctrl+C-only) recording from
    `NIAN_VISION_RTSP_URL` or a file into rotating `.mkv` segments
    (development only; never in CI).
* A real recording pipeline (`nian-recorder`): packets are copied
  faithfully (side data and flags preserved), segments start on video
  keyframes, rotation waits for keyframes after the media-time target,
  finalization is durable before the no-replace publish, and failed or
  empty segments stay recoverable partials — never fake recordings.
* Resilience: deadlines bound every blocking FFmpeg operation (RAII-scoped
  so they can never leak into unrelated operations), failures are
  classified centrally into typed categories (retryable = source-side
  only), partial recovery proves readability through the real demuxer, and
  `nian-application`'s `WorkerSupervisor` restarts crashed workers without
  ever putting credentials in argv.
* Storage/index management (M4): `nian-storage` owns canonical filesystem
  inventory and recovery-transaction facts, `nian-index` owns bundled
  SQLite persistence/migrations, and `nian-application::StorageManager`
  orchestrates reconciliation, rebuild/query seams and age + high/low
  watermark retention. Retention reports quota trigger/usage/target status;
  sub-second finalization events are normalized to the same whole-second local
  identity encoded by filenames. SQLite failures never roll back a published
  media file.
* Playback/timeline (M6): timeline queries stay inside `nian-index`, while every
  open and every new media HTTP request revalidates the filesystem-derived
  recording identity. Recovered finals are first-class entries; unknown duration
  stays unknown until worker inspection. Playback holds a read handle plus a
  `PlaybackPin`; retention skips pinned finals and rechecks the pin immediately
  before deletion, independently of `CameraLease`.
* Clean crate boundaries with `unsafe` confined to the FFmpeg layers plus audited
  Windows-only platform boundaries: storage no-replace publication and the M7
  `nian-platform-windows` power-notification/Job-Object wrapper. Application and
  desktop orchestration remain safe Rust. The workspace keeps a race-safe storage
  layout, credential redaction, and a full quality-gate setup (fmt/clippy/tests,
  ESLint/tsc/vitest/vite build).

## Architecture

```text
React UI ─ typed Tauri commands ─ nian-desktop host
                 │                  │
                 │                  ├─ platform app-data/settings.sqlite3
                 │                  ├─ native OS credential store
                 │                  ├─ tray/autostart/power + worker containment
                 │                  ├─ loopback HTTP playback (127.0.0.1 only)
                 │                  │ NDJSON control IPC (stdio)
                 │              nian-media-worker
                                │ FFmpeg FFI (dynamic, LGPL)
                            RTSP cameras
```

See `docs/architecture.md` and `docs/adr/` for the decisions behind this
layout (process isolation, FFmpeg strategy, container choice, storage
model, authoritative-settings/native-secret split, M6 playback transport, M7
desktop lifecycle/worker containment, M8 distribution/updater design and M9
per-camera recording coordination).

## Requirements

* Rust 1.98.0 (pinned in `rust-toolchain.toml`; mise users get it from
  `.mise.toml`)
* Node 26.7.0 + pnpm 11.22.0 (exact pins, mirrored by CI)
* FFmpeg 8.x runtime libraries (ABI 62). Development packages are optional —
  see `docs/ffmpeg.md` for the no-`-dev` setup (`scripts/setup-ffmpeg-linux.sh`).
* Linux desktop shell builds additionally need the Tauri Linux prerequisites
  (WebKit2GTK/GTK dev packages).

## Development

```bash
mise install
pnpm install
scripts/setup-ffmpeg-linux.sh   # if FFmpeg -dev packages are absent
cargo test --workspace
pnpm --filter nian-ui test
```

Full commands and conventions: `docs/development.md`.

## Desktop releases

Linux x86_64 ships as an AppImage built against the accepted Debian 12 baseline.
Windows x86_64 is packaged as an NSIS current-user installer on `windows-2022`
using `x86_64-pc-windows-msvc`. Both build FFmpeg 8.0.3 from the same SHA-256-pinned
upstream archive and bundle the media worker plus application-owned FFmpeg runtime.
Windows uses Tauri's normal WebView2 `downloadBootstrapper` policy; users do not
need a separate FFmpeg installation and the installer does not modify global PATH.

Platform build jobs produce unsigned candidates without protected signing secrets.
Separate `sign-linux` and `sign-windows` jobs in the protected
`production-release` environment apply the shared Tauri updater trust root. The
Windows signing boundary can additionally apply and mechanically verify
Authenticode for the desktop executable, media worker and final NSIS installer. If
credentials are absent the candidate is explicitly classified unsigned, and a
repository policy switch can require Authenticode and fail closed.

Forgejo remains the authoritative source repository and normal push/PR/quality CI
platform. GitHub is a one-way mirror used only for hosted release CI and public
GitHub Releases. Release tags originate on Forgejo and the mirror must synchronize
tags as well as branches. One verification stage assembles Linux and Windows into a
single `latest.json`, multi-platform release manifest and global `SHA256SUMS.txt`.
The existing draft-first GitHub Release flow then uploads, re-downloads and
byte-verifies every public asset before publication. See `docs/releasing.md` and
ADR-0011. macOS packaging remains deferred.

## Supported camera protocols

* RTSP (H.264) — implemented at the media layer, manual smoke test via
  `NIAN_VISION_RTSP_URL` (credentials never go on the command line).
* ONVIF — planned for M10, after RTSP recording is mature.

## License notes

* Nian Vision's own code is proprietary (private repository).
* FFmpeg is dynamically linked. Linux and Windows release jobs build the exact
  FFmpeg 8.0.3 source pin in LGPL shared mode and mechanically reject
  GPL/nonfree/static drift before packaging. Release artifacts include
  the exact configure flags, FFmpeg LGPL text and `THIRD_PARTY_NOTICES.txt`; see
  `docs/ffmpeg.md` and `thirdparty/README.md` for provenance.
