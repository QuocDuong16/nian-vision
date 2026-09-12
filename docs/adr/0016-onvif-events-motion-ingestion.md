# ADR-0016: ONVIF PullPoint motion-event ingestion

- Status: Proposed (implementation complete; final remediation review pending)
- Milestone: M13

## Context

M10 introduced a hardened local ONVIF discovery/authentication boundary and M12 reused it for an optional PTZ control plane. M13 needs local motion-event ingestion without making ONVIF a prerequisite for RTSP recording/live view, without exposing subscription endpoints or credentials to React, and without treating noisy camera notifications as authoritative history.

The supported event surface is intentionally narrow: standard ONVIF PullPoint subscriptions plus namespace-qualified standard motion topics `RuleEngine/CellMotionDetector/Motion` (`IsMotion`), `RuleEngine/MotionRegionDetector/Motion` (`State`), and `VideoSource/MotionAlarm` (`State`). A fingerprint-gated compatibility adapter exists only for the reference TP-Link/Tapo C200 family: it may normalize the observed ONVIF-topic `RuleEngine/PeopleDetector/People`, `RuleEngine/TPSmartEventDetector/TPSmartEvent`, and `RuleEngine/LineCrossDetector/LineCross` states into the existing local motion transition model. This does not expose rule/analytics configuration or make arbitrary vendor event dialects generic ONVIF support. Presets, rule configuration, other vendor event dialects, analytics configuration, push notifications, talkback and cloud delivery are not part of M13.

## Decision

### Persist Event intent and binding separately

Settings schema v5 adds an optional `event_bindings` row plus per-camera `event_monitoring_enabled`. The binding stores only the selected ONVIF Device-service authority, endpoint reference, opaque credential reference and credential ownership bit. Event-service XAddr, PullPoint SubscriptionReference, SOAP/XML, subscription lifetime and passwords remain runtime-only.

Pair and enable are separate operations. Pair proves and stores the optional association but leaves Desired Event Monitoring Off. Enable/Disable changes persisted Desired intent. Runtime failure never clears Desired intent. Unpair atomically disables Desired intent and removes only the Event binding; it does not alter RTSP recording/live/PTZ state.

Camera mutation remains authority-aware. Changing the RTSP host requires Event unpair. Replacing camera credentials also requires Event unpair when the binding reuses the camera credential reference; an Event-owned credential remains independent. Camera deletion captures Event credential metadata before the database transaction and performs only post-commit external credential cleanup, deduplicated against camera/PTZ refs.

### Pairing remains explicit and generation-bound

For an already configured RTSP camera, initial Event pairing reuses the camera credential only inside the desktop/native credential boundary. The backend silently performs WS-Discovery, exact-host matches the discovered ONVIF device to the saved RTSP host, authenticates the Event service, and hands `PreparedEventPairing` directly to `EventController`; the frontend sends only `camera_id`. Explicit discovery/device selection and alternate ONVIF credentials remain a fallback when the saved camera credential is rejected. The older session/device path remains generation-bound for that explicit fallback and replacement flow. Credentials never enter the Event pairing command.

`EventController` also re-reads the current RTSP camera and Event binding before runtime authenticated traffic and requires exact host equality. Event-service and PullPoint URLs are independently checked by `nian-onvif`: HTTP(S) only, same physical host as the selected Device service, no userinfo, query or fragment. Redirect following remains disabled and the client remains proxy-free.

### `nian-onvif` owns PullPoint protocol handling

The protocol layer resolves the Event service through authenticated `GetServices`; when firmware omits Events there, it falls back to standard `GetCapabilities(Events)` and applies the same-host authority validation before continuing. Generic devices must advertise a compatible motion topic through `GetEventProperties`. If bounded `GetDeviceInformation` fingerprints the device as TP-Link/Tapo C200, the adapter may additionally derive the fixed same-host TCP 2020 `/onvif/service` candidate and may proceed to a real PullPoint subscription probe even when Event properties omit a compatible motion advertisement. The derived candidate still passes the normal event-XAddr validation and never changes host, accepts redirects, embeds credentials, or bypasses response/XML bounds. The client then creates a PullPoint subscription, requests a synchronization point, long-polls `PullMessages`, renews the subscription at roughly two-thirds of a validated lifetime, and unsubscribes on teardown. The compatibility policy is carried by the runtime subscription and is re-fingerprinted when the worker reconnects; it is not persisted as authoritative camera configuration. When both timestamps exist, a remote lifetime must be positive and at least 5 seconds; shorter advertised lifetimes are rejected rather than clamped upward. Lifetimes above 24 hours are shortened locally to the 24-hour safety bound. Missing timestamp metadata uses the finite 40-second fallback. Renew responses use the same lifetime validation before replacing the current subscription metadata, and renewal uses checked `Instant` arithmetic. Pull timeout, message limit, response size, XML depth/text, namespace count and URL size remain bounded. DTD/custom entities remain rejected.

