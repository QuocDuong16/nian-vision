# ADR-0007: Resilience and supervision model

* Status: Accepted (2026-08-27)
* Deciders: Nian Vision core
* Related: ADR-0002 (FFmpeg integration), ADR-0003 (process isolation and IPC), ADR-0004 (recording container/timestamps), ADR-0005 (storage)

## Context

M2 delivered one recording session over ONE healthy connection. Real cameras
reboot; Wi-Fi drops; routers restart; RTSP streams stall without closing;
the media worker process itself can crash. The product must survive all of
these automatically while never corrupting recordings and never looping
forever over permanent local failures (M3 §"product goal").

The M2 review also left two structural debts that M3 had to repay first:
`--until-stdin-eof` was parsed but effectively ignored in the manual CLI,
and `RecordingEndReason::SourceError` conflated media AND storage errors —
too coarse for any reconnect decision.

## Decision

### 1. Timeout vs cancellation is a typed distinction at the FFmpeg boundary

The interrupt callback is the single authority on WHY a blocking operation
aborted. `InterruptState::abort_cause()` classifies into `Cancelled`
(operator intent, atomic flag) and `DeadlineExceeded` (operation-scoped
deadline); cancellation wins when both hold. The mapping happens in exactly
one place (`error_util`), producing the distinct errors:

* `MediaError::Interrupted` — operator cancelled → supervisors STOP;
* `MediaError::TimedOut` — operation deadline passed → retryable.

`MatroskaMuxer::write_packet` no longer reports a constant
`interrupted = false` after `av_interleaved_write_frame`; if the interrupt
fired during a blocked interleave flush it surfaces as Interrupted/TimedOut
correctly. Regression-tested.

### 2. Deadlines are operation-scoped RAII

`ScopedDeadline` arms a deadline for exactly one blocking operation and
clears/restores on drop — including error paths. No expired deadline can
leak into unrelated local operations (mux writes, finalization). The
recorder arms its configured budgets per phase: session open/connect
(`SourceTimeouts::open`), each packet read (`read`). Defaults: 15 s open,
15 s read stall.

### 3. Failure classification is centralized, never string-parsed

`RecordingError::category()` maps every error to a closed
[`FailureCategory`] set (operator stop/cancellation, clean EOF, source
open/read/timeout failures, output write failure, storage failure,
permanent configuration). Retryability (`is_retryable`) covers exactly the
three source-side categories. Supervisors branch on these types only.

Clean EOF gets special treatment by SOURCE KIND: a finite file EOF is
completion (`EofInterpretation::Completed`); an RTSP peer vanishing
silently is connection loss (`ConnectionLost`) — same demuxer signal,
deliberately different supervisor meaning (M3 §8).

### 4. Reconnect orchestration lives ABOVE the session; sessions stay M2

`RecordingSession` remains one healthy connection with M2 semantics.
`CameraRecordingSupervisor` (worker side) owns the explicit lifecycle
`Idle → Connecting → Recording → Backoff → Stopped/Failed`, emits every
transition as a typed event, and builds one FRESH MediaInput +
InterruptHandle + RecordingSession per reconnect — reuse of a cancelled
FFmpeg context is structurally impossible because each attempt consumes
its input.

Reconnect delays REUSE `nian_domain::ReconnectBackoff` (2s→5s→10s→30s→60s);
never duplicated. Reset policy (M3 §4): opening alone does NOT reset the
streak — a connection must record ≥ `stable_recording_threshold`
(default 30 s) of media time first, or flapping cameras would hammer the
network while pretending recovery. Bounded ±jitter (default ±2 s,
seeded xorshift, injectable) prevents multi-camera lockstep after router
reboots without altering base entries.

Waiting and jitter are injected traits (`Waiter`, `Jitter`): unit tests use
a virtual waiter and NEVER sleep out the schedule tail. Production wires
`SleepWaiter`.

### 5. Shutdown ordering with race coverage

No reconnect may occur once stop was requested (checked at loop top, after
open, after sessions end, and inside backoff waits). Stop-during-connecting,
-during-recording, -during-backoff and simultaneous-with-source-failure are
unit-covered races on scripted supervisors (§16). Operator cancellation
flavored failures surfacing through a session resolve as STOP, not reconnect.

### 6. Process supervision mirrors camera supervision in nian-application

`WorkerSupervisor` spawns the worker binary with argv containing ONLY the
program + subcommand, verifies hello (protocol + capability report present),
restores desired recording state via `recording.start` over stdin, monitors
by blocking framed reads until EOF, distinguishes requested shutdown from
crash (ack response / EOF-before-ack), and drives restart episodes across
the SAME domain backoff with a process-level stability rule: episodes
shorter than 60 s escalate a fast-death tally toward permanent stop
(default 5); stable episodes reset it. Configuration-shaped start refusals
(`invalid_params:*`) map to PERMANENT failure — no infinite loops over bad
configuration.

