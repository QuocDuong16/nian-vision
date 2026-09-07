---
type: Reference
title: Source Map
description: "Workspace structure of the nian-vision repository — applications, crates, tools, documentation, CI/CD, and release scripts."
tags: [source-map, navigation, workspace, file-inventory]
timestamp: "2026-08-26"
---

# Source Map

Workspace structure for the `nian-vision` repository. The workspace contains 12 library crates, 2 applications, and 3 tools.

## Workspace Layout

```text
nian-vision/
├── apps/
│   ├── nian-desktop/              # Tauri 2 host application
│   │   ├── src/lib.rs             # Tauri commands, lifecycle, tray, single-instance
│   │   ├── src/main.rs            # Entry point
│   │   ├── tauri.conf.json        # Tauri config (version authoritative)
│   │   ├── windows/nsis-hooks.nsh # Windows installer hooks
│   │   └── capabilities/          # Tauri v2 capability definitions
│   └── nian-media-worker/         # Isolated media process
│       ├── src/main.rs            # probe CLI + run IPC entry
│       ├── src/job.rs             # RecordingJobManager
│       ├── src/live.rs            # Live session writer
│       └── build.rs               # FFmpeg linking
│
├── crates/
│   ├── nian-domain/               # Pure vocabulary (no I/O)
│   │   └── src/
│   │       ├── camera.rs          # CameraId, CameraConfig, endpoint types
│   │       ├── recording.rs       # RecordingId, failure categories
│   │       ├── events.rs          # EventId, MotionState, EventIndex query types
│   │       ├── ptz.rs             # PTZ types, movement generations
│   │       ├── ids.rs             # Path-safe ID construction
│   │       ├── media.rs           # MediaSource, codec types
│   │       ├── retention.rs       # RetentionPolicy, StorageQuota
│   │       ├── secret.rs          # Redacted credential display
│   │       └── backoff.rs         # ReconnectBackoff schedule
│   │
│   ├── nian-application/          # Orchestration controllers
│   │   └── src/
│   │       ├── lib.rs             # Public controller re-exports
│   │       ├── camera_service.rs  # Camera CRUD, validation, settings
│   │       ├── recording_controller.rs  # Per-camera recording slots
│   │       ├── probe_controller.rs      # Bounded probe admission
│   │       ├── playback_controller.rs   # Session management, loopback HTTP
│   │       ├── live_controller.rs       # Live view sessions, fragment reaper
│   │       ├── onvif_controller.rs      # ONVIF discovery/provisioning
│   │       ├── ptz_controller.rs        # PTZ binding, movement, registry
│   │       ├── event_controller.rs      # PullPoint workers, motion normalization
│   │       ├── storage_manager.rs       # Reconciliation, retention, quarantine
│   │       ├── supervisor.rs            # WorkerSupervisor (process lifecycle)
│   │       ├── notification.rs          # Desktop notification dispatcher
│   │       ├── desktop_lifecycle.rs     # Running/Suspending/Quitting gates
│   │       └── worker_process.rs        # WorkerGuard, spawn/kill/reap
│   │
│   ├── nian-onvif/                # ONVIF protocol (no Tauri, no FFmpeg)
│   │   └── src/
│   │       ├── client.rs          # SOAP client, auth negotiation
│   │       ├── discovery.rs       # WS-Discovery, bounded multicast
│   │       ├── authority.rs       # HTTPS authority validation
│   │       ├── types.rs           # ONVIF response types
│   │       └── xml.rs             # Hardened XML parser
│   │
│   ├── nian-index/                # SQLite recording + event catalogs
│   │   └── src/
│   │       ├── lib.rs             # Recording timeline, WAL, schema
│   │       └── events.rs          # EventIndex, motion history, retention
│   │
│   ├── nian-settings/             # Authoritative non-secret config
│   │   ├── src/lib.rs             # Settings schema v1→v6, migrations
│   │   └── tests/persistence.rs   # Schema migration/round-trip tests
│   │
│   ├── nian-storage/              # Filesystem layout + publication
│   │   └── src/
│   │       ├── lib.rs             # RecordingsLayout, canonical paths
│   │       ├── paths.rs           # Path construction, classification
│   │       ├── classification.rs  # Writeability pre-flight
│   │       ├── inventory.rs       # Deterministic filesystem scan
│   │       ├── transaction.rs     # claim_segment, publish_no_replace
│   │       ├── lease.rs           # CameraLease (cross-process)
│   │       ├── recovery.rs        # Recovery tombstone validation
│   │       ├── error.rs           # Typed storage errors
│   │       ├── sqlite_family.rs   # SQLite corruption quarantine
│   │       └── test_hooks.rs      # Quarantine test helpers
│   │
│   ├── nian-ipc/                  # NDJSON protocol
│   │   └── src/
│   │       ├── lib.rs             # Serve loop, dispatch
│   │       ├── framing.rs         # Size-capped line framing
│   │       ├── message.rs         # Envelope types
│   │       ├── server.rs          # Protocol handler
│   │       ├── handshake.rs       # HELLO version contract
│   │       └── error.rs           # IPC error types
│   │
│   ├── nian-media/                # Backend-agnostic facade
│   │   └── src/
│   │       ├── lib.rs             # Probe, MediaSource traits
│   │       ├── source.rs          # Source abstraction
│   │       ├── probe.rs           # Probe result types
│   │       └── error.rs           # MediaError (timeout vs cancellation)
│   │
│   ├── nian-media-ffmpeg/         # Safe FFmpeg wrapper
│   │   └── src/
│   │       ├── lib.rs             # Public API
│   │       ├── backend.rs         # RAII backend init/shutdown
│   │       ├── input.rs           # MediaInput demux
│   │       ├── muxer.rs           # MatroskaMuxer, fragmented MP4
│   │       ├── packet.rs          # FfmpegPacket (packet-faithful copy)
│   │       ├── interrupt.rs       # Operation-scoped deadlines
│   │       ├── version.rs         # ABI version guard
│   │       ├── error_util.rs      # FFmpeg error classification
│   │       └── logging.rs         # FFmpeg log integration
│   │
│   ├── nian-recorder/             # Segmented recording engine
│   │   └── src/
│   │       ├── lib.rs             # RecordingSession
│   │       ├── session.rs         # Session loop, keyframe rotation
│   │       ├── recovery.rs        # Partial recovery, remux
│   │       └── supervisor.rs      # CameraRecordingSupervisor
│   │
│   ├── nian-ffmpeg-sys/           # Raw FFI (generated)
│   │   └── src/bindings.rs        # Committed from vendored 8.0.3 headers
│   │
│   └── nian-platform-windows/     # Win32 boundary (unsafe isolated)
│       └── src/lib.rs             # Suspend/resume, Job Object
│
├── ui/                            # React/TypeScript/Vite frontend
│   ├── src/
│   │   ├── App.tsx                # Main app shell, screen switching
│   │   ├── navigation.ts          # Screen definitions
│   │   ├── lib/tauri.ts           # Typed Tauri command wrappers
│   │   ├── screens/               # One screen per feature area
│   │   │   ├── CamerasScreen.tsx  # Camera list, CRUD, Start/Stop
│   │   │   ├── LiveViewScreen.tsx # Multi-camera live tiles
│   │   │   ├── EventReviewScreen.tsx # Event history + playback
│   │   │   ├── TimelineScreen.tsx # Recording day/range + playback
│   │   │   ├── StorageScreen.tsx  # Storage settings + retention
│   │   │   └── SettingsScreen.tsx # App settings, launch-at-login
│   │   ├── components/            # Reusable UI (Sidebar, StatusChip, PtzControls, EmptyState)
│   │   └── types.ts               # Shared TS types
│   ├── package.json               # nian-ui package
│   └── vite.config.ts
│
├── tools/
│   ├── bindgen-gen/               # Generates nian-ffmpeg-sys bindings
│   ├── nian-release-verifier/     # Verifies Tauri updater signatures
│   └── nian-settings-fixture/     # Creates test settings databases
│
├── scripts/
│   ├── release/                   # Release pipeline
│   │   ├── release-config.json    # Release configuration
│   │   ├── version-check.mjs      # Version consistency validation
│   │   ├── assemble-release.mjs   # Multi-platform asset assembly
│   │   ├── write-tauri-*.mjs      # Tauri config generation
│   │   ├── stage-linux.sh         # Linux staging + AppImage smoke
│   │   ├── stage-windows.ps1      # Windows staging + NSIS smoke
│   │   ├── sign-authenticode-windows.ps1  # Windows code signing
│   │   └── *.test.mjs             # Release script unit tests
│   ├── ci-install-ffmpeg.sh       # CI FFmpeg setup
│   ├── setup-ffmpeg-linux.sh      # Local FFmpeg dev setup
│   └── generate-fixture.sh        # Generate test media fixtures
│
├── docs/
│   ├── architecture.md            # Comprehensive architecture (900+ lines)
│   ├── adr/0001-*.md – 0018-*.md  # 18 Architecture Decision Records
│   ├── development.md             # Setup guide, daily commands
│   ├── testing.md                 # Test layers, fixture generation
│   ├── releasing.md               # Release process, signing, publication
│   ├── release-checklist.md       # Step-by-step release procedure
│   ├── known-limitations.md       # Intentional v1 boundaries
│   ├── v1-production-audit.md     # M15 hardening status
│   └── ffmpeg.md                  # FFmpeg ABI, library paths
│
├── thirdparty/                    # Vendored FFmpeg 8.0.3 headers
│
├── .forgejo/workflows/
│   ├── quality.yml                # CI: fmt, clippy, test, deny, lint, build
│   └── openwiki-update.yml        # Scheduled wiki regeneration
│
├── .github/workflows/
│   └── release.yml                # GitHub release pipeline (mirror only)
│
├── Cargo.toml                     # Workspace root (version 1.0.0-rc.4)
├── package.json                   # Root pnpm workspace
├── rust-toolchain.toml            # Pinned Rust 1.98.0
├── .mise.toml                     # Node 26.7.0, Rust
├── deny.toml                      # cargo-deny config
└── README.md                      # Project overview + milestone status
```

