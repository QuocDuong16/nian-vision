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

An RC is a real prerelease source version, not a tag alias over final-version source. For example every authoritative source surface is `1.0.0-rc.1` and the only valid tag is `v1.0.0-rc.1`; that GitHub Release is `prerelease=true` and `latest=false`. If RC validation finds a blocker, the next commit advances the source prerelease and receives a new immutable matching tag. After RC acceptance, a separate minimal version-only commit changes all surfaces to `1.0.0`, passes Forgejo CI/review, and only then may receive `v1.0.0` and become GitHub `latest`. RC binaries are never promoted as final artifacts.

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

`recordings.sqlite3` and `events.sqlite3` are derived operational indexes. Both use the shared restart-convergent SQLite-family quarantine protocol: main/WAL/SHM share one numeric generation, a `.quarantine-pending` marker persists that generation across interruption, no-replace moves preserve prior evidence, canonical members are verified absent before a fresh index may be created, and partial main-only/WAL-only/SHM-only generations count toward the same bounded retention set. M15 retains at most four corruption families per index. Future index schema versions fail closed and are not treated as corruption.

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

Automated source/release-contract tests are necessary but do not substitute for release-candidate installation and hardware interoperability. The RC source commit must first pass Forgejo CI/review and receive an exact prerelease tag such as `v1.0.0-rc.1`. M15 is not accepted, and final `v1.0.0` source/tag must not exist, until the documented clean Windows/Linux RC checklist is completed. Any RC blocker requires a new commit, advanced prerelease source version, Forgejo CI pass and new immutable matching RC tag; final acceptance then uses a separate `1.0.0` version-only commit and review.

## Known limitations

The normative v1 limitations are maintained in `docs/known-limitations.md`. In particular v1 is H.264 packet-copy only, has no transcoding/H.265/cloud/mobile/macOS, and retains the accepted 8 Recording / 4 Live / 16 Event-monitor capacity limits.