Worker side keeps ONE recording job per process (`RecordingJobManager`);
each attempt deposits its fresh interrupt handle so a second host stop
press force-cancels blocking I/O exactly like the manual CLI's Ctrl+C
escalation.

### 7. Partial-file startup reconciliation is conservative and layered

`nian-storage::scan_camera_partials` walks ONLY canonical layout paths,
parses every name strictly, and classifies by filesystem facts: size below
64 bytes ⇒ empty/header-only; EBML magic + Cues trailer element ⇒
finalized-but-unpublished; anything else ⇒ recoverable media candidate.
It never deletes, never renames.

`nian_recorder::recover_camera_partials` performs the MEDIA-level proof:
demux (15 s scoped deadline) → require video stream + validated time base →
claim a FRESH output slot → discard until first selected video keyframe →
stream-copy readable packets (truncation expected) → durable finalize →
no-replace publish → REMOVE the original LAST. Everything unprovable stays
in place quarantined and reported; nothing is invented. If publication
refuses (destination taken), neither file disappears. Remux outputs keep
M2 rules: empty salvage never publishes, write-failure stops copying but
finalization decides what survives.

### 8. Worker IPC gains a versioned recording namespace

`recording.start` / `recording.stop` / `recording.status` plus unsolicited
events, served by the generic `Handler` trait which now receives `&mut
FramedWriter` so handlers may emit protocol events before replying.
Secrets travel ONLY inside request payloads on private stdin; URLs are
held behind redacting newtypes; Debug output cannot leak them. Status
snapshots carry labels/counters only.

## Consequences

* The manual `record` command's stop modes are now explicit and mutually
  exclusive (`SignalOnly` default; `--duration N`; `--until-stdin-eof`),
  fixing the accepted-review complaint without touching §9's two-stage
  Ctrl+C design.
* `MediaError::is_interrupted()` became STRICTLY cancellation; code that
  treated deadlines as interruption must check `is_timed_out()` too — the
  recorder's teardown policy does exactly that (timeout ⇒ salvage healthy
  prefix like any source failure).
* SQLite remains absent; the filesystem is authoritative; M4 will absorb
  quarantine cleanup policies.
* Jitter uses xorshift64*, not cryptographic randomness — good enough for
  desynchronizing cameras, fully deterministic for tests.

## Alternatives considered

* **Reconnect loop INSIDE RecordingSession** — rejected: destroys the M2
  abstraction boundary the milestone mandates preserving; makes fault
  injection impossible above honest connections.
* **String matching on ffmpeg error text** — rejected outright: fragile and
  secret-hostile; classification had to be type-level before any supervisor
  depended on it.
* **Deadlines installed permanently on the shared handle** — rejected: an
  expired deadline aborts later local mux writes spuriously; RAII scoping
  makes leakage mechanically impossible instead of carefully avoided.
* **Deleting unreadable partials during scan** — rejected: reporting beats
  deletion for a system whose brand promise is "recoverable"; cleanup policy
  belongs to retention (M4+).

---

## Amendment (M3 review remediation, 2026-08-27)

### Stop/cancellation control domain

The supervisor's run-level stop flag is handed to EVERY `open_session` call
and baked into the fresh session via `RecordingSession::from_input_with_stop`.
Stop-while-Recording reaches the active session BY CONSTRUCTION — one shared
`Arc<AtomicBool>` behind supervisor and session; no relay thread, no flag
splicing. One graceful press always suffices (race tests cover stop during
Recording / Connecting / Backoff / concurrent-with-failure).

### Source-kind-aware retry matrix

Retryability now depends on the source kind, not only on the error category:

| Failure | RTSP | File |
|---|---|---|
| open failed | retry | **permanent** |
| read failed | retry | **permanent** |
| read timeout | retry | **permanent** |
| clean EOF | retry (connection lost) | completed |
| output write fail | permanent | permanent |
| storage fail | permanent | permanent |
| config invalid | permanent | permanent |
| operator stop/cancel | stop | stop |

A local file neither heals nor changes under us — looping over it forever was
a latent infinite-retry bug.

### Authoritative segment accounting

`SegmentFinalized` events are THE authoritative cross-attempt counter. An
attempt that publishes 3 segments and then dies contributes 3, whether its
`Result` is `Ok` or `Err`. Summaries are never added on top (no double-count).

### Worker process supervision rebuild

* Dedicated stdout READER thread → channel: the coordinator's event loop is
  `recv_timeout`-driven, so shutdown intent is observed without ANY incoming
  worker frame, and death is observed in every phase.
