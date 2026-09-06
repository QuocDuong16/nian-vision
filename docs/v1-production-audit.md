# Nian Vision v1 production-hardening audit

This document records the M15 source-level audit. It distinguishes automated/source evidence from release-candidate/manual evidence. A source audit is not a substitute for clean-machine or physical-camera testing.

## Persistent schemas and recovery

### Authoritative settings

`settings.sqlite3` is schema v6. Supported historical schemas v1, v2, v3, v4 and v5 have deterministic migration fixtures into v6. The fixtures preserve data that existed at each version, including camera definitions/opaque credential references, Recording Desired, storage/retention/quota/autostart, PTZ bindings, Event binding/Desired state, and the v6 notification preference default.

Every migration step uses an explicit transaction and advances `PRAGMA user_version` only inside that transaction. Future versions return `FutureSchema` before migration. Random/corrupt authoritative settings bytes cause open failure and remain unchanged; the application does not quarantine or recreate authoritative configuration.

Settings intentionally uses SQLite's normal rollback journal (observed `journal_mode=delete` in the local v6 fixture) rather than forcing WAL. That is not shared with rebuildable runtime indexes. Atomic migration transactions and fail-closed preservation are the settings durability contract.

### Derived indexes

The recording/playback index and Event index use WAL and verify the expected schema/journal invariants. Future schemas fail closed. Corrupt derived indexes can be quarantined/rebuilt because authoritative camera settings and media are elsewhere.

M15 bounds retained corruption evidence to four SQLite families per derived index. Recording quarantine treats main/WAL/SHM as one serial family, survives the existing interrupted-quarantine marker path, uses checked serial allocation and bounded collision attempts. Event quarantine similarly bounds main/WAL/SHM evidence. After EventIndex corruption recovery, a fresh normalized Event can be inserted, fetched and queried by Event Review immediately.

## Storage and recording failure behavior

Existing recorder/application fault-injection coverage includes storage/infrastructure failure, segment finalization failure, output-open/write/metadata failures, index-update failure, retention filesystem-delete failure, stale/symlink replacement, quota/age pressure, active camera leases and playback pins.

The accepted publication boundary remains: incomplete output is a partial; only successfully finalized/no-replace-published media is a finalized recording. SQLite failure cannot turn an unpublished partial into valid media, and a published file is not rolled back merely because a later index operation fails. Startup reconciliation is the convergence mechanism. Camera-local recording failures do not revoke unrelated camera ownership.

A storage infrastructure failure is a typed permanent recording failure, not a retryable source failure, which prevents a tight restart/write loop on disk-full/read-only/unavailable storage. Partial recovery is bounded to owned recording names and never performs directory-wide destructive repair.

## Startup, crash and lifecycle behavior

Persisted Recording Desired and Event Desired restore independently. Restoration order is deterministic where capacity selection applies, and over-capacity cameras keep Desired state while receiving runtime capacity failure instead of being selected by hash iteration. Live, PTZ movement and playback are transient and are not restored.

Existing lifecycle tests cover Hide, repeated Suspend/Resume, Quit and Update idempotency/races. Hide retains Recording/Event/notification background ownership while settling Live/PTZ. Suspend closes new admission and settles runtime ownership before Resume restoration. Update enters the same terminal ownership boundary before installer handoff.

Windows release smoke has a hard-death Job Object proof for the exact packaged media worker. Linux package smoke proves worker runtime closure and desktop startup without development paths; clean-machine Linux controlled-shutdown/orphan verification remains mandatory release-candidate evidence in `docs/release-checklist.md` rather than being claimed by source tests.

The notification dispatcher owns a bounded queue and an owned native helper delivery. A helper has a three-second deadline; timeout, Suspend, Quit or Update terminates/reaps the owned helper. Queue saturation and native delivery failure do not block Event ingestion, and stale queued work is not replayed on Resume/startup.

## Long-lived ownership and locks

Audited long-lived resources are controller-owned rather than global fire-and-forget work: Recording runners/supervisors, Live sessions/reaper, Playback sessions/server, PTZ workers, Event PullPoint workers, notification dispatcher/helper, tray watcher and power dispatcher all have explicit shutdown/cancellation paths covered by lifecycle tests.

Media-worker stdin/stdout helper threads terminate when their owned channel/pipe closes; child-process shutdown remains bounded and can force-terminate an unresponsive worker. They do not own application teardown independently.

Controller design preserves per-camera ownership and avoids holding one global registry lock across camera network I/O or media-worker joins. Lifecycle coordination uses explicit admission/control gates so terminal transitions win races without inventing lock-order dependence. No new lock-order inversion was found in M15 changes.

## Panic, arithmetic and memory bounds

