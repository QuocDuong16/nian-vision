# Nian Vision 1.0.0-rc.5
Nian Vision v1 is a local-first desktop NVR for configured IP cameras on Windows x86_64 and Linux x86_64.

RC1 through RC4 remain immutable historical release attempts. RC4 reached the Windows release job but stopped immediately on a PowerShell parser error in the native-command failure message. RC5 contains the parser-safe formatting fix while preserving the accepted RC4 release cache, validation, timeout, signing and platform-hardening contracts.

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
