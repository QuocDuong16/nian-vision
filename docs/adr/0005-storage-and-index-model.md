# ADR-0005: Storage and index model

- Status: accepted
- Date: 2026-08-26

## Context

Recordings must survive application crashes, database corruption and disk
pressure. The database is convenient for timeline queries but must never be
the only place a recording's existence is known.

## Decision

**Filesystem is the source of survival; SQLite is a disposable, rebuildable
query/index cache.** Recording publication never depends on database health.

The canonical media layout remains owned by `nian-storage::RecordingsLayout`:

```text
<storage_root>/<camera-id>/<year>/<month>/<day>/HH-MM-SS[-N].mkv
<storage_root>/<camera-id>/<year>/<month>/<day>/HH-MM-SS[-N].recovered.mkv
```

SQLite control state is centralized under a reserved directory that cannot
parse as `CameraId`:

```text
<storage_root>/.nian/recordings.sqlite3
```

WAL/SHM sidecars stay beside that database. `.nian` is never a camera tree,
never scanned as footage, never counted toward recording quota and never
removed by recording retention.

### Filesystem ownership

* `<camera-id>` is a validated `CameraId`; display names never become paths.
* Finalized normal and recovered MKVs are first-class recordings. Canonical
  partials, exact recovery scratch, recovery tombstones, camera lease files,
  write probes and Unknown files are not recordings.
* Deterministic inventory traverses only the exact
  `<camera>/<YYYY>/<MM>/<DD>/<file>` grammar. `symlink_metadata` is used at
  trust boundaries and directory/file symlinks are never followed.
* Segment claims and publication keep the M2/M3 race-safe rules: exclusive
  claim, whole-second identity + sequence, post-claim identity fence and
  atomic no-replace publication.
* `CameraLease` continues to mean kernel lock ownership, never lock-file
  existence. M4 uses a temporary non-blocking lease only when inspecting or
  cleaning active/unresolved partial or recovery artifacts. Old normal finalized
  recordings and fully settled historical recovered recordings do not require
  the camera-wide lease for retention; the latter rely on strict transaction
  revalidation instead, so continuous recording cannot pin them forever.

### SQLite boundary

`nian-index` is the only SQLite persistence crate. It forbids project unsafe
code, has no FFmpeg/Tauri/credential dependency, and uses pinned
`rusqlite 0.40.2` with bundled SQLite.

Schema v1 stores row id, validated camera id, UNIQUE relative recording path,
kind (`normal`/`recovered`), state, local naive wall-clock `started_at`,
sequence, size and nullable media duration. The timeline index is
`(camera_id, started_at, sequence)`. Absolute media paths and media blobs are
not stored.

Migrations are versioned through `PRAGMA user_version`, ordered and
transactional. A future schema version fails loudly. Runtime pragmas are WAL,
`foreign_keys=ON`, a bounded 2 s busy timeout, and `synchronous=NORMAL`. The
requested journal mode and foreign-key setting are queried back at startup;
anything other than actual `wal` / `foreign_keys=1` is a typed startup failure.
The last choice is intentional: SQLite transactions must be atomic, but index
bytes do not deserve stronger durability than the footage they can rebuild from.

The filename timestamp is local naive wall-clock time, not UTC. M4 preserves
that semantic explicitly rather than inventing an offset the filesystem never
encoded. Incremental finalized-event timestamps are normalized through the same
storage identity helper to the whole second encoded by the filename before
identity comparison or persistence; nanoseconds that cannot be reproduced from
disk never become part of SQLite identity.

### Reconciliation and rebuild

`nian-application::StorageManager` owns orchestration:

```text
filesystem inventory + SQLite snapshot
                 ↓
       reconciliation plan
                 ↓
      SQLite transaction
```

File-without-row is inserted; stale metadata is updated; row-without-file is
removed/reported missing; matching entries are no-ops. Partials are reported
active when another process holds the camera lease, or recovery-pending when a
temporary lease can be acquired. M4 never remuxes media.