* Dedicated stdin WRITER thread: one owner, no unsynchronized writes.
* Injected `SupervisorDeadlines` bound hello / start-ack / shutdown-ack;
  tests use milliseconds. A silent-but-alive worker = unhealthy episode →
  restart policy, never an indefinite block.
* Crash/restart applies to ALL phases (before/during hello, awaiting start
  ack, while recording, while shutting down). Protocol-version mismatch and
  malformed frames stay PERMANENT; transient start refusals flow through the
  backoff instead of ending supervision; configuration-shaped refusals stay
  permanent. Episodes ending with a still-alive worker force-kill + reap so
  nothing blocks or leaks.
* The coordinator polls `recording.status`; terminal job states (`stopped`,
  `completed`, `failed:<category>`) are observable at the parent while the
  PROCESS stays alive — worker-alive ≠ recording-healthy. Backoff waits are
  shutdown-sliced like camera backoff.

### Recovery pipeline hardening

Order fixed to open-original → validate → PROVE a primary-video keyframe
exists → then claim/open/copy; a keyframe-less candidate manufactures ZERO
junk partials. Any mux write failure POISONS the recovery output (never
finalized/published; original kept; failure reported). Size lookup after
finalize is required — no `unwrap_or(0)`; metadata failure aborts before
publication. Cleanup of the original happens after dropping the source input
and is OBSERVABLE (`RecoveryOutcome::Recovered.original_removed`) rather than
silently ignored; idempotency rests on no-replace publication plus
non-canonical leftovers being invisible to the scanner.

### Worker exit semantics

`cmd_run` replaces its fixed 5 s sleep with a real shutdown lifecycle:
graceful job stop → grace join (> any bounded read + finalize headroom) →
force-cancel if needed → absolute-bound join; expiry is an explicit forced-
shutdown path that leaves the partial recoverable and exits non-zero. Job
stop presses are per-manager state (fresh job ⇒ zero presses).

## Amendment (M3 final remediation, 2026-08-28)

The second review round tightened the control plane. Decisions taken,
layer by layer:

### Canonical `recording.status` wire shape (review §1)

The status result IS the worker's `JobStatus` object — `finished`,
`end_kind`, `failure_category` and the recovery summary sit at the top
level of the response `result`. There is no wrapper object. The parent's
`JobTerminal::parse` consumes exactly that shape, and the stub protocol
fixtures replicate it byte-for-byte (including `{"started":true}` start
acks and id-echoed `{"bye":true}` shutdown acks). A REAL-worker
integration test pins the round trip: file EOF → `JobCompletedCleanly`.

Because a completed/stopped job leaves the worker's serve loop alive and
idle, the parent now DELIVERS a bounded protocol shutdown after observing
such a terminal state, instead of waiting forever on a process that will
never exit on its own.

### Stable failure-category wire values (review §9)

`FailureCategory::as_str()` defines explicit snake_case protocol strings
(`storage_failed`, `output_write_failed`, `permanent_configuration`,
`source_open_failed`, …); `from_wire` parses them back. Rust `Debug`
output is no longer an IPC contract anywhere.

### Permanent job failures never restart the worker (review §2)

A terminal `failed` status observed through `recording.status` maps to a
new typed error, `ApplicationError::PermanentRecordingFailure { category }`
(secret-safe: the category is machine vocabulary and never reaches the UI
message). It is NOT a retryable episode: the camera supervisor's
retryability policy already ran inside the worker, so restarting the
process could not fix `storage_failed`, `output_write_failed`,
`permanent_configuration` or an exhausted source-retry budget. A
`run_forever` test with a spawn-counting launcher proves StorageFailed
produces exactly ONE worker process, ever.

### Monitor-phase responsiveness (review §3)

Every status poll now has a response deadline (`SupervisorDeadlines::
status_response`, default 3 s) plus a consecutive-miss bound (`MAX_MISSED_
STATUS_POLLS = 2`). A worker that acks start and then goes silent becomes
`Unresponsive { phase: "monitor" }` → force-kill → restart backoff. Any
valid expected status response resets the counter. Reader-channel
disconnection now uniformly maps to end-of-stdout (death) semantics — an
EOF drained by a pacing window can never masquerade as a protocol error.

### Graceful shutdown vs operator escalation (review §4)

`RecordingJobManager` exposes separate operations: `request_graceful_stop()`
(IDEMPOTENT — never escalates), `force_cancel_current_io()` (operator
press 2 / expired grace only) and a bounded `join_until` that actually
takes and joins the finished handle. Protocol `shutdown` sends exactly one
graceful request; `cmd_run` waits the grace window (> max bounded read +
finalize headroom), force-cancels ONLY if grace expires, then joins with
an absolute bound. The operator two-press counter is never control flow
for shutdown. A REAL-worker test proves ONE shutdown finalizes/publishes
the healthy active segment — zero abandoned partials — with exit status 0.

