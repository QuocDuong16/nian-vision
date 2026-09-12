# Testing strategy

Testing is part of the definition of done for every milestone (master spec
§17) — not a final-phase activity.

## Current layers

### Unit tests (in-crate, always run)

| Crate | Covers |
|---|---|
| `nian-domain` | camera-id path safety, credential redaction, URL encoding, retention validation, quota watermarks, backoff schedule, time-base math |
| `nian-application` | config validation bounds, UI-safe error messages; stub-worker supervision matrix: crash-before-hello retryable, wedged-hello deadline-bounded, version mismatch permanent, transient refusal retries, configuration refusal stops, failed-job-observed-while-alive, shutdown interrupts backoff waits |
| `nian-application` (M5) | camera CRUD/service validation, versioned credential-ref transaction failure boundaries, safe DTO/Debug output, storage-settings validation, original single-active `RecordingController` transitions and probe admission |
| `nian-settings` (M5) | schema v1 creation, reopen persistence, stable CameraId across rename, duplicate rejection, future-schema preservation, migration rollback, validated HIGH/LOW quota round-trip + corrupt/mismatched quota rejection, camera delete leaves footage untouched, password sentinel absent from DB bytes |
| `nian-application` (M4) | `StorageManager`: reconciliation idempotency + fail-closed retention gate, startup/runtime corruption repair, convergent SQLite-family quarantine, lease-aware partial classification, whole-second incremental finalized upsert, age/quota OR semantics + target observability, settled-recovered retention during active recording, recovered transaction commit revalidation, filesystem-first crash convergence, conservative artifact cleanup |
| `nian-index` (M4) | schema v1 migration/reopen, future-version refusal, migration rollback, verified WAL + `foreign_keys=ON`, timeline index, idempotent upsert/query, atomic snapshot replacement, random-byte/runtime corruption classification |
| `nian-index` (M6) | complete-only camera range queries, start-inclusive/end-exclusive boundaries, normal + recovered ordering, same-second sequence ordering, available days, previous/next, duration writeback fenced by stable filesystem identity |
| `nian-application` (M6) | playback path revalidation, explicit filesystem→index refresh, normal/recovered freshness with active-partial exclusion, session expiry/token handling, full/middle/suffix HTTP Range, 416/403/410 transport failures, rebuild-stable recording identity, lazy duration enrichment, symlink rejection, cross-process playback-cache instance locking, playback-pin retention skip and deterministic plan→pin→delete race closure |
| `nian-application` / desktop (M7) | lifecycle admission (`Running`/`Suspending`/`Quitting`), persisted desired-recording restoration, lifecycle-owned recorder shutdown/join, probe cancellation/admission, playback suspend/resume/shutdown, single-instance activation policy, close-to-tray, autostart reconciliation/rollback, suspend/resume convergence and deterministic Quit ordering |
| `nian-settings` / `nian-application` / desktop (M9) | schema v3 multi-desired migration + rollback, bounded per-camera recording slots, independent runner/supervisor ownership, camera-scoped Start/Stop/status, transactional Stop All, capacity ordering, failure isolation, multi-camera startup/suspend/resume/update restoration, concurrent admission races, tray/UI aggregation without synthetic global state |
| `nian-onvif` / `nian-application` / desktop/UI (M10) | bounded/cancellable discovery aggregation, hostile XML and authority validation, Device/Media2/legacy Media fixtures, Digest/UsernameToken secret safety, H.264 profile selection, stream-URI sanitation, opaque session invalidation, per-device failure isolation, lifecycle cancellation and ONVIF/manual onboarding UI |
| `nian-application` / media worker / desktop/UI (M11) | four-camera live capacity, duplicate/open reservation RAII, opaque loopback session capability, Host/Origin/path/method rejection, background keepalive expiry/reaper, per-camera worker-status isolation, bounded live retry/cancel, recording/live independence, suspend/hide/update cleanup and reconnect media remount |
| `nian-domain` / `nian-settings` / `nian-onvif` / `nian-application` / desktop/UI (M12) | schema-v4 optional PTZ binding, secret-safe credential reuse/separation/rollback, PTZ service/profile/configuration parsing and authority hardening, capability-gated pan/tilt/zoom, per-camera bounded workers, movement generations, renew/dead-man Stop, lifecycle cancellation/no resume resurrection, pairing/unpairing and frontend pending/stale-generation races |
| `nian-domain` / `nian-settings` / `nian-index` / `nian-onvif` / `nian-application` / desktop/UI (M13) | schema-v5 Event binding + independent Desired monitoring, PullPoint service/subscription authority hardening, standard CellMotion parsing/source hashing, transition normalization/dedupe, bounded event history retention/quarantine, per-camera Event ownership/reconnect, storage-root runtime switch, lifecycle settlement, Pair/Enable/Disable/Unpair and low-frequency motion UI isolation |
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
persisted desired-state restoration/failure visibility, the historical M7
single-camera admission behavior, queued Suspend/Resume delivery during startup
initialization, duplicate-free suspend/resume, resume partial-failure convergence,
Rust-authoritative tray
Starting/Connecting/Recording/Backoff/Failed projection, Stop-button intent
semantics, tray watcher Shutdown/join, and deterministic teardown ordering with
process exit last.

