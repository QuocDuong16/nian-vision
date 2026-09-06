# Nian Vision v1 release-candidate checklist

Record the exact tag, commit SHA, OS image/VM and result for every run. A checkbox is evidence only when the step was actually executed. Do not reuse a tag after changing source.

## Pre-tag authority

- [ ] Release commit is on authoritative Forgejo default branch.
- [ ] Forgejo normal CI is green: fmt, check, full workspace/all-feature Clippy, workspace tests, cargo-deny, frontend lint/typecheck/Vitest/build.
- [ ] Code review accepts M15 and confirms no v2 feature scope.
- [ ] `node scripts/release/version-check.mjs --tag <candidate-tag> --require-clean` passes on the exact release commit.
- [ ] Mirror configuration is known to propagate tags and `RELEASE_MIRROR_ACTOR` is configured.
- [ ] Production updater public key/signing secrets are provisioned in the protected GitHub environment.
- [ ] If `REQUIRE_WINDOWS_AUTHENTICODE=true`, valid Authenticode credentials/timestamp URL are provisioned.

## GitHub RC workflow

- [ ] Use an immutable prerelease tag such as `v1.0.0-rc.1` for workflow validation before the final tag.
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

Only after the RC evidence above is accepted should `v1.0.0` be created from the reviewed Forgejo commit. If any source/config change is needed, create a new commit and new RC/final tag. Never move an already-tested tag.
