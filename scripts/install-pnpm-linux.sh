#!/usr/bin/env bash
set -Eeuo pipefail

# Install the checksum-verified standalone pnpm executable.
version="12.4.2"
case "$(uname -m)" in
  x86_64)
    arch="x64"
    checksum="ce1ed690fe9c2f091d7267e1afbe9380abb08bb577e95348fda194a41147d2ec"
    ;;
  aarch64)
    arch="arm64"
    checksum="dc4a29d9848ef005bec3d36d49aae84b115ec3b18c110a3a7f33c37f7fd520bf"
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
