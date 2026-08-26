# ADR-0005: Storage and index model

- Status: accepted
- Date: 2026-08-26

## Context

Recordings must survive application crashes, database corruption and disk
pressure. The database is convenient for timeline queries but must never be
the only place a recording's existence is known.

## Decision

**Filesystem is the source of survival; SQLite is a rebuildable index.**

Layout (master spec §10), implemented in `nian-storage::RecordingsLayout`:

```text
<storage_root>/<camera-id>/<year>/<month>/<day>/HH-MM-SS[-N].mkv
```

* `<camera-id>` is a validated `CameraId` (`[a-z0-9][a-z0-9_-]{0,63}`) —
  user-provided display names never touch the filesystem. Every path
  component passes a traversal check (`checked_component`) that rejects
  separators, control characters, `.` and `..`.
* Open segments carry `.partial.mkv`; finalization is an atomic rename.
* The optional `-N` suffix (from `-2` on) disambiguates segments that start
  within the same second (rapid reconnect/restart). `allocate_segment`
  picks the smallest sequence with no existing partial or finalized file,
  so an existing recording is never truncated or overwritten; the parser
  accepts exactly the canonical forms the allocator emits.
* Startup reconciliation (M4) scans the tree and repairs the index:
  * DB entry without file → mark `missing`, then delete entry;
  * `.partial.mkv` file → inspect, mark `recovering`/`corrupted`;
  * file without DB entry → index it.
* SQLite (M4) runs with WAL mode, migrations from day one, and an index on
  `(camera_id, started_at)` for timeline queries. No media blobs in the DB.
* Retention (M4) supports `max_age_days` + `max_storage_bytes` with
  high/low watermarks (`StorageQuota`): cleanup triggers above the high
  watermark and deletes oldest **finalized** segments until the low one.
  Active/exporting/locked segments are never deleted.

## Consequences

* Losing or corrupting the database degrades search, not footage.
* Deletion is oldest-first by segment, keeping the timeline honest (gaps
  from retention look identical to gaps from outages — by design).
* Path logic is pure and unit-tested without touching a filesystem.
