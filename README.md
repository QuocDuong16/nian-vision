# Nian Vision

Local-first desktop NVR (network video recorder) for IP cameras. The first
supported camera is the TP-Link Tapo C200 over RTSP, with a camera-agnostic
domain so other RTSP/ONVIF cameras can follow.

**Status**: milestone **M5 (desktop camera management)** is implemented on top
of M0–M4. The desktop persists camera definitions and recorder/storage settings
in an authoritative platform app-data `settings.sqlite3`, while camera passwords
remain in the operating system credential store. The M4 recording catalog at
`<storage_root>/.nian/recordings.sqlite3` remains disposable and rebuildable from
footage. Users can add/edit/delete saved RTSP cameras, test a connection through
the media worker, start/stop the single M5 active recording, and observe typed
recording/reconnect state. Multiple camera definitions may be saved, but M5
allows at most one active desired recording. Recording desired state is
session-only in M5: saved configuration survives restart, recording itself does
not auto-start. Playback/timeline and live video remain M6 scope.

## What it does today

* Tauri 2 desktop application (React/TypeScript/Vite UI) with managed M5 state:
  camera CRUD, pre-save connection testing, Start/Stop controls, typed recording
  status, delete confirmation and persisted storage/retention settings. The
  webview has no RTSP/network privilege; privileged work flows through narrow
  Tauri commands.
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
    `camera.probe` source-only connection tests;
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
* Clean crate boundaries with `unsafe` confined to the FFmpeg layers (plus
  one audited Windows publication primitive), a race-safe storage layout,
  credential redaction, and a full quality-gate setup (fmt/clippy/tests,
  ESLint/tsc/vitest/vite build).

## Architecture

```text
React UI ─ typed Tauri commands ─ nian-desktop host
                 │                  │
                 │                  ├─ platform app-data/settings.sqlite3
                 │                  ├─ native OS credential store
                 │                  │ NDJSON IPC (stdio)
                 │              nian-media-worker
                                │ FFmpeg FFI (dynamic, LGPL)
                            RTSP cameras
```

See `docs/architecture.md` and `docs/adr/` for the decisions behind this
layout (process isolation, FFmpeg strategy, container choice, storage
model, authoritative-settings/native-secret split).

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

## Supported camera protocols

* RTSP (H.264) — implemented at the media layer, manual smoke test via
  `NIAN_VISION_RTSP_URL` (credentials never go on the command line).
* ONVIF — planned for M10, after RTSP recording is mature.

## License notes

* Nian Vision's own code is proprietary (private repository).
* FFmpeg is used dynamically and, for distribution, will be built in LGPL
  mode (no `--enable-gpl`, no `--enable-nonfree`); see `docs/ffmpeg.md` and
  `thirdparty/README.md` for vendored-header provenance.
