# Testing strategy

Testing is part of the definition of done for every milestone (master spec
§17) — not a final-phase activity.

## Current layers

### Unit tests (in-crate, always run)

| Crate | Covers |
|---|---|
| `nian-domain` | camera-id path safety, credential redaction, URL encoding, retention validation, quota watermarks, backoff schedule, time-base math |
| `nian-application` | config validation bounds, UI-safe error messages; stub-worker supervision matrix: crash-before-hello retryable, wedged-hello deadline-bounded, version mismatch permanent, transient refusal retries, configuration refusal stops, failed-job-observed-while-alive, shutdown interrupts backoff waits |
| `nian-application` (M5) | camera CRUD/service validation, versioned credential-ref transaction failure boundaries, safe DTO/Debug output, storage-settings validation, single-active `RecordingController` transitions and probe admission |
| `nian-settings` (M5) | schema v1 creation, reopen persistence, stable CameraId across rename, duplicate rejection, future-schema preservation, migration rollback, validated HIGH/LOW quota round-trip + corrupt/mismatched quota rejection, camera delete leaves footage untouched, password sentinel absent from DB bytes |
| `nian-application` (M4) | `StorageManager`: reconciliation idempotency + fail-closed retention gate, startup/runtime corruption repair, convergent SQLite-family quarantine, lease-aware partial classification, whole-second incremental finalized upsert, age/quota OR semantics + target observability, settled-recovered retention during active recording, recovered transaction commit revalidation, filesystem-first crash convergence, conservative artifact cleanup |
| `nian-index` (M4) | schema v1 migration/reopen, future-version refusal, migration rollback, verified WAL + `foreign_keys=ON`, timeline index, idempotent upsert/query, atomic snapshot replacement, random-byte/runtime corruption classification |
| `nian-index` (M6) | complete-only camera range queries, start-inclusive/end-exclusive boundaries, normal + recovered ordering, same-second sequence ordering, available days, previous/next, duration writeback fenced by stable filesystem identity |
| `nian-application` (M6) | playback path revalidation, explicit filesystem→index refresh, normal/recovered freshness with active-partial exclusion, session expiry/token handling, full/middle/suffix HTTP Range, 416/403/410 transport failures, rebuild-stable recording identity, lazy duration enrichment, symlink rejection, cross-process playback-cache instance locking, playback-pin retention skip and deterministic plan→pin→delete race closure |
| `nian-application` / desktop (M7) | lifecycle admission (`Running`/`Suspending`/`Quitting`), persisted desired-recording restoration, lifecycle-owned recorder shutdown/join, probe cancellation/admission, playback suspend/resume/shutdown, single-instance activation policy, close-to-tray, autostart reconciliation/rollback, suspend/resume convergence and deterministic Quit ordering |
| `nian-storage` | recordings layout, partial/final naming round-trip, traversal rejection, exclusive claims (incl. sub-second clock regression), no-replace publication (success, collision refusal, recoverable abandoned partials) |
| `nian-storage` (M3/M4) | partial-file classification plus deterministic exact-grammar filesystem inventory; normal + recovered first-class recordings; foreign/control artifacts excluded; recording-looking symlinks never followed; shared strict recovery-tombstone v2 validation; typed path-presence semantics where only `NotFound` proves absence; shared whole-second filesystem identity normalization |
| `nian-ipc` | envelope round-trips, framing limits (1 MiB cap, CRLF, truncation), dispatch loop (ping/describe/shutdown/unknown), protocol version guard, handler event emission through the writer before replies (M3) |
| `nian-media` | RTSP URL redaction invariants |
| `nian-media` errors (M3) | timeout vs cancellation category matrix: every error maps to exactly one typed `FailureCategory`; retryability is exactly the source-side set; local output/storage/config never loops |
| `nian-recorder` | stream-plan selection, ceiling target-to-ticks conversion, read-only rotation decision + transactional clock commits, teardown policy (poison/cancel ⇒ never publish); fault injection runs over the real pipeline |
| `nian-recorder` stop wiring (M3 rem.) | stop flag handed to every open_session and baked into fresh sessions by construction; race tests: stop during recording/connecting/backoff/concurrent failure — one graceful press suffices |
| `nian-recorder` matrix (M3 rem.) | source-kind-aware retryability: RTSP open/read/timeout retry, file rows permanent; SegmentFinalized events are the authoritative cross-attempt counter (failed attempts still count) | (never sleeps the real 60 s tail): file EOF completes without backoff, RTSP EOF means connection-lost and reconnects, retryable failures follow the exact schedule 2s/5s/10s/30s, cancellation skips reconnects, output-write failures fail supervision immediately, permanent open failures fail without retry, timeout classification, ordered StateChanged chains, seeded jitter stays within ±half deterministically, saturating jitter composition |

