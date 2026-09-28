# Media worker and recording invariants

- `apps/nian-media-worker` contains FFmpeg and media failure risk; the Tauri host never links the FFmpeg runtime. Application code uses the backend-agnostic `nian-media` facade, implemented by safe `nian-media-ffmpeg` over raw `nian-ffmpeg-sys`.
- `nian-recorder` records packet-faithfully. Segment rotation uses video DTS in the validated time base and starts new segments on keyframes; wall clock names files but does not drive rotation.
- Claim partial output exclusively, finalize durably, then publish with no-replace semantics. Failed/poisoned output is never published.
- Recovery is conservative: classify partials, prove readable packets, remux to a newly claimed output, publish before removing the original; unresolved or unprovable evidence remains preserved.
- Source-side timeouts/failures may reconnect under typed retry policy; cancellation/operator intent is distinct, and local storage/config/output failures do not retry as source failures.
- See `docs/architecture.md`, ADRs 0002-0007, and `docs/testing.md` for authoritative details and fixture/runtime evidence boundaries.