Power lifecycle seam tests additionally retain/leak the callback-owned Sender to
model Win32 unregistration failure and prove the dispatcher still terminates from
its explicit Shutdown control message. A successful-unregistration model proves
callback state remains reclaimable. The Windows platform crate is cross-compiled
independently to cover native power registration and kill-on-close Job Object worker
containment.

### Linux and Windows distribution/updater (M8)

Release-script/tool tests cover strict SemVer/tag convergence, mechanically reject
FFmpeg GPL/nonfree/static-link drift, and validate the hybrid release topology:
Forgejo has no active tag-release workflow, GitHub release automation is tag-only,
every `uses:` action is immutable-SHA pinned, default GitHub permissions are
read-only, only publication receives `contents: write`, and Forgejo quality CI
remains present. Mirror trust checks require `GITHUB_SHA`/tag identity, the configured
mirror actor and default-branch reachability.

Structural tests require `build-linux` and `build-windows` to be independent peers,
with explicit `windows-2022` for Windows. Protected signing jobs are separate from
pnpm/Vite, FFmpeg compilation and ordinary tests. Updater private key material is
confined to the updater-signing steps; Windows PFX/password material is confined to
Windows Authenticode steps. Generated Linux and Windows Tauri configs contain only
public updater configuration and bundle/resource mappings with
`beforeBuildCommand` disabled.

Both platform jobs build the exact SHA-256-pinned FFmpeg 8.0.3 source authority.
Linux keeps its accepted LGPL/shared candidate and media fixture integration suite.
Windows uses the MSVC FFmpeg toolchain, requires shared DLLs plus the MSVC import
libraries, and runs the same media integration surface before packaging. Static and
runtime contracts reject GPL/nonfree/static drift, missing required DLLs, MSYS2 or
vcpkg runtime authority, repository build paths and `NIAN_FFMPEG_LIB_DIR` runtime
dependence.

`scripts/release/stage-linux.sh` preserves the accepted Linux worker RUNPATH/ABI and
fixture smoke. `scripts/release/stage-windows.ps1` recursively inspects the worker, final desktop
and DLL closure with `dumpbin /dependents`. The shared classifier checks application-
local files first, classifies `VCRUNTIME*`/`MSVCP*`/`CONCRT*` as redistributable before
System32, copies them from `VCToolsRedistDir`, and only then accepts API-set/System32
OS dependencies. A deterministic Windows regression simulates a conflicting System32
VC runtime and proves the VC-redist bytes still win. The stage then invokes the cross-
platform worker smoke with a clean Windows environment. Worker
HELLO still proves IPC protocol, application version and FFmpeg ABI 62/62/60 before
fixture `camera.probe`, fixture `playback.prepare` and clean shutdown.

The actual Linux AppImage is still extracted and smoke-tested, then launched under
isolated Xvfb/D-Bus until backend readiness. The actual Windows NSIS installer is
silently installed into a disposable runner-local directory. The installed desktop,
worker and full application-local DLL closure must be byte-identical to the exact
bundle inputs. Installed worker media smoke runs without development FFmpeg
overrides. The installed desktop must reach backend readiness, prove successful real
Windows power-notification subscription, and prove the accepted kill-on-close Job
Object reaps the exact installed worker after hard desktop death.

