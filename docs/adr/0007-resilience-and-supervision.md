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
