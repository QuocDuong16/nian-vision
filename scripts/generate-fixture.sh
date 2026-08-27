#!/usr/bin/env bash
# Regenerates the deterministic test fixtures used by media integration tests.
# All fixtures use FFmpeg's native (LGPL) encoders (mpeg4 video, native AAC)
# and synthetic sources (testsrc2/sine), so no GPL components and no real
# camera are involved.
#
#   sample.mkv         video only, 2 s — minimal happy-path fixture
#   sample_av.mkv      video + audio, 2 s — stream selection/mapping
#   session_av.mkv     video + audio, 30 s, keyframe every 2 s — the
#                      segmentation source: at a 5 s target it produces
#                      multiple rotations in the recorder tests
#   reordered_av.mkv   audio listed BEFORE video (video = stream index 1) —
#                      proves the recorder never assumes video index 0
#   bframes_av.mkv     MPEG-4 with explicit B-frames (-bf 2): PTS is
#                      reordered relative to DTS, exercising decode-order
#                      rotation and no-loss boundary handling
#   audio_only.mkv     audio only — must be rejected (no primary video)
set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture_dir="$repo_root/crates/nian-media-ffmpeg/tests/fixtures"
mkdir -p "$fixture_dir"

ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc2=duration=2:size=160x120:rate=10" \
  -c:v mpeg4 -q:v 10 -g 5 \
  -f matroska "$fixture_dir/sample.mkv"

ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc2=duration=2:size=160x120:rate=10" \
  -f lavfi -i "sine=frequency=440:duration=2:sample_rate=44100" \
  -map 0:v -c:v mpeg4 -q:v 10 -g 5 \
  -map 1:a -c:a aac -b:a 64k \
  -f matroska "$fixture_dir/sample_av.mkv"

ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc2=duration=30:size=160x120:rate=10" \
  -f lavfi -i "sine=frequency=440:duration=30:sample_rate=44100" \
  -map 0:v -c:v mpeg4 -q:v 10 -g 20 \
  -map 1:a -c:a aac -b:a 32k \
  -f matroska "$fixture_dir/session_av.mkv"

ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc2=duration=2:size=160x120:rate=10" \
  -f lavfi -i "sine=frequency=440:duration=2:sample_rate=44100" \
  -map 1:a -c:a aac -b:a 32k \
  -map 0:v -c:v mpeg4 -q:v 10 -g 5 \
  -f matroska "$fixture_dir/reordered_av.mkv"

ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc2=duration=12:size=160x120:rate=10" \
  -c:v mpeg4 -q:v 10 -g 20 -bf 2 \
  -f matroska "$fixture_dir/bframes_av.mkv"

ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i "sine=frequency=440:duration=2:sample_rate=44100" \
  -c:a aac -b:a 32k \
  -f matroska "$fixture_dir/audio_only.mkv"

ls -la "$fixture_dir"
