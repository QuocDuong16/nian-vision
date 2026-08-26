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

## Manual recording smoke test (M2)

Records a real source into the recordings layout with the production
recorder pipeline. Not run in CI.

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

* Stop conditions: `--duration <SECONDS>`, `--until-stdin-eof` (pipe close /
  Ctrl+D), or Ctrl+C — on Ctrl+C the process dies and the active segment
  stays behind as a recoverable `.partial.mkv` by design.
* `--no-audio` records video only; `--segment-target` is in seconds
  (default 300).
* The URL never appears in argv, stdout/stderr, or logs; native FFmpeg
  logging stays silenced (`AV_LOG_QUIET`) for the same reason.

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