### True recovery idempotency (review §5)

Recovery identity is now DETERMINISTIC per original partial
(`<base>.recovered-tmp` scratch, `<base>.recovered.mkv` final,
`<base>.recovered.mkv.done` tombstone, all derived from the original's
canonical name). The deterministic final's existence is the authoritative
already-recovered signal: a repeat pass recognizes it BEFORE opening the
demuxer and never remuxes again — so the same surviving original can never
produce a second recording, no matter how many startup passes run or where
the crash landed. The tombstone is created (atomically, no-replace) only
AFTER publication; its persistence failure is observable
(`tombstone_recorded=false`) without invalidating the recording. Scratch
names never parse as canonical segments, keeping the scanner (and M4
retention) blind to recovery artifacts; a stale scratch from a crashed
pass is always safe to delete because the original outlives it. The old
`finals_after >= finals_before` allowance is replaced by the real
invariant `finals_after == finals_before`.

### Async startup recovery + typed classification (reviews §6/§7/§8)

`recording.start` validates CHEAP parameters and replies promptly. The job
thread then acquires the per-camera kernel lease before filesystem pre-flight
or recovery, and runs `pre-flight → recovering → connecting → recording`.
Recovery is asynchronous; its summary (`recovered / quarantined / failed /
infrastructure_failures`) rides the status snapshot, and stop/shutdown
interrupt it in a bounded way (recovery observes a shared interrupt handle;
`recover_camera_partials_with_interrupt` aborts blocked demux/mux I/O and
stops between files). Failures are TYPED: recovery infrastructure errors may
fail the job permanently, `Unreadable` is a per-file content verdict
(quarantine; recording continues), and `Cancelled` is neither. The parent
still accepts the legacy permanent `storage_unavailable` start refusal, while
new workers report post-ack pre-flight failure as terminal `storage_failed`.
`start_failed` (thread spawn) remains the only transient refusal code.

## Amendment (M3 final correctness remediation, 2026-08-28)

### Unique per-attempt recovery scratch (review §1)

The deterministic recovered FINAL and tombstone remain the idempotency
anchor, but the recovery SCRATCH is now PER-ATTEMPT UNIQUE
(`<base>.recovery-<pid>-<serial>-<nanos>.tmp`, created with atomic
no-replace `create_new`). No attempt ever deletes a scratch it does not
own — the previous shared-scratch design let two concurrent recovery
processes unlink each other's active file (on Unix, orphaning a written
inode while the other process owns the pathname). Concurrent recovery of
one original is now arbitration-by-final: both attempts write distinct
scratches, one no-replace publish commits, the loser observes
`DestinationExists`, recognizes the deterministic final as already
committed, removes ONLY its own scratch and reports `AlreadyRecovered`.
A deterministic two-lane test (publication barrier hook) proves: exactly
one final, distinct scratch pathnames, independently probeable final,
loser = AlreadyRecovered, original never overwritten. Stale scratch
cleanup is deferred to M4's janitor, which can classify them
(`RecoveryScratch`) and delete only when ownership/staleness is provable.

### Cooperative graceful stop during recovery (review §2)

Recovery now observes BOTH stop domains: the InterruptHandle (force
escalation) and the run-level StopFlag (the FIRST graceful press). At
every safe boundary — before each partial, before the source reopen,
before scratch acquisition, between packet operations, before finalize
and before publication — a graceful stop abandons the attempt: private
scratch removed, original untouched, nothing published unless the
transaction already crossed its durable commit point, no further partial
started, no camera connection. A pre-publication checkpoint hook (armed
only by test builds) holds recovery mid-flight deterministically; worker
tests prove start → `recovering` → ONE `recording.stop` → bounded
`stopped` with no connection and no second press, and protocol shutdown
ending recovery gracefully before its force-cancel grace.

### Terminal job state is authoritative (review §3)

When the parent observes a terminal `recording.status`, that outcome is
recorded FIRST and returned REGARDLESS of what the worker cleanup
shutdown does. The cleanup shutdown is best-effort process hygiene: if it
fails to complete, the worker is force-terminated and reaped, the anomaly
is logged, and the already-known outcome is returned — Completed stays
`JobCompletedCleanly`, Failed stays `PermanentRecordingFailure`, Stopped
stays `RequestedShutdown`. A wedged cleanup can never reinterpret the
recording into a retryable episode, and `run_forever` tests with spawn
counters prove no restart for any of the three terminal kinds when the
shutdown ack is lost.

### Episode-local monotonic status-poll ids (review §4)

