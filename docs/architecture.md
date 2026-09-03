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
| `nian-application` | config validation and orchestration policies | `WorkerSupervisor` (M3), `StorageManager` (M4), camera/record/probe controllers, M6 `PlaybackController`, M7 lifecycle admission, M10 `OnvifController`, M11 `LiveViewController` session/capacity/reaper boundary |
| `nian-onvif` | ONVIF discovery/protocol infrastructure | bounded WS-Discovery, SOAP Device/Media2/Media client, XML/authority hardening; no Tauri, settings, keyring or FFmpeg |
| `nian-index` | rebuildable SQLite recording catalog | bundled SQLite, schema v1 migrations, WAL, timeline queries; no camera settings or credentials (M4) |
| `nian-settings` | authoritative non-secret desktop configuration | app-data `settings.sqlite3`, schema v2 camera/storage + desired-recording/autostart settings; no FFmpeg/Tauri/process logic |
| `nian-storage` | recordings layout, claiming, publication, inventory/transaction facts | traversal-proof paths, race-safe `claim_segment`, atomic no-replace publish, lease-aware partial primitives, symlink-safe deterministic inventory (M4) |
| `nian-ipc` | NDJSON protocol + serve loop | versioned envelopes, size-capped framing; handlers may emit events mid-request (M3) |
| `nian-media` | backend-agnostic facade | `Probe`, `MediaSource`; errors distinguish cancellation vs timeout (M3); packets travel as backend-owned types |
| `nian-media-ffmpeg` | safe FFmpeg wrapper | input/muxer/packet/interrupt/ABI guard; operation-scoped RAII deadlines + typed abort causes (M3) |
| `nian-recorder` | segmented recording engine + supervision | keyframe rotation, durable finalize/publish; camera reconnect supervisor, partial recovery (M3) |
| `nian-ffmpeg-sys` | raw FFI (generated) | committed bindings from vendored 8.0.3 headers |
| `nian-platform-windows` | isolated Win32 desktop boundary | M7 suspend/resume notifications and kill-on-close Job Object worker containment |
| `apps/nian-desktop` | Tauri 2 host | single-instance/tray/autostart/lifecycle owner, platform app-data + native credentials, playback/live loopback HTTP owner, thin typed commands |
| `apps/nian-media-worker` | media process | `probe` CLI, `run` IPC with `recording.*`, bounded `camera.probe`, M6 `playback.prepare`, M11 `live.*`, manual `record` smoke command |

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
while Desired stays On. M6 playback/timeline semantics are unchanged. Live view was
outside M7 and is added later as the separate transient M11 subsystem; thumbnails
remain out of scope. See ADR-0008, ADR-0010 and ADR-0014.

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

M7 originally introduced authoritative settings schema v2 with `launch_at_login`
and one `recording_enabled` camera. A partial unique index deliberately enforced the
then-current single-camera desired-recording constraint while lifecycle semantics
were hardened. M9 supersedes only that single-camera constraint with schema v3 and
per-camera recording ownership; the M7 admission, Desired-vs-Runtime and lifecycle
ordering rules remain authoritative. See the M9 section below and ADR-0012.

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

## Linux and Windows distribution with signed updates (M8)

M8 now defines two required production release candidates from the same mirrored
Forgejo tag and application revision. Linux uses `x86_64-unknown-linux-gnu` as an
AppImage. Windows uses `x86_64-pc-windows-msvc` as an NSIS installer on the explicit
GitHub-hosted `windows-2022` runner. Linux remains the already validated release
target; Windows is implemented in the release graph but is not marked validated
until the hosted Windows tag path completes successfully. macOS remains deferred.

The Linux AppImage contains the desktop host, a sibling `nian-media-worker`, and an
application-owned FFmpeg 8.0.3 shared runtime. The pre-bundle worker is linked with
relative RUNPATH `$ORIGIN/../lib/nian-vision`; Tauri normalizes it to
`$ORIGIN/../lib` inside the image, where the required FFmpeg SONAMEs are installed
in private `/usr/lib`. Staging and extracted-AppImage smoke prove ABI 62/62/60 and
media fixture behavior with development overrides removed.

