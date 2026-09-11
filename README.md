# Nian Vision

Local-first desktop NVR (network video recorder) for IP cameras. The first
supported camera is the TP-Link Tapo C200 over RTSP, with a camera-agnostic
domain so other RTSP/ONVIF cameras can follow.

**Status**: milestones **M0–M14 are accepted**. **M15 is the final v1 production-hardening/release milestone**. The current release-candidate source identity is Nian Vision 1.0.0-rc.36; final 1.0.0 source is created only after RC acceptance. RC1 through RC9 remain immutable historical release attempts: RC1 exposed the Linux Bash-shell mismatch, RC2 exposed container Git ownership trust, RC3 exposed Windows CRLF/fail-open native-command issues plus hosted-runner resource pressure, RC4 exposed a PowerShell parser error in the native-command failure message, RC5 reached Linux AppImage bundling before failing with an opaque linuxdeploy error, RC6 proved disk headroom was adequate while exposing the actual AppImage FFmpeg library-layout mismatch plus a pathological Windows FFmpeg extraction stall, RC7 proved deterministic extraction while exposing an MSVC dependency-command escaping failure at the uncontrolled Windows make boundary, RC8 failed in Windows preflight because a representation-level MSVC HostX64 path check rejected the real hosted-runner tool directory before FFmpeg configure, and RC9 proved the semantic MSVC remediation while exposing that hosted MSYS2 lacked `/usr/bin/make` and silently fell through to `/c/mingw64/bin/make`. RC10 consumed those toolchain protections and reached the aggregate Windows preflight probe, then exposed a separate transport failure: the large generated multi-line Bash program was passed through `bash -lc` and Bash reported an unexpected EOF before tool reporting completed. RC11 preserved the RC10 toolchain contract and file-backed generated Bash execution, then successfully passed Windows FFmpeg download, source SHA-256 verification, extraction and configure before exposing a PowerShell post-configure collection-cardinality bug: statement output could unwrap `$configureDiagnostics` to null or a scalar and StrictMode then rejected `.Count`. RC12 is now immutable: it preserved RC10/RC11 hardening, passed Windows preflight, source SHA-256 verification, extraction, configure, collection diagnostics, CCDEP on-disk validation and GNU make CCDEP expansion, then began real MSVC compilation and failed at `libavformat/cbs.o` with the authoritative root failure `fatal error C1001: Internal compiler error`; GNU make's later Error 127 was only a consequence. FFmpeg upstream commit `6a59c847b50c6bc30630df7fca56ccd6cd8a5a8c` identifies the empty CBS-in-lavf configuration as illegal C that may trigger MSVC ICE. RC13 is now immutable: it successfully applied that exact upstream CBS-in-lavf backport, passed the targeted `libavformat/cbs.o` MSVC regression compile, passed the full Windows FFmpeg compile and install, then failed only in post-install release validation because the validator incorrectly expected global `CONFIG_NETWORK` in `config_components.h`; FFmpeg 8.0.3 actually generates `CONFIG_NETWORK` in `config.h`. The observed failure did not show networking was disabled or that `CONFIG_NETWORK` was zero. RC14 is now immutable: it correctly validated `CONFIG_NETWORK` from `config.h`, passed the CBS targeted compile and full Windows FFmpeg compile/install, then failed in post-install validation because the release configure contract requested nonexistent URL protocol `rtsp`; FFmpeg 8.0.3 exposes RTSP as a demuxer, not a URLProtocol, so `CONFIG_RTSP_PROTOCOL` was an invalid expectation. RC15 is now immutable: it removed that nonexistent protocol request, kept the RTSP demuxer, and validated resolved requested components immediately after configure before compilation; it then failed later in Windows Rust quality because pinned Rust 1.98.0 MSVC was installed with `--profile minimal` without `rustfmt`, so `cargo fmt --all --check` could not find `cargo-fmt.exe`, while `clippy` was also not explicitly provisioned and remained a latent deterministic sibling. RC16 is now immutable: it explicitly provisioned `rustfmt` and `clippy` for the Windows build and successfully reached real Rust quality, then exposed two later packaging/build blockers: Windows `tauri-build` could not generate its resource because `apps/nian-desktop/icons/icon.ico` was absent, while the Linux AppImage contained the intended private FFmpeg runtime under `/usr/lib/nian-vision` plus duplicate legacy FFmpeg SONAMEs copied by linuxdeploy into `/usr/lib`. RC17 added the required Windows ICO asset and removed only byte-identical legacy FFmpeg duplicates before AppImage smoke, then exposed two later deterministic quality/package blockers: Windows all-target Clippy rejected a Unix-only `NaiveDate` test import as unused under `-D warnings`, while Tauri/linuxdeploy had rewritten the packaged worker RUNPATH from `$ORIGIN/../lib/nian-vision` to `$ORIGIN/../lib`. RC18 removed the platform-specific unused import and restored plus validated the private worker RUNPATH before repacking, then hosted validation exposed two later boundary issues: a Unix-only absolute storage path fixture failed Windows config tests, and GitHub artifact transfer stripped the downloaded AppImage executable bit before signed smoke. RC19 made the initial config unit-test fixture platform-native and restored plus verified the downloaded AppImage executable mode before signing, then hosted Windows workspace tests exposed a second Unix-only storage-root fixture in the camera-service integration suite. RC20 removed the remaining Unix-only storage/output test fixtures, locked that portability contract, and provisioned the host EGL runtime for AppImage desktop smoke, then hosted Windows workspace tests exposed a separate Bash-stub transport bug: native temporary script paths were passed as `bash -c` command strings, so supervisor fixtures produced no valid JSON hello frame. RC21 executed those fixtures as script-file arguments with ad-hoc slash-normalized native paths, but hosted Windows still produced malformed hello frames across the supervisor suite, while Linux progressed past the host EGL check and then exposed missing system `libGLESv2.so.2`. RC22 replaces the Windows fixture transport with the already-preflighted MSYS2 Bash plus `cygpath` contract, makes the version-mismatch fixture prove it reached the actual protocol validator, and provisions plus verifies both host EGL and GLES loaders for Linux AppImage smoke without injecting graphics loaders into the bundle. Hosted RC22 then exposed a Windows-only Clippy `needless_return` in the cfg-gated path conversion helper before tests could run, while Linux workspace tests exposed a separate test-only concurrency bug: the global retention pre-delete gate could be consumed by a sibling retention test and leave the intended worker blocked indefinitely. RC23 expresses Windows path conversion as a cfg-specific helper result with no needless return, and moves the retention race gate onto the individual `StorageManager` test instance so parallel tests cannot steal its synchronization signals. Hosted RC23 then exposed two independent release-boundary regressions: Windows checkout could convert the signed ASCII `artifact.bin` fixture from LF to CRLF because it lacked a binary Git attribute, invalidating the positive Minisign fixture, while Linux signed AppImage smoke reached tray initialization and failed because the clean host lacked `libayatana-appindicator3.so.1`. RC24 marks the signed verifier artifact fixture binary in `.gitattributes`, locks that Windows checkout contract, and explicitly provisions plus verifies the Ayatana AppIndicator runtime on both Linux build and signing smoke hosts. Hosted RC24 then exposed a Windows-only Unix path fixture in `nian-storage` and a Linux mixed-runtime failure where Ubuntu 24.04 Ayatana loaded against the older GLib bundled from the Bookworm AppImage build. RC25 made the storage layout example platform-native and bundled the Bookworm AppIndicator seed before linuxdeploy so its matching tray dependency closure travels with the AppImage, but hosted Windows failed immediately in `pnpm release:test`: `write-tauri-release-config.test.mjs` compared `node:path.resolve()` output with a hard-coded POSIX `/workspace/...` source path, so the drive-qualified Windows path could never match. RC26 made those release-config fixtures platform-native with `node:path.resolve()` and added a static Windows regression that rejects the POSIX-rooted fixture. Hosted RC26 Linux passed, while Windows progressed to staging and failed because `build-metadata.mjs` attempted to execute the `pnpm` Windows command shim directly through Node `execFileSync`, yielding `ENOENT`. RC27 makes pnpm provenance platform-native: Windows staging resolves and invokes `pnpm.cmd`, Linux staging invokes `pnpm`, and both pass the observed version into the metadata generator, which verifies it against the pinned package-manager version without spawning pnpm itself. Hosted RC27 Windows then reached the signed NSIS install smoke, which correctly detected that the installed desktop bytes differed from the signed source binary. Tauri bundler patches its main-binary bundle marker from `__TAURI_BUNDLE_TYPE_VAR_UNK` to `__TAURI_BUNDLE_TYPE_VAR_NSS` immediately before NSIS packaging and restores the source afterward; RC27 therefore invalidated the desktop Authenticode signature inside the installer by letting Tauri patch after signing while bundling with `--no-sign`. RC28 moves that exact fail-closed NSS marker patch ahead of Authenticode signing so NSIS embeds the already patched-and-signed desktop bytes without weakening the install smoke. Hosted RC28 passed installer byte identity and then failed in the installed desktop setup hook because Tauri Windows app-data resolution returned `UnknownPath` inside the smoke's deliberately redirected profile environment. RC29 preserves Tauri Known Folder resolution first and falls back only on Windows to an absolute `%APPDATA%` root plus the bundle identifier when that API cannot resolve a path. Hosted RC29 then passed installer byte identity, isolated runtime smoke, the APPDATA fallback, desktop startup readiness, native power subscription, and containment-worker readiness before the PowerShell smoke harness failed because two unsuppressed `ProcessStartInfo.Environment.Remove(...)` Boolean results were emitted into the function pipeline, turning the intended session object into a multi-value array under StrictMode. RC30 suppresses those environment-removal return values with `[void]` so `Start-DesktopContainmentSmoke` returns exactly one session object while preserving every existing startup and containment assertion. Hosted RC30 Windows kept the process alive through installer and clean staged-runtime validation, but an installed desktop smoke did not reach all startup, native-power, and containment readiness markers before the fixed 15-second deadline. The smoke redirected the synthetic Windows profile without assigning WebView2 an explicit user-data folder, leaving WebView2 profile placement and cold-start timing dependent on host/runtime defaults. RC31 pre-creates an isolated WebView2 profile and passes it through `WEBVIEW2_USER_DATA_FOLDER`, keeps all three readiness assertions fail-closed, extends only the hosted desktop cold-start deadline to 45 seconds, and reports the individual marker states on timeout. Hosted RC31 Windows then passed release secret scanning, clean staged-runtime smoke, the Windows app-data fallback, and `desktop_startup_ready`; the install smoke failed immediately afterward while verifying that a fresh install had not enabled launch-at-login. The desired fresh state has no `Nian Vision` value under the Windows Run key, but `Get-ItemPropertyValue` treats that absent property as an error even though the assertion expected absence. RC32 replaces optional Run-value reads with a fail-closed helper that returns null for an absent key/value, reads present values through `Get-ItemProperty` plus exact `PSObject.Properties` lookup, preserves the explicit required-value assertion after reconciliation, and uses the same semantics for the post-uninstall stale-entry check. Hosted RC32 Windows then passed release secret scanning, clean staged-runtime smoke, three installed-desktop startups, the absolute APPDATA fallback, and authoritative settings/footage/binding preservation before M7 launch-at-login validation found that the deliberately stale Windows Run command had not been repaired. `reconcile_autostart` compared only the plugin Boolean enabled state, so an existing stale Run value and persisted `launch_at_login=true` both appeared enabled and no rewrite occurred. RC33 refreshes an enabled registration on every startup when the persisted preference is true, relying on the pinned `auto-launch` Windows implementation to overwrite the Run value with the current executable path and `--startup-hidden`; persisted false still disables only when the OS reports enabled. Manual acceptance of hosted-green RC33 then exposed a Windows desktop/worker launch discrepancy: the installed `nian-media-worker.exe` and bundled FFmpeg runtime independently passed startup, redirected stdio and HELLO validation, but desktop camera probing could still surface `worker_unavailable` and worker launches flashed a console window. RC34 moves production worker containment to the pre-spawn parent Job Object inheritance boundary, applies `CREATE_NO_WINDOW` to background worker commands, removes the redundant post-spawn assignment from production paths, distinguishes probe start versus handshake failures, and adds repeated worker-probe regression coverage. RC34 also reports unconfigured playback storage explicitly and classifies updater metadata/network, platform, transport and signature failures instead of labeling every updater error as a cryptographic verification failure. Manual Windows acceptance of RC34 then proved camera probe, saved credentials and worker startup were healthy while Live View repeatedly transitioned from connecting through backoff to a media failure during H.264 stream copy. RC35 keeps recording Matroska unchanged, uses a live-only fragmented-MP4 timestamp normalizer that rebases RTP-derived epochs and repairs duplicate or backwards DTS while preserving PTS-DTS offsets, and surfaces read/create/write/finalize/capacity failure categories instead of collapsing every live media failure to `media_failed`. Hosted RC35 then failed during Windows workspace tests in `close_all_signals_every_worker_before_any_join_wait`: the deterministic snapshots already proved all four workers were signalled before any join wait, but an additional `<250ms` wall-clock assertion was sensitive to hosted-runner scheduling. RC36 removes only that redundant timing oracle and retains the ordering assertions. v1 is not considered released until Forgejo CI, final review, tag-build validation and the documented Windows/Linux release-candidate checklist pass. M10 adds local ONVIF discovery and provisioning without
changing the accepted RTSP recording architecture. M11 adds
a separate user-selected Live View surface for up to four H.264 cameras. Each live
camera owns an independent worker/session and opaque loopback capability; live capacity,
lifecycle and failures remain separate from M9 recording Desired/Runtime ownership. Live
media is a bounded rolling fragmented-MP4 window (2-second target, six retained fragments,
16 MiB hard fragment limit) rather than an ever-growing session file. Explicit keepalive
plus a background reaper cleans abandoned sessions/cache, and controller-owned in-flight
openings make hide/suspend/quit/update cancellation deterministic. M12 adds an optional
ONVIF PTZ control plane for explicitly paired cameras: continuous pan/tilt plus
capability-gated zoom use per-camera bounded workers, movement generations and backend +
camera-side dead-man timeouts. PTZ binding/failure stays independent of RTSP recording/live
ownership. The desktop persists
camera definitions, recorder/storage
settings, launch-at-login preference and independent per-camera desired recording intent
in authoritative platform app-data `settings.sqlite3`, while camera passwords remain in
the operating system credential store. The M4 recording catalog at
`<storage_root>/.nian/recordings.sqlite3` remains disposable and rebuildable from
footage. Users can add cameras manually by RTSP or discover ONVIF devices on the local
network, authenticate transiently, inspect H.264 media profiles, resolve and verify the
selected RTSP stream through the existing media worker, then provision it into the same
camera model and credential-reference transaction used by manual setup. M6 provides
recording-day/range queries, filesystem-revalidated normal/recovered playback, lazy
duration enrichment, seekable packet-copy H.264/AAC fragmented-MP4 playback over
tokenized loopback HTTP, and retention playback pins. M7 adds single-instance
activation, close-to-tray, coordinated Quit, launch-at-login, persisted recording
restoration, Windows suspend/resume handling and Job Object worker containment. M8
provides validated Linux x86_64 AppImage and Windows x86_64 NSIS release paths with the
same pinned LGPL FFmpeg 8.0.3 source authority and signed Tauri updater trust root. M9
owns independent per-camera recording slots with a conservative eight-recording safety
cap and separate Desired/Runtime state. M10 keeps ONVIF observational until explicit
provisioning and never makes ONVIF availability a prerequisite for an already configured
RTSP camera. M12 keeps PTZ optional and explicitly paired. M13 adds independent ONVIF PullPoint motion-event monitoring with persisted Desired state, bounded per-camera workers, normalized MotionStarted/MotionEnded history, and a dedicated rebuildable `<storage_root>/.nian/events.sqlite3` index. M14 adds a dedicated Event Review screen backed only by that persisted index, bounded keyset pagination, recording correlation by CameraId plus local receive time, five-second pre-roll playback, explicit missing-footage states, and optional post-persistence local desktop MotionStarted notifications with bounded dispatch and per-camera rate limiting. macOS distribution, ONVIF presets/talkback/vendor event dialects, H.265 recording/live view, transcoding, motion analysis, AI, cloud and remote streaming remain outside v1.

