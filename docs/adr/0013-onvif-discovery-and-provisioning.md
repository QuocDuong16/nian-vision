# ADR-0013: ONVIF discovery and provisioning

- Status: Accepted
- Milestone: M10

## Context

Nian Vision already has an accepted camera domain, native credential store, RTSP
recording pipeline, media-worker probe path and M9 per-camera Desired/Runtime model.
M10 needs local ONVIF discovery and setup without making ONVIF a runtime dependency
for an already configured camera or creating a second persistence model. Network
cameras and their SOAP/XML/URI responses are untrusted input, and credentials must not
leak into React state beyond submission, persistent settings, logs, argv or errors.

## Decision

### ONVIF is provisioning only

ONVIF discovers a device, authenticates, enumerates media profiles and resolves a
selected profile to RTSP endpoint fields. A successful provision produces the same
`CameraDraft`/`CameraConfig` shape as manual RTSP creation. Recording remains RTSP via
`nian-media-worker`; M9 recording ownership, storage and lifecycle semantics do not
change. Manual RTSP setup remains first-class.

### Protocol infrastructure is isolated in `nian-onvif`

The safe-Rust `nian-onvif` crate owns WS-Discovery, SOAP request/response handling,
authentication, media profile parsing and stream URI authority validation. It has no
Tauri, FFmpeg, SQLite, settings or keyring dependency. Discovery is bounded and
cancellable, deduplicates endpoint identities and limits datagram/device/XAddr/scope
allocation. SOAP parsing caps body/depth/text/profile counts and rejects DTD/custom
entity expansion plus namespace spoofing of recognized ONVIF fields. Every ONVIF HTTP/SOAP
client explicitly disables automatic system/environment proxy discovery; local camera
authentication never relies on `NO_PROXY`. HTTP redirects are disabled and HTTPS keeps
normal certificate verification.

Discovery responder identity is part of the authority proof. M10 accepts a discovered
device-service XAddr only when its IP literal equals the UDP responder; hostname aliases
are not automatically trusted because they end in `.local`/`.lan` or otherwise look
private. Duplicate EndpointReferences from different responder addresses remain separate
discovery identities rather than merging their XAddrs. After device authentication, a
service XAddr may receive those credentials only when its host exactly matches the
authenticated device host; another port/path is allowed, but an unrelated RFC1918,
link-local or other local authority is not.

### Frontend authority is opaque and Rust-owned

The application `OnvifController` converts discovered devices to random session/device
handles. React receives safe display metadata, not discovery XAddr authority. After
authentication, credentials remain only in the Rust session. Profile tokens are only
accepted when they belong to that authenticated session/device; refresh, cancel,
suspend, quit and update invalidate the session. Resume reopens admission without
restoring prior discovery/authentication state.

### Authentication does not weaken secret handling

For an authority whose authentication mode is not yet known, the client first sends a
credential-free SOAP request so an HTTP Digest challenge can be negotiated. Digest is
preferred when available. WS-Security UsernameToken PasswordDigest remains a legacy
fallback when no usable Digest challenge is exposed. Only the chosen authentication
mode is cached per authority; passwords and challenges are not cached there. Passwords
are never serialized by protocol or application DTOs and credential-bearing input
structs deliberately avoid `Debug` and `Serialize`. Authenticated stream URI userinfo
is stripped before any endpoint crosses the protocol boundary, arbitrary RTSP query
strings are rejected rather than persisted/exposed, and the stream host must remain
bound to the authenticated device host. Redirects cannot carry authentication to another
authority.

### Media2 is preferred; H.264 remains the recording compatibility boundary

Device Management discovers service endpoints. Validated candidates are retained and
tried deterministically, preferring Media2 before legacy Media and HTTPS before HTTP.
Timeout, unreachable, protocol and unsupported failures may advance to the next validated
candidate; `AuthFailed` stops immediately, and authority rejection never weakens the trust
model. Profile metadata remains user-visible enough to choose among meaningful H.264
streams. Other codecs, including H.265, may be reported but are unsupported for M10
selection. M10 does not transcode or broaden the recorder codec contract.

### Final provisioning must prove the resolved RTSP source

`GetStreamUri` is parsed into validated host/port/path fields and never persisted as
an authenticated URL. The desktop prepares an unsaved ordinary camera probe, releases
its lifecycle admission lock, runs the existing media-worker/FFmpeg probe, then
rechecks the same ONVIF session/profile and lifecycle state before calling
`CameraService::create_camera`. Thus cancellation or lifecycle change during the
network/media probe prevents a stale camera save. Credential persistence and settings
insert use the existing native credential reference transaction and rollback behavior.

### Network work stays outside global application locks

Discovery and SOAP work run off the Tauri main thread. Blocking network I/O is not
performed while holding recording-controller or lifecycle admission locks. ONVIF
operations may fail independently of recording/playback and a failed discovered device
does not invalidate unrelated configured cameras.

## Consequences

- Adding ONVIF does not change `CameraId`, settings schema, credential-store ownership,
  media-worker IPC or M9 recording slots.
- Normal CI can use deterministic UDP/XML/HTTP fixtures; LAN broadcast and physical
  camera access remain optional manual validation.
- Cameras that advertise only hostname-based ONVIF device-service aliases are not
  auto-provisioned in M10 because no bounded alias-equivalence resolver is implemented;
  manual RTSP remains available.
- Cameras with unsupported/malformed ONVIF implementations can still be configured
  manually by RTSP.
- M10 intentionally does not include PTZ, presets, events, talkback, live-view
  redesign, H.265 recording, transcoding, cloud discovery or automatic adoption.
