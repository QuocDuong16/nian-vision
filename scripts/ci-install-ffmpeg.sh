#!/usr/bin/env bash
# Builds a minimal FFmpeg 8.0.1 from the sha256-pinned upstream tarball for
# CI environments whose distro FFmpeg has the wrong ABI (e.g. Debian's 7.x).
#
# The build is deliberately tiny (no encoders beyond what the test fixture
# needs, no external libraries) so it stays LGPL and finishes in minutes.
#
# Output: $PWD/ffmpeg-dist/{lib,include}
set -Eeuo pipefail

FFMPEG_VERSION="8.0.1"
FFMPEG_TARBALL_SHA256="05ee0b03119b45c0bdb4df654b96802e909e0a752f72e4fe3794f487229e5a41"

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
  --enable-parser=h264,mpeg4video,mpegaudio \
  --enable-decoder=mpeg4

make -j"$(nproc)"
make install

echo "FFmpeg installed to $dist_dir"
ls "$dist_dir/lib"
