# ADR-0001: Desktop architecture

- Status: accepted
- Date: 2026-08-26
- Deciders: Nian Vision engineering

## Context

Nian Vision is a local-first desktop NVR. The first target platform is
Windows; Linux is the primary development environment. The product must
record from RTSP cameras for long, unattended periods, so process lifetime,
crash containment and storage correctness dominate every other concern.

## Decision

The application is a Tauri 2 desktop app:

```text
React + TypeScript + Vite UI  (ui/)
        │  Tauri commands
Rust desktop host             (apps/nian-desktop)
        │  spawn + NDJSON IPC
nian-media-worker             (apps/nian-media-worker)
        │  FFmpeg FFI
RTSP cameras / container files
```

Supporting Rust crates live in `crates/`:

| Crate | Responsibility | unsafe |
|---|---|---|
| `nian-domain` | product vocabulary: cameras, recordings, media concepts | forbidden |
| `nian-application` | configuration, orchestration policies | forbidden |
| `nian-storage` | recordings layout, path safety | forbidden |
| `nian-ipc` | NDJSON protocol + framing + serve loop | forbidden |
| `nian-media` | backend-agnostic facade (traits + plain types) | forbidden |
| `nian-media-ffmpeg` | safe FFmpeg wrapper | unsafe internals |
| `nian-ffmpeg-sys` | raw generated FFI declarations | raw by definition |

Rationale:

* **Tauri 2** gives a native window with a web UI and a Rust host in one
  process, without shipping a browser runtime; the UI stack (React/Vite) is
  mainstream and testable.
* **A separate media worker process** contains FFmpeg/C failures (ADR-0003).
* **A facade crate (`nian-media`)** keeps application code free of FFmpeg
  types so the backend can evolve (or gain alternatives) without touching
  domain/application code.
* Package management is **pnpm**; Rust is pinned via `rust-toolchain.toml`
  and mise (ADR-0006).

## Consequences

* The desktop host never links FFmpeg; a media crash cannot take the UI down.
* Every cross-process interaction needs an explicit protocol message; this
  is deliberate friction that keeps boundaries honest.
* Windows-first packaging (installer, bundled FFmpeg DLLs) is deferred to
  M8 but the linking strategy (dynamic, LGPL) is fixed now (ADR-0002).