The M15 authoritative Clippy gate is `cargo clippy --workspace --all-targets --all-features -- -D warnings`. Runtime crates deny/flag unchecked unwrap/expect/panic use except narrow documented invariants, notably media-worker snapshot mutexes whose guard closures are intentionally panic-free. `bindgen-gen` was corrected so the full workspace gate no longer needs legacy `expect`/print exceptions.

Externally influenced collection/size controls already include bounded camera/controller capacities, 1 MiB IPC frames, bounded Event query pages/range/camera filters, Event retention cap/batches, notification queue/rate-limiter table, Live fragment/count/byte caps, ONVIF discovery/profile/XML/Event-item caps, and filesystem/recovery iteration policies. M15 corruption-backup retention also uses bounded working sets rather than collecting arbitrary directory contents.

Checked/saturating arithmetic is used at externally sensitive boundaries such as cursor/time/offset/size conversion, notification deadlines, recovery serial allocation and retention/media calculations. No new arithmetic blocker was found.

## Camera network and protocol security

ONVIF network operations retain finite deadlines: discovery defaults to 3 seconds, HTTP/SOAP to 5 seconds, PullMessages to 4 seconds, and camera-side PTZ movement timeout to 1 second. The HTTP client uses `no_proxy()` and `Policy::none()` redirects while ordinary TLS verification remains enabled.

ONVIF responses are bounded before parse (1 MiB SOAP maximum), XML depth/text/namespace/profile/topic/simple-item collections are capped, DTD/entity/deep payload regressions are rejected, and namespace-aware topic parsing is tested. Device/Media/PTZ/Event/PullPoint authorities remain same-device validated; M15 does not relax those rules for compatibility.

RTSP credentials stay inside backend secret wrappers/private IPC and are never command-line arguments. Frontend DTOs do not contain authenticated RTSP URLs, raw SOAP, authorization headers or credential-store values. Event source tokens are normalized/hashed before crossing the ONVIF boundary.

## Credential and logging audit

RTSP camera, independent PTZ and independent Event credential lifecycles already have create/replace/rollback/delete/shared-ownership regression coverage. Camera deletion and PTZ/Event unpair cleanup do not double-delete another subsystem's shared credential reference. Repeated failed provisioning/pairing paths clean transaction-owned secret refs rather than accumulating them.

Production tracing is sparse and state/category-oriented. No tracing call was found that intentionally emits passwords, auth headers, raw SOAP or authenticated source URLs. The desktop and worker currently log through tracing/stderr/platform launch environment; v1 does not add a new persistent log-file subsystem merely for M15. Operators should never be asked to include credentials in diagnostics.

## IPC audit

`nian-ipc` caps every NDJSON envelope at 1 MiB and tests exact-boundary, oversized, truncated and malformed frames. Protocol/application version mismatch fails closed. Worker supervision uses monotonic request IDs for status polling, rejects stale replies, gives hello/start/status/shutdown phases finite deadlines and classifies malformed terminal status as a protocol violation rather than silently reinterpreting it.

## Package, paths, updater and release supply chain

Production desktop resolves `nian-media-worker[.exe]` as a sibling of `current_exe()`. Release staging verifies the worker and pinned application-owned FFmpeg runtime; no production path depends on `target/debug`, repository-relative paths, `NIAN_FFMPEG_LIB_DIR` or developer PATH. Searches for `target/debug` find documentation/test-only references.

Release-critical versions are pinned or locked; M15 does not introduce gratuitous dependency upgrades. FFmpeg stays at source-pinned 8.0.3 shared LGPL configuration. GitHub release actions are immutable-SHA pinned and publication has the only `contents: write` permission.

The release workflow requires both Linux and Windows candidates before assembling metadata/checksums. Tauri updater signatures are mandatory; Windows Authenticode is optional unless the external repository policy marks it required. RC commits use synchronized prerelease source metadata such as `1.0.0-rc.1` and only the exact matching tag may build them; RC Releases are prereleases with `latest=false`. After RC acceptance a separate final version-only commit changes all surfaces to `1.0.0`, passes Forgejo CI/review, and only its exact final tag is eligible for production `latest`.

## Remaining non-source evidence

The following cannot be truthfully completed from source inspection alone and remain release prerequisites:

- authoritative Forgejo CI result for the pushed M15/RC commit;
- actual mirrored RC tag execution on GitHub-hosted Linux and Windows runners;
- clean Windows x86_64 install/upgrade/lifecycle smoke;
- clean Linux x86_64 AppImage install/runtime/lifecycle smoke;
- physical-camera interoperability, including the separately recorded Tapo C200 RTSP/Live/ONVIF/PTZ/Event matrix;
- real desktop notification presentation on supported Windows/Linux notification services;
- final updater handoff using production signing configuration;
- final downloaded-asset checksum/signature/install validation.

Until those are recorded, the source may be release-candidate ready but Nian Vision v1 is not declared complete.