## What it does today

* Tauri 2 desktop application (React/TypeScript/Vite UI) with managed M13 state:
  camera CRUD, manual RTSP plus ONVIF onboarding, pre-save connection testing,
  per-camera Start/Stop controls, typed recording status, delete confirmation, persisted
  storage/retention settings, recording-day navigation, a gap-aware timeline, native
  video playback controls and adjacent recording navigation, tray controls and
  launch-at-login preference. Recording cards show persisted Desired state independently
  from transient Runtime state and a small aggregate summary without manufacturing a fake
  global RecordingState. A dedicated Live View screen lets the user select up to four
  configured cameras; tiles expose independent live/reconnect/failure state beside the
  existing recording Desired/Runtime controls. A MediaSource client consumes a small
  loopback manifest plus bounded session-scoped MP4 fragments; stale fragments are reclaimed
  only after active HTTP readers release them. Unselected cameras consume no live-view
  capacity. Closing the main window hides promptly and releases transient live ownership on
  backend blocking work while recording intent remains authoritative; explicit Quit owns
  deterministic teardown. The
  webview never receives an absolute recording path or authenticated RTSP live URL;
  privileged work flows through narrow Tauri commands.
* ONVIF onboarding is local and explicit. Discovery returns opaque Rust-owned
  session/device handles plus safe display metadata; discovery XAddrs, raw SOAP/XML,
  passwords and authenticated RTSP URLs never enter persisted settings or frontend
  output. Device Management discovers services, Media2 is preferred with legacy
  Media fallback, and H.264 profiles remain the M10 compatibility boundary. Final
  `Test & Add` must pass the existing media-worker RTSP probe before the ordinary
  camera/credential transaction is committed.