The Windows runtime is built from the same SHA-256-pinned FFmpeg 8.0.3 archive with
`--toolchain=msvc`, shared libraries enabled and GPL/nonfree/static output disabled.
The MSVC FFmpeg build emits `avformat.lib`, `avcodec.lib` and `avutil.lib` import
libraries for the Rust `x86_64-pc-windows-msvc` link while the installed runtime uses
`avformat-62.dll`, `avcodec-62.dll`, `avutil-60.dll` plus any mechanically discovered
application-local VC runtime closure. `dumpbin /dependents` recursively validates
the worker and FFmpeg DLLs; dependencies must resolve from the staged application,
a copied Visual C++ redistributable DLL, an API-set, or Windows System32. MSYS2,
vcpkg, repository target directories and developer PATHs are never runtime
authorities. Tauri's Windows resource directory is the executable directory, so the
DLLs and sidecar follow the normal application-local Windows loader model without
global PATH or System32 mutation.

Desktop and worker share the accepted application-version HELLO contract in both
packages. IPC protocol version and FFmpeg ABI remain independently authoritative.
Clean staged runtime smoke on each platform verifies HELLO, application version, ABI
62/62/60, fixture `camera.probe`, fixture `playback.prepare` and clean shutdown.

Windows packaging uses NSIS current-user installation and Tauri's normal WebView2
`downloadBootstrapper` policy. The actual installer is smoke-tested in an isolated
test root: installed desktop/worker/DLL bytes must match the exact bundle inputs,
the worker media fixture smoke runs without FFmpeg development overrides, and the
desktop must reach backend readiness. A CI-only diagnostic seam additionally proves
the installed desktop successfully registered the real `nian-platform-windows`
power subscription and that the accepted kill-on-close Job Object reaps the exact
installed sibling worker after hard desktop death.

The Windows install smoke also creates authoritative M7 settings through the real
`nian-settings` API, deliberately seeds a stale launch-at-login executable path,
reinstalls the candidate, and proves camera configuration, credential reference,
`recording_enabled`, user-selected footage root and footage bytes survive. Startup
reconciliation must repair the Windows Run entry. Silent uninstall removes packaged
binaries and stale autostart registration while leaving settings and footage intact.
No downgrade migration is introduced.

Updater ownership stays Rust/Tauri-owned. React receives only the narrow update
commands. Update download/signature verification happens before M7 terminal teardown;
persisted desired recording intent remains authoritative and normal startup restore
continues after update. The public updater trust root is shared across Linux and
Windows.

Release signing is isolated from compilation. `build-linux` and `build-windows` are
peer jobs with no protected release environment. They run frontend lifecycle code,
FFmpeg compilation and ordinary Rust/media tests, then emit unsigned build inputs.
`sign-linux` and `sign-windows` run separately in the protected
`production-release` environment and do not run pnpm/Vite lifecycle commands. They
apply the Tauri updater signature to the exact final platform artifact and verify it
with `nian-release-verifier`.

Windows has a separate Authenticode trust boundary from the Tauri updater signature.
When PFX credentials are configured, Windows SDK `signtool` signs and then verifies
`nian-desktop.exe`, `nian-media-worker.exe` and the final NSIS installer before the
Tauri updater signature is generated. When credentials are absent, the platform
manifest explicitly records `authenticode_signed: false`; the repository variable
`REQUIRE_WINDOWS_AUTHENTICODE=true` makes verification fail closed. Authenticode
private material and updater private keys are step-scoped and never serialized into
Tauri config or build metadata.

