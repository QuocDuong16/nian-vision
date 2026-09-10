# Nian Vision v1 release-candidate checklist

Record the exact tag, commit SHA, OS image/VM and result for every run. A checkbox is evidence only when the step was actually executed. Do not reuse a tag after changing source.

Current retry candidate: `1.0.0-rc.27` / `v1.0.0-rc.27`. RC1 through RC26 remain immutable. RC10 preserved the pinned MSYS2 GNU make/diffutils and selected MSVC/Windows SDK authority, then exposed the generated-Bash `bash -lc` transport failure. RC11 preserved those protections with file-backed UTF-8/LF generated Bash plus mandatory `bash -n`, passed Windows FFmpeg download, source SHA-256 verification, extraction and configure, then exposed the post-configure PowerShell collection-cardinality bug. RC12 preserved RC10/RC11 hardening and passed Windows preflight, source SHA-256 verification, extraction, configure, collection diagnostics, CCDEP on-disk validation and GNU make CCDEP expansion; it then began real MSVC compilation and failed at `libavformat/cbs.o` with the authoritative root failure `fatal error C1001: Internal compiler error`. The later GNU make Error 127 was consequential, not causal. FFmpeg upstream commit `6a59c847b50c6bc30630df7fca56ccd6cd8a5a8c` identifies the empty CBS-in-lavf configuration as illegal C that may trigger MSVC ICE. RC13 successfully applied that exact backport, passed the targeted `libavformat/cbs.o` MSVC regression compile, full Windows FFmpeg compilation and install, then failed only because post-install validation incorrectly looked for global `CONFIG_NETWORK` in `config_components.h` instead of `config.h`; this did not show networking was disabled. RC14 correctly validated `CONFIG_NETWORK` from `config.h`, passed the targeted CBS compile and full Windows FFmpeg compile/install, then failed only because the configure contract requested nonexistent URL protocol `rtsp`, causing the post-install validator to expect invalid `CONFIG_RTSP_PROTOCOL`; RTSP remained enabled as a demuxer. RC15 removed that invalid protocol request, retained `--enable-demuxer=matroska,mov,rtsp`, and validated every directly requested resolved component immediately after configure before compilation; it then failed later in Windows Rust quality because Rust 1.98.0 MSVC was installed with `--profile minimal` without `rustfmt`, causing `cargo fmt --all --check` to fail because `cargo-fmt.exe` was absent, while `clippy` was also not explicitly provisioned. RC16 explicitly provisioned `rustfmt` and `clippy` and preflighted actual Cargo fmt/clippy executable availability before frontend dependencies, FFmpeg, or remaining Rust quality work. It then exposed two independent later blockers: Windows `tauri-build` required `apps/nian-desktop/icons/icon.ico`, which was absent, and Linux AppImage smoke found byte-identical FFmpeg duplicates under legacy `/usr/lib` in addition to the intended `/usr/lib/nian-vision` runtime. RC17 added the Windows ICO asset and removed only byte-identical legacy FFmpeg copies before smoke validation, then failed later on two deterministic contracts: Windows all-target Clippy rejected a Unix-only `NaiveDate` test import as unused under `-D warnings`, and Linux AppImage smoke found the packaged worker RUNPATH rewritten by Tauri/linuxdeploy to `$ORIGIN/../lib` rather than `$ORIGIN/../lib/nian-vision`. RC18 removed the platform-specific unused import and restored plus validated the exact private worker RUNPATH in the extracted AppDir before repacking, then failed later because a Unix-only absolute storage path fixture was invalid on Windows and the Linux signing job received its AppImage from the GitHub artifact boundary without executable mode. RC19 made the initial config unit-test fixture platform-native and restored plus verified AppImage executable mode immediately after download before signing, then hosted Windows workspace tests exposed a second Unix-only storage-root fixture in the camera-service integration suite; Linux reached actual desktop startup smoke and exposed that the minimal smoke host did not provide system `libEGL.so.1`, which AppImage intentionally leaves to the host graphics stack. RC20 removed the remaining Unix-only storage/output fixtures, added their Windows regression, and provisioned the host EGL runtime in both Linux smoke jobs, then hosted Windows workspace tests exposed that worker-supervisor Bash fixtures passed native temporary script paths as `bash -c` command strings and therefore never emitted a valid JSON hello frame. RC21 passed each fixture to Bash as a script-file argument with normalized separators, but hosted Windows still produced malformed hello frames and Linux progressed from EGL to a missing system `libGLESv2.so.2` loader. RC22 switches the fixtures to the preflighted MSYS2 Bash plus `cygpath -u` transport, makes the protocol-version fixture prove it reaches the real validator, and provisions/verifies both host EGL and GLES loaders without bundling them. Hosted RC22 then failed Windows all-target Clippy on a cfg-gated needless `return` in the path-conversion helper, while Linux workspace tests exposed a test-only cross-test deadlock because the global retention pre-delete gate could be consumed by the wrong parallel manager. RC23 removes the needless return via cfg-specific helper expressions and makes the retention gate one-shot state owned by the exact `StorageManager` test instance. Hosted RC23 then exposed Windows line-ending conversion of the signed ASCII verifier fixture and a missing Ayatana AppIndicator runtime on the Linux signed-smoke host. RC24 marks `artifact.bin` binary in `.gitattributes`, locks that checkout boundary in release tests, and explicitly provisions/verifies `libayatana-appindicator3-1` for Linux AppImage smoke. Hosted RC24 then exposed the Unix-only `/srv/nian-vision/recordings` storage-layout test fixture on Windows and, on Linux, a host/AppImage GLib generation mismatch after tray loading: Ubuntu 24.04 `libayatana-ido3` requires `g_once_init_leave_pointer` from GLib 2.80 while the Bookworm-built AppImage carries older GLib. RC25 made the storage spec fixture platform-native and bundled the Bookworm AppIndicator seed before linuxdeploy so the matching Ayatana/dbusmenu closure is inside the AppImage; AppIndicator is no longer a signed-smoke host prerequisite. Hosted RC25 Windows failed immediately in `pnpm release:test` because the release-config unit test compared `node:path.resolve()` output against a POSIX-rooted `/workspace/...` fixture, which becomes a drive-qualified native path on Windows. RC26 made those path fixtures platform-native and added a static Windows regression against reintroducing the POSIX-rooted source path. Hosted RC26 Linux passed. Hosted RC26 Windows progressed past release tests and then failed in `stage-windows.ps1` because `build-metadata.mjs` directly spawned `pnpm`; the hosted package manager is exposed as `pnpm.cmd`, so Node `execFileSync` returned `ENOENT`. RC27 makes each staging script obtain pnpm version through its platform-native invocation and passes it explicitly to build metadata, which verifies the value against the pinned `packageManager` contract without spawning a command shim.