Reconciliation is idempotent and fail-closed for retention readiness: the gate
is cleared at the start of every reconciliation/rebuild attempt and is restored
only after the complete operation succeeds. Deleting the database and rebuilding
from the filesystem restores all finalized normal + recovered recordings;
duration may remain NULL. SQLite corruption discovered at initial open or later
during reconciliation closes the unusable connection, quarantines the database
family and rebuilds a fresh schema from disk. A `.quarantine-pending` marker is
created before moving `recordings.sqlite3`, `-wal` and `-shm`; interrupted
quarantine is resumed before a future canonical database is opened, preventing
an orphan old sidecar from being associated with a new database. Existing
`.corrupt-N` evidence is never overwritten. Corrupt database state is never
authority for deleting media.

Incremental finalized-recording upsert is an optimization seam only. If it
fails, the media remains published and later reconciliation repairs the cache.

### Retention and crash ordering

Age and quota eligibility combine with OR semantics. The quota high watermark
is explicitly `RetentionPolicy.max_storage_bytes == StorageQuota.max_bytes`;
a separate lower `cleanup_target_bytes` is mandatory when quota retention is
configured. Cleanup starts only when usage is above HIGH and deletes oldest
finalized recordings until usage is at or below LOW. Stable ordering is
`started_at → sequence → relative path`. A blocked or uninspectable candidate is
reported and skipped while later eligible candidates are still considered. The
retention report records whether quota triggered, usage before/after, and whether
the LOW target was actually reached.

Immediately before deletion, retention revalidates that the candidate is still
inside the canonical root, is the same canonical recording kind/identity, is a
regular non-symlink file and still has the planned size. Control/recovery
artifacts and Unknown files neither count toward quota nor become candidates.

Filesystem and SQLite cannot form one atomic transaction. Therefore deletion
ordering is deliberately:

```text
revalidate media → remove media file → cleanup proven tombstone if applicable
                 → remove SQLite row
```

A filesystem deletion failure leaves the DB row. A crash after media deletion
but before DB deletion leaves a stale row that the next reconciliation removes.
For inserts/upserts, the filesystem object already exists before the SQLite
transaction. Every interrupted boundary converges toward filesystem truth.

Recovered recordings require extra resurrection protection. The v2 tombstone
parser/validator lives in `nian-storage` and is shared with `nian-recorder`.
Filesystem absence is never inferred from arbitrary metadata failure: only
`NotFound` proves a transaction path absent; permission/other IO errors are
uninspectable and preserve media/evidence. Retention considers a recovered final
settled only when the current final is the canonical trusted regular object bound
by an exact v2 tombstone and the original partial is proven absent. No
camera-wide lease is required merely because this historical transaction is
settled. Immediately before deleting the final, retention revalidates the final
identity and size, exact tombstone evidence, and that the original partial is
still `NotFound`. It then removes the recovered final first, verifies the
tombstone again before marker cleanup, and removes the DB row last. If any
revalidation fails, the candidate is skipped and footage/evidence are preserved.

Stale recovery scratch cleanup is a separate pass and requires successfully
acquiring the matching `CameraLease`. Tombstones are removed only in
conservative, resolved stale-marker states where both final and original paths
are proven `NotFound`; an uninspectable path preserves the marker and is reported
as an inspection failure. Unknown files are never deleted.

## Consequences

* Losing, deleting or corrupting SQLite degrades query availability, not
  footage; a filesystem rebuild restores the catalog without FFmpeg.
* SQLite is not allowed to roll back or invalidate already-published media.
* Retention remains available during continuous recording because old normal
  finalized files and settled recovered transactions do not require the live
  camera lease.
* Recovery artifacts require stronger evidence than normal finals; ambiguous
  transactions deliberately leak storage rather than risk footage loss or
  resurrection loops.
* The filesystem/DB boundary is intentionally eventually consistent after a
  crash, with reconciliation as the convergence mechanism.