Windows upgrade/data smoke creates settings through the real `nian-settings` API. It
proves camera configuration plus camera/PTZ/Event credential references and ownership,
Recording Desired, Event Desired/binding, notification preference, `launch_at_login`,
selected footage root and footage bytes survive reinstall. The
second installer run is executed directly while the installed desktop and Job Object
worker are alive; bounded readiness markers prove the desktop is handled by the NSIS
app-running path, the owned worker exits with its desktop, file replacement succeeds
without a global process-name kill, and the new desktop starts afterward. A stale
Windows Run entry is deliberately seeded and startup reconciliation must repair it.
Fresh install must keep launch-at-login off. Silent uninstall must remove application
binaries and stale autostart registration while preserving authoritative settings and
footage.

Updater cryptographic regression vectors continue to prove matching-key success plus
mismatched-key, mutated-artifact and mutated-signature failure through
`nian-release-verifier`. Windows updater signing uses the same verifier/trust root as
Linux. When Authenticode is enabled, `signtool` mechanically verifies the desktop,
worker and installer after signing; when it is disabled the Windows platform manifest
records `authenticode_signed: false`, and the required-policy mode rejects that state.

Platform jobs emit candidate fragments rather than competing public metadata.
Assembly tests prove Linux/Windows version/commit/FFmpeg authority convergence, one
Tauri `latest.json` with `linux-x86_64` and `windows-x86_64`, one multi-platform
manifest and one global `SHA256SUMS.txt`. Mutation tests reject stale Windows artifact
bytes and checksum drift. `verify-release` requires both signed candidates, re-verifies
both updater signatures, scans the combined release boundary, and only then emits
`verified-release`. Draft publication still uploads every asset, downloads them back,
compares the exact filename set and bytes, verifies the global checksums and publishes.
Release-contract tests additionally require prerelease SemVer tags to publish as GitHub
prereleases with `latest=false`, so an RC cannot silently replace the production updater channel.

### Simultaneous multi-camera recording (M9)

Settings tests create/reopen schema v3, migrate real schema-v2 bytes while preserving
the previously desired camera, prove the v2 partial unique index is removed, allow A
and B to persist Desired=On simultaneously, and fault-inject v2→v3 migration failure
to prove both schema version and old index state roll back atomically.

`RecordingController` tests prove two camera IDs create independent runner instances,
duplicate Start is scoped to one camera, Stop A leaves B owned, one camera failure does
not disturb another, finished slots are joined/removed and release capacity, and
lifecycle teardown signals every owned slot before any blocking join. The explicit
capacity is eight live slots; terminal status history does not consume capacity.

Desktop tests exercise camera-scoped Start/Stop/status, deterministic status
collections, transactional Stop All rollback, concurrent Start A+B, duplicate
concurrent Start A+A, deterministic restoration beyond capacity, missing-credential
failure isolation, A+B Suspend/Resume, multi-intent update teardown, and Start races
against Suspend/Quitting admission. Deletion tests prove Desired=On blocks deleting
that camera, while an unrelated recording does not block deleting a stopped camera.
Recording-critical settings still reject changes while any slot is active.

UI tests render independent Desired/Runtime state per camera and prove starting one
row does not disable another row's recording controls. Tray projection tests use
aggregate counts and `Stop All Recordings`; they never fabricate one global runtime
state from incompatible per-camera states.

Storage/playback regressions hold concurrent CameraLease ownership for A and B while
old finalized files exist. Retention must exclude active partials from both cameras,
honor a playback pin on old A, and still delete eligible old B. PlaybackController
must open old finalized A and B while both camera leases are held, proving active
recording ownership does not become a camera-wide read lock.

### ONVIF discovery and provisioning (M10)

`nian-onvif` unit and local HTTP fixture tests cover discovery response parsing,
duplicate endpoint merging, multiple devices, malformed datagram tolerance, empty
bounded collection and cancellation. Protocol fixtures exercise Device Management,
Media2 profile enumeration, legacy Media fallback, HTTPS-before-HTTP endpoint selection,
credential-free HTTP Digest negotiation, UsernameToken legacy fallback, bare
authentication failure, redirect refusal and `GetStreamUri` sanitation including
embedded userinfo, non-default RTSP port/path and XML-escaped query parameters. The
UsernameToken-mode cache is asserted to remember only the authority/mode decision, not
the password. Hostile-input tests reject oversized XML, excessive nesting,
DTD/entity expansion, namespace spoofing of recognized ONVIF fields and excessive
namespace bindings.
No normal CI test requires multicast LAN access or a physical camera.