Status polls allocate fresh, monotonically increasing request ids (from
3; start stays 1, shutdown 2). Only the freshly allocated id can mark a
poll answered and reset the missed-response counter; a late reply to an
older poll is ignored. A stub test proves a late id-3 response arriving
during poll 2's window does not satisfy it — the episode still becomes
`Unresponsive{phase:"monitor"}` within the missed-poll bound.

### AlreadyRecovered repairs the transaction (review §5)

The already-recovered recognition path now also verifies/creates the
tombstone and RETRIES the original's cleanup, reporting
`original_removed` on the outcome. Convergence: publish-succeeded +
cleanup-failed eventually becomes "final exists, original gone, tombstone
in known state" instead of re-scanning the same leftover forever; if
cleanup still fails the original stays safe and a later startup retries.

### Recording-tree file classification (review §6)

`nian_storage::classify_recording_file_name` defines the M4-facing
contract: `NormalRecording`, `RecoveredRecording` (`.recovered.mkv` is a
FIRST-CLASS recording — M4 reconciliation enumerates it, retention
accounts/deletes it), `ActiveOrCrashPartial`, `RecoveryScratch`,
`RecoveryTombstone`, `Unknown`. Scratch and tombstones are never
recordings.

### Infrastructure vs artifact storage failures (review §7)

`RecoveryError::Storage` is split: `Infrastructure { operation, source }`
(scan failure, scratch-claim failure — the storage target is unsafe for
new recording → the worker fails the job permanently, regardless of other
files' successes) and `Artifact { operation, source }` (stat of this
attempt's finalized scratch, non-collision publish failure — coexists
with continued recording). No OS-error-string parsing anywhere; the
worker's summary exposes `infrastructure_failures` strictly from the
Infrastructure kind.

### Writeability pre-flight (review §8)

`ensure_camera_dir` now proves NEW-file writability, not just directory
existence: after `create_dir_all`, a uniquely-named probe file
(`.nian-write-probe-<pid>-<n>.tmp`, reserved non-recording name,
classify-`Unknown`) is created with `create_new`, closed, and removed.
It never overwrites user data; creation failure surfaces a genuine
storage error (terminal `storage_failed` in the lease-owning worker job).

### Per-camera live-partial ownership lease (M3 live-partial remediation)

Each camera tree owns a stable `.nian-camera.lock` control file. The file may
remain forever; ownership is exclusively the OS lock held by the open
`std::fs::File`. The job thread acquires it with non-blocking `try_lock()`
before storage pre-flight, partial scanning, recovery, or session creation,
and holds it through Connecting, Recording, Backoff, reconnects, graceful
Stopping, and final thread exit. `WouldBlock` becomes terminal
`camera_in_use`, so the parent does not churn worker restarts. Process death
closes the handle and releases the lease automatically, making the previous
owner's canonical partials valid crash-recovery candidates for the next job.
The lock file classifies `Unknown` and never reserves recording identity.

The manual `nian-media-worker record` smoke path follows the SAME boundary:
argument validation → layout construction → camera lease → writeability
pre-flight → media/session open → recording/finalization → lease drop. Its
small `ManualRecordingSession` wrapper keeps the lease alive while
`RecordingSession::run` consumes the session, including graceful Ctrl+C
finalization and force-cancel teardown. If ownership is already held, manual
recording fails before media open and before any canonical partial is created.

**Ownership invariant:** No production `RecordingSession` may write a
canonical recording tree unless its caller holds the matching `CameraLease`
for the entire session lifetime. The only production session factories are
the supervised worker factories (covered by the job-thread lease) and the
manual wrapper above. Low-level crate tests may deliberately bypass this
boundary to exercise transaction mechanics.

Standalone recovery distinguishes ownership from storage health:
`CameraAlreadyActive` and `CameraLeaseMismatch` map to
`RecoveryError::Ownership` (`is_infrastructure() == false`) before scanning;
real lock-file/open filesystem failures remain `Infrastructure`.

## Amendment 4 — M3 final safety remediation (2026-08-28)

### Tombstone transaction/conflict contract (review §1)

A deterministic destination's mere EXISTENCE is never proof that the
original was recovered: directories, zero-byte files, corrupt files,
foreign files or unrelated valid media may sit at the deterministic
pathname, and nothing may be deleted on name-based inference. The
explicit contract:

* **A** — final absent → normal recovery attempt;
* **B** — final present AND a TRUSTED tombstone (magic line
  `NIAN-RECOVERY-TOMBSTONE v1` + strict `original:`/`final:` name lines,
  validated against THIS transaction) proves the pair →
  `AlreadyRecovered` with a cleanup retry (`NotFound` counts as
  converged);