### Media integration tests (`nian-media-ffmpeg/tests/`)

Run real FFmpeg through the safe wrapper against deterministic fixtures
(`tests/fixtures/`, generated by `scripts/generate-fixture.sh` from synthetic
lavfi sources; the H.264 playback asset uses libx264 only at fixture-generation
time, never in Nian Vision runtime code):

* `sample.mkv` (2 s video), `sample_av.mkv` (2 s video+audio) for the media
  layer;
* `session_av.mkv` (30 s, keyframe every 2 s, video+AAC) as the recorder's
  segmentation source;
* `reordered_av.mkv` (audio listed before video), `bframes_av.mkv`
  (`-bf 2` decode-order coverage), `playback_h264.mkv` (4 s H.264 + AAC,
  one-second GOPs for M6 packet-copy playback) and `audio_only.mkv`.

Covered: runtime ABI matches compiled bindings; probe reports
format/streams/duration; packet reads are keyframe-first with monotonic DTS;
stream-copy remux produces independently probeable files; per-packet
keyframe patterns and side data survive the copy; cancellation aborts open
as `Interrupted`; an expired deadline aborts open as `TimedOut` — distinct
typed causes (M3). M6 additionally packet-copy remuxes the committed H.264/AAC
fixture to fragmented MP4, proves the output remains H.264/AAC with sane duration,
and asserts a global `sidx` is present for byte-range random access.

### Recorder integration tests (`nian-recorder/tests/`)

Deterministic end-to-end recording over the fixtures above — no physical
camera in CI. Segments are inspected through the safe packet/probe API:

* a continuous source rotates into multiple independently valid segments
  whose names are canonical and unique, with no leftover partials;
* every segment starts on a video keyframe taken from a source keyframe
  instant; consecutive segments never overlap and the boundary keyframe
  belongs to the new segment; a full session reproduces the source video
  stream exactly once;
* rotation timing respects the target within one GOP (measured via
  DTS-derived spans AND cross-checked against `SegmentFinalized`
  bookkeeping within packet-duration tolerance);
* startup alignment discards exactly the packets before the next video
  keyframe (pre-drained input) and drops leading audio;
* graceful stop finalizes the mid-GOP tail without waiting for a keyframe;
  stopping before any keyframe claims nothing;
* an audio-only source is rejected (`NoVideoStream`); a reordered source is
  recorded from video stream index 1 (index 0 never assumed);
* forced cancellation abandons partials and never publishes;
* fault injection at the I/O boundaries (in-crate runs over the real
  pipeline): mux/output write failure poisons the segment — abandoned,
  never published, original error returned — while a demux read failure
  salvages and publishes the healthy prefix when finalization succeeds;
* segment durations are validated from the stored packets themselves
  (first/last video DTS via the segment time base);
* B-frame fixture proves decode-order progression: PTS reordering neither
  triggers nor blocks rotation, DTS stays monotonic across boundaries with
  no packet lost or duplicated.

### Recovery integration tests (`nian-recorder/tests/recovery_integration.rs`, M3 §12)

Real FFmpeg, committed fixtures copied into temp trees (never corrupted in
place): zero-byte partials quarantined untouched; header-only/garbage
payloads refused by demux but preserved; truncated-but-readable crash
partials remuxed into independently probeable recordings (original removed
only after durable publication); finalized content stuck under `.partial`
names recovered without fakery; pre-existing finals survive byte-identical;
a mixed-class run handles each class without cross-contamination; claim
failures preserve originals; non-canonical subtrees/names are invisible to
recovery.

### Supervisor integration tests