Application tests use fake discovery/device backends to prove opaque discovery handles,
refresh/session invalidation, secret-free serialized DTOs, typed authentication
failures, prepared RTSP drafts with transient credentials, suspend cleanup without
resume resurrection, and isolation where one unreachable discovered device does not
prevent another device in the same discovery session from connecting. Direct
provisioning regressions then feed an ONVIF-produced `CameraDraft` through the real
`CameraService`: one asserts the ordinary RTSP camera/credential-reference model is
committed, and one injects a settings insert failure and proves the newly written
credential is rolled back with no camera row left behind.

Desktop regression coverage keeps the accepted M9 lifecycle suite green while ONVIF
admission is cancelled on suspend/quit/update and reopened only on resume. Close-to-tray
also invalidates transient ONVIF sessions, while the UI clears any in-progress wizard
credential state. A deterministic application regression proves cancellation wins even
after discovery network work completes but before a session can be committed. The final
provisioning path passes through the existing media-worker RTSP probe before
`CameraService::create_camera`, so ONVIF retains the existing keyring/settings
transaction instead of inventing a second persistence model.

`CamerasScreen` tests retain manual RTSP create/probe behavior and add empty discovery,
authentication failure with password clearing, H.264/H.265 profile presentation,
successful Test & Add using only opaque handles/safe fields, provisioning failure and
discovery refresh/stale-result behavior. Assertions forbid passwords or authenticated
RTSP URIs from rendered output or provisioning command arguments. Existing per-camera
recording concurrency/convergence tests remain unchanged.

Physical ONVIF camera validation is optional/manual and never a Forgejo CI dependency.
No hardware model is claimed validated by the M10 automated suite.

### Independent multi-camera live view (M11)

`nian-application` live-controller tests use deterministic fake runners plus real loopback
sockets. The suite proves the four-camera cap, duplicate refusal, RAII reservation release,
opaque/secret-safe DTOs, per-camera status isolation and background expiry without a
follow-up command. The remediation suite additionally proves that 80 simulated finalized
fragments collapse to the configured 24-fragment rolling window, four sessions are bounded
independently, reader-owned old fragments survive trimming until reader release, explicit
close removes transient cache, and startup removes only canonical owned `session-<uuid>`
directories while preserving lookalikes/unrelated files.

HTTP tests cover the session base capability, manifest and fixed fragment grammar plus
wrong Host/Origin, non-GET/HEAD methods, malformed/arbitrary paths, stale sessions and reader
limits. No request may supply an RTSP URL or arbitrary filesystem path. The configured
resource contract under test is a 500 ms fragment target, 24 retained-fragment target,
26-finalized-fragment hard count ceiling, the preserved 96 MiB retained-byte ceiling, 16 MiB
maximum per fragment, two readers per live session, eight concurrent live HTTP requests and
four simultaneous live sessions.

Lifecycle tests exercise the split runner stop/join contract and tracked draining phase. A
Condvar-blocked fake proves `close(A)` removes A from frontend-visible active state but keeps
it controller-owned as draining; concurrent shutdown cannot complete until A reap is
released. A four-worker case proves all stop signals exist before join, all four remain
observable as draining while blocked, concurrent shutdown waits, and each runner completes
exactly one join. Rapid-reactivation regressions synchronously capture a hide batch, resume
admission before that batch starts or while its join is blocked, open a fresh same-camera
session and prove old teardown cannot remove the new session or its HTTP capability. A
four-session hide capture also proves all draining workers continue consuming capacity until
reap. The stale-draining identity regression remains covered. In-flight-open tests continue
to prove lifecycle owns/cancels startup work. Keepalive remains cheap and the reaper owns
expensive expiry cleanup.

Media-worker unit tests cover strict RTSP/absolute-owned-directory fragment parameters,
fixed-width owned fragment names/partial cleanup, hard size/count policy bounds, counting only
owned finalized regular files, cancellable backpressure at the finalized-fragment ceiling,
and unified finalization-failure cleanup for finalize error, oversize validation and
destination collision while preserving a successful finalized `.mp4`. The bounded
five-attempt retry terminal state and lifecycle cancellation during backoff remain covered.
Existing FFmpeg ABI/muxer, probe, playback and recording tests remain unchanged and pass in
the workspace suite; M11 does not refactor recorder muxing.