Platform signing jobs emit `linux-release-candidate` and
`windows-release-candidate`, each with a platform manifest fragment. Neither job is
authoritative for public updater metadata. `verify-release` requires both candidates,
revalidates tag/version/commit identity and both updater signatures, then assembles
one `latest.json` using the Tauri `linux-x86_64` and `windows-x86_64` keys, one
multi-platform `release-manifest.json`, one `RELEASE_NOTES.md` and one global
`SHA256SUMS.txt`. Every updater URL names the exact tagged GitHub Release asset.

Forgejo remains the authoritative source and normal push/PR/quality CI platform.
GitHub remains a one-way release mirror. Mirrored release tags are checked against
`GITHUB_SHA`, the configured mirror actor and default-branch reachability before any
build starts. Workflow permissions default to `contents: read`; only
`publish-release` receives `contents: write`. Publication keeps the accepted
draft-first boundary: upload all Linux, Windows and shared assets, download them
back, compare the exact filename set and bytes, verify global SHA-256 sums, then
publish. Drafts are not advertised by the stable
`releases/latest/download/latest.json` endpoint.

See ADR-0011 and `docs/releasing.md` for the release contract.

## Simultaneous multi-camera recording (M9)

M9 replaces the desktop-global recording run with a bounded coordinator keyed by
`CameraId`. Every live camera slot owns an independent stop signal, thread handle,
status and `RecordingRunner`; production creates a distinct `WorkerSupervisor` and
`nian-media-worker` supervision tree per active camera. Finished threads are joined
and removed independently, while terminal status may be retained outside the live
slot map. The current explicit capacity is eight simultaneous recording slots.

Settings schema v3 removes the M7 partial unique index while keeping
`recording_enabled` on each camera row. Multiple cameras may therefore be Desired=On.
The v2→v3 migration preserves existing desired state and is transactional. Desired
cameras are read in deterministic CameraId order. Interactive Start/Stop mutate only
their target camera; tray `Stop All Recordings` first clears every desired flag in one
settings transaction and signals runtime slots only after that commit succeeds.

Startup and Resume restore desired cameras independently in deterministic order.
Credential/configuration/worker/capacity failure for A surfaces as A-specific Failed
state and does not prevent unrelated B from starting. Excess desired cameras remain
Desired=On when the capacity boundary is reached. Runtime state is exposed per camera
through camera-scoped status plus a deterministic status collection; neither React
nor the tray invents one synthetic global recording state. Recording-critical global
settings remain immutable while any slot is active.

Lifecycle fan-out preserves the M7 ordering contract. Suspend, Quit and signed-update
teardown close admission and signal every recording slot before joining the first
one. Suspend/Resume does not rewrite desired intent; update teardown likewise leaves
all desired flags authoritative for the next normal startup. Windows keeps every
worker in the desktop Job Object and Linux keeps ordinary child ownership/reaping.

`CameraLease` remains per-camera cross-process writer/recovery authority. Separate
leases for A and B may coexist, but a second writer/recovery owner for A still fails.
Retention stays global-storage aware: active partials from every camera are excluded,
settled finals remain eligible, and playback pins continue to protect only their
specific finalized recording paths. Old finalized A/B recordings remain playable
while A and B are concurrently recording new segments. See ADR-0012.

## ONVIF discovery and provisioning (M10)

M10 adds ONVIF only as a local discovery/provisioning layer. It does not change
recording transport or persistence: a successfully provisioned camera is still an
ordinary `CameraConfig` with a structured RTSP host/port/path plus a native
credential reference, and recording still enters the existing media-worker/FFmpeg
path. Manually configured RTSP cameras remain independent of ONVIF availability.

`nian-onvif` owns untrusted network protocol handling. WS-Discovery sends bounded
multicast probes on practical non-loopback IPv4 interfaces, tolerates malformed
datagrams, deduplicates repeated endpoint identities and caps device/XAddr/scope
collection. SOAP responses are body/depth/text/count bounded; DTD/custom entity
expansion and recognized-field namespace spoofing are rejected. HTTPS discovery and
service candidates are preferred ahead of HTTP when the same trusted local authority
offers both. HTTP redirects are disabled, HTTPS uses the normal rustls certificate
verifier, and service/stream authorities are validated before use.
Authenticated stream URIs are sanitized immediately: userinfo never crosses into a
DTO, log, error or persistent camera row.

