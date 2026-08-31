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
│  CameraService       (M5) ✓  │
│  RecordingController (M5) ✓  │
│  ProbeController     (M5) ✓  │
│  Storage Manager     (M4) ✓  │
│  PlaybackController  (M6) ✓  │
│  DesktopLifecycle    (M7) ✓  │
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
  `nian-ffmpeg-sys` (raw declarations), plus audited Windows-only platform
  boundaries: `nian-storage`'s no-replace publication primitive and M7's
  `nian-platform-windows` suspend/resume + Job Object wrapper. Application and
  desktop orchestration remain safe Rust; Win32 raw handles/callback pointers do
  not leak across those boundaries.

## Crate map

| Crate | Role | Notes |
|---|---|---|
| `nian-domain` | Camera/Recording/Media vocabulary | path-safe IDs, redacted credentials, backoff schedule |
| `nian-application` | config validation and orchestration policies | `WorkerSupervisor` (M3), `StorageManager` (M4), M5 camera/record/probe controllers, M6 `PlaybackController` + playback pins, M7 lifecycle admission |
| `nian-index` | rebuildable SQLite recording catalog | bundled SQLite, schema v1 migrations, WAL, timeline queries; no camera settings or credentials (M4) |
| `nian-settings` | authoritative non-secret desktop configuration | app-data `settings.sqlite3`, schema v2 camera/storage + desired-recording/autostart settings; no FFmpeg/Tauri/process logic |
| `nian-storage` | recordings layout, claiming, publication, inventory/transaction facts | traversal-proof paths, race-safe `claim_segment`, atomic no-replace publish, lease-aware partial primitives, symlink-safe deterministic inventory (M4) |
| `nian-ipc` | NDJSON protocol + serve loop | versioned envelopes, size-capped framing; handlers may emit events mid-request (M3) |
| `nian-media` | backend-agnostic facade | `Probe`, `MediaSource`; errors distinguish cancellation vs timeout (M3); packets travel as backend-owned types |
| `nian-media-ffmpeg` | safe FFmpeg wrapper | input/muxer/packet/interrupt/ABI guard; operation-scoped RAII deadlines + typed abort causes (M3) |
| `nian-recorder` | segmented recording engine + supervision | keyframe rotation, durable finalize/publish; camera reconnect supervisor, partial recovery (M3) |
| `nian-ffmpeg-sys` | raw FFI (generated) | committed bindings from vendored 8.0.3 headers |
| `nian-platform-windows` | isolated Win32 desktop boundary | M7 suspend/resume notifications and kill-on-close Job Object worker containment |
| `apps/nian-desktop` | Tauri 2 host | managed M7 state, single-instance/tray/autostart/lifecycle owner, platform app-data + native credentials, playback HTTP owner, thin typed commands |
| `apps/nian-media-worker` | media process | `probe` CLI, `run` IPC with `recording.*`, bounded `camera.probe`, M6 `playback.prepare`, manual `record` smoke command |

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

## Desktop camera management (M5)

M5 adds authoritative user configuration without changing M4 filesystem truth:

```text
platform app-data/
  settings.sqlite3                 # authoritative, NOT disposable

recordings storage root/
  .nian/recordings.sqlite3         # disposable/rebuildable M4 index
  <camera-id>/.../*.mkv            # footage survives camera config deletion

native OS credential store
  credential refs -> username/password
```

`nian-settings` persists structured, non-secret RTSP endpoint data (`host`,
`port`, `path`), display name, stable `CameraId`, audio policy, credential ref and
recorder/storage settings. Storage quota configuration preserves the M4 invariant
that the retention HIGH watermark equals `StorageQuota.max_bytes` and requires an
explicit lower cleanup target; the settings layer validates this on both read and
write. The Tauri host alone resolves the platform app-data path. A corrupt/future
settings database fails safely in place; unlike the M4
recording index it is never silently quarantined/rebuilt because its contents are
not reconstructable from footage.