Desktop regressions use the real `CameraService`/`LiveViewController` seam with fake live
runners. They prove Stop All Recording leaves live ownership untouched; close-to-tray
preserves recording Desired/Runtime while live admission closes; hide capture occurs before
reactivation and an old blocked teardown cannot close a fresh same-camera live session; a
Condvar-blocked hide teardown remains visible to an immediate quit shutdown and is not
double-joined; suspend invalidates live sessions while Resume restores Desired recording and
only reopens live admission; update teardown preserves recording intent. CSP tests require
only self/loopback `connect-src` and self/`blob:`/loopback `media-src`. Blocking live
open/close/status work is dispatched through Tauri's blocking runtime, while keepalive
remains bounded bookkeeping.

`LiveViewScreen` tests use manually controlled deferred Promises for the opening races. They
cover remove while `live_open` is pending, unmount/navigation-equivalent cleanup with two
pending opens, stale generation 1 resolving after generation 2 has been requested, immediate
`live_close` of late results, exclusion of late sessions from the keepalive set and prevention
of stale overwrite. A deferred `live_statuses` test proves repeated one-second ticks cannot
overlap one aggregate refresh, resolve/reject both release polling ownership, and unmount
ignores a late result. Existing tests continue to cover per-tile failure isolation, recording
Start/Stop independence and reconnect remount. Media failure coverage now proves automatic
fresh-session replacement, bounded 250/750/1500 ms recovery, hard stop after three consecutive
automatic recoveries, exposure of the original structured failure, manual Retry reset and timer/session cleanup on unmount. Fragment consumption uses
a single sequence watermark rather than retaining an unbounded historical sequence set. The
complete UI gate is TypeScript typecheck + ESLint + Vitest + Vite production build.

Physical live-camera validation remains optional/manual and is not claimed by CI. A real
Tauri WebView smoke should select one then multiple configured H.264 cameras, verify the
MediaSource path fetches only `http://127.0.0.1:<ephemeral>/live/<uuid>/...`, leave live view
running long enough to confirm the cache stays near the rolling-window bound, interrupt a
camera/network connection, and confirm one tile's failure/reconnect does not stop other live
tiles or recording. H.265/transcoding/WebRTC/PTZ/events remain outside M11.

### Optional ONVIF PTZ control (M12)

`nian-settings` migration tests cover fresh schema v4, v1/v2/v3 upgrade paths, preservation
of existing camera/desired rows, PTZ-binding round-trip and camera-delete cascade. Persisted
PTZ rows contain only non-secret Device-service identity and credential references; camera
passwords remain behind the existing native credential boundary.

`nian-onvif` PTZ fixtures cover PTZ service discovery, media-profile/configuration
association, continuous pan/tilt and zoom velocity-space parsing, invalid ranges, namespace
spoofing and service-authority mismatch. The client tests also assert bounded
`ContinuousMove`/`Stop` request construction and the same redirect/authentication/response
hardening used by M10. No test creates a generic proxy or accepts an unrelated PTZ host.

`PtzController` tests use deterministic fake settings, credentials and PTZ backends. They
prove an unpaired RTSP camera remains valid; exact camera-authority pairing; runtime rejection
of a manually stale host binding before `backend.control`; credential reuse versus PTZ-specific
ownership; pair/unpair independence from RTSP camera and recording Desired state; capability-
gated zoom; one-camera failure isolation; stale Stop generation refusal; 400 ms renewal extending
the one-second lease; automatic dead-man Stop; and lifecycle cancellation without Resume
resurrection.

Opening-reservation tests prove same-camera single-flight/Busy behavior, the
`opening + active + draining <= 16` worker-capacity bound, lifecycle cancellation of blocked
network establishment, full-shutdown waiting for that cancelled opening, and Busy/stale-opening
refusal after reactivation. Mutation tests now exercise registry-owned `mutating` state directly:
delete and unpair reject fresh same-camera session admission before any new `backend.control`, an
opening prepared against B1 is cancelled before a B2 replacement becomes authoritative, many
same-camera mutation contenders all fail fast Busy without a waiter queue or extra credentials,
and a mutation on camera A does not block camera B.

