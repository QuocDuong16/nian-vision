# ADR-0014: Independent multi-camera live view

- Status: Accepted
- Milestone: M11

## Context

Nian Vision already has authoritative camera definitions, native credential storage, M9
independent recording slots, M10 ONVIF provisioning, isolated FFmpeg workers and an M6
loopback HTTP capability pattern. M11 needs up to four user-selected H.264 live views
without coupling live ownership to recording, exposing authenticated RTSP URLs to React,
or introducing a second camera/credential model.

The initial M11 implementation proved the ownership split but retained every byte of a
live session in one growing MP4 and left several teardown/opening races. A production live
view must instead have bounded media storage, coordinated bounded teardown, controller
ownership of workers while they are still opening and race-safe frontend ownership.

## Decision

### Live ownership remains separate from recording ownership

`LiveViewController` owns M11 live sessions independently of `RecordingController`.
Starting/stopping recording does not create, close or replace a live session, and live
capacity does not consume M9 recording capacity. M11 admits at most four simultaneous
opening/active cameras and at most one live owner per camera. Recording and live viewing
may therefore use separate RTSP connections for the same camera.

Admission reserves camera/capacity under a short registry lock. Worker spawn/start runs
outside that registry lock. Admission and started handles remain RAII safety nets so
cancellation or task failure cannot strand reservations or temporary session storage.

### In-flight openings are controller-owned work

Each reservation points to an `OpeningState` containing camera/session identity, temporary
cache directory, cancellation state, the spawned runner and a completion condition. The
worker process is spawned first, installed into `OpeningState`, and only then performs the
hello plus `live.start` handshake. Lifecycle therefore owns the process while startup is
blocked, rather than discovering it only after startup succeeds.

`stop_accepting` marks all openings cancelled. Production hello/request waits poll that
cancellation at short bounded intervals, so lifecycle cancellation does not wait for the
normal five-second IPC deadline. A cancelled opening cannot commit. Lifecycle teardown
waits until each opening has either been reaped or has completed its failure cleanup.
Stale RAII cleanup is generation/session-id checked and cannot erase a newer reservation.

### Controller ownership continues through draining teardown

The live registry distinguishes opening, frontend-visible active sessions and draining
owners. `live_close` removes an active session from frontend-visible ownership and invalidates
its HTTP capability promptly, then moves the same session identity into controller-owned
draining state before any worker join/reap begins. `close_all` performs the same transition
for every opening and active owner. Draining owners count against the four-worker live
capacity bound until process ownership is gone.

Teardown is single-owner and waitable. The first caller that starts a session reap owns the
runner join; concurrent lifecycle callers observe the same draining session and wait on its
completion condition rather than starting a second join. A later Quit/Update/Suspend can
therefore observe a close-to-tray teardown already running on Tauri's blocking runtime and
cannot report lifecycle completion before that worker is reaped. Draining retirement is
session-id plus Arc-identity checked, so completion of an older same-camera session cannot
remove a newer active/opening/draining generation.

Worker/process ownership and deferred reader file cleanup are deliberately distinct. Once
the worker is reaped, draining worker ownership may retire even if an already-admitted HTTP
reader still pins a fragment. The capability remains invalid, the file is not deleted under
the reader, and the last reader deterministically performs deferred session-directory
cleanup.

### Live media uses a bounded rolling fragmented-MP4 window

The media worker opens RTSP through the existing FFmpeg wrapper, requires H.264 video and
packet-copies video only. It creates independently finalized fragmented-MP4 files beginning
on video keyframes. There is no decode/transcode path and recorder muxing semantics are not
changed.

The production bounds are explicit:

- `MAX_SIMULTANEOUS_LIVE_VIEWS = 4`;
- fragment target: 2 seconds;
- target retained finalized fragments per session: 6;
- hard finalized-fragment ceiling per session: 8 (six-window target plus two possible reader pins);
- nominal retained live window: about 12 seconds, subject to camera keyframe cadence;
- maximum finalized fragment size: 16 MiB;
- maximum HTTP readers per session: 2;
- maximum concurrent live HTTP requests across the listener: 8;
- live keepalive expiry: 2 minutes.

The worker rotates at a keyframe after the target window. Byte pressure also requests a
keyframe rotation; a pathological stream that cannot produce a safe bounded fragment is
failed rather than allowed to consume disk indefinitely. The application reaper trims old
finalized fragments continuously. If finalized files reach the hard ceiling, the worker
backpressures at the next keyframe boundary until retention or reader release creates room;
lifecycle cancellation interrupts that wait. Four sessions are bounded independently.

Every fragment finalization path owns exact cleanup of its `.partial.mp4`. Finalize, metadata/
validation and rename failures all return the original media failure while best-effort
removing only that worker-owned partial path. A successful rename preserves the finalized
`.mp4`; unrelated cache files and finalized fragments are never cleanup targets.

### Fragment readers own deletion safety

The loopback server exposes only session-scoped paths under `/live/<uuid>`: a bounded
manifest and fixed-grammar fragment names. A fragment request acquires explicit reader
ownership before opening the file. Retention never removes a fragment while a reader owns
it. Reader release retriggers trimming.

