#!/usr/bin/env bash
set -Eeuo pipefail

# Resolve toolchain versions and verified artifacts from mise.toml/mise.lock.
# CI bootstrap steps use this before Node or Rust is available, so it must not
# depend on either runtime. mise.toml is authoritative; mise.lock supplies the
# per-platform URLs and checksums.

root_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
mise_toml="$root_dir/mise.toml"
mise_lock="$root_dir/mise.lock"

die() {
  printf 'toolchain-pins: %s\n' "$*" >&2
  exit 1
}

pin() {
  local tool="$1" value
  value="$(awk -v key="$tool" '
    /^[[:space:]]*\[/ {
      in_tools = ($0 ~ /^[[:space:]]*\[tools\][[:space:]]*$/)
      next
    }
    !in_tools { next }
    {
      line = $0
      sub(/[[:space:]]*#.*$/, "", line)
      if (line ~ ("^\"?" key "\"?[[:space:]]*=")) {
        sub(/^[^=]*=[[:space:]]*"/, "", line)
        sub(/".*$/, "", line)
        print line
        exit
      }
    }
  ' "$mise_toml")"
  [[ -n "$value" ]] || die "missing $tool pin in mise.toml"
  printf '%s\n' "$value"
}

lock_value() {
  local tool="$1" platform="$2" key="$3" value
  value="$(awk -v section="[tools.${tool}.\"platforms.${platform}\"]" -v key="$key" '
    $0 == section { in_section = 1; next }
    /^\[/ { in_section = 0 }
    in_section && index($0, key " = \"") == 1 {
      line = $0
      sub(/^[^=]*= "/, "", line)
      sub(/"$/, "", line)
      print line
      exit
    }
  ' "$mise_lock")"
  [[ -n "$value" ]] || die "missing $tool.$platform.$key in mise.lock"
  printf '%s\n' "$value"
}

lock_sha256() {
  local value
  value="$(lock_value "$1" "$2" checksum)"
  printf '%s\n' "${value#sha256:}"
}

field="${1:-}"
case "$field" in
  rust | node | pnpm)
    pin "$field"
    ;;
  cargo-audit | cargo-deny | cargo-llvm-cov | cargo-machete | cargo-outdated)
    pin "cargo:$field"
    ;;
  node-url-* | pnpm-url-*)
    lock_value "${field%%-url-*}" "${field#*-url-}" url
    ;;
  node-sha256-* | pnpm-sha256-*)
    lock_sha256 "${field%%-sha256-*}" "${field#*-sha256-}"
    ;;
  *)
    die "usage: $0 <rust|node|pnpm|cargo-*|node-url-<platform>|node-sha256-<platform>|pnpm-url-<platform>|pnpm-sha256-<platform>>"
    ;;
esac
