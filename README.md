# Nian Vision

Local-first desktop NVR (network video recorder) for IP cameras. The first
supported camera is the TP-Link Tapo C200 over RTSP, with a camera-agnostic
domain so other RTSP/ONVIF cameras can follow.

**Status**: milestones M0 (foundation) and M1 (FFmpeg FFI media spike) are
implemented and under review. Recording (M2) has not been started.

## What it does today

* Tauri 2 desktop shell (React/TypeScript/Vite UI) that builds and runs on
  Linux and Windows.
* An isolated `nian-media-worker` process that talks FFmpeg through FFI:
  * `nian-media-worker probe <file|credential-free-rtsp-url>` prints stream
    information (codec, resolution, duration);
  * `nian-media-worker run` serves a versioned NDJSON IPC protocol on
    stdin/stdout (ping/describe/shutdown).
* Stream-copy remux into Matroska (the exact mechanism the recorder will
  use), covered by integration tests against a deterministic fixture.
* Clean crate boundaries with `unsafe` confined to the FFmpeg layers, a
  versioned storage layout, credential redaction, and a full quality-gate
  setup (fmt/clippy/tests, ESLint/tsc/vitest/vite build).

## Architecture

```text
React UI ─ Tauri commands ─ nian-desktop host
                                │ NDJSON IPC (stdio)
                            nian-media-worker
                                │ FFmpeg FFI (dynamic, LGPL)
                            RTSP cameras
```

See `docs/architecture.md` and `docs/adr/` for the decisions behind this
layout (process isolation, FFmpeg strategy, container choice, storage
model).

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
