#!/usr/bin/env bash
set -Eeuo pipefail

# Install the checksum-verified standalone pnpm executable pinned by mise.
# The version, URL, and checksum come from mise.toml/mise.lock so CI cannot
# drift from the repository toolchain.
root_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
pins="$root_dir/scripts/toolchain-pins.sh"

version="$("$pins" pnpm)"
case "$(uname -m)" in
  x86_64) platform="linux-x64" ;;
  aarch64) platform="linux-arm64" ;;
  *) echo "Unsupported Linux architecture: $(uname -m)" >&2; exit 1 ;;
esac

url="$("$pins" "pnpm-url-$platform")"
checksum="$("$pins" "pnpm-sha256-$platform")"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

archive="$tmp_dir/$(basename "$url")"
curl --fail --silent --show-error --location --output "$archive" "$url"
printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check --strict -
tar --extract --gzip --file "$archive" --directory "$tmp_dir" pnpm
install_dir="${PNPM_INSTALL_DIR:-/usr/local/bin}"
install -m 0755 "$tmp_dir/pnpm" "$install_dir/pnpm"

installed_version="$("$install_dir/pnpm" --version)"
[[ "$installed_version" == "$version" ]] || {
  echo "pnpm version mismatch: expected $version, got $installed_version" >&2
  exit 1
}
