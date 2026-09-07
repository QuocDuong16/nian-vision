#!/usr/bin/env bash
set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ffmpeg_dir="${1:-$repo_root/dist/ffmpeg-linux-x86_64}"
stage="$repo_root/dist/linux-x86_64"
target="x86_64-unknown-linux-gnu"
worker_source="$repo_root/target/release/nian-media-worker"
worker_stage="$stage/bin/nian-media-worker"
lib_stage="$stage/lib/nian-vision"

required=(
  "$worker_source"
  "$ffmpeg_dir/lib/libavformat.so.62"
  "$ffmpeg_dir/lib/libavcodec.so.62"
  "$ffmpeg_dir/lib/libavutil.so.60"
  "$ffmpeg_dir/FFMPEG_CONFIG.h"
  "$ffmpeg_dir/FFMPEG_BUILD_FLAGS.txt"
  "$ffmpeg_dir/FFMPEG-LGPL-2.1.txt"
  "$repo_root/THIRD_PARTY_NOTICES.txt"
)
for path in "${required[@]}"; do
  test -e "$path" || {
    echo "release staging prerequisite missing: $path" >&2
    exit 1
  }
done

node "$repo_root/scripts/release/validate-ffmpeg-config.mjs" \
  --config-header "$ffmpeg_dir/FFMPEG_CONFIG.h" \
  --flags "$ffmpeg_dir/FFMPEG_BUILD_FLAGS.txt"

rm -rf "$stage"
mkdir -p "$stage/bin" "$stage/tauri" "$lib_stage"
install -m 0755 "$worker_source" "$worker_stage"
install -m 0755 "$worker_source" "$stage/tauri/nian-media-worker-$target"
for soname in libavformat.so.62 libavcodec.so.62 libavutil.so.60; do
  cp -L "$ffmpeg_dir/lib/$soname" "$lib_stage/$soname"
  chmod 0644 "$lib_stage/$soname"
done
install -m 0644 "$repo_root/THIRD_PARTY_NOTICES.txt" "$stage/THIRD_PARTY_NOTICES.txt"
install -m 0644 "$ffmpeg_dir/FFMPEG-LGPL-2.1.txt" "$stage/FFMPEG-LGPL-2.1.txt"
install -m 0644 "$ffmpeg_dir/FFMPEG_BUILD_FLAGS.txt" "$stage/FFMPEG_BUILD_FLAGS.txt"
install -m 0644 "$ffmpeg_dir/FFMPEG_CONFIG.h" "$stage/FFMPEG_CONFIG.h"
node "$repo_root/scripts/release/build-metadata.mjs" --output "$stage/BUILD_METADATA.json" --target "$target"

worker_runpath="$(readelf -d "$worker_stage" | awk '/\(RUNPATH\)|\(RPATH\)/ { sub(/^.*\[/, ""); sub(/\].*$/, ""); print }')"
if [[ "$worker_runpath" != '$ORIGIN/../lib/nian-vision' ]]; then
  echo "staged worker has unexpected FFmpeg RUNPATH: ${worker_runpath:-<missing>}" >&2
  exit 1
fi

worker_closure="$(env -u LD_LIBRARY_PATH -u NIAN_FFMPEG_LIB_DIR ldd "$worker_stage")"
printf '%s\n' "$worker_closure"
if grep -q 'not found' <<<"$worker_closure"; then
  echo "runtime dependency closure is incomplete for $worker_stage" >&2
  exit 1
fi

for object in "$lib_stage/libavformat.so.62" "$lib_stage/libavcodec.so.62" "$lib_stage/libavutil.so.60"; do
  closure="$(env -u NIAN_FFMPEG_LIB_DIR LD_LIBRARY_PATH="$lib_stage" ldd "$object")"
  printf '%s\n' "$closure"
  if grep -q 'not found' <<<"$closure"; then
    echo "runtime dependency closure is incomplete for $object" >&2
    exit 1
  fi
  if grep -E 'lib(avdevice|avfilter|postproc|swresample|swscale)\.so' <<<"$closure"; then
    echo "staged FFmpeg runtime unexpectedly depends on a disabled FFmpeg component" >&2
    exit 1
  fi
done

for soname in libavformat.so.62 libavcodec.so.62 libavutil.so.60; do
  resolved="$(awk -v soname="$soname" '$1 == soname && $2 == "=>" { print $3; exit }' <<<"$worker_closure")"
  if [[ -z "$resolved" || "$(readlink -f "$resolved")" != "$(readlink -f "$lib_stage/$soname")" ]]; then
    echo "worker does not resolve $soname from the staged application runtime" >&2
    exit 1
  fi
done
if grep -E 'lib(avdevice|avfilter|postproc|swresample|swscale)\.so' <<<"$worker_closure"; then
  echo "worker unexpectedly depends on a non-staged FFmpeg component" >&2
  exit 1
fi

# Release artifacts must not disclose the build user's home or private key data.
if grep -RIE '/home/[^/]+/|[A-Za-z]:\\Users\\|BEGIN (RSA |OPENSSH )?PRIVATE KEY|vcpkg[/\\]installed|msys64' \
  "$stage" --include='*.json' --include='*.txt' --include='*.h'; then
  echo "release staging contains an absolute build path or secret-like material" >&2
  exit 1
fi
node "$repo_root/scripts/release/scan-release-secrets.mjs" "$stage"

node "$repo_root/scripts/release/stage-runtime-smoke.mjs"
printf 'Linux x86_64 release runtime staged at %s\n' "$stage"
