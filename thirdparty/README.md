# Vendored FFmpeg headers

These are the **unmodified public header files** of
[FFmpeg 8.0.1](https://ffmpeg.org/releases/ffmpeg-8.0.1.tar.xz)
(sha256 `05ee0b03119b45c0bdb4df654b96802e909e0a752f72e4fe3794f487229e5a41`),
restricted to the transitive `#include` closure of the public API headers used
by `nian-ffmpeg-sys` (`libavutil`, `libavcodec`, `libavformat`).

Purpose:

* input for `cargo run -p bindgen-gen`, which (re)generates the committed
  FFI declarations in `crates/nian-ffmpeg-sys/src/bindings.rs`;
* lets regeneration happen offline and byte-identically on any machine.

They are **not** compiled into Nian Vision and are not distributed as part of
any binary artifact. FFmpeg is licensed LGPL 2.1 or later; these headers
retain their original license and copyright. The runtime libraries Nian
Vision links against are provided by the operating system or an installer
bundle and are never built from this directory.

To refresh: run `scripts/fetch-ffmpeg-headers.sh` (re-downloads and verifies
the pinned tarball), then `cargo run -p bindgen-gen`.