* **Real-pipeline camera supervision** (`nian-recorder/tests/
  supervisor_integration.rs`): the camera supervisor above REAL FFmpeg
  sessions survives an injected connect failure through Backoff and then
  records a full multi-segment session, ending as operator stop during the
  post-EOF backoff. Exact expected state-chain asserted.
* **Process-level crash/restart** (`nian-application/tests/
  worker_supervision_integration.rs`, M3 §15): parent supervisor spawns the
  REAL `nian-media-worker` binary, verifies hello, sends
  `recording.start` for a file source, hard-kills the child mid-recording
  (deterministic killer thread that fires as soon as the first partial
  segment is disk-visible — local fixtures remux at hundreds of times
  realtime, so a fixed-delay kill would land after completion), observes
  the crash episode, restarts the worker with bounded waiting,
  re-handshakes, restores desired recording state, and only then delivers
  protocol shutdown once disk-visible progress exists. No sleep-of-faith:
  shutdown armament is driven by published finals appearing on disk.
* **Real-worker protocol rows (final remediation §1/§2/§4, same file)**:
  * file EOF reaches the parent as `JobCompletedCleanly` through the
    canonical `recording.status` shape (result IS the JobStatus object);
  * a permanently un-openable source ends as a terminal `failed` status
    with the stable `source_open_failed` wire category observed while the
    process lives → typed `PermanentRecordingFailure`, never a restart;
  * ONE protocol shutdown on an active recording finalizes and publishes
    the healthy active segment (zero abandoned partials) and the worker
    exits with status 0.

### Stub-worker supervision rows (`nian-application/tests/worker_supervisor_stubs.rs`, M3 rem. + final rem.)

Shell-script workers driven through the REAL coordinator with
millisecond-scale injected deadlines; every fixture replicates the real
worker protocol exactly (canonical status shape, `{"started":true}` start
acks, id-echoed `{"bye":true}` shutdown acks, snake_case failure
categories): crash-before-hello → retryable; wedged hello → bounded
unhealthy; protocol-version mismatch → permanent; `start_failed` refusal
→ transient retry; `storage_unavailable`/`invalid_params` refusals →
permanent (spawn-count proves no second worker); terminal
`storage_failed` and `camera_in_use` job statuses → typed permanent errors
and NO respawn (`run_forever` + spawn counter); start-ack-then-total-silence →
`Unresponsive{phase:"monitor"}` within the missed-poll bound, then
restartable; backoff waits interrupted by operator shutdown.

### Recovery idempotency + classification (final rem. §5/§7 + final correctness §1/§2/§5/§7)

* Deterministic recovery identity tests (`nian-recorder`): a surviving
  original whose deterministic final already exists is reported
  `AlreadyRecovered` WITHOUT remuxing (original bytes untouched, exactly
  one recovered final); a repeat pass after a forced cleanup failure
  yields `finals_after == finals_before`; tombstone persistence failure is
  observable (`tombstone_recorded=false`) without invalidating the
  recording; poisoned/metadata-failed passes remove only recovery-owned
  scratch and never publish.
* Typed classification: a camera tree that cannot be scanned (camera path
  occupied by a file) surfaces `RecoveryError::Infrastructure` with
  `is_infrastructure()==true` — the worker-level permanent-job signal —
  while content failures quarantine per file. A held camera lease instead
  yields `RecoveryError::Ownership` with `CameraAlreadyActive`, and a lease
  supplied for the wrong camera/layout yields `RecoveryError::Ownership` with
  `CameraLeaseMismatch`; both return before scan and both have
  `is_infrastructure()==false`.
* Manual record lease regression (`nian-media-worker`): holder A keeps a live
  canonical partial while holding the camera lease; manual record B uses an
  invalid source but fails on `camera already active` before media open, creates
  no second partial/recovery final/scratch/tombstone, and leaves A's bytes
  untouched. After A releases the lease, the same manual helper opens normally.
* Concurrent recovery (final correctness §1): two process-shaped
  attempts against the same original, synchronized at their publication
  step by a deterministic barrier hook — unique per-attempt scratch
  pathnames, exactly one recovered final, independently probeable, loser
  reports AlreadyRecovered and removes only its own scratch.