* Optional M12 PTZ pairing reuses explicit ONVIF discovery/authentication but persists a
  separate non-secret control binding. The configured RTSP camera remains authoritative. A
  paired camera cannot silently change RTSP host, and shared camera credentials cannot be
  replaced, until PTZ is explicitly unpaired; runtime also revalidates current camera/binding
  host equality before authenticated control traffic. PTZ owns one per-camera registry across
  opening, active, draining and mutation state. Same-camera pair/replace/unpair/update/delete
  mutation contention fails fast Busy, blocks fresh PTZ opening admission, and never grows an
  arbitrary waiter queue; other cameras remain independent. Worker capacity still counts only
  opening + active + draining ownership together (maximum 16). Opening commit revalidates the
  current binding identity/epoch so a worker prepared against a replaced binding cannot publish.
  Continuous pan/tilt and advertised zoom remain serialized per camera with a bounded queue;
  stale frontend releases cannot stop a newer generation, lease expiry automatically sends
  `Stop`, and every `ContinuousMove` also carries a camera-side one-second timeout. Hide,
  suspend, quit and update cancel motion while retaining controller ownership of in-flight
  openings, drains and mutations; terminal teardown waits credential rollback/cleanup before
  completion and resume never restores a previous direction. Camera deletion keeps mutation
  ownership through DB outcome and post-commit credential cleanup. Camera update runs through
  Tauri `spawn_blocking` so PTZ coordination never blocks the main thread.
