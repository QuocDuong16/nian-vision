# ADR 0018 — v1 Production and Release Architecture

- Status: Accepted for M15 implementation
- Date: 2026-09-06

## Context

Nian Vision M0–M14 defines the v1 product. M15 hardens that accepted architecture for production and release; it does not create another feature milestone. The independent Camera ownership planes remain Recording, Live, PTZ and Events. FFmpeg remains isolated in `nian-media-worker`, credentials remain backend-only, Event Review remains a projection of normalized `EventIndex` rows, and local notifications remain isolated from Event ingestion.

## Decision

### Source and CI authority

Forgejo is the authoritative source repository and the only normal push/PR CI authority. Its quality workflow runs formatting, workspace check, the full workspace/all-target/all-feature Clippy gate with warnings denied, workspace tests, dependency policy and frontend lint/typecheck/test/build.

GitHub is a one-way mirror. `.github/workflows/release.yml` runs only for intentional `v*` tag pushes delivered by the configured mirror identity. It must never become duplicate normal CI.

### Release identity

`[workspace.package].version` is the application version authority. The committed Tauri, root package and UI versions must match it, and `scripts/release/version-check.mjs` rejects drift or a release tag other than `v<application-version>`. A release is built from the exact tag commit and that commit must be reachable from the mirrored default branch.

A prerelease SemVer tag such as `v1.0.0-rc.1` is published as a GitHub prerelease and is explicitly not `latest`. This prevents a release-candidate workflow validation from becoming the production updater channel. Only a final SemVer release may become GitHub `latest`.

### Supported release matrix

v1 release targets are:

- Linux x86_64: AppImage, Debian 12/glibc 2.36 build baseline;
- Windows x86_64: current-user NSIS installer, MSVC target on `windows-2022`.

macOS and mobile are not v1 targets.

Both packages contain the desktop executable, sibling `nian-media-worker`, the application-owned FFmpeg 8.0.3 shared runtime and release provenance/license evidence. Production code resolves the media worker relative to the installed desktop executable rather than a repository or current-working-directory path.

### Release artifact and publication model

Linux and Windows build independently, then signing jobs finalize platform candidates. Final verification requires both candidates, verifies updater signatures, platform/runtime hashes and source identity, assembles one multi-platform `latest.json`, one release manifest and one SHA-256 manifest, and scans the final boundary for configured secret sentinels.

GitHub publication is draft-first. The verified payload is uploaded, downloaded again, filename and byte equality are checked, `SHA256SUMS.txt` is reverified, and only then is the draft published. A required platform failure prevents publication. An already-published tag is never silently overwritten.

Tauri updater signing is mandatory. Windows Authenticode remains a separately provisioned publisher-identity layer; repository policy may require it and fail closed. No private signing material is committed to source.

### Persistence and corruption policy

`settings.sqlite3` is authoritative user configuration. It is not disposable. Future schema versions fail closed; migration steps are transactional; corrupt authoritative bytes are preserved and startup fails rather than replacing or quarantining the database. Credential values are not stored there, only opaque credential references.

`recordings.sqlite3` and `events.sqlite3` are derived operational indexes. Corrupt index families may be quarantined and rebuilt while authoritative settings/footage remain untouched. M15 bounds retained corruption evidence to four SQLite families per index so repeated corruption cannot consume storage without limit. Future index schema versions fail closed and are not treated as corruption.

EventIndex recovery produces a fresh usable index: new normalized Events can be inserted and Event Review queries work afterward. Historical Event rows lost with a corrupt derived index are not reconstructed into fabricated history.

### Recording/storage failure behavior

Recording output remains camera-local. Media is published only after successful finalization; incomplete/failed output remains partial or is classified by bounded recovery and is never advertised as finalized footage. Storage/index/retention failures are typed and do not authorize destructive directory-wide repair. Retention revalidates filesystem identity and playback pins before deletion, and deletion failures preserve the index/media state for a later retry.

### Lifecycle and process ownership

Close-to-tray keeps Recording, Event monitoring and notifications alive while closing transient Live and PTZ ownership. Suspend settles transient/runtime ownership and Resume restores only persisted Desired Recording/Event monitoring. Quit and Update are terminal ownership transitions and settle controller workers, playback, Live/PTZ, Event workers, notification dispatcher/helper, tray/power workers and media workers before final handoff/exit according to their existing contracts.

Windows media workers remain inside the existing kill-on-close Job Object. The M14 native notification call remains inside a short-lived helper process owned by the dispatcher; its delivery deadline is three seconds and timeout/lifecycle teardown terminates and reaps the helper. No historical notification work is replayed after startup/resume.

### Security and privacy boundary

Camera RTSP, PTZ and Event passwords stay in the native CredentialStore. Authenticated RTSP URLs, authorization headers, raw SOAP/XML and raw Event source tokens do not cross into frontend DTOs or ordinary SQLite. ONVIF keeps proxy/redirect restrictions, TLS verification, same-device authority checks, bounded XML and namespace-aware parsing. Camera-originating network/media operations retain finite deadlines and bounded buffers/queues.

Nian Vision v1 has no cloud video upload, remote-access service or cloud notification service. Recordings and Event history remain local to the configured machine/storage.

## Acceptance boundary

Automated source/release-contract tests are necessary but do not substitute for release-candidate installation and hardware interoperability. M15 is not accepted, and `v1.0.0` must not be tagged, until the authoritative Forgejo commit is green and the documented clean Windows/Linux release checklist is completed. Any blocker found after an RC requires a new commit, Forgejo CI pass and a new immutable tag.

## Known limitations

The normative v1 limitations are maintained in `docs/known-limitations.md`. In particular v1 is H.264 packet-copy only, has no transcoding/H.265/cloud/mobile/macOS, and retains the accepted 8 Recording / 4 Live / 16 Event-monitor capacity limits.
