# Nian Vision architecture

Nian Vision is a local-first desktop NVR for IP cameras (first target:
TP-Link Tapo C200 over RTSP). This page is the map; the ADRs record why.

## Process topology

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
│  Camera Manager      (M5)    │
│  Recording Manager   (M2+)   │
│  Storage Manager     (M4)    │
│  Worker Supervisor   (M3) ✓  │
│    (nian-application)        │
└──────────────┬───────────────┘
               │ NDJSON IPC over stdio (nian-ipc)
┌──────────────▼───────────────┐
│      nian-media-worker       │
│     (apps/nian-media-worker) │
│  RecordingJobManager (M3) ✓  │
│  CameraRecordingSupervisor   │
│    (nian-recorder)           │
│                              │
│  nian-media facade           │
│  └─ nian-media-ffmpeg        │
│     └─ nian-ffmpeg-sys       │
│        └─ libav* (dynamic)   │
└──────────────┬───────────────┘
               │ RTSP
          IP cameras
```

Key properties:

* The desktop host never links FFmpeg; media crashes are contained in the
  worker (ADR-0003).
* Application code never sees FFmpeg types; `nian-media` is the seam
  (ADR-0001).
* `unsafe` exists inside `nian-media-ffmpeg` (safe public API) and
  `nian-ffmpeg-sys` (raw declarations), plus one audited exception:
  `nian-storage`'s Windows no-replace publication primitive (`MoveFileExW`,
  compiled only on Windows targets). Every other crate has
  `#![forbid(unsafe_code)]`.

## Crate map

| Crate | Role | Notes |
|---|---|---|
| `nian-domain` | Camera/Recording/Media vocabulary | path-safe IDs, redacted credentials, backoff schedule |
| `nian-application` | config validation, policies, worker supervision | `AppConfig`, `WorkerSupervisor` (spawn/handshake/crash-restart, M3) |
| `nian-storage` | recordings layout, claiming, publication, partial scan | traversal-proof paths, race-safe `claim_segment`, atomic no-replace publish, canonical partial classification (`scan_camera_partials`, M3) |
| `nian-ipc` | NDJSON protocol + serve loop | versioned envelopes, size-capped framing; handlers may emit events mid-request (M3) |
| `nian-media` | backend-agnostic facade | `Probe`, `MediaSource`; errors distinguish cancellation vs timeout (M3); packets travel as backend-owned types |
| `nian-media-ffmpeg` | safe FFmpeg wrapper | input/muxer/packet/interrupt/ABI guard; operation-scoped RAII deadlines + typed abort causes (M3) |
| `nian-recorder` | segmented recording engine + supervision | keyframe rotation, durable finalize/publish; camera reconnect supervisor, partial recovery (M3) |
| `nian-ffmpeg-sys` | raw FFI (generated) | committed bindings from vendored 8.0.3 headers |
| `apps/nian-desktop` | Tauri 2 host | window + commands |
| `apps/nian-media-worker` | media process | `probe` CLI, `run` IPC loop with `recording.*` namespace, manual `record` smoke command |

## Recording data flow

```text
RTSP (H.264) / local container
  → MediaInput demux (per-read stall deadline + interrupt-bounded)
  → FfmpegPacket (packet-faithful: side data + flags preserved)
  → Recorder (nian-recorder): discard until first video keyframe,
    rotate at the first keyframe after the media-time target
  → claim_segment → HH-MM-SS[-N].partial.mkv
  → MatroskaMuxer stream copy (av_packet_rescale_ts only)
  → av_write_trailer + final flush/close (both must succeed)
  → publish_no_replace (renameat2 NOREPLACE / MoveFileExW / hard-link)
  → HH-MM-SS[-N].mkv
```

Rotation is driven by packet DTS in the validated video time base — never
by wall clock alone; wall clock only names segments. Timestamps are
preserved from the source across segment boundaries (ADR-0004).

## Supervision layers (M3)

```text
Desktop/App Core
      ↓ WorkerSupervisor              (nian-application — process boundary)
      ↓ IPC: recording.start/stop/status + events
nian-media-worker                     (one recording job per process)
      ↓ RecordingJobManager           (worker job lifecycle)
      ↓ CameraRecordingSupervisor     (reconnect state machine ABOVE sessions)
      ↓ RecordingSession              (ONE healthy connection — unchanged M2 shape)
FFmpeg / RTSP
```

* **Timeout vs cancellation** is a typed distinction at the FFmpeg
  boundary (`MediaError::TimedOut` vs `Interrupted`, derived from the
  interrupt callback's cause — ADR-0007): operator intent stops
  supervision; timeouts salvage healthy segments and reconnect.
* **Failure classification** is centralized in
  `RecordingError::category()`/`is_retryable()` — supervisors never parse
  strings. Only source-side failures (open/read/timeout) retry.
* **Reconnect policy** reuses `nian_domain::ReconnectBackoff`
  (2s→5s→10s→30s→60s) plus bounded seeded jitter; the streak resets only
  after a connection recorded ≥ 30 s of media time (stability rule), not
  on mere successful opens. Clean EOF means completion for files but
  connection-lost for RTSP.
* **Restarting** after worker crashes uses the same schedule at the
  process level with fast-death escalation; configuration-shaped start
  refusals stop supervision permanently instead of looping.

## Startup reconciliation (M3)

After crashes or forced shutdowns `.partial.mkv` files remain. Recovery is
layered and conservative: `scan_camera_partials` classifies by name +
size + EBML structure sniff (empty / truncated media /
finalized-but-unpublished), then `recover_camera_partials` PROVES
readability by remuxing readable packets — keyframe-aligned — into a fresh
exclusively-claimed output, finalizing durably and publishing no-replace
before removing the original. Unprovable leftovers stay quarantined,
never renamed, never invented into recordings (ADR-0007).

## Failure model

Camera and network failures are normal operation (master spec §11):
wrong credentials, unreachable host, timeouts, RTSP disconnects, Wi-Fi
loss, camera/router reboots, PC sleep/wake. Blocking FFmpeg calls are
bounded by operation-scoped deadlines through the interrupt callback;
deadlines never outlive their operation (RAII guard). Reconnects use the
fixed 2s→5s→10s→30s→60s schedule (`nian_domain::ReconnectBackoff`). The
filesystem remains the source of survival; SQLite is a rebuildable index
(ADR-0005).

Sleep/wake note (M3 §17): no native Windows power-event integration yet —
the supervised state machines tolerate long wall-clock interruptions by
construction (timeouts fire, dead connections fail, supervisors
reconnect), but native suspend/resume event handling remains M7 scope.

## Documentation index

* ADRs: `docs/adr/` (resilience model: ADR-0007)
* FFmpeg specifics: `docs/ffmpeg.md`
* Development setup: `docs/development.md`
* Testing: `docs/testing.md`