Credentials are owned by the `CredentialStore` abstraction. Production uses the
native OS-backed keyring; tests use fakes/in-memory stores. Credential references
are application-generated `nian-vision/<camera-id>/<uuid-v4>` identities via an
injectable `CredentialRefGenerator`; they do not depend on wall clock, PID or a
process-local counter. Native keyring entry identity behaves as a mutable key, so
reference uniqueness is part of transaction safety. Credential updates allocate a
distinct new ref, put the new secret, commit the camera row to that ref, and only
then clean the old ref. A generated candidate equal to the committed old ref is
rejected before any keyring write, and any other candidate already occupied in the
native credential store is retried before `set_secret`. This makes pre-commit failure preserve the old
authoritative secret and post-commit cleanup failure merely orphan the old secret.
Passwords never return through Tauri DTOs and still reach the media worker only
inside stdin IPC.

Desktop commands are thin adapters over managed state. `CameraService` owns CRUD
and settings validation; `RecordingController` wraps the existing M3
`WorkerSupervisor` asynchronously; `ProbeController` admits at most one bounded
source-only probe process at a time. M5 has multiple **saved** cameras but at
most one active desired recording. Starting camera B while A is active is a typed
error. Active cameras reject endpoint/credential/audio mutation and deletion; a
display-name-only edit remains legal because `CameraId` and the recording tree do
not change.

Normal Stop is graceful: the controller sets `Stopping`, asks the existing
supervisor to shut down, and ownership is released only after worker teardown.
The UI polls the stable application status DTO (`Starting`, `Recovering`,
`Connecting`, `Recording`, `Backoff`, `Stopping`, `Stopped`, `Failed`) rather than
assuming optimistic state.

`camera.probe` is implemented in `nian-media-worker`, not Tauri. It opens only the
source under a bounded deadline, returns safe stream summary fields, then closes;
it never acquires a camera recording lease or touches storage. The desktop can
probe an unsaved form. An edit with blank password can reuse the committed secret
to test a changed non-secret endpoint without exposing that secret to React.

M5 originally chose session-only desired recording state. M7 supersedes only that
lifetime rule: the single allowed desired camera is now persisted in authoritative
settings and restored through the normal `RecordingController` path after desktop
restart or resume. Runtime state remains separate and may legitimately be `Failed`
while Desired stays On. M6 playback/timeline semantics are unchanged; live view
and thumbnails remain out of scope. See ADR-0008 and ADR-0010.

## Recording timeline and playback (M6)

M6 keeps the M4 authority split intact:

```text
React recording ID (opaque relative identity)
  → recording_timeline / playback_open
  → nian-index row lookup
  → RecordingsLayout + exact camera/date/filename grammar
  → symlink_metadata on root and every component
  → finalized normal/recovered regular file with expected size
  → PlaybackPin + read handle
  → media-worker playback.prepare
  → packet-copy fragmented MP4 in app cache
  → http://127.0.0.1:<ephemeral>/playback/<uuid-token>
  → HTML <video> HTTP Range requests
```

The recording ID is the canonical filesystem-derived relative key already stored
by `nian-index`. It survives index deletion/rebuild; the frontend treats it as an
opaque identifier and never turns it into a path. The backend resolves the key
through the index and revalidates the current filesystem object immediately before
opening it. Absolute paths, non-normal path components, wrong camera/date/name
grammar, partial/recovery-control artifacts, symlinks, non-regular files and size
changes are rejected as typed missing/stale/not-finalized failures. Every later
HTTP media request revalidates the original recording identity again, so a path
replacement cannot silently redirect an existing session to foreign content.

Timeline reads are database queries, not per-request tree rescans. `nian-index`
provides available days, `[start,end)` camera range queries and previous/next
lookups with stable ordering `started_at → sequence → relative_path`; only complete
recordings participate. Normal and recovered finals use the same timeline DTO.
Filesystem freshness is explicit: `PlaybackController::refresh_index()` delegates
to M4 reconciliation when the Timeline first activates for a camera and when the
user presses Refresh. Newly finalized normal/recovered files therefore appear
without restarting the desktop, while active partials remain protected by the
existing CameraLease-aware reconciliation rules. Refresh discovers metadata from
the filesystem only; it does not media-probe duration.
Known duration produces `end_at = started_at + media_duration`; unknown duration
stays NULL and renders honestly. When a recording is opened, `playback.prepare`
may discover duration through libav. The application writes it back only while the
indexed camera/kind/time/sequence/size identity still matches; writeback failure
does not invalidate playback and a full M4 rebuild intentionally returns duration
to unknown.

