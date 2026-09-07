---
type: Reference
title: Quickstart
description: "Entry point for the nian-vision repository wiki — explains what Nian Vision is, how to set up the development environment, and where to navigate for deeper topics."
tags: [quickstart, navigation, overview]
timestamp: "2026-08-26"
---

# nian-vision — Quickstart

**Repository**: `nian-vision` (Forgejo authoritative; GitHub release mirror)
**Current version**: 1.0.0-rc.4 (milestones M0–M14 accepted; M15 in progress)
**Stack**: Tauri 2 (Rust host + React/TypeScript/Vite UI), FFmpeg 8.0.3 via FFI

## What Nian Vision Is

A **local-first desktop NVR** (network video recorder) for IP cameras. The first supported camera is the TP-Link Tapo C200 over RTSP, with a camera-agnostic domain model. There is no cloud, WebRTC, remote server, or mobile component in v1.

The application records RTSP streams to local MKV files, provides multi-camera live view (up to 4 H.264 cameras), optional ONVIF discovery/provisioning, optional PTZ control, motion-event monitoring via ONVIF PullPoint, and an Event Review screen with optional desktop notifications.

## Prerequisites

| Tool | Version | Managed by |
|------|---------|------------|
| Rust | 1.98.0 (pinned) | `rust-toolchain.toml` / mise |
| Node | 26.7.0 (exact) | mise (`.mise.toml`) |
| pnpm | 11.22.0 | root `package.json` |
| FFmpeg | 8.x runtime (ABI 62) | system packages |

Linux needs WebKit2GTK/GTK dev packages for the Tauri desktop shell. See `docs/ffmpeg.md` for FFmpeg library path options.

## First-Time Setup

```bash
mise install                      # install node/rust per .mise.toml
pnpm install                      # frontend dependencies
scripts/setup-ffmpeg-linux.sh     # only if FFmpeg dev packages are absent
cargo build                       # workspace (default members)
```

## Daily Commands

```bash
# Format
cargo fmt --all

# Lint (Rust)
cargo clippy --all-targets -- -D warnings

# Test (Rust)
cargo test --workspace

# Frontend
pnpm --filter nian-ui lint
pnpm --filter nian-ui typecheck
pnpm --filter nian-ui test
pnpm --filter nian-ui build

# Run the desktop (requires display + ui dev server or built ui/dist)
cargo run -p nian-desktop
```

## What's in This Wiki

| Page | What it covers |
|------|----------------|
| [Architecture Overview](/openwiki/architecture/overview.md) | Process topology, crate map, design principles, per-camera ownership planes |
| [Source Map](/openwiki/source-map.md) | Workspace structure: apps, crates, tools, docs, scripts |

## What's in the Source Docs

| Doc | Path |
|-----|------|
| Architecture | `docs/architecture.md` — comprehensive 900+ line reference |
| ADRs (18) | `docs/adr/0001-*.md` through `0018-*.md` |
| Development | `docs/development.md` — setup, daily commands, smoke tests |
| Testing | `docs/testing.md` — test layers, fixture generation, CI matrix |
| FFmpeg | `docs/ffmpeg.md` — ABI, library paths, vendored headers |
| Releasing | `docs/releasing.md` — Forgejo/GitHub authority model, signing, publication |
| Release checklist | `docs/release-checklist.md` — step-by-step release procedure |
| Known limitations | `docs/known-limitations.md` — intentional v1 boundaries |
| Production audit | `docs/v1-production-audit.md` — M15 hardening status |

## Project Structure at a Glance

```text
nian-vision/
├── apps/
│   ├── nian-desktop/          # Tauri 2 host (single-instance, tray, lifecycle)
│   └── nian-media-worker/     # Isolated media process (FFmpeg FFI)
├── crates/
│   ├── nian-domain/           # Camera/Recording/Media vocabulary
│   ├── nian-application/      # Orchestration controllers
│   ├── nian-onvif/            # ONVIF discovery/protocol
│   ├── nian-index/            # SQLite recording + event catalogs
│   ├── nian-settings/         # Authoritative non-secret config
│   ├── nian-storage/          # Filesystem layout, claiming, publication
│   ├── nian-ipc/              # NDJSON protocol + serve loop
│   ├── nian-media/            # Backend-agnostic facade
│   ├── nian-media-ffmpeg/     # Safe FFmpeg wrapper
│   ├── nian-recorder/         # Segmented recording engine
│   ├── nian-ffmpeg-sys/       # Raw FFI bindings
│   └── nian-platform-windows/ # Win32 suspend/resume + Job Object
├── ui/                        # React/TypeScript/Vite frontend
├── tools/                     # bindgen-gen, release-verifier, settings-fixture
├── scripts/release/           # Release pipeline scripts
├── docs/                      # Architecture, ADRs, testing, releasing guides
└── thirdparty/                # Vendored FFmpeg headers
```

## Key Architectural Principles

1. **Filesystem is authoritative; SQLite is disposable** — recordings survive DB corruption; index is rebuildable from disk.
2. **Process isolation** — FFmpeg lives in `nian-media-worker`; the desktop host never links it.
3. **Three ownership boundaries** — settings (non-secret config), native credential store (passwords), recording index (disposable).
4. **Four independent per-camera planes** — Recording, Live, PTZ, Events — each with its own controller and lifecycle.
5. **No secrets in React** — RTSP URLs with credentials never pass through Tauri DTOs; credential references are opaque.
