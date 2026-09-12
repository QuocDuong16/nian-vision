# Nian Vision v1 known limitations

These are intentional v1 boundaries, not hidden roadmap promises.

- **Platforms:** Windows x86_64 and Linux x86_64 only. No macOS or mobile release.
- **Media codec:** RTSP H.264 is required for recording and Live View. There is no H.265 support and no transcoding.
- **Recording capacity:** at most 8 simultaneous owned Recording sessions. Persisted Desired state beyond capacity remains Desired On but runtime admission fails deterministically.
- **Live capacity:** at most 4 simultaneous user-opened Live View sessions. After first visit the Live View workspace remains resident across ordinary tab navigation, and a reloaded WebView can reattach an existing backend session; lifecycle Suspend/Quit/Update still settle transient live ownership and Resume does not resurrect stale session ids.
- **Event capacity:** at most 16 simultaneous ONVIF Event-monitoring sessions. Event Desired state is independent of Recording and remains persisted when runtime capacity is unavailable.
- **PTZ:** optional continuous pan/tilt and capability-gated zoom only. No presets, patrol/tours or talkback. PTZ movement does not restore after restart or Resume.
- **ONVIF compatibility:** discovery/provisioning/PTZ/Events depend on what a camera advertises and on standards-compatible behavior. Manual RTSP configuration remains valid when ONVIF is unavailable.
- **Events:** v1 stores normalized MotionStarted/MotionEnded history and supports post-persistence motion-triggered recording episodes with a five-second post-roll and a five-minute maximum segment duration. There is no true pre-event recording buffer, AI/person/object analysis performed by Nian, analytics configuration, synthesized/exported event clips or thumbnails. Camera-provided Tapo People/Smart/LineCross signals remain compatibility inputs, not Nian AI.
- **Notifications:** local desktop MotionStarted notifications only. No email, webhook, cloud push, schedules or quiet hours. The current desktop notification API does not provide the Event-id click callback required for reliable deep-link activation, so v1 does not pretend that it does.
- **Remote access:** no cloud streaming, WebRTC, remote server mode, multi-user/RBAC service or mobile access.
- **Signing:** Tauri updater signatures are mandatory release artifacts. Windows Authenticode depends on externally provisioned signing credentials and repository policy; source control never contains a private signing key.

## Tapo C200 interoperability status

TP-Link Tapo C200 is the reference camera target, but automated protocol/media tests and physical-device compatibility are different evidence. A final v1 release report must record real-device results separately for RTSP recording, Live View, ONVIF discovery/provisioning, PTZ and Events. Any item not manually exercised must be reported as unverified rather than inferred from automated fixtures.