`PlaybackController` is application-owned state rather than state hidden inside
individual Tauri commands. It bounds active sessions, owns the loopback server and
temporary playback cache, tracks activity, closes/expunges abandoned sessions
after a bounded idle timeout, and owns a per-process `instance-<uuid>` cache
directory whose `.nian-playback-instance.lock` is held with an exclusive kernel
file lock for the controller lifetime. Each playback session also retains a
shared handle to that locked file, so an in-flight HTTP request can extend the
instance lease safely through controller shutdown. Startup cleanup removes another instance
only when that instance lock can be acquired, so a second live desktop process
cannot delete active prepared media. A cache-root coordination lock closes the
instance-creation/cleanup race; PID files and timestamps are not ownership.
Each session has an unguessable UUID token mapping to exactly one
validated recording. The server binds `127.0.0.1` on an ephemeral port, serves no
directory listing or arbitrary path parameter, validates Host/Origin where
practical, limits request/header/buffer sizes, and supports GET/HEAD plus single
HTTP byte ranges. It never binds `0.0.0.0` or creates a LAN video server. Tauri's
CSP independently permits media only from `'self'` and `http://127.0.0.1:*`, not
arbitrary HTTP/LAN origins.

Recorded footage remains canonical MKV packet-copy media. For the current Tapo
C200 target, the worker accepts H.264 video and copies AAC audio when present; an
unsupported audio stream may be omitted rather than transcoded. The worker uses
the existing direct libav/FFmpeg FFI facade and never invokes an `ffmpeg` CLI.
The source MKV is packet-copy remuxed once when the playback session opens into a
unique cache `media.mp4` with
`frag_keyframe+empty_moov+default_base_moof+global_sidx`. No decoder or encoder is
used. The MP4 lives outside the recording tree, is never indexed or retention-
managed, and is deleted with the session.

Storage reconfiguration is prepared before authoritative settings are committed.
The candidate `StorageManager` opens its layout/index and reconciles independently
of the active playback controller. Candidate failure leaves both settings and
playback storage unchanged. If settings persistence fails, the prepared candidate
is discarded and playback stays on the old root. Only after settings commit does
the controller swap the already-prepared candidate and close old sessions; that
final swap performs no fallible I/O and therefore cannot create a settings/root
split-brain state.

`playback_open` remains synchronous in M6 and the desktop command holds the
controller mutex during bounded worker preparation. Close and settings operations
may therefore queue behind a long packet-copy, but there is no unbounded wait:
the worker has a 60-second media deadline plus a small bounded host response
margin. `WorkerGuard` owns the child immediately after spawn and shuts down or
kills/reaps it on every return path. Playback pins and cache session directories
are RAII-owned, so timeout or preparation failure cannot leave permanent playback
ownership artifacts. Moving preparation outside the controller mutex is an
optimization deferred beyond this remediation, not an M6 correctness dependency.

Seeking in this M6 implementation is therefore a two-stage contract: libav builds
the keyframe-fragmented MP4 and global segment index once, then WebView2/browser
seeks use HTTP Range against that prepared representation. A seek does **not**
decode or remux from recording start. It is keyframe/GOP-granular rather than
frame-perfect. M6 does not expose a separate per-seek libav operation because the
session representation is already random-access indexed; if future playback moves
to on-demand remux streaming, that transport must use libav keyframe seeking rather
than replaying packets from zero.

Playback ownership is deliberately different from recording ownership.
`CameraLease` continues to protect active partials, recovery mutation and canonical
writer ownership. A finalized historical recording needs no camera-wide lease and
can be read while the same camera records a new segment. Instead, a successful
playback open acquires a `PlaybackPin` keyed by canonical relative path and keeps a
source read handle open. Retention consults those pins before work and, critically,
rechecks immediately before filesystem deletion; a session opened after retention
planned a candidate therefore wins the race and the candidate is reported as
`skipped_playback`. HTTP requests refresh activity and cannot expire mid-response,
but buffered browser playback is also represented explicitly: while the mounted
video/session exists, Timeline sends `playback_keepalive` every 45 seconds. Keepalive
expires stale sessions before lookup and never reactivates an expired or closing
session. Explicit close, navigation, media failure and unmount stop heartbeats;
renderer disappearance therefore falls back to the ten-minute idle TTL. Once the
last active request/session ownership signal is gone and the TTL expires, the pin
is released and normal filesystem-first retention resumes. This product contract
is portable to Unix, where an open file descriptor alone would not prevent unlink.