Draining tests block Stop/join to prove Hide -> immediate shutdown and camera-delete -> shutdown
share one controller-owned leader, with no double Stop/join and no stale same-camera removal.
Lifecycle side-effect tests block after a PTZ credential write, during unpair credential cleanup,
and during coordinated camera-delete completion. Suspend/Quit/Update's shared PTZ teardown path
must remain blocked until the mutation drops only after rollback/cleanup settles; Resume then
proves no stale pairing or motion is restored. Test-only ownership introspection asserts
`(opening, active, draining, mutating) == (0, 0, 0, 0)` after terminal settlement.

`CameraService` fault-injection tests cover paired-camera host replacement rejection, shared-
credential replacement rejection, delete with reused versus PTZ-owned credentials, failed DB
delete preserving every secret/binding, and post-commit keyring cleanup failure returning a
warning without database rollback. Desktop coverage also holds a same-camera PTZ mutation while
invoking the camera-update helper and proves typed `ptz_busy` returns before the owner is released;
the production Tauri command dispatches that helper through `spawn_blocking` rather than running
mutation coordination on the main thread. `OnvifController` blocks PTZ capability lookup and
cancels/reconnects the owning discovery session before release, proving stale authenticated
session/device/connection-generation work cannot return a prepared PTZ pairing.

Frontend `PtzControls` tests use deferred Promises and fake timers to cover press/release,
release before `ptz_move` resolves, Left→Right stale response ordering, periodic renew,
unmount cleanup and per-camera failure isolation. `LiveViewScreen` regressions keep
recording/live ownership independent while a tile mounts the PTZ pad. Camera-management
coverage proves initial Pair PTZ sends only `camera_id`, reuses the saved credential through silent exact-host backend discovery without opening the scan/login UI, falls back to explicit discovery only on `onvif_auth_failed`, and keeps Replace/Unpair explicit; raw service/token values are never rendered.

Physical PTZ validation is manual and never required by Forgejo CI. On a Tapo C200 or other
candidate camera, verify every advertised direction, hold-to-renew behavior, navigation/hide
while moving, suspend/resume with no movement restoration, and continued recording/live
ownership during PTZ failure or unpair. Unsupported zoom is recorded as capability absence.

### ONVIF PullPoint motion events (M13)

`nian-settings` tests cover the schema v5 Event migration with Desired state defaulting Off, independent
EventBinding round-trip, atomic disable+unpair and camera-delete cascade. CameraService tests prove
shared Event credentials block camera credential replacement, Event-owned credentials survive an
independent camera credential replacement, and camera deletion cleans camera/PTZ/Event-owned
credentials once and only after the database commit.

`nian-onvif` parser fixtures cover namespace-qualified `RuleEngine/CellMotionDetector/Motion` + `IsMotion`, `RuleEngine/MotionRegionDetector/Motion` + `State`, and `VideoSource/MotionAlarm` + `State`, QName-prefix resolution for notification Topic text, rejection/ignore of identical local names under vendor namespaces, synchronization `Initialized`, malformed/lookalike messages, distinct missing-Events-service versus missing-compatible-motion-topic diagnostics, PullPoint lifetime validation that accepts the 5-second minimum, rejects shorter remote lifetimes instead of clamping upward, clamps only downward at 24 hours, preserves missing-metadata fallback, and handles invalid/extreme timestamps safely. Renew-response fixtures apply the same minimum validation before mutating subscription metadata. SHA-256 source-token normalization remains unchanged. A local HTTP fixture executes the complete
GetServices/GetEventProperties/CreatePullPointSubscription/SetSynchronizationPoint/PullMessages/
Renew/Unsubscribe sequence using the production client. It checks the four-second bounded poll,
32-message cap, secret redaction and normalized source digest. A separate fixture advertises a
cross-host Event service and proves rejection occurs before follow-up authenticated traffic.

`OnvifController` blocks Event capability lookup while the owning discovery session is cancelled or
reconnected. Both paths must reject the stale `PreparedEventPairing`, including connection-generation
replacement after credentials change.

