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
desktop_session_pid=""
cleanup() {
  if [[ -n "$desktop_session_pid" ]] && kill -0 "$desktop_session_pid" 2>/dev/null; then
    kill -TERM -- "-$desktop_session_pid" 2>/dev/null || true
    for _ in $(seq 1 50); do
      kill -0 "$desktop_session_pid" 2>/dev/null || break
      sleep 0.1
    done
    if kill -0 "$desktop_session_pid" 2>/dev/null; then
      kill -KILL -- "-$desktop_session_pid" 2>/dev/null || true
    fi
    wait "$desktop_session_pid" 2>/dev/null || true
  fi
  runtime_root="$work_dir/desktop-home/runtime"
  if [[ -d "$runtime_root" ]]; then
    shopt -s nullglob dotglob
    for candidate in "$runtime_root"/*; do
      # D-Bus services may leave detached GVFS/document-portal FUSE mountpoints.
      # Trying every top-level runtime entry is harmless for ordinary directories
      # and avoids depending on a mount namespace visible to findmnt/mountpoint.
      fusermount3 -uz "$candidate" 2>/dev/null || umount -l "$candidate" 2>/dev/null || true
    done
    shopt -u nullglob dotglob
  fi
  rm -rf "$work_dir"
}
trap cleanup EXIT

appimage="${images[0]}"
(
  cd "$work_dir"
  "$appimage" --appimage-extract >/dev/null
)
appdir="$work_dir/squashfs-root"

required=(
  "$appdir/usr/bin/nian-desktop"
  "$appdir/usr/bin/nian-media-worker"
  "$appdir/usr/lib/nian-vision/libavformat.so.62"
  "$appdir/usr/lib/nian-vision/libavcodec.so.62"
  "$appdir/usr/lib/nian-vision/libavutil.so.60"
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

for name in libavformat.so.62 libavcodec.so.62 libavutil.so.60; do
  if [[ -e "$appdir/usr/lib/$name" ]]; then
    echo "AppImage still contains legacy FFmpeg runtime layout: $appdir/usr/lib/$name" >&2
    exit 1
  fi
done

worker_runpath="$(readelf -d "$appdir/usr/bin/nian-media-worker" | awk '/\(RUNPATH\)|\(RPATH\)/ { sub(/^.*\[/, ""); sub(/\].*$/, ""); print }')"
if [[ "$worker_runpath" != '$ORIGIN/../lib/nian-vision' ]]; then
  echo "AppImage worker has unexpected FFmpeg RUNPATH: ${worker_runpath:-<missing>}" >&2
  exit 1
fi

worker_ldd="$(env -u LD_LIBRARY_PATH -u NIAN_FFMPEG_LIB_DIR ldd "$appdir/usr/bin/nian-media-worker")"
for name in libavformat.so.62 libavcodec.so.62 libavutil.so.60; do
  expected="$appdir/usr/lib/nian-vision/$name"
  resolved="$(awk -v name="$name" '$1 == name && $2 == "=>" { print $3 }' <<<"$worker_ldd")"
  resolved_real="$(readlink -f "$resolved" 2>/dev/null || true)"
  expected_real="$(readlink -f "$expected")"
  if [[ -z "$resolved_real" || "$resolved_real" != "$expected_real" ]]; then
    echo "AppImage worker FFmpeg dependency did not resolve from installation-local runtime: $name => ${resolved:-<unresolved>} (expected $expected)" >&2
    exit 1
  fi
done

# Scan the actual extracted application tree after Tauri bundling. The scanner is
# binary-safe and never prints the configured sentinel value.
node "$repo_root/scripts/release/scan-release-secrets.mjs" "$appdir"

env -u LD_LIBRARY_PATH -u NIAN_FFMPEG_LIB_DIR \
  node "$repo_root/scripts/release/stage-runtime-smoke.mjs" \
  "$appdir/usr/bin/nian-media-worker" "$work_dir/runtime-smoke"

# Launch the real AppImage under an isolated X11 + D-Bus session. Readiness is a
# backend marker emitted only after Tauri setup has completed. Polling is bounded;
# there is no arbitrary long sleep pretending to be a readiness check.
desktop_home="$work_dir/desktop-home"
mkdir -p \
  "$desktop_home/.config" \
  "$desktop_home/.cache" \
  "$desktop_home/.local/share" \
  "$desktop_home/runtime"
chmod 0700 "$desktop_home/runtime"
desktop_log="$work_dir/desktop.log"

setsid env \
  -u LD_LIBRARY_PATH \
  -u NIAN_FFMPEG_LIB_DIR \
  APPIMAGE_EXTRACT_AND_RUN=1 \
  HOME="$desktop_home" \
  XDG_CONFIG_HOME="$desktop_home/.config" \
  XDG_CACHE_HOME="$desktop_home/.cache" \
  XDG_DATA_HOME="$desktop_home/.local/share" \
  XDG_RUNTIME_DIR="$desktop_home/runtime" \
  GSETTINGS_BACKEND=memory \
  NO_AT_BRIDGE=1 \
  dbus-run-session -- \
  xvfb-run -a \
  "$appimage" --startup-hidden \
  >"$desktop_log" 2>&1 &
desktop_session_pid=$!

ready=0
deadline=$((SECONDS + 20))
while (( SECONDS < deadline )); do
  if grep -Fq 'desktop startup ready' "$desktop_log"; then
    ready=1
    break
  fi
  if ! kill -0 "$desktop_session_pid" 2>/dev/null; then
    echo "desktop AppImage exited before startup readiness" >&2
    sed -n '1,160p' "$desktop_log" >&2
    wait "$desktop_session_pid" 2>/dev/null || true
    desktop_session_pid=""
    exit 1
  fi
  sleep 0.1
done
if [[ "$ready" -ne 1 ]]; then
  echo "desktop AppImage did not reach startup readiness before the bounded deadline" >&2
  sed -n '1,160p' "$desktop_log" >&2
  exit 1
fi

# Prove it remains alive after readiness rather than emitting the marker on its
# way to an immediate crash.
for _ in $(seq 1 10); do
  if ! kill -0 "$desktop_session_pid" 2>/dev/null; then
    echo "desktop AppImage exited during the post-readiness stability interval" >&2
    sed -n '1,160p' "$desktop_log" >&2
    wait "$desktop_session_pid" 2>/dev/null || true
    desktop_session_pid=""
    exit 1
  fi
  sleep 0.1
done

kill -TERM -- "-$desktop_session_pid" 2>/dev/null || true
for _ in $(seq 1 50); do
  kill -0 "$desktop_session_pid" 2>/dev/null || break
  sleep 0.1
done
if kill -0 "$desktop_session_pid" 2>/dev/null; then
  echo "desktop AppImage did not terminate after controlled smoke shutdown" >&2
  exit 1
fi
wait "$desktop_session_pid" 2>/dev/null || true
desktop_session_pid=""

printf 'AppImage worker and desktop startup smoke passed: %s\n' "$appimage"
