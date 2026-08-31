# ADR-0011: Linux AppImage distribution and signed updater handoff

* Status: Accepted
* Milestone: M8

## Context

M0-M7 created a local-first NVR whose correctness depends on process isolation,
an exact FFmpeg ABI, durable recording publication and an explicit desktop
lifecycle. Distribution cannot simply copy the desktop executable: the media
worker and its FFmpeg runtime are part of the application compatibility boundary,
and replacing binaries while recording must not bypass M7 teardown.

Maintaining Windows, macOS and Linux packaging simultaneously would also expand
the release surface before one platform has been proven end to end.

## Decision

### Linux x86_64 AppImage is the only M8 production target

M8 ships `x86_64-unknown-linux-gnu` as an AppImage. Windows x86_64 remains the
primary future product target, but packaging/signing validation is deferred until
a Forgejo Windows runner exists. macOS packaging is deferred. Release CI builds on
Debian 12 to keep a deliberate glibc baseline.

AppImage is also the updater artifact on Linux, avoiding two competing ownership
models such as a distro package manager plus an in-app binary replacer.

### The application owns its FFmpeg runtime

Release CI builds FFmpeg 8.0.3 from a SHA-256-pinned upstream source archive with
shared libraries enabled and GPL/nonfree disabled. The candidate configuration is
mechanically validated and exercised by the real media integration suite.

The packaged worker resolves `libavformat.so.62`, `libavcodec.so.62` and
`libavutil.so.60` from application-owned files. Pre-bundle release staging uses
`$ORIGIN/../lib/nian-vision`; Tauri's AppImage bundler normalizes the packaged
worker RUNPATH to `$ORIGIN/../lib`, so the final AppImage stores the three SONAME
libraries in its private `/usr/lib`. Release staging and extracted-AppImage smoke remove
development overrides and proves those libraries come from the application-owned
runtime. License text, build flags and third-party notices are shipped with the
application.

### Desktop/worker application versions must match in packaged builds

IPC protocol compatibility alone is insufficient for a release pair. Worker
HELLO therefore carries `application_version`; packaged hosts reject a worker with
a missing or different application version even if the NDJSON protocol version is
otherwise compatible. Debug builds retain limited tolerance for legacy test stubs
that omit the field, but an explicit mismatch is always rejected.

### Signed update verification precedes lifecycle teardown

`tauri-plugin-updater` owns transport/signature verification in Rust. The frontend
has only narrow check/install commands. Update installation is explicit user
action.

The host downloads and verifies the updater artifact before changing lifecycle
state. After verification it marks update admission, closes new work and reuses
M7's graceful shutdown ownership. Desired recording intent is preserved. If the
installer handoff fails or returns after teardown, the current application
restarts so it cannot remain stranded in Quitting.

### Release configuration and private signing material stay out of Git and ordinary steps

Release-only Tauri configuration contains only the updater public key/endpoint and
bundle/resource mapping. The updater private key and password are injected only
into the signed AppImage build step, never job-wide. Frontend assets are built
before that step and the release config disables `beforeBuildCommand`, preventing
Vite/package lifecycle code from inheriting `TAURI_SIGNING_*`. The generated config
is removed after post-build verification in successful CI.

Production generation requires HTTPS updater/download authorities and rejects
local, loopback and reserved placeholder endpoints. Finalized artifacts include
checksums and non-secret provenance metadata; private keys and secret values are
never embedded.

### Release-time verification defends against signing-secret misconfiguration

Tauri runtime signature verification remains authoritative for downloaded updates.
In addition, release CI verifies the exact generated AppImage and `.sig` against
the exact public key configured into the release build before finalization. The
release verifier uses `minisign-verify`, matching the verifier family used by
`tauri-plugin-updater`; no custom signature algorithm is introduced.

Fixed offline regression vectors cover matching key, mismatched key, mutated
artifact and mutated signature. Secret-canary scans run at staging, extracted
AppImage and finalized release boundaries. The actual AppImage is also launched
under isolated Xvfb/D-Bus and must reach a backend readiness marker and remain
stable for a bounded interval.

Final metadata is mechanically revalidated so `latest.json`, the release manifest
and `SHA256SUMS.txt` all identify/hash the exact finalized AppImage/signature.
Forgejo artifact upload is not treated as production updater publication; a
separate deployment must make immutable payloads available before exposing
`latest.json`.

## Consequences

* Linux has one deterministic, updater-compatible release artifact.
* A release cannot accidentally pair the desktop with a worker from another app
  version.
* The worker does not depend on the user's system FFmpeg installation.
* Update checking does not interrupt recording; installation uses the same
  teardown invariants as explicit Quit.
* Release CI is intentionally more expensive because it builds and tests the exact
  FFmpeg runtime that will ship.
* Windows x86_64 release validation remains deferred for lack of an appropriate
  Forgejo Windows runner; existing Windows-first runtime architecture remains in
  scope and must not be removed. macOS distribution remains deferred.

## Rejected alternatives

* **Ship system FFmpeg dependencies**: rejected because ABI/configuration would be
  outside the application's control.
* **Static FFmpeg**: rejected by the existing dynamic/LGPL integration strategy.
* **Unsigned updater downloads**: rejected because transport TLS alone does not
  establish artifact authenticity.
* **Stop recording before download/verification**: rejected because update checks
  and failed downloads must not cause avoidable recording downtime.
* **Package all desktop OSes in M8**: rejected to keep the first distribution
  milestone auditable and actually testable end to end.
