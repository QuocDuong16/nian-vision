# ADR-0014: Independent multi-camera live view

- Status: Accepted
- Milestone: M11

## Context

Nian Vision already has camera definitions in authoritative settings, native credential
storage, M9 independent recording slots, M10 ONVIF provisioning, an isolated FFmpeg
media worker and an M6 tokenized loopback HTTP pattern for browser playback. M11 needs
up to four user-selected live camera views without turning recording into a prerequisite,
exposing authenticated RTSP URLs to React, or creating a second camera/credential model.

Live viewing is transient UI-owned work. A hidden or crashed frontend must not leave an
RTSP connection or worker process owned forever, and a failing camera must not stall or
tear down unrelated live or recording sessions.

## Decision

### Live ownership is separate from recording ownership

`LiveViewController` owns M11 live sessions independently of `RecordingController`.
Starting/stopping recording does not create, close or replace a live session, and live
capacity does not consume M9 recording capacity. M11 admits at most four simultaneous
live cameras. One camera may have at most one active/opening live session.

Each admitted live camera owns its own media-worker process. The desktop reserves the
camera/capacity under the controller registry, starts and handshakes the worker outside
that registry lock, then commits the started session. Admission/started handles are
RAII-owned so cancellation before commit releases the reservation and temporary files.
Per-session runner locks allow status IPC for different cameras to proceed independently.

### Existing camera and credential boundaries are reused

`CameraService::prepare_live` resolves an existing `CameraConfig` plus its native
credential reference and creates secret-bearing worker input only inside Rust. That
prepared input is deliberately not a frontend DTO. React receives only camera identity,
an opaque UUID session id, typed live status and a `127.0.0.1` media URL. Authenticated
RTSP URLs are not logged, persisted in another store or returned through Tauri.

### Live media is H.264 packet-copy fragmented MP4

The live worker opens RTSP through the existing FFmpeg wrapper, selects the video stream,
requires H.264 and packet-copies video only into fragmented MP4. M11 does not transcode,
decode for analysis, add H.265 compatibility or couple audio support to live admission.

Transient source/media loss uses a bounded live-specific reconnect schedule of
1/2/4/8/15 seconds with five total attempts. Cancellation interrupts an active FFmpeg
operation and also aborts backoff promptly. Worker status is camera-local; a failed status
request is projected as a typed failure for that camera rather than failing the aggregate.

### Browser media access is an opaque loopback capability

The application binds an ephemeral `127.0.0.1` HTTP listener. A committed session maps
one random UUID path `/live/<uuid>` to one application-created media file. The endpoint
accepts only GET/HEAD, validates the loopback Host and expected desktop/development Origin,
rejects malformed/arbitrary paths, and cannot be supplied a camera URL or filesystem path.
Closing/expiring a session removes that capability; subsequent requests receive a stale
session response. The desktop CSP remains restricted to loopback media.

### Keepalive plus an application reaper owns abandoned-session cleanup

The frontend sends one aggregate live keepalive loop every 30 seconds. Session expiry is
two minutes. Expiry does not depend on another command arriving: a background application
reaper removes abandoned sessions, invalidates their HTTP capability, stops the worker and
releases capacity. Explicit close and controller shutdown use the same ownership cleanup.

### Desktop lifecycle does not manufacture persistent live intent

Live selection is transient and is never persisted as Desired state. Close-to-tray hides
the main window and closes all live sessions while leaving recording Desired/Runtime
ownership untouched. Suspend closes live admission/sessions; Resume only reopens admission
and never resurrects old session ids. Quit and update close admission and tear down live
workers before process/install handoff. Recording restoration continues through the M9
Desired path and is independent of live-view cleanup.

### The UI uses aggregate control loops and per-tile failure isolation

The dedicated Live View screen selects only cameras the user asks to view, up to four.
One aggregate status poll fetches live statuses, recording statuses and recording intent;
one aggregate keepalive loop services active live sessions. Each tile renders live and
recording state separately. During worker backoff the media element is unmounted; when the
same session returns to `live` with a new reconnect attempt the element is remounted so it
reopens the growing fragmented-MP4 stream. Media-element failure releases the backend
session before offering a fresh Retry.

## Consequences

- M11 adds no settings schema, camera database, credential store or persistent live intent.
- Recording and live viewing may coexist for the same camera using independent RTSP
  connections; this trades an extra connection for isolation and avoids destabilizing the
  accepted recording pipeline.
- Four live cameras may own four worker processes in addition to recording/playback workers;
  the explicit cap bounds this resource cost.
- A frontend crash or lost keepalive cannot permanently consume live capacity.
- One camera's reconnect/failure/status IPC does not invalidate other live tiles or recording.
- H.265 live view, transcoding, WebRTC, PTZ, events/motion, talkback, cloud/remote streaming
  and automatic persisted live layouts remain outside M11.
