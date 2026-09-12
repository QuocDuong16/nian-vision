# Nian Vision v1 release-candidate checklist

Record the exact tag, commit SHA, OS image/VM and result for every run. A checkbox is evidence only when the step was actually executed. Do not reuse a tag after changing source.

Current retry candidate: `1.0.0-rc.46` / `v1.0.0-rc.46`. RC1 through RC45 remain immutable. RC46 hardens Tapo C200 motion persistence and PTZ compatibility, keeps Live View resident across ordinary tab navigation, allows WebView reload reattachment to an already-open live session, and blocks browser-style reload controls in the desktop shell.

## Pre-tag authority

- [ ] RC commit is on authoritative Forgejo default branch and every version surface is the same prerelease SemVer, currently `1.0.0-rc.46`.
- [ ] Forgejo normal CI is green: fmt, check, full workspace/all-feature Clippy, workspace tests, cargo-deny, frontend lint/typecheck/Vitest/build.
- [ ] Code review accepts M15 and confirms no v2 feature scope.
- [ ] `node scripts/release/version-check.mjs --tag <candidate-tag> --require-clean` passes on the exact release commit.
- [ ] Mirror configuration is known to propagate tags and `RELEASE_MIRROR_ACTOR` is configured.
- [ ] Production updater public key/signing secrets are provisioned in the protected GitHub environment.
- [ ] If `REQUIRE_WINDOWS_AUTHENTICODE=true`, valid Authenticode credentials/timestamp URL are provisioned.

## GitHub RC workflow

- [ ] Create a new immutable prerelease tag exactly matching the RC source version, currently `v1.0.0-rc.46`. Never move, delete or reuse any consumed tag `v1.0.0-rc.1` through `v1.0.0-rc.45`; source/tag cross-pairing is forbidden.
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
- [ ] Live View opens and closes; selected camera layout and Fit/Native preference survive tab remount while the old sessions close and fresh sessions reopen on return. Verify no hidden live session remains active after leaving the tab. During an in-session RTSP hiccup, `backoff -> connecting -> live` must keep the same video/MSE presentation mounted (a brief frozen frame is acceptable; a blank Connecting tile/remount is not), and a stable >=10-second live period must reset the consecutive reconnect budget. Fit tile remains default for a fresh preference, Native pixels visibly avoids upscale when the tile has spare room, and Diagnostics reports source/display/DPR scaling without changing the live session.
- [ ] Playback opens/seeks/closes.
- [ ] Compatible camera ONVIF discovery/provisioning succeeds.
- [ ] Compatible camera PTZ works; movement stops on lifecycle teardown and never restores by itself. On Tapo C200 V5 verify multiple/partial velocity-space advertisements do not poison a later complete pan/tilt candidate; any remaining failure must retain the typed services/profiles/options stage instead of generic `onvif_protocol`.
- [ ] Compatible camera Event monitoring persists normalized Events and Event Review can open correlated footage where available. On Tapo C200 V5/1.4.6 verify the fingerprint-gated adapter can probe same-host TCP 2020 PullPoint, remains Polling rather than Backoff on valid vendor extensions/duplicate state echoes, and record observed CellMotion/People/Smart Event/Line Cross transitions without exposing raw source tokens. If it fails, record the stage-specific `event_control_*`, `event_pull_*`, or `event_renew_*` code.
- [ ] Local notification appears when enabled; missing native notification support/failure remains non-fatal. Run the delivery-timeout isolation regression repeatedly on Windows and verify observing `delivery_timeouts` implies the timed-out native delivery has already been terminated.
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