M6 is local-recording playback only. It does not implement live RTSP viewing,
thumbnail generation, clip export, motion analysis, simultaneous multi-camera
recording, ONVIF, AI or cloud behavior. Tray/autostart/power lifecycle is layered
above M6 by M7 without changing the playback transport. See ADR-0009.

## Desktop production lifecycle (M7)

M7 adds a Rust-authoritative desktop lifecycle with `Running`, `Suspending` and
terminal `Quitting` states. A desktop `control_gate` serializes transitions with
operations whose correctness depends on admission or recording ownership. Camera
mutation, Start, Probe, playback open and settings mutation prove `Running` before
they commit work. Suspend and Quit close subsystem admission before any blocking
teardown begins.

The desktop is single-instance. The single-instance Tauri plugin is registered
first, so a secondary process exits before initializing media/storage resources.
A manual second launch activates the existing window; the exact autostart marker
`--startup-hidden` does not. The main window is created hidden, normal interactive
startup explicitly shows/unminimizes/focuses it, and Close hides it to the tray.
Only explicit coordinated Quit tears down the backend.

Authoritative settings schema v2 stores `launch_at_login` and one
`recording_enabled` camera. A partial unique index enforces the existing
single-camera desired-recording constraint. Start first proves runtime-controller
admission while the desktop control gate and controller ownership are stable, then
persists Desired=On, then starts the runtime controller. A rejected second-camera
Start therefore cannot replace the previous desired intent, including while the
previous controller is Stopping. User Stop persists Desired=Off before signalling
runtime teardown. Suspend and Quit never rewrite desired intent. Startup and resume
therefore restore Desired=On through the same `RecordingController` path, while
runtime `Failed` remains independently visible to the UI.

Recording status changes are projected to the native tray from Rust through a
controller observer and host-owned tray watcher; React polling is not authoritative
for tray correctness. Persisted-intent changes separately wake the same watcher.
Both the tray watcher and the power dispatcher use explicit Shutdown messages and
are joined during Quit.

Windows suspend/resume events come from the isolated `nian-platform-windows`
boundary. Notifications are subscribed before desired-recording restoration so an
early event is queued rather than lost, but dispatch begins only after startup
initialization is complete. Resume is convergent per subsystem rather than
all-or-nothing: playback expiry/index resync, recording ownership
completion/restoration and probe admission are attempted independently. A playback
refresh failure is reported but cannot leave recording/probe/playback admission
permanently wedged. Duplicate Resume while already Running is a no-op.

Explicit Quit follows deterministic ownership order: recording shutdown/join,
playback shutdown, probe shutdown, tray watcher shutdown/join, power notification
unregister/dispatcher join, then process exit. Dispatcher termination is controlled
explicitly and does not depend on Win32 callback context reclamation. Hard Windows
desktop termination uses a process-owned Job Object with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; the desktop joins it before any worker spawn,
workers inherit membership atomically, and every production spawn verifies
containment or kills/reaps the uncontained child. See ADR-0010.

## Linux distribution and signed updates (M8)

M8 currently defines one CI-validated production distribution target:
`x86_64-unknown-linux-gnu`, shipped as an AppImage. Windows x86_64 remains the next
M8 release target and will use an explicit GitHub-hosted Windows runner such as
`windows-2022`; packaging/signing is not yet implemented or marked validated.
macOS packaging is also deferred.

The AppImage contains the desktop host, a sibling `nian-media-worker`, and an
application-owned FFmpeg 8.0.3 shared runtime. The pre-bundle worker is linked with
relative RUNPATH `$ORIGIN/../lib/nian-vision`, which makes release staging
self-contained. Tauri normalizes the worker RUNPATH to `$ORIGIN/../lib` inside the
AppImage, where the three FFmpeg SONAME libraries are installed in the image's
private `/usr/lib`. Both staging and extracted-AppImage smoke verify that
`libavformat.so.62`, `libavcodec.so.62` and `libavutil.so.60` resolve from the
application-owned runtime with development overrides removed. The candidate FFmpeg build is SHA-256 pinned, explicitly LGPL/shared,
and validated by the real media integration suite before packaging.

