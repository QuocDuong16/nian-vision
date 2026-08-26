#!/usr/bin/env bash
# Builds a minimal FFmpeg 8.0.3 from the sha256-pinned upstream tarball for
# CI environments whose distro FFmpeg has the wrong ABI (e.g. Debian's 7.x).
#
# The build is deliberately tiny (no encoders beyond what the test fixture
# needs, no external libraries) so it stays LGPL and finishes in minutes.
#
# Output: $PWD/ffmpeg-dist/{lib,include}
set -Eeuo pipefail

FFMPEG_VERSION="8.0.3"
FFMPEG_TARBALL_SHA256="6136812ea6d4e68bdba27e33c2a94382711cdf4f8602ffef056ff792bd6f9818"

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT
dist_dir="$PWD/ffmpeg-dist"

tarball="$work_dir/ffmpeg-$FFMPEG_VERSION.tar.xz"
curl --fail --silent --show-error --location --output "$tarball" \
  "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz"
echo "$FFMPEG_TARBALL_SHA256  $tarball" | sha256sum --check --strict -

tar -xJf "$tarball" -C "$work_dir"
cd "$work_dir/ffmpeg-$FFMPEG_VERSION"

./configure \
  --prefix="$dist_dir" \
  --disable-doc \
  --disable-programs \
  --disable-static \
  --enable-shared \
  --disable-everything \
  --disable-x86asm \
  --enable-network \
  --enable-protocol=file,tcp,rtsp,rtp,udp \
  --enable-demuxer=matroska,rtsp \
  --enable-muxer=matroska \
  --enable-parser=h264,mpeg4video,mpegaudio,aac \
  --enable-decoder=mpeg4,aac

make -j"$(nproc)"
make install

echo "FFmpeg installed to $dist_dir"
ls "$dist_dir/lib"
