#!/usr/bin/env bash
# Prepares FFmpeg link shims on Linux systems that ship FFmpeg runtime
# libraries (libavformat.so.NN) but no development packages (no
# libavformat.so symlink, no pkg-config file).
#
# Creates unversioned symlinks under <repo>/.ffmpeg-lib/ pointing at the
# versioned runtime libraries. Linking resolves through these shims; at
# runtime the loader uses the system SONAMEs, exactly as if the -dev package
# were installed.
#
# Usage: scripts/setup-ffmpeg-linux.sh
set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="$repo_root/.ffmpeg-lib"
mkdir -p "$target_dir"

for lib in avformat avcodec avutil; do
  versioned="$(ldconfig -p 2>/dev/null | awk -v lib="lib${lib}.so" '$1 ~ lib"\\." {print $NF; exit}')"
  if [[ -z "$versioned" ]]; then
    # Fall back to a direct filesystem search when ldconfig has no entry.
    versioned="$(find /usr/lib /lib -name "lib${lib}.so.*" -type f 2>/dev/null | sort -V | tail -1)"
  fi
  if [[ -z "$versioned" ]]; then
    echo "error: no runtime library found for lib${lib}" >&2
    echo "Install FFmpeg (e.g. 'apt install libavformat62') first." >&2
    exit 1
  fi

  ln -sfn "$versioned" "$target_dir/lib${lib}.so"
  echo "lib${lib}.so -> $versioned"
done

cat <<'EOF'

Done. cargo will pick up .ffmpeg-lib automatically.
Verify with:  cargo build -p nian-ffmpeg-sys
EOF