* Optional M13 motion-event monitoring reuses explicit ONVIF selection but persists a separate non-secret Event binding and Desired flag. Pairing alone leaves monitoring Off; Enable/Disable controls Desired state independently of runtime Polling/Backoff/Failed status. `EventController` owns bounded per-camera PullPoint workers (`opening + active + draining <= 16`) plus fail-fast same-camera mutation exclusion. Controller-owned drain state remains registered through join completion, terminal runtime failures remain owned until intentional cancellation, and motion-source state is capped at 64 sources with overflow reported as unknown aggregate motion. Workers request a synchronization baseline, persist only normalized motion transitions, renew subscriptions using bounded lifetime metadata, recreate with camera-local backoff that resets only after a healthy Pull, and unsubscribe during terminal teardown. Namespace-qualified ONVIF Topics are resolved rather than prefix-stripped, and reconnect normalization retains bounded state so timestamp-less replay cannot manufacture duplicate transitions. Raw source tokens are SHA-256 hashed before leaving `nian-onvif`. Event history lives at `<storage_root>/.nian/events.sqlite3` with finite retention, a 250k-row hard cap, bounded cleanup and corruption quarantine. Event status is exposed through an aggregate off-main command with frontend single-flight polling. Close-to-tray keeps background Events running; Suspend/Resume/Quit/Update settle or restore them independently of recording/live/PTZ. Storage-root changes switch the Event index at runtime without requiring app restart.
* M14 Event Review reads only normalized persisted `EventIndex` rows. The default UI range is the last 24 hours; backend pages are keyset-ordered by receive timestamp plus event ID, bounded to 31 days and 200 rows per request, and executed off the Tauri main thread. Camera/time filters never load the full retained index into React. Selecting an Event resolves existing finalized footage by `CameraId + received_time_utc`; playback seeks five seconds before the Event when possible and never changes recording Desired state. Missing/retained-away footage is an ordinary unavailable state, not an ingestion error. Optional desktop motion notifications default Off and are persisted independently in settings schema v6. Only a newly committed `MotionStarted` can enter the 32-item non-blocking notification queue; per-camera notifications are limited to one per 15 seconds, notifier failures cannot break Event monitoring, and historical rows are never replayed at startup. Hide leaves notifications running; Suspend/Resume/Quit/Update own bounded dispatcher teardown/restoration. Native notification content is limited to `Motion detected` plus the camera display name. The current Tauri desktop notification abstraction does not expose a click callback, so M14 deliberately does not fake notification deep-link activation.
* Playback sessions bind only an ephemeral `127.0.0.1` port. An unguessable
  per-session token maps to one already-validated finalized recording; HTTP Range
  requests provide browser seeking without a directory listing, arbitrary path
  parameter or LAN listener.