## Key Entry Points

| What | Where |
|------|-------|
| Desktop app entry | `apps/nian-desktop/src/main.rs` → `src/lib.rs` (Tauri commands) |
| Media worker entry | `apps/nian-media-worker/src/main.rs` (probe CLI or run IPC) |
| Settings schema | `crates/nian-settings/src/lib.rs` (schema v1→v6) |
| Recording data flow | `nian-recorder/src/session.rs` → `nian-storage/src/transaction.rs` |
| Playback flow | `nian-application/src/playback.rs` → `nian-media-worker/src/main.rs` (playback.prepare) |
| Live view flow | `nian-application/src/live_controller.rs` → `nian-media-worker/src/live.rs` |
| ONVIF discovery | `nian-onvif/src/discovery.rs` → `nian-application/src/onvif_controller.rs` |
| PTZ control | `nian-application/src/ptz_controller.rs` → `nian-onvif/src/client.rs` |
| Event ingestion | `nian-application/src/event_controller.rs` → `nian-index/src/events.rs` |
| Release pipeline | `.github/workflows/release.yml` → `scripts/release/` |
| CI quality gates | `.forgejo/workflows/quality.yml` |

## ADR Index

| ADR | Title | Key Decision |
|-----|-------|-------------|
| 0001 | Desktop architecture | Tauri 2 + separate media worker; nian-media facade |
| 0002 | FFmpeg integration | Vendored 8.0.3, FFI via nian-ffmpeg-sys, safe wrapper |
| 0003 | Process isolation and IPC | NDJSON over stdio; crash containment |
| 0004 | Recording container | Matroska (.mkv); keyframe-aware rotation by DTS |
| 0005 | Storage and index model | Filesystem authoritative; SQLite disposable |
| 0006 | Toolchain pinning | rust-toolchain.toml + mise |
| 0007 | Resilience and supervision | Typed timeout vs cancellation; RAII deadlines |
| 0008 | Settings and credentials | Three ownership boundaries |
| 0009 | Playback transport | Packet-copy MKV→fragmented MP4; loopback HTTP |
| 0010 | Desktop lifecycle | Running/Suspending/Quitting; Job Object containment |
| 0011 | Linux/Windows distribution | AppImage + NSIS; Forgejo authoritative; GitHub mirror |
| 0012 | Multi-camera recording | Per-camera slots; 8-slot capacity; schema v3 |
| 0013 | ONVIF discovery | Local-only; bounded WS-Discovery; hardened SOAP |
| 0014 | Multi-camera live view | 4 sessions; bounded fragmented-MP4 window |
| 0015 | ONVIF PTZ control | Optional paired; generation-based movement; dead-man timeouts |
| 0016 | ONVIF events/motion | PullPoint workers; normalized transitions; EventIndex |
| 0017 | Event review + notifications | EventIndex projection; 5s pre-roll; rate-limited notifications |
| 0018 | v1 production/release | Forgejo authoritative; draft-first publication |