`EventController` fake-backend tests cover synchronization baseline semantics, transition-only persistence, repeated-state suppression, Desired-On survival after runtime startup failure, runtime exact-host rejection before authenticated Event traffic, and shutdown waiting for a blocked pull plus Unsubscribe. Motion normalization uses prepare/commit ordering: deterministic Event-index unavailability tests prove failed MotionStarted persistence leaves Idle authoritative and replay retries exactly once, failed MotionEnded leaves Active authoritative and replay retries, while a duplicate fingerprint commits runtime state without inserting another row. Event insert plus required retention cleanup settle in one SQLite transaction before normalized state commits, and timestamp-less replay continues to dedupe only after successful persistence. Drain tests hold Pull blocked while two shutdown callers race and assert both observe one controller-owned DrainState until the single worker join completes. Terminal Unsupported behavior remains an owned live `Failed` worker until intentional cancellation, so no terminated JoinHandle can remain active. Source-state tests feed more than the 64-source bound and assert no growth plus conservative `motion_active=None` overflow behavior. Scripted reconnect tests assert repeated Pull failures request `2, 5, 10, 30, 60` seconds and a successful Pull resets the next failure to two seconds. Timestamp-less replay tests preserve bounded source state across subscription recreation, suppress duplicate MotionStarted after successful persistence, then prove MotionEnded followed by a new MotionStarted still persists.

The dedicated Event index tests cover stable insert/query, fingerprint dedupe, bounded cleanup, corrupt DB quarantine/recreation and future-schema refusal without replacement. M15 fault-injection additionally interrupts the shared SQLite-family quarantine after the WAL move and after WAL+SHM moves, then proves the persisted generation marker resumes the same operation before SQLite reopen, preserves main/WAL/SHM evidence, removes stale canonical family members and permits new Event insertion/Event Review queries. Sidecar-only generations count toward pruning, repeated interrupted generations remain bounded, and pre-existing target evidence is never overwritten.
Desktop settings tests switch recording storage root at runtime and assert the new authoritative Event
index appears at `<new-root>/.nian/events.sqlite3`; a failed settings commit keeps the previous storage
root authoritative and reopens Event admission. Existing close-to-tray regression also asserts Event
admission remains available while transient live/PTZ ownership is released. Suspend/Resume/Quit/Update
continue through the terminal Event settlement path rather than the Hide path.

Frontend coverage pairs initial Events directly from a saved camera using only `camera_id`, proves the ONVIF scan/login dialog stays hidden when saved credentials authenticate, falls back to explicit ONVIF credentials only on `onvif_auth_failed`, verifies Pair leaves Desired Off until Enable, and exercises Unpair. Cameras loads the aggregate `event_statuses` result instead of issuing N per-camera Event status calls. Live View polls the same aggregate at five-second cadence with an explicit single-flight guard; a deferred-request test fires multiple timer ticks and proves only one backend request remains active until resolution, after which one new poll may start. Motion/error projection still does not close or replace a healthy video tile. Desktop has a thread-identity regression proving aggregate Event status collection executes through `spawn_blocking`, not on the caller/main thread.

Physical Event validation is manual and never claimed by CI. On a compatible camera, verify any advertised supported standard CellMotion/MotionRegion/MotionAlarm start/end transitions, reconnect replay suppression, camera reboot, network interruption,
Suspend/Resume fresh synchronization baseline, close-to-tray continued monitoring, storage-root
switching, and continued RTSP recording/live/PTZ behavior while Event monitoring fails.

### Event Review and local notifications (M14)

`nian-index` Event Review tests cover all-camera and single-camera filtering, bounded receive-time ranges, deterministic receive-time/event-ID ordering, equal-timestamp tie breaking, page limits, keyset next cursors, no duplicate/skip across stable pages, malformed cursors, excessive limits, invalid ranges, empty results and retained-away rows. Recording-index fixtures cover event-before/inside/after segment boundaries, correct selection across multiple segments, CameraId isolation at identical timestamps and conservative rejection when media duration is unknown. Playback unit tests fix the M14 pre-roll at five seconds and prove clamping at segment start.

`EventController` projection tests prove a notification signal appears only after a genuinely new committed Event insert, a duplicate fingerprint discovered by a fresh normalizer does not publish again, and persistence failure publishes nothing while preserving the prior normalization state for retry. Notification-dispatch tests use deterministic owned-delivery fakes and cover MotionStarted eligibility, MotionEnded suppression, per-camera 15-second rate limiting, camera isolation, notifier failure isolation, queue-full non-blocking drop, unsupported capability, blocking-delivery cancellation on Suspend/Shutdown, 3-second production deadline semantics via an injected short test deadline, timeout isolation, stale queued-generation drop, and Resume with a fresh queue. Settings persistence tests cover schema v6 default-Off migration and independent preference round-trip.