* Authoritative application settings (`nian-settings`) live in the platform
  app-data directory, separate from recording storage. Camera credentials live
  in the native OS credential store behind `CredentialStore`; neither SQLite
  database contains passwords.
* An isolated `nian-media-worker` process that talks FFmpeg through FFI:
  * `nian-media-worker probe <file|credential-free-rtsp-url>` prints stream
    information (codec, resolution, duration);
  * `nian-media-worker run` serves a versioned NDJSON IPC protocol on
    stdin/stdout with the `recording.*` namespace (one supervised job per
    worker; start/stop/status plus typed reconnect events), bounded `camera.probe`
    source-only connection tests and M11 `live.start`/`live.status`/`live.stop` for
    one H.264 packet-copy live job per live worker. Live output is independently finalized
    rolling MP4 fragments under the application-owned transient live cache, with retention
    enforced by the application. M6 `playback.prepare` inspects a
    host-validated finalized MKV and packet-copy remuxes supported H.264 plus AAC
    into fragmented MP4 under the application playback cache;
  * `nian-media-worker record --storage <DIR> --camera <ID> ...` is the
    manual smoke path with explicit stop modes (`--duration N`,
    `--until-stdin-eof`, or Ctrl+C-only) recording from
    `NIAN_VISION_RTSP_URL` or a file into rotating `.mkv` segments
    (development only; never in CI).