Authentication credentials are transient. React submits username/password only to
the connect command; `OnvifController` keeps the validated credentials in a Rust
session and exposes opaque `session_id`/`device_id` handles plus safe device/profile
metadata. Discovery XAddrs and raw SOAP/XML never enter React. A previously unseen
authority is first probed without credentials so an HTTP Digest challenge can be
negotiated; Digest is preferred when available. WS-Security UsernameToken
PasswordDigest is the legacy fallback when the camera does not expose a usable Digest
path. Only the selected authentication mode is cached per authority, never credentials
or reusable challenges. The protocol crate itself owns no secret persistence.

Device interrogation retrieves Device Management information and service endpoints,
prefers Media2 and falls back to legacy Media for profile enumeration. H.264 is the
only M10 recording-compatible video codec. Multiple H.264 profiles remain visible
for explicit user choice; H.265/other profiles may be displayed as unsupported but
are never silently selected or transcoded. `GetStreamUri` is resolved only after a
profile choice, then normalized into safe RTSP host/port/path fields.

Provisioning is deliberately two-phase. The desktop resolves an opaque ONVIF
session/profile into an unsaved `CameraDraft`, releases lifecycle admission locks,
uses the existing `ProbeController`/media worker to prove that the resulting RTSP
source really opens, then reacquires admission and re-resolves the same session
before `CameraService::create_camera`. A refresh/cancel/suspend/quit/update during
the probe therefore makes the handle stale and prevents persistence. Final credential
write + settings insert uses the existing credential-reference allocation and rollback
semantics; ONVIF does not introduce a second camera database or secret store.

Discovery/probe work runs off the Tauri main thread and does not hold recording
controller or global lifecycle locks across network I/O. Suspend, Quit and update
close ONVIF admission and clear transient sessions; Resume reopens admission but does
not restart discovery or resurrect credentials. One failed discovered device does not
poison another device/session operation.

M10 explicitly excludes PTZ, presets, events/motion subscriptions, talkback, live-view
redesign, H.265 recording, transcoding, cloud/remote discovery and automatic camera
adoption. See ADR-0013.

## Independent multi-camera live view (M11)

M11 is a transient subsystem beside recording, not another recording mode.
`CameraService::prepare_live` resolves an existing camera plus native credential reference
entirely inside Rust. The authenticated RTSP source is handed only to a dedicated live
worker; React receives camera identity, typed state, an opaque UUID session id and an
ephemeral `http://127.0.0.1:<port>/live/<uuid>` capability. No settings schema or persisted
live-layout state is added.

`LiveViewController` admits at most four opening/active cameras and at most one live owner
per camera. Admission reserves camera/capacity under a short registry lock. The worker
process is then spawned outside that registry lock and installed immediately into a
controller-owned `OpeningState` before hello or `live.start` is awaited. `OpeningState`
owns the cancellation flag, runner, temporary session directory and completion condition,
so hide/suspend/quit/update can enumerate, cancel and reap startup work even while the IPC
handshake is blocked. Admission/started RAII handles remain the fallback that releases
reservations and temporary resources on task failure. A cancelled/stale opening cannot
commit or erase a newer reservation.

The worker accepts `live.start`, `live.status` and `live.stop`, requires H.264 and
packet-copies video only. Instead of one growing MP4, it writes independently finalized
fragmented-MP4 files beginning on keyframes. Production limits are: four live sessions, a
2-second fragment target, a six-fragment retained target per session (nominally about a
12-second live window), an eight-finalized-fragment hard ceiling that accounts for two
possible reader pins, 16 MiB maximum per fragment, two HTTP readers per session and eight
concurrent live HTTP requests globally. The worker backpressures at a keyframe boundary when
the hard fragment-count ceiling is full, uses byte pressure as a second rotation trigger,
and fails pathological media that cannot stay within the hard fragment-size bound; no
decode/transcode path is introduced. Transient source/media loss retains the bounded
1/2/4/8/15-second, five-attempt reconnect policy.

