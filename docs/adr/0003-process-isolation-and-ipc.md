# ADR-0003: Process isolation and IPC

- Status: accepted
- Date: 2026-08-26

## Context

FFmpeg is a C library. A malformed stream, a camera that half-closes a TCP
connection, or a bug in a demuxer can crash the process or block it forever.
The desktop UI must survive all of that. Additionally, one misbehaving camera
must not affect others once multi-camera support lands (M9).

## Decision

All FFmpeg usage lives in the `nian-media-worker` process. The desktop host
supervises workers and communicates over newline-delimited JSON on
stdin/stdout (`nian-ipc`).

* **Transport**: NDJSON over stdio. Trivially correct framing, works with
  plain process pipes, trivially testable with byte buffers.
* **Protocol**: versioned envelopes (`v: 1`); request/response correlation by
  id; worker-initiated events. Unknown versions are rejected loudly.
* **Logs**: stderr only, structured via `tracing`. stdout is reserved for
  protocol (and the `probe` CLI product output, which is not an IPC session).
* **Secrets**: RTSP URLs with credentials are never passed as command-line
  arguments (visible in process listings). The worker reads them from the
  `NIAN_VISION_RTSP_URL` environment variable (manual testing) or will
  receive them in IPC payloads once the credential store exists (M5).
* **Supervision** (M3): host spawns one worker per camera, restarts crashed
  workers with the reconnect backoff, and treats worker exit as a camera
  state transition — never as an application failure.
* **Blocking**: worker media calls block their thread; FFmpeg operations are
  bounded by the `AVIOInterruptCB` deadline/cancellation mechanism
  (`nian-media-ffmpeg::InterruptHandle`), so a dead camera cannot hang the
  worker forever.

## Consequences

* A worker crash costs one camera's recording session, not the app.
* Every new capability needs protocol work; this keeps the control plane
  explicit and testable.
* The worker is a plain binary, runnable and debuggable standalone:
  `nian-media-worker probe <file>` and `nian-media-worker run` work without
  the desktop app.
