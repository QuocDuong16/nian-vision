#!/usr/bin/env bash
set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
bundle_dir="$repo_root/target/release/bundle/appimage"
mapfile -t images < <(find "$bundle_dir" -maxdepth 1 -type f -name '*.AppImage' -print | sort)
if [[ "${#images[@]}" -ne 1 ]]; then
  echo "expected exactly one AppImage in $bundle_dir, found ${#images[@]}" >&2
  exit 1
fi

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT
appimage="${images[0]}"
(
  cd "$work_dir"
  "$appimage" --appimage-extract >/dev/null
)
appdir="$work_dir/squashfs-root"

required=(
  "$appdir/usr/bin/nian-desktop"
  "$appdir/usr/bin/nian-media-worker"
  "$appdir/usr/lib/libavformat.so.62"
  "$appdir/usr/lib/libavcodec.so.62"
  "$appdir/usr/lib/libavutil.so.60"
  "$appdir/usr/share/doc/nian-vision/THIRD_PARTY_NOTICES.txt"
  "$appdir/usr/share/doc/nian-vision/FFMPEG-LGPL-2.1.txt"
  "$appdir/usr/share/doc/nian-vision/FFMPEG_BUILD_FLAGS.txt"
  "$appdir/usr/share/doc/nian-vision/FFMPEG_CONFIG.h"
  "$appdir/usr/share/nian-vision/BUILD_METADATA.json"
)
for path in "${required[@]}"; do
  test -e "$path" || {
    echo "AppImage installed layout is missing: $path" >&2
    exit 1
  }
done

if ! readelf -d "$appdir/usr/bin/nian-media-worker" | grep -Fq '$ORIGIN/../lib'; then
  echo "AppImage worker lost its installation-local FFmpeg RUNPATH" >&2
  exit 1
fi

env -u LD_LIBRARY_PATH -u NIAN_FFMPEG_LIB_DIR \
  node "$repo_root/scripts/release/stage-runtime-smoke.mjs" \
  "$appdir/usr/bin/nian-media-worker" "$work_dir/runtime-smoke"

printf 'AppImage extracted-layout smoke passed: %s\n' "$appimage"
