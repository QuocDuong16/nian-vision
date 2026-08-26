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
│  Worker Supervisor   (M3)    │
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
| `nian-application` | config validation, policies | `AppConfig`, `SegmentTargetDuration` |
| `nian-storage` | recordings layout, claiming, publication | traversal-proof paths, race-safe `claim_segment`, atomic no-replace publish |
| `nian-ipc` | NDJSON protocol + serve loop | versioned envelopes, size-capped framing |
| `nian-media` | backend-agnostic facade | `Probe`, `MediaSource`; packets travel as backend-owned types (`FfmpegPacket`) |
| `nian-media-ffmpeg` | safe FFmpeg wrapper | input/muxer/packet/interrupt/ABI guard |
| `nian-recorder` | segmented recording engine | keyframe-aware rotation, startup alignment, durable finalize + publish |
| `nian-ffmpeg-sys` | raw FFI (generated) | committed bindings from vendored 8.0.3 headers |
| `apps/nian-desktop` | Tauri 2 host | window + commands |
| `apps/nian-media-worker` | media process | `probe` CLI, `run` IPC loop, manual `record` smoke command |
| `tools/bindgen-gen` | one-shot binding generator | requires libclang, run manually |

## Recording data flow (M2, implemented)

```text
RTSP (H.264) / local container
  → MediaInput demux (interrupt-bounded reads)
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

## Failure model

Camera and network failures are normal operation (master spec §11):
wrong credentials, unreachable host, timeouts, RTSP disconnects, Wi-Fi
loss, camera/router reboots, PC sleep/wake. Blocking FFmpeg calls are
bounded by the interrupt callback (`InterruptHandle`); reconnects use the
fixed 2s→5s→10s→30s→60s schedule (`nian_domain::ReconnectBackoff`). The
filesystem remains the source of survival; SQLite is a rebuildable index
(ADR-0005).

## Documentation index

* ADRs: `docs/adr/`
* FFmpeg specifics: `docs/ffmpeg.md`
* Development setup: `docs/development.md`
* Testing: `docs/testing.md`