Each fragment is capped before being read and the server reads at most one bounded fragment
into memory before writing it to the socket. Slow HTTP writes are bounded by socket timeouts.
Session teardown has a bounded reader-drain deadline; if a reader has not released by that
deadline, teardown does not delete the file underneath it. The last reader performs deferred
session-directory cleanup after the capability has been deactivated.

### The live cache is transient and crash-cleaned safely

The live cache root is application-owned app-data. On startup, cleanup considers only direct
children matching the canonical `session-<uuid-v4-compatible UUID grammar>` layout and only
real directories, never symlinks. Lookalike names and unrelated files are preserved. A
stale-cache removal failure is warning-level observable and does not cause arbitrary broader
deletion.

### Browser media access remains an opaque loopback capability

The application binds an ephemeral `127.0.0.1` HTTP listener. React receives only camera
identity, opaque session UUID, typed state and the loopback session base URL. Authenticated
RTSP URLs and cache paths never cross the Tauri boundary.

GET/HEAD only, loopback Host validation, expected desktop/development Origin validation,
UUID/fragment grammar, bounded headers/readers/requests and fixed application-owned cache
paths prevent the endpoint becoming a LAN proxy or arbitrary file server. Closed/expired
capabilities become unusable. The CSP permits live fetches only from self plus
`http://127.0.0.1:*`, and media only from self, `blob:` and the same loopback origin.

The frontend consumes the rolling window with `MediaSource`: it polls the small session
manifest, fetches only unseen bounded fragments, appends them to an H.264 SourceBuffer and
trims old buffered media. Frontend fragment bookkeeping is O(1): it keeps only the highest
successfully appended sequence rather than an ever-growing set of historical sequence ids.
No remote streaming protocol or generic streaming server is introduced.

### Keepalive is cheap; the reaper owns expensive expiry cleanup

The frontend sends one aggregate keepalive loop every 30 seconds. `live_keepalive` validates
an active opaque session and updates only its timestamp. It does not expire another session,
perform worker IPC or wait for teardown. The background reaper owns expiry, fragment
retention and abandoned-session teardown.


The one-second aggregate status refresh is also single-flight. One mounted Live View screen
may have at most one `live_statuses` + `recording_statuses` + `recording_intent` refresh in
flight; interval ticks are skipped while it is pending. Resolve or rejection releases the
guard, and unmount ignores late results. M11 does not create per-camera polling loops.

### Teardown is signal-all-before-join and blocking work stays off Tauri's main path

`LiveRunner` separates `request_stop` from `join_or_reap`. Teardown first invalidates HTTP
capabilities and signals cancellation/stop to every opening and committed worker. Only after
the signal phase completes does a bounded fan-out join/reap workers and clean cache. With a
maximum of four live owners, this avoids both unbounded thread creation and the serial
`stop A -> wait A -> stop B` failure mode.

`live_open`, `live_close` and `live_statuses` run blocking controller work through Tauri
`spawn_blocking`. `live_keepalive` stays synchronous because it is bounded bookkeeping only.
Close-to-tray stops admission and hides the window immediately, then performs live teardown
on the blocking runtime rather than the window event thread. Suspend, quit and update wait
for live opening/active/draining worker ownership to be fully reaped before their required
lifecycle completion point.

### Frontend ownership uses per-camera generations

React keeps current selection, mounted state, committed sessions, in-flight opens and a
monotonic generation per camera in refs that remain authoritative across `await` points.
When a `live_open` result returns, it is committed only if the component is still mounted,
the camera is still selected and the generation still matches. Otherwise the returned
session is immediately closed and never enters the keepalive set.

Remove/unmount/retry/media-error all invalidate the current generation before asynchronous
cleanup. If a newer generation is requested while an older open is pending, the stale open
is closed on completion and the newer generation starts afterward; an old result can never
replace a newer session.

### Lifecycle does not manufacture persistent live intent

Live selection remains transient and is never persisted as Desired state. Close-to-tray
releases live ownership while leaving recording Desired/Runtime untouched. Suspend closes
live admission and ownership; Resume only reopens admission and never resurrects stale
session ids. Quit/update stop admission, cancel openings, signal committed workers, reap all
live ownership and only then complete their lifecycle handoff. Recording restoration remains
the M9 Desired path.

Transient source/media loss still uses the bounded live reconnect schedule 1/2/4/8/15
seconds with five total attempts. One camera's failure/status/reconnect remains isolated
from unrelated live and recording cameras.

## Consequences

- M11 adds no settings schema, camera database, credential store or persisted video-wall
  layout.
- Live resource use is bounded by explicit session, fragment, byte and HTTP-reader limits
  instead of elapsed session duration.
- A frontend crash, late Promise, lost keepalive or lifecycle race cannot permanently own a
  live worker/session/capacity slot.
- Same-camera recording + live view intentionally trades an extra RTSP connection for
  isolation and avoids destabilizing the accepted recording pipeline.
- H.265 live view, transcoding, WebRTC, PTZ, ONVIF Events, motion/AI, talkback, cloud/remote
  streaming and persisted live layouts remain outside M11.
