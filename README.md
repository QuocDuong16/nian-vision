# Nian Vision

Local-first desktop NVR (network video recorder) for IP cameras. The first
supported camera is the TP-Link Tapo C200 over RTSP, with a camera-agnostic
domain so other RTSP/ONVIF cameras can follow.

**Status**: milestone **M2 (recorder)** is implemented and ready for
review: a continuous segmented recording pipeline that turns one healthy
input into durable, independently playable Matroska segments —
keyframe-aware rotation, exclusive segment claiming and atomic no-replace
publication, deterministic fixture-based integration tests. M0 (foundation)
and M1 (FFmpeg FFI media spike) passed architecture/media review.

## What it does today

* Tauri 2 desktop shell (React/TypeScript/Vite UI) that builds and runs on
  Linux and Windows.
* An isolated `nian-media-worker` process that talks FFmpeg through FFI:
  * `nian-media-worker probe <file|credential-free-rtsp-url>` prints stream
    information (codec, resolution, duration);
  * `nian-media-worker run` serves a versioned NDJSON IPC protocol on
    stdin/stdout (ping/describe/shutdown);
  * `nian-media-worker record --storage <DIR> --camera <ID> ...` is the
    manual smoke path that records from `NIAN_VISION_RTSP_URL` or a file
    into rotating `.mkv` segments (development only; never in CI).
* A real recording pipeline (`nian-recorder`): packets are copied
  faithfully (side data and flags preserved), segments start on video
  keyframes, rotation waits for keyframes after the media-time target,
  finalization is durable before the no-replace publish, and failed or
  empty segments stay recoverable partials — never fake recordings.
* Clean crate boundaries with `unsafe` confined to the FFmpeg layers (plus
  one audited Windows publication primitive), a race-safe storage layout,
  credential redaction, and a full quality-gate setup (fmt/clippy/tests,
  ESLint/tsc/vitest/vite build).

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