* Cooperative graceful stop in recovery (final correctness §2): the
  run-level stop flag is observed at every safe boundary (before each
  partial, before reopen, before scratch claim, between packets, before
  finalize/publication); a pre-publication delay hook holds attempts
  mid-flight so recorder- and worker-level tests prove ONE stop during
  `recovering` ends bounded as `stopped` without a camera connection,
  without publishing, and without a second press; the EXPLICIT
  keyframe-alignment probe checks the stop domains before every packet
  read, with an alignment-gate seam parking an attempt INSIDE the
  alignment phase (recorder- and worker-level: one stop ends the job
  without any camera connection); protocol shutdown ends recovery
  gracefully before its force-cancel grace.
* Tombstone transaction/conflict contract (final safety §1 + identity
  safety §1): a deterministic destination's mere existence is NEVER proof
  — directory, zero-byte, foreign-valid-MKV and malformed/foreign-tombstone
  destinations all yield `RecoveryConflict` preserving BOTH files (no
  remux, no retroactive tombstone, no deletion); a trusted v2 tombstone
  (magic + strict original/final name binding + the PUBLISHED SIZE,
  re-verified against the object CURRENTLY at the final path as a regular
  file) yields `AlreadyRecovered` with a cleanup retry; a tombstone whose
  destination was since replaced (directory / zero-byte /
  different-size file) is a conflict, never a deletion; legacy v1 markers
  are rejected as untrusted; a publish-hold seam deterministically opens
  the winner's not-yet-tombstoned window and proves the loser never
  deletes the original there; concurrent attempts still converge to
  exactly one final, one trusted tombstone, original gone.
* Identity reservation + clock rollback (identity safety §2/§3): the
  allocator marks `(started_at, sequence)` occupied for EVERY Nian-owned
  name (recovered final, tombstone, valid scratch — not only parseable
  segment names), so `08-30-00.recovered.mkv` (or its `.done` alone)
  forces the next claim to `08-30-00-2.partial.mkv`; foreign/Unknown files
  reserve nothing; whole-second granularity still holds. The end-to-end
  clock-rollback regression re-claims the same wall-clock second through
  the real layout after a finished recovery and proves the new footage
  takes a distinct identity, recovers as a NEW transaction, is never
  claimed by the old tombstone, and both recovered recordings stay
  independently identifiable.
* Post-claim identity fence (atomic identity claim §2/§3/§5/§6): a
  day-dir-keyed one-shot gate parks a `claim_segment` AFTER its advisory
  scan chose sequence 1 but BEFORE the candidate is created — the
  deterministic stand-in for a stale non-atomic directory snapshot. A
  recovered final, tombstone, normal final or exact-grammar scratch
  planted inside that window forces the fence to fire: the losing
  candidate is relinquished (removed cleanly, planted object untouched)
  and the claim retries to `-2`; foreign/Unknown files do NOT trigger the
  fence. The end-to-end concurrent-transition regression drives REAL
  recovery (publish → tombstone → remove original) against the racing
  REAL claim and proves sequence 1 is never returned, the old trusted
  tombstone never claims the new footage, and both recovered recordings
  survive independently identifiable.
* Sub-second identity + fence cleanup (sub-second identity §1–§4):
  filesystem identity is explicitly (WHOLE-second local time, sequence) —
  `claim_segment` normalizes the live claim timestamp once and shares it
  across allocation, naming and the post-claim fence. The race regressions
  are duplicated with sub-second timestamps (`08:30:00.123456789` against a
  planted `08-30-00.recovered.mkv` at the storage layer; a full
  concurrent-transition E2E at `2026-08-27 08:30:00.877`) and were proven
  FAILING against the raw-comparison fence before the fix. A fence that
  cannot even validate still never returns its candidate, and a failing
  relinquish now surfaces typed (`StorageError::ClaimFenceCleanup` carrying
  the fence error AND the cleanup error) instead of a silent best-effort
  discard — exercised by a day-directory-loss hook that makes both failures
  real ENOENTs. The two E2E gate users serialize on `FAULT_LOCK` (arming
  the day-dir-keyed gate replaces the single armed slot).
