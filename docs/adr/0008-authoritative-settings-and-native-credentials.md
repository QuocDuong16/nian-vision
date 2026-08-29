# ADR-0008: Authoritative settings and native credential storage

- Status: accepted
- Date: 2026-08-29

## Context

M4 introduced `<storage_root>/.nian/recordings.sqlite3` as a deliberately
disposable index of facts recoverable from recording files. Camera definitions,
recorder launch settings and credentials are different: losing them is user-data
loss, and passwords must not be recoverable from ordinary application files.

A single SQLite database cannot satisfy both meanings without making the M4 index
non-disposable. Storing passwords beside either database would also turn backups,
crash copies and ad-hoc database inspection into credential exposure paths.

## Decision

Nian Vision uses three independent ownership boundaries.

1. **`nian-settings` owns authoritative non-secret application configuration.**
   The Tauri host resolves its platform application-data directory and passes the
   absolute path `<app-data>/settings.sqlite3` into `nian-settings`. The crate does
   not call Tauri APIs, link FFmpeg or launch processes. Schema migration begins at
   v1 and a future/corrupt database fails without silent replacement.
2. **The native credential store owns camera secrets.** `CameraConfig` persists
   only an opaque, versioned `CredentialRef`. Production desktop builds use the
   operating-system-backed `keyring` implementation; tests use an in-memory/fake
   `CredentialStore` and therefore do not require a graphical keychain.
3. **`nian-index` remains the disposable recording catalog.**
   `<storage_root>/.nian/recordings.sqlite3` never becomes a source of camera
   configuration or credentials and remains rebuildable from footage.

RTSP endpoints are persisted structurally (`host`, `port`, `path`, audio policy)
with no userinfo. Credential references use collision-resistant random identity
with the stable grammar `nian-vision/<camera-id>/<uuid-v4>`; UUID generation is
owned by the application layer through an injectable `CredentialRefGenerator`,
not by `nian-settings` and not by wall-clock/PID/process-local counters.

The native credential store entry identifier is effectively a mutable key:
`set_secret(existing identity)` updates the secret already stored at that
identity. CredentialRef uniqueness is therefore a transaction-safety invariant,
not cosmetic naming. Username/password material is never part of the reference.

Credential replacement is versioned rather than in-place:

```text
put NEW secret ref
  -> commit camera row pointing at NEW ref
  -> delete OLD secret ref
```

Failure before the database commit leaves the old row/ref authoritative and the
new ref is cleaned up. Failure after commit may leave only an orphan old secret;
the committed camera continues to work. Delete performs the settings-row delete
first and secret cleanup second; cleanup failure never resurrects a deleted row.
Replacement allocation defensively rejects a candidate equal to the committed
old ref before any keyring write, and rejects any other candidate already occupied
in the native credential store before `set_secret`. Create also checks for a locally-known duplicate
CameraId before writing a credential; the database UNIQUE constraint remains the
authoritative arbiter for concurrent processes.

For M5, desired recording state is intentionally **session-only**. Saved camera
definitions and recorder/storage settings survive desktop restart, but recording
does not auto-start. Tray/autostart/power lifecycle remains M7 scope. M5 permits
multiple saved cameras and exactly one active desired recording.

## Consequences

- Deleting camera configuration never deletes historical footage. `CameraId`
  remains the stable filesystem identity even when the display name changes.
- Passwords are absent from both SQLite databases, Tauri responses, normal Debug
  output, worker argv/environment and filenames. The existing stdin IPC secret
  boundary remains the only desktop-to-worker transport.
- Settings-database corruption is operationally more serious than recording-index
  corruption and is preserved for diagnosis rather than auto-rebuilt.
- Native keychain availability is a production runtime dependency for camera
  credentials; CI unit/integration tests remain keychain-independent.
- M6 playback/timeline work is not implied by this state split.