* **C** — final present WITHOUT trusted evidence → new
  `RecoveryOutcome::RecoveryConflict`: original and destination both
  preserved, no remux, no retroactive success tombstone, no deletion of
  either file. Reported observably (`RecoverySummary.conflicts` on the
  status wire).

Publication order stays: `publish_no_replace` → tombstone (create_new +
durable sync) → remove original LAST. A crash between publication and
tombstone is a preserved conflict, never a guess. Concurrent races: the
loser observing `DestinationExists` resolves it through the SAME
contract; inside the winner's not-yet-tombstoned window the loser
reports the pending conflict and NEVER deletes the original — the
winner's own tombstone+cleanup converges the tree (deterministically
tested via the post-publication hold seam).

### Alignment probe observes graceful stop (review §2)

The keyframe-alignment probe is an EXPLICIT loop: graceful stop and
forced cancellation are checked before EVERY `next_packet`, and errors
are classified deliberately (cancellation → `Cancelled`; media failure →
per-file content quarantine). The old `while let Some(...) =
next_packet().ok().flatten()` shape — which collapsed EOF, media errors,
timeouts and cancellation into one silent path — is gone. Deterministic
coverage parks an attempt INSIDE the alignment phase (recorder-level
gate seam and a worker-level test: one stop ends the job without any
camera connection).

### Exact scratch grammar (review §3)

`RecordingFileKind::RecoveryScratch` matches ONLY the production
generator's shape `<canonical-stem>.recovery-<pid>-<serial>-<nonce>.tmp`
(three non-empty all-numeric dash-separated components before `.tmp`).
Wrong extensions, wrong component counts, non-numeric components and
foreign stems all classify `Unknown` — the future M4 janitor never
receives a broad matcher.

### Hardened writeability pre-flight (review §4)

`ensure_camera_dir` now proves, in order: directory create/access,
exclusive file creation (`create_new`), actual byte write + `sync_all`,
clean close, and probe REMOVAL. Probe-cleanup failure is surfaced as a
genuine storage error (a tree that cannot delete files cannot host the
retention lifecycle) — never silently ignored. Probe naming is
collision-safe (pid + monotonic serial + clock nanos) with fresh-identity
retries on `AlreadyExists`; probes keep classifying `Unknown`.

### Output-side failure classification (review §5)

Muxer open, write and finalize/trailer/flush failures on THIS attempt's
own scratch are `RecoveryError::Artifact` (operations `open the recovery
output`, `write the recovery output`, `finalize the recovery output`) —
never `Unreadable`, which is reserved for content verdicts about the
original. Invariant preserved: any output failure removes only this
attempt's scratch and never publishes; the original stays untouched.
`Infrastructure` remains reserved for mechanically justified storage-
target unsafety (scan/claim); FFmpeg error strings are never parsed.

### Scratch serial + protocol strictness (reviews §6/§7/§8)

The scratch serial is a genuinely PROCESS-GLOBAL monotonic counter
documented as such (pid + global serial + nanos, with `create_new` as
the final arbiter). `JobTerminal::parse` is now strict: `finished=true`
with an unknown/missing `end_kind`, or `failed` with a missing/invalid
`failure_category`, is a PROTOCOL VIOLATION (`PermanentProtocol`, typed
`TerminalParseError`) — never silently "still running", never an invented
`Unknown` category. The category vocabulary itself moved to the
FFmpeg-free `nian-domain` crate (`FailureCategory` re-exported by
`nian-recorder`) so parent and worker validate the SAME strings.
`SupervisorState` gained an explicit stable wire representation
(`as_str`: `idle`/`connecting`/`recording`/`backoff`/`stopped`/`failed`),
replacing the Debug-then-lowercase conversion in the worker's status
fold.

## Amendment 5 — M3 identity safety remediation (2026-08-28)

The fifth review round closes the transaction-IDENTITY gaps: evidence must
describe the object it vouches for, and a consumed recording identity must
never be re-issued — even across wall-clock rollbacks.

### Tombstone v2: evidence bound to the actual recovered file (§1)

`NIAN-RECOVERY-TOMBSTONE v2` adds the published final's exact size:

```
NIAN-RECOVERY-TOMBSTONE v2
original: <canonical original basename>
final: <canonical recovered basename>
size: <published final size in bytes>
```

Case B (`AlreadyRecovered` + original cleanup) now requires ALL of:
deterministic final exists; it is a REGULAR file (`symlink_metadata` —
symlinks deliberately refused); the tombstone parses strictly as v2; the
original basename matches; the final basename matches; AND the current
object's size equals the recorded published size. Any failure — a
directory, a zero-byte/truncated replacement, a different-size file at the
recovered pathname — demotes the state to `RecoveryConflict`: original and
destination both preserved, no remux, no retroactive success tombstone, no
deletion of either object. Names alone never authorize a deletion.