* Alignment read errors vs stop domains (identity safety §4): a
  deterministic read-error seam fails the alignment probe's read after a
  configurable hold — with no stop domain active it is an honest
  `KeptUnrecoverable` content verdict; a graceful stop landing INSIDE the
  failing read classifies as `Cancelled`, never a false "unreadable".
* Tombstone durability (identity safety §6): the marker's bytes are
  synced and (POSIX, best-effort) the parent directory is fsynced for
  directory-entry durability; the contract stays safe if a tombstone
  vanishes after power loss (final without trusted evidence → preserved
  conflict).
* Storage classification (final correctness §6/§7/§8): recovered MKV
  classifies as a recording; scratch/tombstones as artifacts; unknown
  names as Unknown; a stat failure on this attempt's finalized scratch is
  a typed ARTIFACT failure that coexists with continued recording (worker
  test) while scan/claim failures are INFRASTRUCTURE failures that fail
  the job; output-side open/write/finalize failures are ARTIFACT
  failures, never content verdicts (deterministic open-fault and
  finalize-fault seams, nothing ever publishes); the camera-dir
  pre-flight exclusively creates a probe, writes and flushes bytes,
  REMOVES it (removal failure surfaced), retries collisions with a fresh
  probe identity, and fails on genuinely unwritable targets; the scratch
  classifier matches ONLY the exact `<stem>.recovery-<pid>-<serial>
  -<nonce>.tmp` grammar.
* Terminal authority (final correctness §3): terminal Completed/Failed/
  Stopped statuses keep their outcomes (`JobCompletedCleanly` /
  `PermanentRecordingFailure` / `RequestedShutdown`) even when the worker
  never acknowledges its cleanup shutdown — spawn counters prove no
  restart; the wedged worker is killed and reaped.
* Poll ids (final correctness §4): a late response to poll N cannot
  satisfy poll N+1 (episode-local monotonic ids); the missed-response
  bound still classifies the episode Unresponsive.
* Terminal-parse strictness (final safety §7): `finished=true` with an
  unknown/missing `end_kind`, or `failed` with a missing/non-canonical
  `failure_category`, is a PERMANENT protocol violation (typed error,
  spawn counter stays at 1) — never silently "still running", never an
  invented `Unknown` category; the canonical vocabulary is validated
  against the shared `nian_domain::FailureCategory`.
* Stable wire values (final safety §8): `SupervisorState::as_str` is the
  explicit status vocabulary (exact strings, unit-tested), replacing the
  Debug-then-lowercase conversion in the worker's status fold.
* Worker job lifecycle (`nian-media-worker` job tests): prompt
  `recording.start` ack with `recovering` state before any media work;
  async recovery accounted once per job; corrupt leftover quarantined
  (`failed>=1`, `infrastructure_failures==0`) while the new recording
  still completes; occupied day-dir path fails the job terminally with
  `storage_failed`; idempotent `request_graceful_stop` never consumes the
  press counter; shutdown during recovery ends bounded without
  connecting.

### Storage/index integration tests (M4)

`nian-application/tests/storage_reconciliation.rs` and
`storage_retention*.rs` build temporary canonical recording trees and exercise
the filesystem/SQLite boundary without FFmpeg or an installed `sqlite3` CLI:

* normal + recovered files are indexed first-class; changed sizes update,
  missing files remove stale rows, and an unchanged second pass has zero DB
  mutations;
* canonical partials are recovery-pending only when a temporary camera lease
  can be acquired; a held kernel lease makes them active and untouched;
* deleting SQLite and calling rebuild restores finalized recordings with
  unknown duration left NULL; random/corrupt DB bytes are quarantined and
  rebuilt without touching media; corruption injected after manager startup is
  automatically repaired from filesystem truth, and an interrupted DB/WAL/SHM
  quarantine converges before the next canonical database open;
* a failed reconciliation clears the retention-ready safety gate; a later
  successful reconciliation restores it;
* incremental finalized upserts accept sub-second event timestamps for normal
  and recovered filenames but persist the canonical whole-second filesystem
  identity;
* age-only, quota-only and combined OR retention are deterministic under an
  injected local naive clock; quota triggers only above HIGH and deletes
  oldest-first until LOW; blocked candidates do not stop later eligible cleanup,
  and the report exposes trigger state, usage before/after and whether LOW was
  actually reached;
