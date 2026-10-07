#!/usr/bin/env bash
set -Eeuo pipefail

# Install the checksum-verified standalone pnpm executable.
version="12.9.1"
case "$(uname -m)" in
  x86_64)
    arch="x64"
    checksum="b4f58449ac02d9d24023ac89e17e05e8331f8ddc12ca1f489cf231bc2bb2bce1"
    ;;
  aarch64)
    arch="arm64"
    checksum="468294b77633d29889c6e4a79cefb1277eba30cd62e7d6a754218cb85a1d730d"
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