* A real recording pipeline (`nian-recorder`): packets are copied
  faithfully (side data and flags preserved), segments start on video
  keyframes, rotation waits for keyframes after the media-time target,
  finalization is durable before the no-replace publish, and failed or
  empty segments stay recoverable partials — never fake recordings.
* Resilience: deadlines bound every blocking FFmpeg operation (RAII-scoped
  so they can never leak into unrelated operations), failures are
  classified centrally into typed categories (retryable = source-side
  only), partial recovery proves readability through the real demuxer, and
  `nian-application`'s `WorkerSupervisor` restarts crashed workers without
  ever putting credentials in argv.
* Storage/index management (M4): `nian-storage` owns canonical filesystem
  inventory and recovery-transaction facts, `nian-index` owns bundled
  SQLite persistence/migrations, and `nian-application::StorageManager`
  orchestrates reconciliation, rebuild/query seams and age + high/low
  watermark retention. Retention reports quota trigger/usage/target status;
  sub-second finalization events are normalized to the same whole-second local
  identity encoded by filenames. SQLite failures never roll back a published
  media file.
* Playback/timeline (M6): timeline queries stay inside `nian-index`, while every
  open and every new media HTTP request revalidates the filesystem-derived
  recording identity. Recovered finals are first-class entries; unknown duration
  stays unknown until worker inspection. Playback holds a read handle plus a
  `PlaybackPin`; retention skips pinned finals and rechecks the pin immediately
  before deletion, independently of `CameraLease`.
* Clean crate boundaries with ONVIF protocol/network handling isolated in safe-Rust
  `nian-onvif`; `unsafe` remains confined to the FFmpeg layers plus audited
  Windows-only platform boundaries: storage no-replace publication and the M7
  `nian-platform-windows` power-notification/Job-Object wrapper. Application and
  desktop orchestration remain safe Rust. The workspace keeps a race-safe storage
  layout, credential redaction, and a full quality-gate setup (fmt/clippy/tests,
  ESLint/tsc/vitest/vite build).

## Architecture

```text
React UI ─ typed Tauri commands ─ nian-desktop host
                 │                  │
                 │                  ├─ platform app-data/settings.sqlite3
                 │                  ├─ native OS credential store
                 │                  ├─ nian-onvif → local WS-Discovery / ONVIF SOAP
                 │                  ├─ tray/autostart/power + worker containment
                 │                  ├─ loopback HTTP playback + live (127.0.0.1 only)
                 │                  │ NDJSON control IPC (stdio)
                 │              nian-media-worker
                                │ FFmpeg FFI (dynamic, LGPL)
                            RTSP cameras
```

See `docs/architecture.md` and `docs/adr/` for the decisions behind this
layout (process isolation, FFmpeg strategy, container choice, storage
model, authoritative-settings/native-secret split, M6 playback transport, M7
desktop lifecycle/worker containment, M8 distribution/updater design, M9
per-camera recording coordination, M10 ONVIF discovery/provisioning and M11
independent multi-camera live view, and M12 optional ONVIF PTZ control).

## Requirements

* Rust 1.98.0 (pinned in `rust-toolchain.toml`; mise users get it from
  `.mise.toml`)
* Node 26.7.0 + pnpm 11.22.0 (exact pins, mirrored by CI)
* FFmpeg 8.x runtime libraries (ABI 62). Development packages are optional —
  see `docs/ffmpeg.md` for the no-`-dev` setup (`scripts/setup-ffmpeg-linux.sh`).
