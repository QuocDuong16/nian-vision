---
type: Reference
title: Architecture Overview
description: "Describes the Nian Vision application architecture — process topology, crate responsibilities, design principles, and per-camera ownership planes. Links to the authoritative docs/architecture.md for full detail."
tags: [architecture, overview, process-topology, crates, design-principles]
timestamp: "2026-08-26"
---

# Architecture Overview

Nian Vision is a local-first desktop NVR for IP cameras built on Tauri 2 (Rust host + React/TypeScript/Vite UI). The canonical architecture document is [`docs/architecture.md`](docs/architecture.md) (900+ lines); this page is a navigable summary.

## Process Topology

```text
┌──────────────────────────────┐
│        Nian Vision UI        │
│      React / TypeScript      │
│              (ui/)           │
└──────────────┬───────────────┘
               │ Tauri commands
┌──────────────▼───────────────┐
│       nian-desktop host      │
│      (apps/nian-desktop)     │
│                              │
│  CameraService, Recording-   │
│  Controller, ProbeController,│
│  PlaybackController,         │
│  DesktopLifecycle,           │
│  LiveViewController,         │
│  OnvifController,            │
│  PtzController,              │
│  EventController,            │
│  WorkerSupervisor,           │
│  StorageManager              │
│    (nian-application)        │
└──────────────┬───────────────┘
               │ NDJSON IPC over stdio (nian-ipc)
┌──────────────▼───────────────┐
│      nian-media-worker       │
│     (apps/nian-media-worker) │
│                              │
│  nian-media facade           │
│  └─ nian-media-ffmpeg        │
│     └─ nian-ffmpeg-sys       │
│        └─ libav* (dynamic)   │
└──────────────┬───────────────┘
               │ RTSP
          IP cameras
```

Key property: **the desktop host never links FFmpeg**; media crashes are contained in the worker process. Application code never sees FFmpeg types — `nian-media` is the seam (ADR-0001, ADR-0003).

## Crate Map

| Crate | Role |
|-------|------|
| `nian-domain` | Camera/Recording/Media vocabulary; path-safe IDs, redacted credentials, backoff schedule |
| `nian-application` | Orchestration controllers: WorkerSupervisor, StorageManager, camera/record/probe/playback controllers, LiveViewController, OnvifController, PtzController, EventController |
| `nian-onvif` | ONVIF discovery/protocol: bounded WS-Discovery, SOAP client, XML/authority hardening; no Tauri, settings, keyring or FFmpeg |
| `nian-index` | Rebuildable SQLite runtime catalogs: recording timeline + motion-event index, WAL, bounded queries/cleanup |
| `nian-settings` | Authoritative non-secret desktop config: `settings.sqlite3` schema v6, camera/storage/PTZ/Event bindings |
| `nian-storage` | Recordings layout, claiming, publication, inventory/transaction facts; traversal-proof paths, atomic no-replace publish |
| `nian-ipc` | NDJSON protocol + serve loop; versioned envelopes, size-capped framing |
| `nian-media` | Backend-agnostic facade: Probe, MediaSource; errors distinguish cancellation vs timeout |
| `nian-media-ffmpeg` | Safe FFmpeg wrapper: input/muxer/packet/interrupt/ABI guard; RAII deadlines |
| `nian-recorder` | Segmented recording engine: keyframe rotation, durable finalize/publish, camera reconnect supervisor |
| `nian-ffmpeg-sys` | Raw FFI (generated): committed bindings from vendored FFmpeg 8.0.3 headers |
| `nian-platform-windows` | Isolated Win32 boundary: suspend/resume notifications, Job Object worker containment |

## Four Per-Camera Ownership Planes

Each plane has its own controller, capacity limits, lifecycle, and failure domain:

| Plane | Controller | Max Capacity | Persisted State | ADR |
|-------|-----------|-------------|----------------|-----|
| **Recording** | RecordingController | 8 slots | Desired On/Off per camera (schema v3) | ADR-0012 |
| **Live View** | LiveViewController | 4 sessions | None (transient) | ADR-0014 |
| **PTZ** | PtzController | 16 workers | PTZ binding (schema v4, optional) | ADR-0015 |
| **Events** | EventController | 16 workers | Event binding + Desired (schema v5) | ADR-0016 |

Recording and Event state survive restarts. Live and PTZ are transient — they do not restore after Restart or Resume.

## Three Ownership Boundaries

| Boundary | Authority | Storage |
|----------|-----------|---------|
| Application settings | `nian-settings` | Platform app-data `settings.sqlite3` (authoritative, never auto-rebuilt) |
| Camera passwords | Native OS credential store | `CredentialStore` abstraction (keyring in production) |
| Recording catalog | `nian-index` | `<storage_root>/.nian/recordings.sqlite3` (disposable, rebuildable from disk) |
| Event history | `nian-index` | `<storage_root>/.nian/events.sqlite3` (disposable, rebuildable, quarantined on corruption) |

Passwords never enter SQLite, React, or Tauri DTOs. Credential references are opaque UUIDs resolved only at IPC time.

## Recording Data Flow

```text
RTSP (H.264) / local container
  → MediaInput demux (per-read stall deadline + interrupt-bounded)
  → FfmpegPacket (packet-faithful: side data + flags preserved)
  → Recorder: discard until first video keyframe,
    rotate at the first keyframe after the media-time target
  → claim_segment → HH-MM-SS[-N].partial.mkv
  → MatroskaMuxer stream copy (av_packet_rescale_ts only)
  → av_write_trailer + final flush/close
  → publish_no_replace (renameat2 NOREPLACE / MoveFileExW / hard-link)
  → HH-MM-SS[-N].mkv
```

Rotation is driven by packet DTS in the validated video time base — never by wall clock alone (ADR-0004).

## v1 Production/Release Boundary

Forgejo is the authoritative source and CI system. GitHub receives a one-way mirror and runs only the tag-triggered Windows/Linux release pipeline. The package matrix is Linux x86_64 AppImage plus Windows x86_64 NSIS; both include the media worker and pinned FFmpeg 8.0.3 runtime.

See [Source Map](/openwiki/source-map.md) for the full workspace structure, and [`docs/architecture.md`](docs/architecture.md) for complete details on every milestone.
