#!/usr/bin/env bash
# Regenerates the deterministic test fixture used by media integration tests.
# The fixture uses FFmpeg's native (LGPL) mpeg4 encoder and the synthetic
# testsrc2 source, so no GPL components and no real camera are involved.
set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture_dir="$repo_root/crates/nian-media-ffmpeg/tests/fixtures"
mkdir -p "$fixture_dir"

ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc2=duration=2:size=160x120:rate=10" \
  -c:v mpeg4 -q:v 10 -g 5 \
  -f matroska "$fixture_dir/sample.mkv"

ls -la "$fixture_dir/sample.mkv"
