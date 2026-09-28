# Repository conventions and boundaries

- Rust 2024. Keep production code explicit and fallible; CI denies Clippy `unwrap_used`, `expect_used`, and `panic` warnings. Tests may enable them locally.
- Default to safe Rust. Keep raw FFmpeg declarations in `nian-ffmpeg-sys`, expose a safe wrapper through `nian-media-ffmpeg`, and keep Win32 unsafe details inside audited platform/publication boundaries.
- Preserve process boundaries: application/UI orchestration must not import FFmpeg types; desktop host must not link FFmpeg; worker communicates through bounded, versioned NDJSON IPC.
- Persist only non-secret camera configuration in `settings.sqlite3`; OS credential storage holds secrets. Never expose credentials or RTSP URLs in UI DTOs, logs, argv, or fixtures; preserve redacted formatting.
- Distinguish authoritative from derived state: settings DB fails closed on corruption/future schema; recording/event catalogs may be quarantined and rebuilt from filesystem/protocol facts; SQLite rows never authorize inventing or deleting footage.
- Recording safety is filesystem-first: canonical/path-safe identities, exclusive claims, no-replace publication, and conservative recovery. Missing/uninspectable evidence must not be treated as proof of absence.
- Pin dependency versions exactly and keep lockfiles, toolchain pins, CI/release workflow pins, and docs aligned.