* partials/control artifacts never become quota candidates, while recovered
  recordings do consume recording bytes;
* a live camera lease does not block deletion of an old normal finalized MKV or
  an old fully settled recovered transaction; the same lease still protects
  unresolved partial/scratch state;
* unresolved/malformed recovered transactions are preserved. Only `NotFound`
  proves original-partial absence; deterministic PermissionDenied/generic IO
  metadata faults are inspection failures and preserve the recovered final;
* a settled v2 transaction is revalidated immediately before commit and removes
  recovered final, then reverified tombstone, then index row;
* filesystem delete failure preserves both media and its DB row; a file-first
  crash boundary (media gone, stale row left) converges on the next
  reconciliation;
* recording-looking symlinks are neither counted nor followed; stale recovery
  scratch cleanup requires the matching camera lease and remains separate from
  retention accounting; stale tombstone metadata inspection errors preserve the
  marker rather than treating the path as absent.

The M4 migration tests use `rusqlite` directly for user-version, verified WAL +
foreign-key startup invariants, timeline-index and rollback assertions. No test
shells out to `sqlite3`, sleeps for age cutoffs or depends on changing the
machine timezone.

### Desktop camera-management integration tests (M5)

M5 adds keychain-independent tests around every cross-store failure boundary.
`camera_service.rs` injects fake settings, credential stores and deterministic
credential-ref generators to prove: UUID-v4 production refs use the stable
`nian-vision/<camera-id>/<uuid-v4>` grammar; a replacement candidate equal to the
committed ref is retried or fails before any secret write; failed update rollback
deletes only the distinct ref allocated by that transaction; and a duplicate
create with a forced ref collision, including a race-shaped hidden pre-check,
never overwrites or deletes the winning committed secret. A
new secret written before a failed camera insert is cleaned up; a failed update
keeps the old credential ref authoritative; a committed update survives failure
to clean the old ref; and a committed delete stays deleted when secret cleanup
fails. Active recordings reject critical edit/delete while display-name-only edit
keeps the same `CameraId`. Password sentinels are absent from safe list DTOs,
Debug output, and the complete M4 recording-index SQLite family (`.sqlite3`, WAL,
SHM) even when the application service holds the secret. Unsaved probes use form
credentials without persistence; edited
probes can reuse a committed secret without exposing it to the caller.

`recording_controller.rs` uses fake runners, no camera/network, to assert
Starting→Recording, graceful Stopping→Stopped, permanent Failed, second-camera
rejection and secret-free desired-recording Debug output. `ProbeController` has a
blocking fake-runner regression proving a concurrent second probe is rejected as
`Busy` rather than launching another worker. Existing M3 real-worker
crash/restart tests remain the authority for restart semantics underneath this
controller.

`apps/nian-media-worker/tests/worker_integration.rs` drives the real stdin/stdout
protocol against committed media fixtures. `camera.probe` reports a video stream
for local media, unreachable RTSP produces a bounded typed source failure, probe
creates no recording artifacts, and existing sentinel-credential output tests
continue to prove native FFmpeg diagnostics do not leak secrets. The worker is
spawned as only `nian-media-worker run`; credential-bearing source data travels
through framed stdin, never argv or environment.

M6 extends the same real-worker harness with `playback.prepare`. The H.264/AAC MKV
fixture is inspected and packet-copy remuxed into a temporary fragmented MP4; the
test asserts codec/resolution/audio/duration metadata, `seekable=true`, and a real
global `sidx` atom. A malformed MKV returns the stable `media_unreadable` category.
No test or production playback path launches an `ffmpeg` CLI process; the CLI is
used only by `scripts/generate-fixture.sh` to regenerate committed synthetic test
assets.

