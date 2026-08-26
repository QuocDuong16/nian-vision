# ADR-0002: FFmpeg integration strategy

- Status: accepted
- Date: 2026-08-26

## Context

Nian Vision needs FFmpeg for RTSP ingest and Matroska stream-copy recording.
Decisions required: exact version, binding strategy, linking mode, license
posture, and how the runtime finds the libraries. Constraints from the
master spec: Windows-first distribution, LGPL-compatible builds only, no
`ffmpeg.exe` subprocess as the media engine, reproducible builds, and a
small audited `unsafe` surface.

## Research summary (2026-08-26, primary sources)

* FFmpeg release lines (ffmpeg.org/download.html): 9.0.1 released
  2026-08-12 (libavformat ABI 63); 8.x line maintained (8.1.2 / 8.0.3,
  libavformat ABI 62).
* Rust binding ecosystem (crates.io): `ffmpeg-the-third`
  6.0.0+ffmpeg-9.0 claims FFmpeg 5.1–9.0 support; `ffmpeg-next` 9.0.0 is in
  maintenance mode with a README that still claims only ≤8.0 support;
  `ffmpeg-sys-next` 9.0.0.
* Licensing (ffmpeg.org/legal.html): default builds are LGPL 2.1+;
  `--enable-gpl` / `--enable-nonfree` flip the whole work to GPL/nonfree.

## Decision

### Version: FFmpeg 8.x (ABI 62), distribution pins an exact 8.0.x build

The development reference runtime is FFmpeg 8.0.1 (Ubuntu package,
`libavformat.so.62`), which is what every test in this repository actually
runs against. 9.0.1 is two weeks old at decision time; no environment in
this project (Linux CI image, Windows packaging, maintainer machines) is
verified against ABI 63 yet. Per the master spec's "verify before pinning"
rule we pin the ABI we can prove, and treat 9.x as a planned upgrade:

* bindings are generated from the **8.0.1** headers (vendored under
  `thirdparty/ffmpeg`, sha256-pinned tarball);
* `nian-media-ffmpeg` refuses to start when the loaded runtime's
  `libavformat`/`libavcodec`/`libavutil` majors differ from the compiled-in
  majors (fail-fast `AbiMismatch`, never a segfault);
* upgrade path: re-vendor 9.0.1 headers → `cargo run -p bindgen-gen` →
  fix compile errors → validate on Windows + CI → bump the distribution
  pin. Recorded as a single documented procedure.

### Bindings: owned `nian-ffmpeg-sys`, generated once with bindgen

We do **not** depend on `ffmpeg-sys-next`/`ffmpeg-the-third`:

* the architecture already mandates an owned raw layer (`nian-ffmpeg-sys`)
  plus a safe wrapper (`nian-media-ffmpeg`); the third-party safe wrapper
  would duplicate `nian-media` and pull its own opinions and unsafe surface;
* the whitelist is tiny (~36 functions, ~12 types) — exactly the demux/mux
  API the recorder needs — so the audit surface stays reviewable;
* generation is a **one-shot, committed** step: `cargo run -p bindgen-gen`
  reads the vendored headers and writes `src/bindings.rs`. Normal builds
  compile the committed file and need neither clang nor network. The
  generator tool lives in `tools/bindgen-gen` (excluded from default
  workspace builds; requires libclang only when regenerating).

### Linking: dynamic only

* `build.rs` discovery order: `NIAN_FFMPEG_LIB_DIR` → repo-local
  `.ffmpeg-lib/` shims (`scripts/setup-ffmpeg-linux.sh` creates unversioned
  symlinks to the system's versioned sonames on machines without `-dev`
  packages) → `pkg-config`.
* No static linking of FFmpeg into any shipped binary. Distribution bundles
  the shared libraries separately (M8), which is also what keeps the LGPL
  notice obligations simple (replace-the-library compliance).

### License: LGPL mode

* Development uses the system FFmpeg; distribution will ship an FFmpeg
  configured **without** `--enable-gpl` and **without** `--enable-nonfree`.
* The test fixture uses the native `mpeg4` encoder and `testsrc2` synthetic
  source — no GPL encoder (x264/x265) is involved anywhere in CI.
* `THIRD_PARTY_NOTICES` obligations land with the M8 installer; the vendored
  headers under `thirdparty/` are unmodified FFmpeg source files and retain
  their LGPL 2.1+ license (see `thirdparty/README.md`).

## Consequences

* `cargo build` works on any machine with FFmpeg runtime libraries, even
  without dev packages, via the shim directory or `NIAN_FFMPEG_LIB_DIR`.
* A runtime/compile ABI mismatch fails at worker startup with an explicit
  error instead of undefined behavior.
* The binding surface is deliberately incomplete; new FFmpeg calls require
  extending the whitelist and regenerating — a feature (review) and a cost
  (a few minutes per capability).
* FFmpeg 9.x adoption is a bounded, planned task, not an emergency.
