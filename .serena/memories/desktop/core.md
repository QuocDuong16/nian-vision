# Desktop host boundary

- `apps/nian-desktop` is the Tauri 2 native host. It owns native app-data paths, OS credential storage, single-instance/tray/autostart/lifecycle integration, and typed commands to the UI.
- Keep UI DTOs safe: no passwords, resolved RTSP URLs, native paths, credential-store authority, or FFmpeg types cross into React.
- The host starts/coordinates `nian-media-worker` through application services and bounded NDJSON IPC; it does not link FFmpeg.
- Playback/live loopback transport ownership and lifecycle cleanup stay native. Suspend/hide/update/quit paths must settle workers/sessions before restart or shutdown.
- See `docs/architecture.md` and ADRs 0003, 0008, 0010, 0011 for process isolation, credential ownership, lifecycle, and distribution decisions.