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

`recording.start` validates CHEAP parameters, runs a storage pre-flight
(`RecordingsLayout::ensure_camera_dir` — the only synchronous place
`storage_unavailable` can be refused, and only for genuine infrastructure
failure), and replies promptly. The job thread then runs
`recovering → connecting → recording`: recovery is asynchronous, its
summary (`recovered / quarantined / failed / infrastructure_failures`)
rides the status snapshot, and stop/shutdown interrupt it in a bounded way
(recovery observes a shared interrupt handle; `recover_camera_partials_
with_interrupt` aborts blocked demux/mux I/O and stops between files).
Failures are TYPED: `RecoveryError::Storage` is infrastructure (may fail
the job permanently when nothing was recovered), `Unreadable` is a
per-file content verdict (quarantine; recording continues), `Cancelled`
is neither. The parent classifies `storage_unavailable` as a PERMANENT
start refusal (no worker restart loop); `start_failed` (thread spawn) is
the only transient refusal code.
