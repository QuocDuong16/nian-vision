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
* `unsafe` exists only inside `nian-media-ffmpeg` (safe public API) and
  `nian-ffmpeg-sys` (raw declarations). Every other crate has
  `#![forbid(unsafe_code)]`.

## Crate map

| Crate | Role | Notes |
|---|---|---|
| `nian-domain` | Camera/Recording/Media vocabulary | path-safe IDs, redacted credentials, backoff schedule |
| `nian-application` | config validation, policies | `AppConfig`, `SegmentTargetDuration` |
| `nian-storage` | recordings layout, path safety | pure path logic, traversal-proof |
| `nian-ipc` | NDJSON protocol + serve loop | versioned envelopes, size-capped framing |
| `nian-media` | backend-agnostic facade | `Probe`, `MediaSource`, `MediaPacket` |
| `nian-media-ffmpeg` | safe FFmpeg wrapper | input/muxer/interrupt/ABI guard |
| `nian-ffmpeg-sys` | raw FFI (generated) | committed bindings from vendored 8.0.1 headers |
| `apps/nian-desktop` | Tauri 2 host | window + commands |
| `apps/nian-media-worker` | media process | `probe` CLI + `run` IPC loop |
| `tools/bindgen-gen` | one-shot binding generator | requires libclang, run manually |

## Recording data flow (M2 target)

```text
RTSP (H.264)
  → libavformat demux (interrupt-bounded reads)
  → compressed packets (stream copy, no decode/re-encode)
  → timestamp rescale (av_packet_rescale_ts)
  → Matroska segment writer (MatroskaMuxer)
  → <storage_root>/<camera>/<Y>/<M>/<D>/HH-MM-SS.partial.mkv
  → av_write_trailer → atomic rename → HH-MM-SS.mkv
```

Rotation happens at the first keyframe at/after the configured target
(~5 minutes), so every segment starts on a keyframe (ADR-0004).

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