Desktop and worker also share an application-version handshake. The IPC protocol
version remains independently authoritative, while packaged builds require the
worker HELLO `application_version` to equal the desktop release version. A copied
worker from another release therefore fails closed instead of silently executing
against a merely wire-compatible host.

The updater is Rust-owned through `tauri-plugin-updater`; React receives only the
narrow `update_check` and `update_install` commands. Checking does not disturb
recording. Installation requires an explicit UI confirmation and first downloads
and verifies the signed updater artifact. Only after cryptographic verification
does the host set the update admission gate and reuse M7 teardown ordering.
Persisted Desired recording intent is not cleared. If updater handoff returns or
fails after runtime teardown, the current application restarts rather than
remaining stranded in Quitting; normal M7 startup restoration then re-applies the
persisted recording intent.

Release-only Tauri configuration is public-only: updater public key, HTTPS endpoint
and bundle/resource mapping. Private updater signing material exists only in the
signed AppImage build step. Frontend assets are built earlier and the release
config disables Tauri's `beforeBuildCommand`, so Vite never inherits signing
secrets. After Tauri signs the AppImage, a release verifier using the same
Minisign-compatible representation as `tauri-plugin-updater` verifies the exact
AppImage/signature against the configured public key before the generated config
is removed.

Production authority validation rejects non-HTTPS, local/loopback and reserved
placeholder hosts. Forgejo remains the authoritative source repository and normal
CI authority; GitHub is a one-way mirror used only for hosted release CI and public
GitHub Releases. Release tags originate on Forgejo, mirror to the same Git object,
and are checked against `GITHUB_SHA`, the configured mirror actor and the mirrored
default branch before any release build starts.

The Linux release build runs on a GitHub-hosted Ubuntu runner with the actual build
inside `rust:1.98.0-bookworm`, preserving the accepted Debian 12/glibc baseline. It
runs frontend/Rust/media gates, clean staged and extracted-AppImage worker smoke,
launches the actual AppImage under isolated Xvfb/D-Bus until the backend emits its
startup-ready marker, and scans staging, extracted application files, frontend
assets and finalized artifacts for a configured secret canary. Finalization emits
`latest.json`, `release-manifest.json`, `BUILD_METADATA.json` and `SHA256SUMS.txt`;
a separate verification job proves those metadata fields, commit identity, hashes
and updater signature describe the exact finalized candidate. Publication is a
separate `contents: write` job: it creates a draft GitHub Release, uploads every
verified asset, downloads them back for filename/byte/checksum validation, and only
then publishes the release. Draft releases are never advertised by the stable
`releases/latest/download/latest.json` updater endpoint.

See ADR-0011 and `docs/releasing.md` for the release contract.

## Failure model

Camera and network failures are normal operation (master spec §11):
wrong credentials, unreachable host, timeouts, RTSP disconnects, Wi-Fi
loss, camera/router reboots, PC sleep/wake. Blocking FFmpeg calls are
bounded by operation-scoped deadlines through the interrupt callback;
deadlines never outlive their operation (RAII guard). Reconnects use the
fixed 2s→5s→10s→30s→60s schedule (`nian_domain::ReconnectBackoff`). The
filesystem remains the source of survival; SQLite is a rebuildable index
(ADR-0005).

Sleep/wake is now explicit M7 lifecycle input on Windows. Native power
notifications move the desktop to `Suspending`, close new work admission and
request bounded recorder/probe/playback interruption. Resume returns admission to
`Running`, independently resynchronizes playback storage, rejoins any stopping
recording controller, restores persisted desired recording through the normal
start path and reopens probe admission. One subsystem's resume failure is surfaced
without preventing the remaining subsystems from converging.

## Documentation index

* ADRs: `docs/adr/` (resilience model: ADR-0007)
* FFmpeg specifics: `docs/ffmpeg.md`
* Development setup: `docs/development.md`
* Linux releases/updater: `docs/releasing.md`
* Testing: `docs/testing.md`