* Linux desktop shell builds additionally need the Tauri Linux prerequisites
  (WebKit2GTK/GTK dev packages).

## Development

```bash
mise install
pnpm install
scripts/setup-ffmpeg-linux.sh   # if FFmpeg -dev packages are absent
cargo test --workspace
pnpm --filter nian-ui test
```

Full commands and conventions: `docs/development.md`.

## Desktop releases

Linux x86_64 ships as an AppImage built against the accepted Debian 12 baseline.
Windows x86_64 is packaged as an NSIS current-user installer on `windows-2022`
using `x86_64-pc-windows-msvc`. Both build FFmpeg 8.0.3 from the same SHA-256-pinned
upstream archive and bundle the media worker plus application-owned FFmpeg runtime.
Windows uses Tauri's normal WebView2 `downloadBootstrapper` policy; users do not
need a separate FFmpeg installation and the installer does not modify global PATH.

Platform build jobs produce unsigned candidates without protected signing secrets.
Separate `sign-linux` and `sign-windows` jobs in the protected
`production-release` environment apply the shared Tauri updater trust root. The
Windows signing boundary can additionally apply and mechanically verify
Authenticode for the desktop executable, media worker and final NSIS installer. If
credentials are absent the candidate is explicitly classified unsigned, and a
repository policy switch can require Authenticode and fail closed.

Forgejo remains the authoritative source repository and normal push/PR/quality CI
platform. GitHub is a one-way mirror used only for hosted release CI and public
GitHub Releases. Release tags originate on Forgejo and the mirror must synchronize
tags as well as branches. One verification stage assembles Linux and Windows into a
single `latest.json`, multi-platform release manifest and global `SHA256SUMS.txt`.
Release-candidate commits use an actual prerelease source version such as `1.0.0-rc.1`;
the immutable tag must be exactly `v1.0.0-rc.1`. RC tags are published as prereleases
and are explicitly excluded from the production `latest` updater channel. After RC
acceptance, a separate minimal version-only commit changes every version surface to
`1.0.0`; only its exact `v1.0.0` tag may become production `latest`.
The existing draft-first GitHub Release flow then uploads, re-downloads and
byte-verifies every public asset before publication. See `docs/releasing.md`, `docs/release-checklist.md` and ADR-0018. macOS packaging remains outside v1.

## Installing a release

Use only artifacts from the same verified GitHub Release/tag and verify `SHA256SUMS.txt` before treating the download as a release candidate or final build.

- **Windows x86_64:** run `Nian-Vision_<version>_windows-x86_64-setup.exe`. The current NSIS strategy is per-user and bundles the media worker plus required application-local runtime files.
- **Linux x86_64:** make `Nian-Vision_<version>_linux-x86_64.AppImage` executable and run it. The AppImage contains the sibling media worker and application-owned FFmpeg runtime and must not depend on a repository checkout or development FFmpeg path.

A fresh first run has no configured cameras. Add one through **Add RTSP manually** or **Discover ONVIF cameras**. Choose the recording storage root in Settings before relying on persistent recording/Event history. Launch-at-login and local motion notifications remain explicit user preferences, not first-run defaults.

## Supported camera protocols

* RTSP (H.264) — implemented at the media layer for recording and M11 live view;
  manual smoke test via `NIAN_VISION_RTSP_URL` (credentials never go on the command
  line). Live view packet-copies video into fragmented MP4 and does not transcode.
* ONVIF — implemented for local discovery/provisioning, optional M12 PTZ, and optional M13 PullPoint motion events. Device Management plus Media2/legacy Media resolves selected H.264 streams; recording/live still use RTSP through the existing media path. PTZ adds explicitly paired continuous pan/tilt and capability-gated zoom. Events add explicitly paired standard CellMotion monitoring with separate Enable/Disable Desired intent and normalized local history. Presets, talkback, vendor event dialects, H.265 recording/live view and transcoding remain out of scope.

## ONVIF camera setup

Choose **Add camera → Discover ONVIF cameras** to scan the local network, select a
device, enter that camera's ONVIF credentials, choose a supported H.264 profile and
use **Test & Add**. The password is submitted only to the Rust host; after successful
authentication the wizard operates with opaque session/device/profile handles. The
resolved stream is probed through the same media worker used by manual RTSP setup
before credentials/settings are persisted. **Add RTSP manually** remains available
and does not depend on ONVIF.