`nian-application` playback tests inject a tiny fake media backend so transport and
filesystem safety remain deterministic and fast. They cover full/middle/suffix
Range responses, invalid Range → 416, non-loopback Host → 403, unknown/expired
token → 410, session timeout releasing its pin, active-request deferral so idle
expiry/explicit close cannot release a pin mid-response, and explicit keepalive
ownership. Keepalive tests mutate `last_activity` directly to prove near-expiry
refresh, no resurrection after TTL, rejection of `close_requested`, and retention
skipping a heartbeat-kept pin until heartbeats stop and the same session expires.
Recovered/normal ordering, duration enrichment, rebuild-stable recording IDs, missing files and
symlink replacement. Freshness coverage starts with only A indexed, publishes
canonical normal B plus a recovered final and an actively leased `.partial.mkv`,
then proves `refresh_index()` discovers only the two finalized additions without
probing duration. The cache ownership regression spawns a child process that owns
an instance lock and session/media, signals READY, then proves cleanup from a
second process leaves it intact until the holder is terminated; the next cleanup
acquires the abandoned instance lock and removes it. No PID/timestamp ownership
or arbitrary sleeps participate in correctness. Retention tests use a
deterministic pre-delete TEST GATE: retention
selects the candidate, playback pins it, the gate resumes, and immediate commit
revalidation skips deletion even on Unix where unlinking an open file is legal.

Production `playback_open` responsiveness is audited separately from the fake
backend tests: worker preparation has a 60-second media timeout plus a bounded
host response margin, `WorkerGuard` is created immediately after spawn and
kill/reaps on every error/timeout path, and the existing failed-prepare regression
proves cache-session and playback-pin RAII cleanup after preparation failure.

The production native credential store is not exercised in CI. `CredentialStore`
fakes/in-memory implementations keep Linux/macOS CI independent of a logged-in
graphical keychain.

### CLI/IPC smoke checks (manual, seconds)

```bash
./target/debug/nian-media-worker probe <file>
printf '{"type":"request","v":1,"id":1,"method":"ping","params":null}\n' \
  | ./target/debug/nian-media-worker run
```

The `recording.*` IPC namespace can be smoke-driven interactively:
`recording.start` (file source) → poll `recording.status` → second start
must refuse `job_already_active` → `shutdown` after segments appear.

### Frontend (`ui/`)

Vitest + Testing Library covers camera management without a physical
camera/network: empty state, create validation, list rendering, password never
rendered after save, unsaved Test Connection loading/result, Start/Stop state
transitions from backend responses, backend error display, delete confirmation
and critical-field/delete lock while recording. Storage/settings tests cover the
paired HIGH/LOW watermark requirement, LOW < HIGH validation, and a valid persisted
update through the typed Tauri client boundary.

M6 timeline tests mock only the typed Tauri boundary and cover: no-recordings
state; chronological normal/recovered rendering; explicit known gaps; unknown
duration; playback open/loading result; previous/next navigation; EOF warning;
typed stale playback errors that remain visible while the timeline refreshes; and
a retained/deleted recording disappearing after refresh. Remediation coverage
also proves Timeline invokes the explicit `recordings_refresh` boundary, Next
from a 23:59 recording into the next day and Previous from 00:01 into the prior
day both keep the newly opened session/video URL alive, the day selector follows
the adjacent recording, and an HTML media-element error becomes a visible
user-safe state that releases the failed session and allows Reopen. Fake-timer
heartbeat coverage proves a mounted playback session sends `playback_keepalive`
at the 45-second cadence; opening another recording transfers heartbeat ownership;
media error and unmount stop the old timer and request close; backend expiry clears
the dead video/session and offers Reopen; isolated internal keepalive failures retry
silently while three consecutive failures surface one safe warning that clears
after recovery. Lint (`eslint`), `tsc --noEmit`, Vitest and Vite build gate the frontend.

Desktop configuration tests parse the committed `tauri.conf.json` and assert the
CSP contains the narrow playback allowance
`media-src 'self' http://127.0.0.1:*` while not allowing `media-src *`, broad
`default-src http:` or LAN origins. jsdom does not enforce CSP, so this remains a
configuration regression rather than a WebView proof.

The desktop settings transaction tests fault-inject all three phases: candidate
playback storage preparation failure leaves authoritative settings and active
playback storage on A; successful candidate preparation followed by settings
persistence failure also leaves both on A; successful preparation + persistence
then swaps the already-prepared playback storage to B. The success case also
prepares a real camera recording request after the commit and asserts its
`storage_root` is B, proving recording and timeline/playback configuration converge.

