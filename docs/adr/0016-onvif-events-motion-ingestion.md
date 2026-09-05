# ADR-0016: ONVIF PullPoint motion-event ingestion

- Status: Accepted
- Milestone: M13

## Context

M10 introduced a hardened local ONVIF discovery/authentication boundary and M12 reused it for an optional PTZ control plane. M13 needs local motion-event ingestion without making ONVIF a prerequisite for RTSP recording/live view, without exposing subscription endpoints or credentials to React, and without treating noisy camera notifications as authoritative history.

The supported event surface is intentionally narrow: standard ONVIF PullPoint subscriptions and the standard `RuleEngine/CellMotionDetector/Motion` topic with an `IsMotion` boolean. Presets, rule configuration, vendor event dialects, analytics, push notifications, talkback and cloud delivery are not part of M13.

## Decision

### Persist Event intent and binding separately

Settings schema v5 adds an optional `event_bindings` row plus per-camera `event_monitoring_enabled`. The binding stores only the selected ONVIF Device-service authority, endpoint reference, opaque credential reference and credential ownership bit. Event-service XAddr, PullPoint SubscriptionReference, SOAP/XML, subscription lifetime and passwords remain runtime-only.

Pair and enable are separate operations. Pair proves and stores the optional association but leaves Desired Event Monitoring Off. Enable/Disable changes persisted Desired intent. Runtime failure never clears Desired intent. Unpair atomically disables Desired intent and removes only the Event binding; it does not alter RTSP recording/live/PTZ state.

Camera mutation remains authority-aware. Changing the RTSP host requires Event unpair. Replacing camera credentials also requires Event unpair when the binding reuses the camera credential reference; an Event-owned credential remains independent. Camera deletion captures Event credential metadata before the database transaction and performs only post-commit external credential cleanup, deduplicated against camera/PTZ refs.

### Pairing remains explicit and generation-bound

React reuses the existing ONVIF wizard and submits only `camera_id`, authenticated `session_id` and selected `device_id` to `event_pair`. Credentials never enter the Event pairing command. `OnvifController::prepare_event_pairing` snapshots the selected endpoint reference and stable authenticated `connection_id`, resolves Event capabilities, then revalidates the same session/device/connection generation after the blocking network operation. Cancel, refresh or reconnect therefore invalidates stale work before `PreparedEventPairing` can escape.

`EventController` also re-reads the current RTSP camera and Event binding before runtime authenticated traffic and requires exact host equality. Event-service and PullPoint URLs are independently checked by `nian-onvif`: HTTP(S) only, same physical host as the selected Device service, no userinfo, query or fragment. Redirect following remains disabled and the client remains proxy-free.

### `nian-onvif` owns PullPoint protocol handling

The protocol layer resolves the Event service through authenticated `GetServices`, inspects `GetEventProperties`, creates a PullPoint subscription, requests a synchronization point, long-polls `PullMessages`, renews the subscription at roughly two-thirds of its advertised lifetime, and unsubscribes on teardown. Pull timeout, message limit, response size, XML depth/text, namespace count and URL size remain bounded. DTD/custom entities remain rejected.

Only the exact standard motion topic is normalized. `IsMotion` accepts the XML boolean forms `true`, `false`, `1`, and `0`; malformed values are protocol errors and lookalike topics are ignored. Source `SimpleItem` tuples are sorted and SHA-256 hashed before leaving `nian-onvif`; raw source tokens are never persisted or rendered. Malformed optional device timestamps degrade to `None` rather than poisoning the worker.

### Runtime ownership is bounded per camera

`EventController` owns a registry of `opening`, `active`, `draining`, and `mutating` state. Worker capacity is `opening + active + draining <= 16`; `mutating` does not consume a worker slot but excludes fresh same-camera admission. Same-camera mutation contention fails fast `Busy` instead of creating an unbounded waiter queue. Other cameras remain independent.

Opening and mutation state carry lifecycle generations and completion ownership. A late network result cannot become active after lifecycle cancellation, binding replacement, unpair or camera deletion. Terminal teardown waits openings, workers and mutation side effects, including credential rollback/cleanup. No registry/global lifecycle mutex is held over SOAP requests, SQLite I/O, keyring I/O or thread joins.

Each active camera owns one long-lived worker. The worker recreates subscriptions with bounded camera-local backoff after recoverable failures. It never spawns a thread per poll. Desired state remains On while Runtime reports Starting/Subscribing/Polling/Backoff/Failed.

### Normalize motion transitions before persistence

Each subscription generation begins with motion state `Unknown`. Synchronization `Initialized` notifications establish baseline state without producing historical rows. Live transitions then produce only:

- `Unknown -> Active`: `MotionStarted`;
- `Unknown -> Idle`: baseline only;
- `Idle -> Active`: `MotionStarted`;
- `Active -> Idle`: `MotionEnded`;
- repeated state: no row.

Reconnect resets the in-memory baseline. When the camera supplies a usable device timestamp, a SHA-256 fingerprint over camera, transition kind, source hash and device timestamp provides cross-generation duplicate suppression. Receive time is always stored and remains the primary host ordering clock.

### Event history is a dedicated rebuildable runtime index

Motion history lives at `<storage_root>/.nian/events.sqlite3`, separate from authoritative `settings.sqlite3` and the recording catalog. Rows contain stable event ID, camera ID, normalized kind, optional hashed source key, optional device timestamp, mandatory receive timestamp and optional internal dedupe fingerprint. Passwords, SOAP, PullPoint URLs and raw source tokens are forbidden.

Cleanup is bounded: the configured age limit is reused when present, otherwise 30 days; a hard cap of 250,000 rows applies; cleanup removes at most 500 rows per pass. UI history queries are bounded.

A corrupt Event SQLite family is rebuildable but evidence is preserved. `DatabaseCorrupt`/`NotADatabase` causes the main DB and any WAL/SHM sidecars to be moved to a `.corrupt-<timestamp>-<attempt>` quarantine name before a fresh index is created. Future schema and ordinary I/O errors are not treated as corruption and are not replaced. Schema compatibility is checked before persisted WAL-mode changes so merely opening a future-schema file does not rewrite it.

Changing recording storage root prepares a candidate Event index, settles Event workers outside the desktop lifecycle gate, rechecks lifecycle/recording ownership, commits settings/playback, swaps Event storage, then restores Desired monitoring outside the global gate. A failed settings commit leaves the old root authoritative and reopens Event admission when the desktop is still Running. A post-commit runtime restore failure is surfaced without pretending the persisted settings rolled back.

### Lifecycle differs deliberately from PTZ/live view

Close-to-tray hides the window and releases transient live/PTZ ownership, but Event workers remain active because monitoring is background Desired work. Suspend stops Event admission and settles workers/mutations before sleep. Resume reopens admission and restores persisted Desired monitoring. Quit and updater handoff perform the same terminal Event settlement before process exit/handoff. No previous in-memory motion state is restored after Resume; a fresh synchronization baseline is required.

## Consequences

- Existing RTSP cameras remain valid with no Event binding.
- Event failures cannot stop recording, live view or PTZ.
- Pairing alone does not start monitoring.
- Motion history contains normalized transitions, not raw ONVIF messages.
- The frontend can show coarse runtime/motion status with low-frequency polling without receiving subscription authority or secrets.
- Exact-host authority proof is intentionally conservative; hostname/IP aliases may require re-pairing rather than silent equivalence.
- Physical-camera interoperability remains a manual validation boundary; CI uses deterministic parser, local HTTP, lifecycle and persistence fixtures.
