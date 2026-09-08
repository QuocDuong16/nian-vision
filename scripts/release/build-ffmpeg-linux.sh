#!/usr/bin/env bash
set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
config_json="$repo_root/scripts/release/release-config.json"
output_dir="${1:-$repo_root/dist/ffmpeg-linux-x86_64}"

read_config() {
  node -e 'const fs=require("fs"); const c=JSON.parse(fs.readFileSync(process.argv[1], "utf8")); process.stdout.write(String(c[process.argv[2]]));' "$config_json" "$1"
}

ffmpeg_version="$(read_config ffmpegVersion)"
ffmpeg_sha256="$(read_config ffmpegSourceSha256)"
ffmpeg_url="$(read_config ffmpegSourceUrl)"

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT
source_dir="$work_dir/ffmpeg-$ffmpeg_version"
tarball="$work_dir/ffmpeg-$ffmpeg_version.tar.xz"
destdir="$work_dir/install-root"
prefix="/opt/nian-vision-ffmpeg"

rm -rf "$output_dir"
mkdir -p "$output_dir"

curl --fail --silent --show-error --location --output "$tarball" "$ffmpeg_url"
printf '%s  %s\n' "$ffmpeg_sha256" "$tarball" | sha256sum --check --strict -
tar -xJf "$tarball" -C "$work_dir"
node "$repo_root/scripts/release/apply-ffmpeg-upstream-patches.mjs" --source-dir "$source_dir"
cd "$source_dir"

configure_flags=(
  "--prefix=$prefix"
  --disable-doc
  --disable-programs
  --disable-static
  --enable-shared
  --disable-gpl
  --disable-nonfree
  --disable-autodetect
  --disable-everything
  --disable-x86asm
  --disable-avdevice
  --disable-avfilter
  --disable-swresample
  --disable-swscale
  --enable-network
  --enable-protocol=file,tcp,rtsp,rtp,udp
  --enable-demuxer=matroska,mov,rtsp
  --enable-muxer=matroska,mov,mp4
  --enable-parser=h264,mpeg4video,mpegaudio,aac
  --enable-decoder=mpeg4,aac
)

printf '%s\n' "${configure_flags[@]}" > "$output_dir/FFMPEG_BUILD_FLAGS.txt"
./configure "${configure_flags[@]}"
cp config.h "$output_dir/FFMPEG_CONFIG.h"
node "$repo_root/scripts/release/validate-ffmpeg-config.mjs" \
  --config-header "$output_dir/FFMPEG_CONFIG.h" \
  --flags "$output_dir/FFMPEG_BUILD_FLAGS.txt"

make -j"$(nproc)"
make install DESTDIR="$destdir"

install_root="$destdir$prefix"
cp -a "$install_root/include" "$output_dir/include"
cp -a "$install_root/lib" "$output_dir/lib"
cp "$source_dir/COPYING.LGPLv2.1" "$output_dir/FFMPEG-LGPL-2.1.txt"

for soname in libavformat.so.62 libavcodec.so.62 libavutil.so.60; do
  test -e "$output_dir/lib/$soname" || {
    echo "required FFmpeg runtime library missing: $soname" >&2
    exit 1
  }
done

# Reject accidental runtime expansion into FFmpeg libraries that are not part of
# the release contract. System libc/libm/libpthread dependencies are allowed.
for library in "$output_dir/lib/libavformat.so.62" "$output_dir/lib/libavcodec.so.62" "$output_dir/lib/libavutil.so.60"; do
  if env LD_LIBRARY_PATH="$output_dir/lib" ldd "$library" | grep -E 'lib(avdevice|avfilter|postproc|swresample|swscale)'; then
    echo "minimal FFmpeg build unexpectedly depends on a disabled FFmpeg component" >&2
    exit 1
  fi
done

printf 'FFmpeg %s Linux shared runtime built at %s\n' "$ffmpeg_version" "$output_dir"
