# Nian Vision 1.0.0-rc.11
Nian Vision v1 is a local-first desktop NVR for configured IP cameras on Windows x86_64 and Linux x86_64.

RC1 through RC9 remain immutable historical release attempts. RC7 proved the deterministic MSYS2 tar/xz extraction path and reached the Windows FFmpeg source build, where uncontrolled Windows/MinGW make behavior corrupted the MSVC dependency command. RC8 pinned the intended FFmpeg build tools and added generated `CCDEP` validation but failed on a representation-level MSVC path assertion. RC9 proved the replacement semantic Visual Studio/MSVC authority on the real `windows-2022` runner, then exposed missing hosted `/usr/bin/make`. RC10 is now an immutable consumed attempt: it successfully provisioned pinned official MSYS2 `make` 4.4.1-3 and `diffutils` 3.12-1, preserved the sanitized selected-MSVC plus `/usr/bin` authority, and reached the aggregate FFmpeg Windows toolchain probe. That probe failed before tool reporting completed because the large generated multi-line Bash body was transported as a `bash -lc` command-line argument and Bash reported an unexpected EOF. RC11 preserves every accepted RC10 tool, cache, extraction and authority contract while moving generated Bash programs to deterministic temporary `.sh` files written as UTF-8 without BOM with LF newlines; each generated script must pass `bash -n` before behavioral execution.

## Main capabilities

- Manual RTSP camera configuration and ONVIF discovery/provisioning.
- Up to 8 simultaneous H.264 stream-copy recording sessions with local retention and crash/partial recovery.
- Up to 4 independent Live View sessions and local playback/timeline review.
- Optional ONVIF PTZ continuous pan/tilt and capability-gated zoom.
- Up to 16 optional ONVIF PullPoint Event-monitoring sessions with normalized local Event history.
- Event Review with existing-recording correlation and five-second pre-roll when footage exists.
- Optional local desktop MotionStarted notifications with bounded dispatch/rate limiting.
- Single-instance desktop lifecycle, close-to-tray, launch-at-login, Windows suspend/resume handling and signed in-app updater artifacts.

## Privacy and security

Camera passwords remain in the native OS credential store; ordinary SQLite stores only opaque credential references. Recordings and Event history remain local. Nian Vision v1 has no cloud video upload, remote-access service or cloud notification service.

## Upgrade notes

Authoritative settings migrations preserve camera definitions, credential references, Recording/Event Desired state, PTZ/Event bindings, storage settings and notification preference. Corrupt authoritative settings are never silently replaced. Recording and Event indexes are derived and may be boundedly quarantined/rebuilt without deleting camera settings or footage.

## Known limitations

v1 requires H.264 and does not transcode or support H.265. ONVIF feature availability depends on the camera. There is no macOS/mobile release, remote/cloud access, AI/person/object detection, motion-triggered recording, generated clips/thumbnails, presets/tours/talkback, email/webhook/cloud push notifications or notification scheduling. See `docs/known-limitations.md` for the normative list.

The final `v1.0.0` tag must not be created until M15 acceptance and the clean Windows/Linux release-candidate checklist are complete.
