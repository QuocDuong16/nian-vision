# ADR-0015: Optional ONVIF PTZ control plane

- Status: Accepted
- Milestone: M12

## Context

M10 established a hardened local ONVIF protocol boundary for discovery and camera
provisioning, while M11 added transient multi-camera live view without changing the
accepted RTSP recording model. M12 needs local pan/tilt and optional zoom for a configured
camera without turning ONVIF into a prerequisite for recording, leaking control service
URLs or credentials to React, or allowing a lost frontend release event to leave a camera
moving indefinitely.

PTZ is therefore a control plane beside RTSP recording/live ownership. A configured RTSP
camera remains valid when no PTZ association exists, and PTZ failure must not stop or
mutate recording/live ownership.

## Decision

### Persist a separate optional PTZ binding

`CameraConfig` remains the authoritative RTSP source definition. Settings schema v4 adds
one optional `ptz_bindings` row keyed by `camera_id`. The binding persists only bounded,
non-secret device identity needed to reconstruct the ONVIF Device service authority:
scheme, host, port, path, endpoint reference, an opaque credential reference and whether
that credential is PTZ-owned.

The binding deliberately does not persist a password, PTZ service XAddr, profile token,
PTZ configuration token, SOAP payload, Digest challenge or capability range. Those values
are resolved and revalidated by the backend when a PTZ runtime session is established.
Deleting a camera cascades its PTZ binding; unpairing PTZ does not alter the RTSP camera or
recording Desired state.

### Pairing is explicit and authority-bound

PTZ pairing starts from an explicit M10 discovery/device authentication session. React
selects a discovered device and submits only opaque session/device handles to the pairing
command. `OnvifController` resolves the selected device and proves a PTZ service/profile/
configuration association before returning a Rust-only `PreparedPtzPairing`.

`PtzController` then compares the selected ONVIF Device-service host with the configured
RTSP camera host. M12 intentionally requires exact host equality; model, manufacturer,
display name and other descriptive metadata are never treated as device identity. A
service XAddr is separately validated by `nian-onvif` against the already trusted Device
service authority before authenticated PTZ traffic is sent.

If the ONVIF credentials equal the camera's existing credentials, the PTZ binding reuses
the camera credential reference. Otherwise a collision-resistant PTZ-specific reference
is allocated in the same native `CredentialStore`. Settings persistence failure rolls back
a newly written PTZ credential. Re-pair/unpair best-effort removes obsolete PTZ-owned
credentials and surfaces a typed orphan-cleanup warning if native cleanup fails.

### `nian-onvif` owns all PTZ protocol/network handling

The existing hardened SOAP client is extended with PTZ service discovery, media-profile to
PTZ-configuration association, configuration-option parsing, `ContinuousMove` and `Stop`.
The existing no-proxy client, disabled redirects, bounded SOAP body/XML depth/text,
namespace validation, DTD/custom-entity rejection, authentication negotiation and service
authority checks remain authoritative.

Velocity is capability driven. Pan/tilt is required for an M12-compatible PTZ binding;
zoom is exposed only when a continuous zoom velocity space is advertised. User input is a
small direction enum, not an arbitrary velocity. The application maps it to a fixed
normalized magnitude of 0.45 and `nian-onvif` maps the normalized value into the device's
advertised bounded velocity range. M12 does not expose presets, absolute/relative moves,
arbitrary speed or vendor-specific PTZ extensions.

Every `ContinuousMove` also carries an ONVIF camera-side timeout of one second. This is a
second dead-man layer in addition to application ownership and protects against host loss
after the camera accepts a move.

### Runtime ownership is per camera and bounded

`PtzController` owns at most 16 active PTZ runtime sessions. Each camera gets one worker
thread and a bounded four-command sync queue. Network I/O is serialized only for that
camera; no global PTZ/settings/lifecycle mutex is held through SOAP I/O. A slow or failed
camera therefore cannot serialize unrelated PTZ cameras.

Runtime session creation re-reads the persisted binding and native credentials, reconstructs
the Device-service URL, re-discovers the current PTZ service/configuration and refuses a
camera that no longer advertises compatible pan/tilt control. PTZ service/configuration
tokens are transient worker-owned values rather than settings authority.

### Movement uses generation ownership plus two dead-men

`ptz_move` allocates a monotonic generation. Starting a new move first stops any movement
currently owned by that camera worker, sends the new `ContinuousMove`, then returns the
generation with a one-second lease and 400 ms renewal cadence. `ptz_renew` extends only the
matching generation. `ptz_stop` stops only the matching generation, so a late release from
an older Left press cannot stop a newer Right press.

If renewal disappears, the worker's one-second lease expires and it sends axis-scoped
`Stop` automatically. The ONVIF `ContinuousMove` one-second timeout is an independent
camera-side fallback. Zoom stop controls only zoom; pan/tilt stop controls only pan/tilt.
Queue overflow fails with a typed Busy error instead of growing memory or blocking an
unbounded caller.

React also tracks per-control generations and pending move promises. Pointer/keyboard
release, pointer cancel/leave/lost capture and component unmount request Stop. If release
happens before `ptz_move` resolves, the late returned generation is immediately stopped.
Stale promise results cannot replace newer frontend ownership. Frontend correctness is
useful for responsiveness but is not the safety boundary; backend and camera dead-men are.

### Lifecycle always cancels motion and never restores it

Hide, Suspend, Quit and updater handoff close PTZ admission and set an out-of-band
`stop_requested` flag on every session before blocking teardown. This flag is independent
of the bounded command queue, so a full queue cannot preserve movement admission.
Existing per-camera workers perform the network Stop; lifecycle joins worker ownership off
the Tauri/window event thread. Hide freezes the worker handles that existed at capture time
into a teardown batch. A later reactivation may admit a new PTZ session immediately, and the
stale hide batch cannot drain or shut down that fresh ownership when its background join
eventually completes.

Resume reopens admission only. It never restores a prior PTZ direction, generation or
session movement. A later PTZ request creates/revalidates runtime ownership normally.

### The Tauri surface remains narrow

React receives safe capability/runtime DTOs and opaque movement generations only. The
desktop exposes pairing/unpairing, configured/capability reads, move/renew/stop commands;
raw SOAP, PTZ service URLs, profile/configuration tokens and passwords do not cross the
frontend boundary. Blocking PTZ work is dispatched through Tauri's blocking runtime.

## Consequences

- Existing manual or ONVIF-provisioned RTSP cameras require no migration action and remain
  recordable/viewable without PTZ.
- PTZ can fail, degrade, be unpaired or exhaust its own capacity without stopping recording
  or live-view ownership.
- Device motion has three independent stop paths: explicit matching-generation Stop,
  application lease expiry/lifecycle cancellation, and the ONVIF camera-side move timeout.
- Settings schema advances to v4 solely for the optional non-secret PTZ binding.
- M12 supports continuous pan/tilt and capability-gated zoom only. Presets, events/motion
  subscriptions, talkback, H.265/transcoding, WebRTC/cloud/remote control and vendor-specific
  extensions remain out of scope.
- Deterministic CI uses local SOAP fixtures and fake PTZ backends. Physical-camera PTZ
  interoperability remains a manual validation boundary.
