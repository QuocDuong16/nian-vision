#!/usr/bin/env bash
# Regenerates the deterministic test fixtures used by media integration tests.
# Both fixtures use FFmpeg's native (LGPL) encoders (mpeg4 video, native AAC)
# and synthetic sources (testsrc2/sine), so no GPL components and no real
# camera are involved.
#
#   sample.mkv    video only — minimal happy-path fixture
#   sample_av.mkv video + audio — exercises stream selection/mapping when a
#                 subset of the input streams is recorded
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

ls -la "$fixture_dir"