### Desktop production lifecycle (M7)

`apps/nian-desktop` regression tests cover window activation order,
close-to-tray vs real Quit, exact `--startup-hidden` handling, truthful lifecycle
activation errors, autostart OS drift/reconciliation plus rollback failure,
launch-at-login-only changes while a recorder or playback session is active,
persisted desired-state restoration/failure visibility, active/Stopping
second-camera Start rejection without desired-intent replacement, queued
Suspend/Resume delivery during startup initialization, duplicate-free
suspend/resume, resume partial-failure convergence, Rust-authoritative tray
Starting/Connecting/Recording/Backoff/Failed projection, Stop-button intent
semantics, tray watcher Shutdown/join, and deterministic teardown ordering with
process exit last.

Power lifecycle seam tests additionally retain/leak the callback-owned Sender to
model Win32 unregistration failure and prove the dispatcher still terminates from
its explicit Shutdown control message. A successful-unregistration model proves
callback state remains reclaimable. The Windows platform crate is cross-compiled
independently to cover native power registration and kill-on-close Job Object worker
containment.

### Linux distribution and updater (M8)

Release-script unit tests cover strict SemVer/tag convergence and mechanically
reject FFmpeg GPL, nonfree and static-link configuration drift. The release
workflow builds the exact SHA-256-pinned FFmpeg 8.0.3 source candidate and runs the
full `nian-media-ffmpeg` fixture integration suite against those libraries before
they are eligible for packaging.

`scripts/release/stage-linux.sh` then exercises the release worker with development
library overrides removed. It verifies the installation-relative worker RUNPATH,
FFmpeg ABI 62/62/60, complete dynamic dependency closure, HELLO application-version
compatibility, fixture `camera.probe`, and fixture `playback.prepare`. The worker
must resolve all three FFmpeg libraries from the staged application-owned runtime,
not system FFmpeg.

Frontend Settings tests cover configured/unconfigured update state, update
discovery, explicit installation confirmation and the update command boundary.
Desktop Rust tests prove update admission blocks new lifecycle work before teardown
and that updater teardown reaches Quitting/Stopped while preserving persisted
Desired recording intent. The production AppImage path additionally extracts the
actual built image and repeats installed-layout worker/media smoke checks before
release metadata and checksums are finalized.

## Planned per milestone

* **M9+**: simultaneous multi-camera orchestration and M10 ONVIF. Windows/macOS
  distribution, live camera viewing, clip export, thumbnails/motion analysis and
  AI/cloud behavior are not part of the Linux-only M8 release milestone.
* **Hardware/manual** (never in CI): real Tapo C200 via
  `NIAN_VISION_RTSP_URL` with
  `nian-media-worker record --rtsp-from-env ...` and/or an IPC-driven
  supervised job (see development.md); checklist in the master spec §17
  (unplug, reboot, sleep/wake, disk near-full, corrupt files). Windows
  run of the MoveFileExW publication path remains manually validated.
* **Windows Tauri/WebView playback CSP smoke** (required for M6 remediation,
  because jsdom cannot enforce CSP): build/run the real desktop app on Windows,
  configure a recordings root containing the committed/known-good H.264 recording
  shape, open Recordings/Timeline, press Refresh, open a finalized recording and
  verify the `<video>` loads from an
  `http://127.0.0.1:<ephemeral>/playback/<uuid>` URL, plays, seeks and issues no
  CSP media-src violation in WebView developer diagnostics. Verify the playback
  server is listening only on 127.0.0.1 and that substituting a LAN-host media URL
  is rejected by CSP/server policy. Close/Reopen once to confirm the failed/closed
  session lifecycle releases cleanly. Record this smoke result with the release or
  review evidence; it is intentionally not faked by jsdom.

## Environment variables

| Variable | Purpose |
|---|---|
| `NIAN_VISION_RTSP_URL` | credential-bearing RTSP URL for manual smoke tests; never logged, never committed |
| `NIAN_FFMPEG_LIB_DIR` | build-time FFmpeg library directory override |
| `NIAN_WORKER_BIN` | optional explicit worker binary path for the process-supervision integration test |
| `RUST_LOG` | tracing filter (worker/desktop default `info`) |