The application reaper continuously trims finalized fragments. Every fragment HTTP request
acquires explicit reader ownership, and trimming skips reader-owned files. Reader release
retriggers trimming. A fragment is capped before loading and at most one bounded fragment
is copied into memory for an HTTP response. Session teardown has a bounded reader-drain
deadline; it never truncates/deletes a file underneath an active reader, and the last reader
performs deferred directory cleanup after capability invalidation when necessary.

The live-cache root is transient app-data. At application startup only direct children with
the canonical `session-<uuid>` owned layout and real-directory type are eligible for stale
cleanup; lookalikes, symlinks and unrelated files are preserved. Cleanup failure is logged
without widening deletion authority.

The loopback server remains bound only to `127.0.0.1`. Session routes expose only the small
manifest and fixed-grammar fragment names under the opaque UUID. Requests cannot supply a
camera URL or filesystem path. GET/HEAD, Host/Origin validation, UUID/fragment grammar and
request/header/reader caps keep the endpoint a narrow capability. The desktop CSP permits
`connect-src` only from self plus loopback HTTP and permits media only from self, `blob:`
and loopback HTTP. React uses `MediaSource` to poll the manifest, fetch unseen fragments,
append H.264 MP4 data and trim older buffered media. It tracks only the highest successfully
appended fragment sequence, so frontend bookkeeping stays O(1) for arbitrarily long sessions.

Keepalive is deliberately cheap: `live_keepalive` validates one active session and updates
its timestamp only. The two-minute timeout and background reaper own expensive expiry and
teardown. `live_open`, `live_close` and `live_statuses` move blocking worker/filesystem work
to Tauri `spawn_blocking`; keepalive does not.

Teardown is two-phase. First, HTTP capabilities are invalidated and every in-flight opening
and committed runner receives cancellation/stop. Only after all stop signals have been
issued does bounded fan-out join/reap workers and remove session resources. A slow camera
therefore cannot delay cancellation delivery to the other cameras. Close-to-tray stops live
admission, hides the window promptly, and performs teardown on the blocking runtime.
Suspend/quit/update do not report required lifecycle completion until in-flight and active
live workers have been reaped. Recording Desired/Runtime ownership remains untouched by
window-hide live cleanup and Resume only reopens live admission.

The Live View UI uses per-camera generations plus mounted/selected/session refs as
authoritative ownership across `await` boundaries. Remove, unmount, retry and media error
invalidate the generation first. A late `live_open` result that no longer matches current
ownership is immediately `live_close`d, never enters React session state and therefore
never joins the keepalive set. A newer generation queued behind a pending open cannot be
replaced by the stale result.

Recording slots and live slots remain separate; the same camera may record and view live
using independent RTSP workers. PTZ/events, H.265 live view, transcoding, WebRTC, remote
streaming, motion/AI and persisted live layouts remain outside M11. See ADR-0014.

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
request bounded recorder/probe/playback interruption and clear transient live sessions.
Resume returns admission to `Running`, independently resynchronizes playback storage,
rejoins any stopping recording controller, restores persisted desired recording through
the normal start path, reopens probe/live admission and does not resurrect old live
session ids. One subsystem's resume failure is surfaced
without preventing the remaining subsystems from converging.

## Documentation index

* ADRs: `docs/adr/` (resilience model: ADR-0007)
* FFmpeg specifics: `docs/ffmpeg.md`
* Development setup: `docs/development.md`
* Linux releases/updater: `docs/releasing.md`
* Testing: `docs/testing.md`
