# ADR-0004: Recording container, segmentation and timestamps

- Status: accepted (amended 2026-08-26 for M2 implementation decisions)
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
  (`RecorderConfig::segment_target`, default
  `nian_recorder::DEFAULT_SEGMENT_TARGET`), so every segment starts on a
  keyframe and is independently playable. Audio packet boundaries never
  rotate a segment.
* Durability pattern: segments are written as `HH-MM-SS[-N].partial.mkv`
  under an exclusive claim (`claim_segment`), then published without
  replacement after `av_write_trailer` **and** a successful final I/O
  flush/close — any failure leaves the partial recoverable, never counted
  as completed. The `-N` disambiguator keeps segments starting in the same
  second collision-free.

### Amendment (M2): keyframe-aware rotation and startup alignment

* **Rotation clock is media time, not wall clock.** Elapsed segment time is
  measured as packet DTS minus the segment's opening DTS, in the video
  stream's *validated* time base (a stream without a usable time base fails
  the session instead of being guessed at). DTS is preferred over PTS
  because stream copy writes packets in decode order; with B-frames PTS
  oscillates around DTS and would mis-measure elapsed time. Wall clock is
  used only for naming segments and human-facing start times.
* **Startup alignment**: packets are discarded until the first selected
  video keyframe; audio received before that keyframe is deliberately
  dropped rather than synchronized. Connecting between GOP boundaries can
  therefore never produce a segment that starts mid-GOP.
* **Boundary ownership**: when the target is reached, the recorder waits
  for the next selected video keyframe, closes the old segment **before**
  it, and writes that keyframe as the first packet of the new segment.
* **Empty-segment rule**: a segment that received no video packet is never
  published; it stays behind as an abandoned `.partial.mkv` (recovery
  eligible). By construction segments open on a keyframe, so header-only
  finals cannot occur.

### Amendment (M2): timestamp policy — source timestamps preserved

Segments keep the **source timestamps** of copied packets (rescaled between
time bases by `av_packet_rescale_ts` only); each segment is *not* rebased to
start near zero. Rationale:

* FFmpeg remux requirements are satisfied without rewriting time:
  `av_interleaved_write_frame` needs monotonically increasing DTS per
  stream within one output, which continuous RTSP sources guarantee and
  which rebasing must carefully preserve. Matroska stores timestamps on an
  absolute nanosecond scale and has no problem with large values; i64 ticks
  at 1/90 000 do not overflow on any realistic timescale.
* Per-stream constant-offset rebasing preserves intra-stream ordering, but
  doing it *correctly* across streams requires deriving every stream's
  offset from one common media instant and handling negative results for
  packets that precede it — with per-source quirks (audio priming, initial
  DTS offsets) that differ per camera. A wrong transform silently corrupts
  A/V sync in every segment, while preserved timestamps cannot regress.
* Milestone mandate: prefer documented correctness over clever transforms;
  rebase remains a possible future amendment once real-camera sources have
  been validated.

Known consequence: because timestamps are absolute, the container duration
element of a segment reflects its position in the source timeline rather
than its span. Playback and seeking are unaffected; tooling must derive
segment spans from packet timestamps (as the recorder's
`media_duration` reporting does).

## Consequences

* Crash recovery can salvage partial segments (M3/M4 reconciliation).
* Web playback (M6) will remux Matroska → fragmented MP4 in the worker; no
  re-encode when codecs are browser-compatible.
* `.mkv` files are slightly larger than MP4 for identical streams; for NVR
  storage this overhead is negligible.