For an existing camera, use **Pair PTZ** on its camera card, explicitly choose the same
physical ONVIF device, authenticate and confirm the pairing. Pairing requires the selected
ONVIF Device-service host to match the configured RTSP host and requires advertised
continuous pan/tilt capability. While PTZ remains paired, changing the camera's RTSP host is
rejected; replacing camera credentials is also rejected when PTZ reuses that same credential
reference. **Unpair PTZ** first when intentionally replacing the physical camera or shared
credentials. Unpair removes only the optional control binding; the RTSP camera, recordings
and desired recording state are unchanged.

For motion monitoring, use **Pair Motion Events** on an existing camera, explicitly select the same physical ONVIF device and authenticate. Pairing stores only the optional Event association and leaves monitoring Off. Use **Enable Events** to start background PullPoint monitoring; **Disable Events** preserves the pairing but turns Desired monitoring Off; **Unpair Events** disables monitoring and removes only the Event binding/owned Event credential. Motion status is shown independently from recording/live state and Live View polls it at low frequency.

Troubleshooting:

* **No devices found**: confirm the desktop and camera are on the same local network
  and that host/firewall/network policy permits WS-Discovery multicast. Discovery is
  bounded and does not scan the internet; manual RTSP configuration remains available.
* **Authentication failed**: use credentials authorized for the camera's ONVIF
  service. Passwords are not shown again after submission.
* **No compatible profile**: M10 provisions H.264 only. Enable/select an H.264 camera
  profile rather than H.265/other codecs; M10 does not transcode.
* **Resolved stream rejected or unreachable**: Nian Vision validates the returned
  RTSP authority and then opens the stream through the existing media worker. Cameras
  returning unusable ONVIF addresses can still be configured with the known-good RTSP
  host/port/path using the manual path.
* **Recording capacity reached**: v1 owns at most 8 simultaneous Recording sessions. Desired state can remain On while runtime reports the capacity failure; stop another recording before retrying runtime admission.
* **Event capacity reached**: v1 owns at most 16 Event-monitoring sessions. Event Desired state remains independent from Recording.
* **Storage failure/read-only/full**: stop relying on new recording output until the configured local storage root is writable and has capacity. Do not delete `.nian` control files to “repair” the system; derived indexes have controlled recovery and authoritative settings live elsewhere.
* **No recording available for an Event**: Event history may outlive the correlated segment or the segment may not yet have trusted duration. This is an ordinary unavailable-footage state, not evidence that the Event row is corrupt.
* **Notifications unsupported/unavailable**: leave the preference Off or restore the desktop notification service. Notification delivery failure is isolated from Event monitoring.
* **Media worker unavailable**: reinstall/repair the matching release package so the sibling `nian-media-worker[.exe]` and bundled runtime are present. Do not point production at a repository `target/debug` worker.

When collecting diagnostics, include safe camera IDs/display names and typed failure categories, never passwords, authenticated RTSP URLs, authorization headers or raw credential-store contents.

Automated Forgejo tests use deterministic fixtures/local servers and do not require a
physical ONVIF camera or LAN multicast access. Physical-camera interoperability is
therefore a manual validation boundary, not a CI guarantee.

**Milestone status:** M0–M14 accepted; M15 hardening/release preparation is in progress. Physical-camera interoperability and clean installed-package checks remain manual release-candidate evidence and are never inferred from automated protocol tests.


## v1 privacy and support boundary

Nian Vision is local-first. Camera RTSP/PTZ/Event passwords stay in the native operating-system CredentialStore; `settings.sqlite3` contains only opaque credential references and non-secret configuration. Recordings stay under the configured local storage root and normalized Event history stays in the local Event index. v1 has no cloud video upload, remote-access service or cloud notification service.

The v1 release targets are **Windows x86_64 (NSIS)** and **Linux x86_64 (AppImage)**. H.264 RTSP is required; there is no H.265 or transcoding. Accepted runtime limits are 8 simultaneous Recording sessions, 4 Live View sessions and 16 Event-monitoring sessions. The complete intentional boundary, including notification and ONVIF limitations, is in `docs/known-limitations.md`.

The Tapo C200 is the reference camera target, not a magic certification stamp. Automated fixtures verify protocol and ownership contracts; the final release checklist records physical-device results separately for RTSP recording, Live View, ONVIF provisioning, PTZ and Events.

## License notes

* Nian Vision's own code is proprietary (private repository).
* FFmpeg is dynamically linked. Linux and Windows release jobs build the exact
  FFmpeg 8.0.3 source pin in LGPL shared mode and mechanically reject
  GPL/nonfree/static drift before packaging. Release artifacts include
  the exact configure flags, FFmpeg LGPL text and `THIRD_PARTY_NOTICES.txt`; see
  `docs/ffmpeg.md` and `thirdparty/README.md` for provenance.
