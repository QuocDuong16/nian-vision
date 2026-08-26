# ADR-0004: Recording container

- Status: accepted
- Date: 2026-08-26

## Context

The recorder (M2) writes continuous camera footage as segmented files. The
container choice affects crash resilience, playback compatibility, seek
behavior and retention tooling. Streams are recorded via stream copy (no
re-encode), typically H.264 from RTSP cameras like the Tapo C200.

## Decision

**Matroska (`.mkv`)** for stored segments, produced by stream copy.

* Matroska is the most forgiving container for interrupted writes: a crash
  mid-segment leaves a file that inspection tools can still read up to the
  last written cluster, unlike MP4 where a missing `moov` atom can make the
  whole file unplayable.
* H.264 in Matroska is universally supported by FFmpeg and by every player
  backend we might adopt for playback (M6 remux pipeline).
* Variable frame rate and missing timestamps — normal for IP cameras — are
  first-class in Matroska.
* Segments rotate at the first video keyframe at/after the ~5 minute target
  (`nian-application::SegmentTargetDuration`), so every segment starts on a
  keyframe and is independently playable.
* Durability pattern (master spec §9): segments are written as
  `HH-MM-SS.partial.mkv`, then atomically renamed to `HH-MM-SS.mkv` after
  `av_write_trailer`. The mechanism is already implemented and tested in
  `nian-media-ffmpeg::MatroskaMuxer` (finalize path); the partial-file
  naming lives in `nian-storage`.

## Consequences

* Crash recovery can salvage partial segments (M3/M4 reconciliation).
* Web playback (M6) will remux Matroska → fragmented MP4 in the worker; no
  re-encode when codecs are browser-compatible.
* `.mkv` files are slightly larger than MP4 for identical streams; for NVR
  storage this overhead is negligible.
