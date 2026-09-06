# ADR 0017: Event Review and local desktop notifications

- Status: Accepted for M14
- Date: 2026-09-05
- Scope: M14 only

## Context

M13 owns ONVIF PullPoint ingestion and persists normalized `MotionStarted` / `MotionEnded` transitions in the local `EventIndex`. M14 must turn that persisted history into an operational review workflow without making review, playback, or notifications an authority for camera runtime ownership.

## Decision

### Event history authority

`EventIndex` is the sole historical review authority. Event Review never queries raw SOAP/XML, PullPoint subscription state, source tokens, or frontend memory to reconstruct history. Persisted rows remain reviewable across application restart, camera outage, disabled Event monitoring, and closed Live View.

The review API is bounded and indexed:

- default frontend range: last 24 hours;
- maximum query range: 31 days;
- maximum page size: 200 rows; the UI requests 50;
- maximum camera IDs in one query: 128;
- optional normalized kind filter: `MotionStarted` or `MotionEnded`; All omits the kind predicate;
- stable order: `received_time_utc DESC, event_id DESC`;
- keyset cursor: `(received_time_utc, event_id)`;
- Tauri history commands run through `spawn_blocking`.

`EventReviewRowDto` contains only normalized safe metadata. `source_key`, SOAP, ONVIF URLs, credential references and raw source tokens are not returned to React. Camera names come from the current camera configuration; if a historical camera identity no longer resolves, the stable `CameraId` is the fallback.

### Recording correlation and playback

Recording correlation uses:

`CameraId + Event.received_time_utc`

`received_time_utc` is authoritative because it describes when the local NVR received the transition. Device time is display-only context.

The recording index resolves only a finalized segment whose known interval contains the local timestamp using `[segment_start, segment_end)`. Unknown media duration is not guessed. Event Review does not scan directories or construct filesystem paths.

Playback uses the existing playback controller and an opaque playback session. M14 applies a fixed 5-second pre-roll and clamps the seek offset to the beginning of the matched segment. Event Review never creates event clips, transcodes media, generates thumbnails, or changes recording Desired state.

If no matching finalized recording exists, the Event remains valid and the UI reports `No recording available for this event.` Recording retention may therefore remove footage while Event metadata remains reviewable.

### Local notification projection

Desktop motion notifications are an optional projection of newly committed Events. The persisted preference is `motion_notifications_enabled`, stored in settings schema v6 and defaulting Off.

Required ordering is enforced in the Event controller:

`normalized transition -> EventIndex insert/commit -> newly inserted event_id -> notification admission`

A duplicate insert returns no new `event_id`, so it cannot generate a second notification. Persistence failure generates no signal. Startup does not query historical rows for notification replay.

`nian-onvif` remains unaware of notifications. A bounded application dispatcher owns notification delivery:

- global queue capacity: 32;
- admission uses non-blocking `try_send`;
- queue saturation drops the notification rather than blocking Event ingestion;
- only `MotionStarted` is eligible by default;
- rate limit: one notification per camera per 15 seconds;
- rate-limiter state: at most 128 camera entries, evicting the oldest entry when necessary;
- notifier failures are isolated from Event monitoring;
- no network I/O and no cloud notification service.

Native notification content is deliberately minimal: title `Motion detected`, body equal to the camera display name. It contains no IP address, ONVIF URL, credential, source token, or recording path.

### Notification activation capability

Tauri `tauri-plugin-notification` 2.4.0 provides desktop notification display on the current Windows/Linux targets, but its desktop Rust abstraction exposes `show()` and does not expose a notification-click/action callback. M14 therefore does not invent a fake deep-link mechanism. Event IDs remain safely resolvable through `event_get`; stale IDs return a non-fatal unavailable result. A future platform abstraction with a real activation callback can call the same Event Review selection path without changing Event history authority.

### Lifecycle

- Hide: Event monitoring and the notification dispatcher continue. Event Review unmount has no backend ownership effect.
- Suspend: notification admission closes and the bounded worker joins before Event monitoring is settled. Queued signals are not carried into Resume.
- Resume: a fresh bounded dispatcher queue starts before Event monitoring is restored. Historical Events are not replayed.
- Quit / Update install: notification admission closes and dispatcher ownership is joined as part of coordinated teardown.
- Storage-root change: Event workers are settled by the existing M13 settings transaction before the Event index is swapped. Subsequent review queries use the committed index.

### Frontend concurrency

Event Review uses generation ownership for filter reloads, pagination and selected-event recording lookups. A stale page cannot append after filters change, and a stale playback lookup cannot replace a newer selection. Polling is single-flight with a finite 10-second interval; the persisted Event index remains the source of truth.

## Security and privacy

Notifications may be displayed by the operating system on a lock screen, so only the minimal motion title and camera display name are shown. No credentials, LAN infrastructure details, ONVIF transport metadata or recording filesystem paths cross the notification boundary or Event Review DTO surface.

## M15 boundary

M14 does not add release tags, GitHub push/PR CI, production signing changes, AI detection, motion-triggered recording, event clips, thumbnails, transcoding, H.265, cloud/remote access, mobile support, email/webhooks, quiet hours, schedules, or advanced notification rules. Production hardening and release work remain M15.
