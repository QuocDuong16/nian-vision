# Development guide

## Prerequisites

| Tool | Version | Managed by |
|---|---|---|
| Rust | 1.98.0 (pinned) | rustup via `rust-toolchain.toml`; mise via `.mise.toml` |
| Node | 26.7.0 (exact) | mise (`.mise.toml`); same version pinned in CI |
| pnpm | 11.22.0 | recorded in root `package.json` |
| FFmpeg | 8.x runtime (ABI 62) | system packages or installer |

Linux build of the desktop shell additionally needs WebKit2GTK/GTK dev
packages (`libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, ...) — the standard Tauri
Linux prerequisites. The media worker and all library crates build without
them.

## First-time setup

```bash
mise install                 # node/rust per .mise.toml
pnpm install                 # frontend dependencies
scripts/setup-ffmpeg-linux.sh  # only if FFmpeg -dev packages are absent
cargo build                  # workspace (default members)
```

If FFmpeg libraries cannot be found, see `docs/ffmpeg.md` for
`NIAN_FFMPEG_LIB_DIR` and pkg-config options.

## Daily commands

```bash
cargo fmt --all                       # format Rust
cargo clippy --all-targets -- -D warnings
cargo test --workspace
pnpm --filter nian-ui lint
pnpm --filter nian-ui typecheck
pnpm --filter nian-ui test
pnpm --filter nian-ui build
```

Run the desktop shell (requires a display):

```bash
cargo run -p nian-desktop            # expects `pnpm --filter nian-ui dev` or a built ui/dist
```

Run the media worker standalone:

```bash
cargo run -p nian-media-worker -- probe crates/nian-media-ffmpeg/tests/fixtures/sample.mkv
cargo run -p nian-media-worker -- run   # NDJSON IPC on stdio
```

## Manual recording smoke test (M2, stop modes explicit in M3)

Records a real source into the recordings layout with the production
recorder pipeline. Not run in CI. Stop modes are now EXPLICIT and mutually
exclusive:

```bash
# From a local file:
cargo run -p nian-media-worker -- record \
  --storage /tmp/nian-recordings --camera cam-1 --segment-target 300 \
  crates/nian-media-ffmpeg/tests/fixtures/session_av.mkv

# From a real camera (credentials only via the environment):
NIAN_VISION_RTSP_URL='rtsp://user:pass@192.168.1.42:554/stream1' \
  cargo run -p nian-media-worker -- record \
  --storage /tmp/nian-recordings --camera tapo-1 --until-stdin-eof --rtsp-from-env
```

* Stop modes: NEITHER flag → Ctrl+C is the only stop; `--duration N` →
  automatic timer stop OR Ctrl+C; `--until-stdin-eof` → stdin EOF (pipe
  close / Ctrl+D) OR Ctrl+C. The two flags are mutually exclusive —
  passing both is a usage error.
* Ctrl+C remains two-stage: the first press requests a graceful stop; a
  second press force-cancels blocking media I/O; further presses are
  ignored (`SIGKILL` remains the hard exit). The graceful flag takes
  effect between packets; since M3 a blocked read additionally has its
  own stall deadline (`SourceTimeouts::read`, default 15 s) and the open/
  connect phase has its own budget (`open`, default 15 s), so a dead
  camera or stalled network can never wedge the process indefinitely.
* A forced cancellation leaves the active segment as a recoverable
  `.partial.mkv` by design; startup reconciliation salvages it (see below).
* `--no-audio` records video only; `--segment-target` is in seconds
  (default 300).
* The URL never appears in argv, stdout/stderr, or logs; native FFmpeg
  logging stays silenced (`AV_LOG_QUIET`) for the same reason.

## Supervised recording via IPC (M3)

The worker's IPC `run` mode hosts ONE supervised recording job per
process with the `recording.*` namespace (see ADR-0007). Useful smoke
shape (each line one NDJSON request on stdin):

* `recording.start` — `{"camera":"cam-1","storage":"/tmp/r",
  "source":{"kind":"file","path":"…"} | {"kind":"rtsp","url":"…"},
  "segment_target_secs":300,"copy_audio":true}`; RTSP URLs ride the
  private stdin channel only, never argv;
* `recording.status` — live snapshot (`state`: connecting/backoff/recording,
  retry attempt, published segment count);
* `recording.stop` — first press graceful, second press forces cancellation
  of blocking I/O.

Reconnect behavior lives above single sessions
(`CameraRecordingSupervisor`): transient source failures back off through
2s→5s→10s→30s→60s (+ jitter) and reconnect into fresh sessions; successful
reconnects that survive ≥ 30 s of recorded media reset the schedule.
Permanent failures (storage, output-write, invalid configuration) end
supervision instead of looping. Partial-file recovery runs at job start:
leftover `.partial.mkv` files from crashes are classified conservatively,
readable content remuxed keyframe-aligned into new no-replace published
segments, originals removed only after durable publication.

## Layout

```text
apps/nian-desktop        Tauri 2 host (window, commands)
apps/nian-media-worker   media process (probe CLI, IPC loop)
crates/                  workspace library crates (see docs/architecture.md)
tools/bindgen-gen        one-time FFmpeg binding generator (libclang needed)
ui/                      React + TypeScript + Vite frontend
thirdparty/ffmpeg        vendored FFmpeg 8.0.3 headers (unmodified)
scripts/                 setup/fixture/icon helper scripts
docs/                    architecture, ADRs, guides
```

## Conventions

* Rust 2024 edition; `#![forbid(unsafe_code)]` everywhere except
  `nian-media-ffmpeg` (safe API), `nian-ffmpeg-sys` (raw FFI) and
  nian-storage's single Windows-only publication primitive.
* Clippy lints `unwrap_used`/`expect_used`/`panic` are denied via CI;
  tests re-enable them locally. Production code uses explicit errors.
* Dependency versions are pinned exactly (`=x.y.z`) and lockfiles are
  committed.
* Secrets (camera passwords) never appear in logs, argv, or fixtures —
  credential types in `nian-domain` redact `Debug`/`Display` output.
* OpenWiki pages under `openwiki/` are generated; do not hand-edit them.
