# Vendored FFmpeg headers

These are the **unmodified public header files** of
[FFmpeg 8.0.3](https://ffmpeg.org/releases/ffmpeg-8.0.3.tar.xz)
(sha256 `6136812ea6d4e68bdba27e33c2a94382711cdf4f8602ffef056ff792bd6f9818`),
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