Backward compatibility: this project is pre-v1 and v1 markers bind only
NAMES — insufficient to prove the object at the final pathname — so v1 is
rejected as untrusted (case C conflict, both files preserved); it is never
silently treated as equivalent to v2.

### Identity reservation across the whole Nian-owned namespace (§2)

`allocate_segment_sequence` previously reserved identities only for names
`parse_segment_file_name` accepts, so `<stem>.recovered.mkv` /
`<stem>.recovered.mkv.done` occupied nothing. The data-loss scenario — old
partial recovered, then a manual clock rollback / NTP backward step / DST
repeated local time re-presents the same wall-clock second and the
allocator hands the SAME identity to new footage, which a crash would
leave look-alike to the old transaction — is now closed by construction:
the new `owned_recording_name` parser (nian-storage `classification`)
resolves every Nian-owned name — normal recording, partial, recovered
final, tombstone, and exact-grammar recovery scratch — to an
`OwnedRecordingName { started_at, sequence, kind }`, and the allocator
marks the `(started_at, sequence)` OCCUPIED for any of them. Foreign and
Unknown files reserve nothing. `08-30-00.recovered.mkv` alone forces the
next claim to `08-30-00-2.partial.mkv`; a tombstone alone reserves the
sequence as well; wasting a sequence is harmless, identity ambiguity is
not.

### Clock-rollback regression (§3)

An end-to-end filesystem/recovery test
(`clock_rollback_cannot_reallocate_a_recovered_transaction_identity`)
seeds a partial at T through the real `claim_segment`, recovers it,
verifies the v2 tombstone's size binding, then simulates a new recording
at the SAME wall-clock second T through the real layout: the claim lands
on `-2`, new media recovers under its own distinct identity, the old
tombstone never claims the new footage, and both recovered recordings
remain independently identifiable.

### Stop domains win over alignment read errors (§4)

After the keyframe-alignment probe's `next_packet` returns `Err`, BOTH
stop domains — the graceful `StopFlag` and `InterruptHandle` cancellation
— are re-checked BEFORE any content verdict; only when neither is active
is the read classified `KeptUnrecoverable`. The asynchronous recovery
status therefore stays honest when a stop races a failing read.

### Tombstone durability, precisely (§6)

The marker's bytes are flushed with `sync_all`. On POSIX, a freshly
created directory ENTRY additionally requires syncing the parent
directory: `record_tombstone` now performs that best-effort (`fsync` on
the opened parent directory handle). On Windows/NTFS no public
directory-fsync exists; namespace changes are journaled and the guarantee
is deliberately documented as weaker. The conflict contract remains safe
under EVERY durability level: if a tombstone disappears after sudden
power loss, the surviving final has no trusted evidence → case C conflict
→ the original partial is preserved, never deleted on inference.

## Amendment 6 — M3 atomic identity claim remediation (2026-08-28)

The sixth review round closes the one remaining cross-process TOCTOU:
`allocate_segment_sequence` reserved the whole Nian-owned namespace, but
`claim_segment`'s scan-then-`create_new` could still miss a reservation
that a competing process published WHILE the directory enumeration was in
flight — and `create_new` is atomic only for the ONE partial pathname, not
for the identity against `<stem>.mkv`, `<stem>.recovered.mkv`,
`<stem>.recovered.mkv.done` and `<stem>.recovery-<attempt>.tmp`.

### Post-claim identity fence (§2/§3)

`RecordingsLayout::claim_segment` now performs, after the candidate partial
is exclusively created and BEFORE the claim is returned: a FRESH
Nian-owned namespace check (`identity_conflict_after_claim`) for the SAME
`(started_at, sequence)`, excluding the candidate itself and consulting the
existing `owned_recording_name` classifier — never a second filename
parser. A recovered final, tombstone, valid recovery scratch, or normal
finalized recording appearing mid-flight forces the candidate to be
relinquished (handle closed FIRST — Windows-first discipline — then
ONLY this attempt's candidate removed) and a higher sequence retried. The
candidate was never returned to the recorder, so no media writer can
observe it; relinquishing is unambiguous. Any removal failure — including
an unexpected NotFound, which would mean an external party deleted the
claim — is ambiguous ownership and surfaces as a typed `StorageError`,
never silently ignored; an unvalidatable fence (enumeration error)
likewise fails the claim typed, with the candidate removed best-effort (an
unremovable empty candidate has a canonical crash-partial name and is
quarantined by the next startup pass — never published, never deleted).

### Recovery ordering makes the fence sound (§4)

The existing recovery ordering — publish the recovered final → write the
trusted tombstone → remove the original partial LAST — is preserved
unchanged, and is exactly what makes the fence effective: a freed original
pathname (the precondition for a competing `create_new` on that name to
succeed) implies the transaction reservation is already published and
observable by a fresh post-claim enumeration. The fence therefore converts
the review's concrete failure (stale scan → claim sequence 1 → new footage
under the old transaction's identity → a later crash lets the old
tombstone swallow it) into a structurally impossible state.