## Pre-tag authority

- [ ] RC commit is on authoritative Forgejo default branch and every version surface is the same prerelease SemVer, currently `1.0.0-rc.27`.
- [ ] Forgejo normal CI is green: fmt, check, full workspace/all-feature Clippy, workspace tests, cargo-deny, frontend lint/typecheck/Vitest/build.
- [ ] Code review accepts M15 and confirms no v2 feature scope.
- [ ] `node scripts/release/version-check.mjs --tag <candidate-tag> --require-clean` passes on the exact release commit.
- [ ] Mirror configuration is known to propagate tags and `RELEASE_MIRROR_ACTOR` is configured.
- [ ] Production updater public key/signing secrets are provisioned in the protected GitHub environment.
- [ ] If `REQUIRE_WINDOWS_AUTHENTICODE=true`, valid Authenticode credentials/timestamp URL are provisioned.

## GitHub RC workflow

- [ ] Create a new immutable prerelease tag exactly matching the RC source version, currently `v1.0.0-rc.27`. Never move, delete or reuse any consumed tag `v1.0.0-rc.1` through `v1.0.0-rc.26`; source/tag cross-pairing is forbidden.
- [ ] GitHub tag resolves to exactly the same commit as Forgejo.
- [ ] Linux build/sign jobs pass.
- [ ] Windows build/sign jobs pass.
- [ ] Both platform candidates are required by final verification.
- [ ] Worker sidecar and FFmpeg runtime are present in both packages.
- [ ] Updater signatures verify with the configured public key.
- [ ] `SHA256SUMS.txt` verifies every public artifact it names.
- [ ] `latest.json` names real published candidate artifacts and has the expected version/signatures.
- [ ] RC GitHub Release is `prerelease=true` and is **not** the production `latest` release.

