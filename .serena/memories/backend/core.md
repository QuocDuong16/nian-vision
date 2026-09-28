# Backend crate boundaries

- `nian-domain`: IDs, redacted credential values, recording/camera/event types and reusable policies.
- `nian-application`: validation, services/controllers, lifecycle admission, worker supervision, and orchestration; it composes domain + infrastructure without owning platform UI.
- `nian-settings`: authoritative non-secret camera/storage/recording configuration in app-data SQLite. Corruption or unknown future schema is a safe refusal, not a rebuild.
- `nian-storage`: canonical recording paths, leases/claims, filesystem inventory, partial recovery facts, atomic no-replace publication. Filesystem truth outranks caches.
- `nian-index`: derived SQLite recording/event catalogs; WAL and schema behavior are verified, and catalogs can be rebuilt from authoritative facts.
- `nian-onvif`: bounded discovery and SOAP Device/Media/PTZ/Events protocol infrastructure; no Tauri, keyring, settings DB, or FFmpeg.
- `nian-ipc`: bounded, versioned NDJSON envelopes and stdio serve loop; handlers can emit events through the shared writer.
- For the full crate map and persistence invariants, consult `docs/architecture.md`; `mem:media/core` covers recording/media boundaries.