Desktop compilation/tests exercise the new state ownership and lifecycle wiring. Event history/query/get/recording-context/playback commands run blocking index/filesystem work through `spawn_blocking`; Event Review never changes recording Desired state. Hide retains the dispatcher, while Suspend/Quit/Update close notification admission and settle dispatcher ownership before Event restoration or process handoff; production native delivery is isolated in an owned helper subprocess that is terminated and reaped on stop/deadline. A desktop unit test proves the production delivery handle terminates/reaps a deliberately long-lived helper process. Resume opens a fresh queue before Desired Event workers restore. The current Tauri desktop notification abstraction exposes display but no click/action callback, so CI does not claim a native click deep-link test that the API cannot perform. Stale Event IDs remain typed/non-fatal through `event_get` and recording lookup.

Frontend Event Review tests cover the default 24-hour bounded query, camera filter propagation, selection-generation protection, stale-pagination rejection after filter changes, periodic-root-refresh invalidation, manual-root-refresh invalidation and exact frozen relative query bounds across later pages. The screen uses semantic controls, finite ten-second single-flight/coalesced root refresh, generation-owned cursor pagination and backend-provided five-second seek offsets. Settings tests verify the dedicated notification preference command rather than routing the toggle through storage settings.

Manual M14 validation on an installed Windows/Linux desktop should verify OS notification presentation while the window is visible and hidden, no historical replay after restart/Resume, MotionStarted-only behavior, notification privacy text, correct Event Review recording jump, missing-footage state after retention, and continued Event ingestion when OS notification display fails. Notification click activation is not claimed on the current Tauri desktop abstraction because it provides no click callback.

### v1 production hardening and release acceptance (M15)

M15 adds an explicit v1 persistence matrix for every supported historical settings schema v1 through v5 into schema v6. Fixtures preserve every field that existed at that historical version and verify current-safe defaults for fields introduced later. The v5 fixture preserves camera credential reference, Recording Desired, PTZ/Event bindings and independent owned credential references, Event Desired, storage/retention/quota/autostart state, while the v6 notification preference enters with the required default Off. Future authoritative schemas and corrupt authoritative settings continue to fail without replacing bytes; migration failures keep their prior user version/evidence.

Derived-index recovery is storage-bounded. RecordingIndex and EventIndex now share one restart-convergent SQLite-family quarantine helper: one numeric serial owns main/WAL/SHM, a `.quarantine-pending` marker persists that serial, moves are no-replace, canonical family absence is verified before reopen, partial generations count toward pruning, and at most four generations are retained. Event fault tests exercise interruption after one and two successful moves plus repeated interruption/collision cases; the recovered index immediately accepts a new normalized Event and Event Review query. Recording-focused regressions remain green on the same helper.

M15 normal Forgejo CI adds `cargo check --workspace` and makes the strict Rust lint contract authoritative: `cargo clippy --workspace --all-targets --all-features -- -D warnings`. `bindgen-gen` now returns typed generator errors instead of using production `expect`/stdout/stderr macros that prevented that gate from passing. GitHub remains release-tag-only and is not a duplicate normal CI system.

Release-contract tests prove strict source/tag identity for both RC and final versions: `1.0.0-rc.1` accepts only `v1.0.0-rc.1`, final `1.0.0` accepts only `v1.0.0`, and both cross-pair mismatches are rejected. Structural workflow tests keep prerelease tags at `prerelease=true`/`latest=false` while final SemVer alone may become production `latest`; mirror identity, least privilege, immutable action pins, required Linux+Windows candidate assembly, updater signatures, deterministic checksums and draft-first byte verification remain enforced. The enhanced Windows installed-upgrade fixture exercises Recording/Event Desired state, PTZ/Event bindings, opaque credential ownership, notification preference, storage settings and footage preservation.

Automated evidence is intentionally not a substitute for clean-machine/hardware release evidence. `docs/release-checklist.md` records the mandatory Windows/Linux package, upgrade, lifecycle, updater and compatible-camera checks. Until that checklist and the actual tag workflow are completed, M15 must be reported as release-ready implementation rather than a completed v1 release.

## Planned per milestone

* **Future/v2 scope**: ONVIF presets, vendor event dialects, push delivery, talkback, macOS distribution,
  H.265/transcoding/WebRTC, clip export, thumbnails/motion analysis and AI/cloud behavior require
  separate design/review and are not part of Nian Vision v1.
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