### Deterministic race regression (§5/§6)

A day-dir-keyed one-shot claim gate (`nian-storage::test_hooks`, compiled
only under the `test-hooks` feature) parks a claim AFTER its advisory scan
chose a sequence but BEFORE the candidate is created — the deterministic
stand-in for a stale non-atomic directory snapshot, no filesystem
scheduling luck. Unit tests plant a competing recovered final, tombstone,
normal final, or exact-grammar scratch inside the window and prove the
fence fires (retry to `-2`, losing candidate removed, planted object
untouched) while foreign/Unknown files do NOT trigger it. The end-to-end
regression (`racing_claim_during_recovery_transition_never_reuses_the_
transaction_identity`) drives the REAL recovery pipeline through the
transition while a REAL `claim_segment` races it via the gate, then proves
the old tombstone never claims the new footage, both recovered recordings
survive independently identifiable, and no data is deleted through
identity reuse.

### Naming discipline (§7)

`allocate_segment_sequence` is documented as ADVISORY selection;
`claim_segment` is the authoritative race-safe acquisition primitive;
`PartialFile::final_path` is documented as the NORMAL live-recording
publication target — startup recovery deliberately publishes recovered
media under the distinct `<stem>.recovered.mkv` slot, never under
`<stem>.mkv`. The distinction is kept explicit for M4.

## Amendment 7 — M3 sub-second identity remediation (2026-08-28)

### Whole-second filesystem identity, normalized once (§1)

The filesystem identity invariant is now explicit: a recording identity is
**(whole-second local time, sequence)** — recording names encode wall-clock
time only to whole seconds, while a live claim timestamp
(`Local::now().naive_local()`) carries nanoseconds. `claim_segment`
normalizes the claim timestamp EXACTLY ONCE (`let identity_time =
whole_seconds(started_at.time());`) and shares that truncated value across
every identity use: the advisory `allocate_segment_sequence`, the candidate
naming (`partial_file_name_with_sequence` /
`segment_file_name_with_sequence`) and the post-claim fence
(`identity_conflict_after_claim`). Before this amendment the fence compared
the RAW caller-supplied `NaiveTime` against whole-second names parsed from
disk, so a claim at `08:30:00.877` missed a reservation parsed from
`08-30-00.recovered.mkv` and the old transaction's identity was handed to
new footage — the exact data-loss window the fence exists to close. The
full (sub-second) timestamp remains available only where human/event
metadata needs it; no helper is relied on to truncate independently.

### Sub-second regression coverage (§2/§3)

Both race regressions are duplicated with sub-second claim timestamps and
were verified to FAIL against the pre-amendment fence before the fix:

* the storage-layer fence test claims at `08:30:00.123456789` against a
  planted `08-30-00.recovered.mkv` reservation and proves the fence treats
  them as the same identity second (retry to `08-30-00-2.partial.mkv`,
  losing candidate relinquished, planted object untouched);
* the end-to-end concurrent-transition regression
  (`racing_claim_with_subsecond_start_time_never_reuses_the_transaction_
  identity`) re-runs the full clock-rollback race — parked claim, real
  recovery transition, `create_new` success on the freed name, fence,
  retry, crash, second startup recovery — at `2026-08-27 08:30:00.877`
  and proves both recovered recordings survive independently.

The two E2E gate users serialize on the process-global `FAULT_LOCK`
(arming the day-dir-keyed gate REPLACES the single armed slot, so parallel
gate-arming tests would clobber each other and hang on `wait_arrived`).

### Fence-cleanup failure is surfaced, never discarded (§4)

When the fence itself cannot validate (enumeration error), the candidate is
still never returned: the claim handle is closed first, ONLY this attempt's
still-empty candidate is removed, and a CLEANUP FAILURE now surfaces as a
dedicated typed `StorageError::ClaimFenceCleanup` carrying BOTH contexts
(the fence error as `#[source]` plus the cleanup OS error) — the previous
best-effort `let _ = remove_file(...)` silently discarded ambiguous
leftover ownership. The `conflicted == true` rollback path is unchanged.
A day-directory-loss test hook (`claim_day_dir_loss_fire`) deterministically
removes the day directory between candidate creation and fence validation,
so both failures are genuine OS errors (ENOENT) and the dual-context error
is exercised end-to-end.