## Clean Windows x86_64 machine/VM

- [ ] NSIS current-user install succeeds from a path containing spaces.
- [ ] First startup succeeds with no cameras and no fake failure state.
- [ ] Add a camera manually; RTSP probe succeeds.
- [ ] Start/stop recording; finalized media is playable.
- [ ] Kill/restart desktop while Recording Desired is On; Desired restores once.
- [ ] Live View opens and closes; Live does not restore after restart.
- [ ] Playback opens/seeks/closes.
- [ ] Compatible camera ONVIF discovery/provisioning succeeds.
- [ ] Compatible camera PTZ works; movement stops on lifecycle teardown and never restores by itself.
- [ ] Compatible camera Event monitoring persists normalized Events and Event Review can open correlated footage where available.
- [ ] Local notification appears when enabled; missing native notification support/failure remains non-fatal.
- [ ] Close hides to tray while Recording/Events/notifications continue and Live/PTZ settle.
- [ ] Suspend/Resume: Recording/Event Desired restore without duplicate workers; Live/PTZ do not restore.
- [ ] Second manual launch activates existing instance; startup-hidden duplicate does not unexpectedly show it.
- [ ] Launch-at-login remains user-controlled and startup-hidden restores Desired Recording/Event state.
- [ ] Tray Quit leaves no owned media worker or notification helper.
- [ ] Install/update over a previous build preserves camera configuration, credential usability, Recording Desired, Event Desired/bindings, storage settings and notification preference.
- [ ] Uninstall removes application binaries/autostart registration but does not delete recordings or authoritative application data contrary to documented policy.

## Clean Linux x86_64 machine/VM

- [ ] AppImage starts without repository checkout, `NIAN_FFMPEG_LIB_DIR` or developer library paths.
- [ ] First startup succeeds with no cameras.
- [ ] Bundled media worker is executable and resolves bundled FFmpeg runtime.
- [ ] Manual camera add and RTSP probe succeed.
- [ ] Recording, restart Desired restore, Live View and playback succeed.
- [ ] Compatible-camera ONVIF provisioning/PTZ/Events are exercised where hardware is available.
- [ ] Event Review works and missing recording is a normal unavailable state.
- [ ] Desktop notification succeeds when a notification service exists; unavailable service remains non-fatal.
- [ ] Close-to-tray and Quit semantics are correct; controlled shutdown leaves no owned media worker/helper.
- [ ] AppImage updater handoff succeeds from a signed candidate and persisted Desired state restores on next startup.

## Final v1 integrity

- [ ] All hardware-dependent results are explicitly recorded, including Tapo C200 items that were not exercised.
- [ ] Required artifacts exist for Windows and Linux and versions match tag.
- [ ] `release-manifest.json` commit equals expected Forgejo release commit.
- [ ] Global checksums and updater signatures verify after downloading from GitHub Release.
- [ ] No release asset contains configured secret sentinel or development path dependency.
- [ ] `RELEASE_NOTES.md`, README, support matrix and known limitations match the artifact being released.
- [ ] No mandatory blocker remains.

Only after the RC evidence above is accepted should a separate minimal final release-version commit change every authoritative version surface from `1.0.0-rc.N` to `1.0.0`. That final commit must pass Forgejo CI and final review before the immutable `v1.0.0` tag is created. Do not reuse RC binaries as final artifacts. If an RC needs any source/config fix, make a new commit, advance the prerelease source version (for example `rc.1` to `rc.2`), and create a new matching immutable RC tag. Never move an already-tested tag.
