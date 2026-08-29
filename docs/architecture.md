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
│  Storage Manager     (M4) ✓  │
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
| `nian-application` | config validation, orchestration policies, worker supervision | `AppConfig`, `WorkerSupervisor` (M3), `StorageManager` reconciliation/rebuild/retention orchestration (M4) |
| `nian-index` | rebuildable SQLite recording catalog | bundled SQLite, schema v1 migrations, WAL, timeline queries; no media/filesystem ownership (M4) |
| `nian-storage` | recordings layout, claiming, publication, inventory/transaction facts | traversal-proof paths, race-safe `claim_segment`, atomic no-replace publish, lease-aware partial primitives, symlink-safe deterministic inventory (M4) |
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

## Storage catalog and retention (M4)

```text
<storage_root>/
  .nian/
    recordings.sqlite3          # rebuildable cache; WAL/SHM live here
  <camera-id>/<YYYY>/<MM>/<DD>/
    HH-MM-SS[-N].mkv
    HH-MM-SS[-N].recovered.mkv
```

`nian-storage` owns filesystem truth: canonical paths, exact recording-file
classification, `CameraLease`, deterministic inventory and recovery tombstone
validation. Inventory follows only the exact camera/date grammar, refuses
symlink traversal, and never descends into `.nian`. `nian-index` owns only
SQLite persistence. Its schema v1 stores relative paths, recording kind/state,
local naive wall-clock `started_at`, sequence, size and optional media duration;
`(camera_id, started_at, sequence)` is a real SQLite timeline index. SQLite runs
WAL + `foreign_keys=ON`, a 2 s busy timeout and `synchronous=NORMAL` because the
catalog is disposable while media is not.
Startup requests WAL and then queries `journal_mode` again; anything other than
`wal` is a typed startup failure. `foreign_keys` is likewise queried back and
must equal `1`.

`nian-application::StorageManager` performs:

1. filesystem inventory + database snapshot;
2. a deterministic reconciliation plan;
3. one SQLite transaction for index upserts/removal of missing rows.

A second reconciliation without filesystem changes produces zero DB mutations.
A retention-ready gate is fail-closed: every reconciliation/rebuild starts by
marking the manager unreconciled and sets it ready again only after the complete
inventory + SQLite operation succeeds. A database deletion triggers an explicit
filesystem rebuild. SQLite corruption discovered either during initial open or
later reconciliation closes the live connection, quarantines the database with
its WAL/SHM sidecars, opens a fresh schema and rebuilds from authoritative disk
facts without probing media through FFmpeg. A `.quarantine-pending` marker makes
an interrupted three-file quarantine convergent on the next startup, so an old
WAL/SHM can never be attached to a newly-created canonical database.
`media_duration_ms` remains NULL when disk facts cannot prove it.

Partials preserve M3 ownership: a camera lease held by another process makes a
partial active and untouchable; an immediately acquirable lease proves only that
it is abandoned/recovery-pending. M4 never remuxes it. Scratch cleanup likewise
requires the camera lease. Old **normal finalized** recordings and fully
**settled recovered** recordings do not require the camera-wide lease, so 24/7
recording cannot disable historical retention. Unresolved partial/scratch work
continues to require M3 ownership.

Retention plans from filesystem facts and an injected local wall-clock time.
Age and quota use OR semantics. Quota cleanup starts only above the explicit
high watermark (`RetentionPolicy.max_storage_bytes == StorageQuota.max_bytes`)
and stops at the lower `cleanup_target_bytes`; candidates are ordered by
`started_at → sequence → relative path`. Immediately before each deletion the
file is revalidated as the same canonical regular recording with the expected
size. Deletion is deliberately **filesystem first, SQLite second**. A crash after
file removal therefore leaves only a stale cache row, which the next
reconciliation removes.

Recovered recordings add stronger resurrection guards. Filesystem absence is a
typed fact: only `NotFound` proves absence; permission/IO failures are
uninspectable and therefore preserve footage/evidence. A recovered final becomes
settled only when the shared strict v2 tombstone proves the exact original→final
transaction, the final is a canonical regular file with the trusted size, and
the original partial is proven absent. Immediately before deleting the final,
retention revalidates that final identity/size, the exact tombstone evidence and
that the original partial is **still** `NotFound`. The final is then deleted,
the tombstone is verified once more before tombstone cleanup, and the index row
is removed last. Any failed revalidation skips that candidate and preserves
media/evidence. Quota cleanup continues to later eligible candidates and reports
whether the LOW watermark was actually reached.

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
