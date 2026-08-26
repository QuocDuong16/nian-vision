# Testing strategy

Testing is part of the definition of done for every milestone (master spec
§17) — not a final-phase activity.

## Current layers

### Unit tests (in-crate, always run)

| Crate | Covers |
|---|---|
| `nian-domain` | camera-id path safety, credential redaction, URL encoding, retention validation, quota watermarks, backoff schedule, time-base math |
| `nian-application` | config validation bounds, UI-safe error messages |
| `nian-storage` | recordings layout, partial/final naming round-trip, traversal rejection |
| `nian-ipc` | envelope round-trips, framing limits (1 MiB cap, CRLF, truncation), dispatch loop (ping/describe/shutdown/unknown), protocol version guard |
| `nian-media` | RTSP URL redaction invariants |

### Media integration tests (`nian-media-ffmpeg/tests/`)

Run real FFmpeg through the safe wrapper against a deterministic fixture
(`tests/fixtures/sample.mkv`: 2 s, 160x120, 10 fps MPEG-4/Matroska from
`testsrc2`, LGPL-native encoder — regenerate with
`scripts/generate-fixture.sh`):

* runtime ABI matches compiled bindings;
* probe reports format/streams/duration;
* packet reads: keyframe-first, monotonic DTS, non-empty payloads;
* stream-copy remux to Matroska produces an independently probeable file
  with sane duration;
* pre-cancelled interrupt aborts open; expired deadline aborts open.

These are the same code paths the M2 recorder will use.

### CLI/IPC smoke checks (manual, seconds)

```bash
./target/debug/nian-media-worker probe <file>
printf '{"type":"request","v":1,"id":1,"method":"ping","params":null}\n' \
  | ./target/debug/nian-media-worker run
```

### Frontend (`ui/`)

Vitest + Testing Library: navigation shell renders honest empty states and
switches screens; formatting utilities. Lint (`eslint`) and `tsc
--noEmit` gate everything.

## Planned per milestone

* **M2**: deterministic recording tests (continuous source → segments,
  rotation at keyframes, finalization, probe validation of outputs).
* **M3**: fault injection — worker kill/restart, disconnect storms,
  partial-file recovery.
* **M4**: retention engine property tests, reconciliation against a seeded
  tree, disk-full behavior.
* **Hardware/manual** (never in CI): real Tapo C200 via
  `NIAN_VISION_RTSP_URL`; checklist in the master spec §17 (unplug,
  reboot, sleep/wake, disk near-full, corrupt files).

## Environment variables

| Variable | Purpose |
|---|---|
| `NIAN_VISION_RTSP_URL` | credential-bearing RTSP URL for manual smoke tests; never logged, never committed |
| `NIAN_FFMPEG_LIB_DIR` | build-time FFmpeg library directory override |
| `RUST_LOG` | tracing filter (worker/desktop default `info`) |
