# ADR-0012: Simultaneous multi-camera recording

- Status: Accepted
- Milestone: M9

## Context

M7 deliberately modeled desired recording as one enabled camera and owned one
`RecordingController` run at a time. That was sufficient to prove lifecycle,
credential, recovery and worker-ownership semantics before introducing process
fan-out. M8 then made the same lifecycle authoritative across signed Linux AppImage
and Windows NSIS updates.

M9 must allow independent cameras to record concurrently without weakening the
accepted per-camera `CameraLease`, recovery, retention, playback, lifecycle or
worker-containment contracts. It is multi-camera **recording**, not live viewing,
ONVIF discovery, motion analysis, transcoding, cloud or remote streaming.

## Decision

### Recording ownership is per camera

`RecordingController` is an application-owned coordinator with a short-lived slots
map keyed by `CameraId`. Every live slot owns its own stop signal, `JoinHandle`,
status and independently-created `RecordingRunner`. Production uses
`SupervisorRecordingRunnerFactory`, so every active camera receives a distinct
`WorkerSupervisor` and therefore a distinct `nian-media-worker` supervision tree.
No mutex is held for the duration of a supervisor loop or worker IPC.

A finished slot is joined and removed independently. Its last public status may be
retained separately from live ownership, so dead threads do not consume capacity.
The accepted terminal-before-thread-return join reasoning remains per slot.

`CameraLease` remains the cross-process writer/recovery authority. A and B can own
their own leases concurrently. A second writer or recovery owner for A still fails
with `camera_in_use`. No desktop-global recording lock is introduced.

### Settings schema v3 makes desired state per camera

Schema v3 drops the M7 partial unique index
`cameras_single_recording_enabled`. `recording_enabled` remains a boolean on every
camera row and multiple rows may be enabled. The v2→v3 migration only removes that
index and advances `user_version`; it preserves the existing desired camera bytes
and rolls back if migration fails.

`set_recording_enabled(camera, enabled)` changes only that row.
`recording_enabled_cameras()` returns all desired cameras in deterministic
`CameraId` order. `set_all_recording_enabled(false)` updates all desired flags in one
transaction for tray Stop All.

Persisted desired state is independent from runtime state. Once a camera's Desired
flag commits On, a genuine startup/runtime failure leaves it On and exposes a
per-camera Failed status.

### Simultaneous recording is explicitly bounded

The current desktop uses `MAX_SIMULTANEOUS_RECORDINGS = 8`. The cap counts live
owned slots, not desired rows or terminal history. Eight is deliberately
conservative for the current one-worker-per-camera architecture and avoids turning
a configuration mistake into unbounded FFmpeg/process fan-out.

Interactive admission beyond the cap returns `recording_capacity`. During startup
restoration, desired cameras are considered in deterministic CameraId order; the
first available slots start, excess desired cameras remain Desired=On and surface
Failed/`recording_capacity` rather than being silently disabled.

### Start, Stop and Stop All are camera-scoped

Start under the desktop lifecycle admission gate validates the requested camera,
prepares its credentials/storage, proves that camera is startable and capacity is
available, persists only that camera Desired=On, then creates its runtime slot. A
different active camera is irrelevant to this admission. Duplicate Start for the
same owned camera returns `already_recording`.

Stop persists only the target camera Desired=Off before signalling that slot. It
does not signal, join or mutate another camera. A terminal worker race after the
durable Off commit is treated as successful convergence rather than a false
not-recording failure.

Tray `Stop All Recordings` first persists every Desired flag Off atomically, then
signals all live slots. If persistence fails, runtime ownership is left untouched.

### Lifecycle teardown signals all before joining any

Quit and signed-update teardown close new admission, request lifecycle stop for all
recording slots, then join all of them before playback/probe/tray/power teardown and
final exit/updater handoff. All stop signals are issued before the first blocking
join, so camera B never receives an extra full shutdown timeout merely because A is
slow to converge. Update teardown never clears Desired flags.

Windows Suspend similarly closes admission and requests stop for all slots without
blocking the native power callback. The dispatcher owns bounded convergence.
Resume completes old recording ownership and restores all desired cameras through
the normal deterministic restoration path. Duplicate Resume does not create extra
slots.

Startup restoration isolates camera failures. Missing credentials, invalid camera
configuration, `camera_in_use`, worker failure or capacity for camera A produces an
A-specific Failed status and does not prevent an unrelated valid B from starting.
Only authoritative settings/database failure aborts the whole restoration read.

### Status and UI are per camera

Desktop commands expose camera-scoped status and deterministic status collections.
Every `RecordingStatus` carries camera identity, state, failure category, reconnect
attempt and finalized-segment count. Observer callbacks run after the per-slot
status mutex is released and identify their owning camera.

The Cameras screen renders Desired and Runtime independently for every row and polls
the authoritative Rust status collection. Starting/stopping one row does not disable
unrelated row controls. The tray and UI may render a concise aggregate count, but no
synthetic global `RecordingState` represents incompatible per-camera states.

### Existing storage and playback authority does not move

Each camera keeps the existing storage tree and CameraLease-scoped publication and
recovery semantics. Retention remains global-storage aware and filesystem-first;
active partials from multiple cameras remain excluded. Playback pins remain keyed by
recording path and do not pin an entire actively-recording camera. Concurrent camera
leases therefore do not prevent playback of old finals or ordinary settled-file
retention.

Windows continues to put every worker in the desktop Job Object. Linux retains normal
child ownership/reaping. Hard desktop death, Quit and update may fan out to N workers,
but intentionally orphaning any worker remains forbidden.

## Consequences

- Camera process count now scales with simultaneous recording count, bounded at 8.
- Runtime failures are isolated to their camera slot instead of becoming a global
  RecordingController failure.
- Settings schema v3 is not downgrade-compatible with the old unique desired-state
  invariant; future-schema refusal remains fail-safe.
- Global recording-critical settings remain immutable while **any** slot is active;
  `launch_at_login` remains independent.
- Probe concurrency is unchanged and remains outside recording slots.
- M10 ONVIF and live multi-camera viewing are explicitly not part of this decision.
