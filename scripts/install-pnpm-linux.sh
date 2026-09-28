#!/usr/bin/env bash
set -Eeuo pipefail

# Install the checksum-verified standalone pnpm executable.
version="12.7.0"
case "$(uname -m)" in
  x86_64)
    arch="x64"
    checksum="68190c7d289efecd66ff088c93b9c5ec361989e8194239e004bd905403d3d3e5"
    ;;
  aarch64)
    arch="arm64"
    checksum="db39a3fc7969dcf3398ebf2f2090c62ba732e5a9887b96147a97134a1a779088"
    ;;
  *) echo "Unsupported Linux architecture: $(uname -m)" >&2; exit 1 ;;
esac
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

archive="$tmp_dir/pnpm-linux-${arch}.tar.gz"
curl --fail --silent --show-error --location \
  "https://github.com/pnpm/pnpm/releases/download/v${version}/pnpm-linux-${arch}.tar.gz" \
  --output "$archive"
printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check --strict -
tar --extract --gzip --file "$archive" --directory "$tmp_dir" pnpm
install_dir="${PNPM_INSTALL_DIR:-/usr/local/bin}"
install -m 0755 "$tmp_dir/pnpm" "$install_dir/pnpm"

installed_version="$("$install_dir/pnpm" --version)"
[[ "$installed_version" == "$version" ]] || {
  echo "pnpm version mismatch: expected $version, got $installed_version" >&2
  exit 1
}