Generic ONVIF normalizes only the supported namespace-qualified standard motion topics. `TopicSet` nodes must resolve to `http://www.onvif.org/ver10/topics`, and notification QName prefixes are accepted only when their in-scope binding resolves to that namespace. `CellMotionDetector/Motion` consumes `IsMotion`; `MotionRegionDetector/Motion` and `VideoSource/MotionAlarm` consume `State`. Under the C200 compatibility policy only, `PeopleDetector/People` consumes `IsPeople`/`IsMotion`, `TPSmartEventDetector/TPSmartEvent` consumes bounded `IsTPSmartEvent`/`IsVehicle`/`IsPet` states, and `LineCrossDetector/LineCross` consumes `IsLineCross`. Each Tapo detector/data-field pair is included in the hashed source discriminator so simultaneous vehicle/pet/people states cannot overwrite one another in the normalizer. Boolean values accept `true`/`false` case-insensitively plus `1`/`0`; malformed values are protocol errors. Vendor/evil namespaces with identical local names and unqualified ambiguous Topic text remain ignored or rejected conservatively. Source `SimpleItem` tuples are sorted and SHA-256 hashed before leaving `nian-onvif`; raw source tokens are never persisted or rendered. Malformed optional device timestamps degrade to `None` rather than poisoning the worker.

### Runtime ownership is bounded per camera

`EventController` owns a registry of `opening`, `active`, `draining`, and `mutating` state. `draining` stores controller-owned `Arc<DrainState>` objects containing the stable session identity, camera identity, one join owner and a completion condition variable. A worker remains registry-visible until its join has completed; one lifecycle caller leads the join while followers wait the same DrainState. Removal uses exact session/Arc identity, so stale completion cannot erase fresh same-camera ownership. Worker capacity is `opening + active + draining <= 16`; `mutating` does not consume a worker slot but excludes fresh same-camera admission. Same-camera mutation contention fails fast `Busy` instead of creating an unbounded waiter queue. Other cameras remain independent.

Opening and mutation state carry lifecycle generations and completion ownership. A late network result cannot become active after lifecycle cancellation, binding replacement, unpair or camera deletion. Terminal teardown waits openings, workers and mutation side effects, including credential rollback/cleanup. No registry/global lifecycle mutex is held over SOAP requests, SQLite I/O, keyring I/O or thread joins.

Each active camera owns one long-lived worker. Recoverable control/subscription/renew/pull/persistence failures remain inside that worker and recreate subscriptions with the camera-local `2s, 5s, 10s, 30s, 60s` backoff. Subscription creation alone does not reset the failure streak; only a successful `PullMessages` response or a completed bounded long-poll timeout resets it. Terminal Unsupported/Auth/Authority failures keep the worker alive in bounded `Failed` state until cancellation, avoiding a dead JoinHandle stranded in `active`. It never spawns a thread per poll. Desired state remains On while Runtime reports Starting/Subscribing/Polling/Backoff/Failed.

### Persist motion transitions before normalized-state commit

Each worker begins with motion state `Unknown`. Synchronization `Initialized` notifications establish baseline state without producing historical rows. The normalizer keeps at most 64 hashed source states for the worker lifetime. Once an unknown source arrives at capacity it is not inserted and aggregate `motion_active` becomes `None` for the rest of that worker lifetime, a conservative signal that an untracked source may still be active. Live transitions then produce only:

- `Unknown -> Active`: `MotionStarted`;
- `Unknown -> Idle`: baseline only;
- `Idle -> Active`: `MotionStarted`;
- `Active -> Idle`: `MotionEnded`;
- repeated state: no row.

Transition evaluation is prepare/commit: the normalizer first computes the candidate next source state without mutating authoritative in-memory state. A transition row and its required retention cleanup then settle in one Event-index transaction; only after that transaction succeeds does the normalizer commit the next state and expose updated `motion_active` / `last_event_at`. Persistence failure therefore leaves the previous normalized source state authoritative, allowing replay after subscription recreation to retry the same transition. A duplicate fingerprint is a successful persistence outcome because the row already exists, so runtime state still commits to the persisted truth. Synchronization baselines and repeated/no-transition observations commit directly without creating history rows.

Subscription recreation does not reset the bounded in-memory source state. This suppresses timestamp-less immediate redelivery after a successfully persisted transition because an already-active source remains Active instead of returning to Unknown. If persistence failed, that next state was never committed, so timestamp-less replay produces the transition again and retries persistence. A genuine later opposite transition still persists normally. A fresh worker, including Suspend/Resume restoration, starts from Unknown and requires synchronization baseline again. When the camera supplies a usable device timestamp, the existing SHA-256 fingerprint over camera, transition kind, source hash and device timestamp remains an additional persistence dedupe key. Receive time is always stored and remains the primary host ordering clock.

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
- Desktop exposes aggregate `event_statuses` through `spawn_blocking`; Cameras joins that bounded result locally, while Live View polls one aggregate request every five seconds with a single-flight guard, so overlapping timer ticks never queue SQLite/status batches.
- Exact-host authority proof is intentionally conservative; hostname/IP aliases may require re-pairing rather than silent equivalence.
- Physical-camera interoperability remains a manual validation boundary; CI uses deterministic parser, local HTTP, lifecycle and persistence fixtures.
