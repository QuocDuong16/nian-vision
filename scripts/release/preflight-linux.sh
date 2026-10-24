#!/usr/bin/env bash
set -Eeuo pipefail

if [[ "$(uname -s)" != "Linux" || "$(uname -m)" != "x86_64" ]]; then
  echo "M8 Linux release requires an x86_64 Linux runner" >&2
  exit 1
fi

required=(
  cargo rustc node pnpm gcc make pkg-config curl sha256sum tar xz
  readelf ldd patchelf file git dbus-run-session xvfb-run xauth setsid fusermount3
)
for command in "${required[@]}"; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "required Linux release tool is unavailable: $command" >&2
    exit 1
  }
done

rust_version="$(rustc --version)"
node_version="$(node --version)"
pnpm_version="$(pnpm --version)"
pins="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)/toolchain-pins.sh"
expected_rust="$("$pins" rust)"
expected_node="$("$pins" node)"
expected_pnpm="$("$pins" pnpm)"
[[ "$rust_version" == "rustc $expected_rust "* ]] || {
  echo "release Rust toolchain mismatch: expected $expected_rust, got $rust_version" >&2
  exit 1
}
[[ "$node_version" == "v$expected_node" ]] || {
  echo "release Node.js mismatch: expected v$expected_node, got $node_version" >&2
  exit 1
}
[[ "$pnpm_version" == "$expected_pnpm" ]] || {
  echo "release pnpm mismatch: expected $expected_pnpm, got $pnpm_version" >&2
  exit 1
}

pkg-config --exists webkit2gtk-4.1 || {
  echo "webkit2gtk-4.1 development files are required for the Tauri Linux build" >&2
  exit 1
}

printf 'Linux release preflight passed: %s; %s; pnpm %s\n' "$rust_version" "$node_version" "$pnpm_version"
