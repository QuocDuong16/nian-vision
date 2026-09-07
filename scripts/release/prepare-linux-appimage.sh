#!/usr/bin/env bash
set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
release_dir="${1:-$repo_root/target/release}"
workspace="${2:-${GITHUB_WORKSPACE:-$repo_root}}"
target_root="$(dirname "$release_dir")"
desktop_binary="$release_dir/nian-desktop"
minimum_free_kb=$((4 * 1024 * 1024))

[[ -d "$workspace" ]] || { echo "Linux AppImage workspace is unavailable: $workspace" >&2; exit 1; }
[[ -d "$release_dir" && ! -L "$release_dir" ]] || { echo "Linux release directory is unavailable or unsafe: $release_dir" >&2; exit 1; }
[[ -f "$desktop_binary" && ! -L "$desktop_binary" && -x "$desktop_binary" ]] || {
  echo "built Linux desktop executable is missing, non-regular, or non-executable: $desktop_binary" >&2
  exit 1
}

print_release_disk_diagnostics() {
  local label="$1"
  echo "$label"
  df -h
  df -Pk "$workspace"
  du -sh "$target_root" 2>/dev/null || true
  du -sh "$release_dir" 2>/dev/null || true
  du -sh "$repo_root/dist" 2>/dev/null || true
  du -sh "$repo_root/ui/dist" 2>/dev/null || true

  echo "Largest target/release entries by allocated KiB (top 20):"
  du -x -k --max-depth=1 "$release_dir" 2>/dev/null | sort -nr | sed -n '1,20p' || true

  echo "Largest target/release top-level files by bytes (top 20):"
  find "$release_dir" -maxdepth 1 -type f -printf '%s\t%p\n' 2>/dev/null | sort -nr | sed -n '1,20p' || true
}

print_release_disk_diagnostics "Post-Tauri-build disk diagnostics before release-target pruning:"

desktop_sha_before="$(sha256sum "$desktop_binary" | awk '{ print $1 }')"
[[ "$desktop_sha_before" =~ ^[0-9a-f]{64}$ ]] || { echo "unable to hash built Linux desktop executable" >&2; exit 1; }

# Tauri CLI 2.11.4 `tauri bundle` resolves its project output directory to
# target/release and opens the main binary directly from that directory. Its
# externalBin/AppImage custom-file inputs remain config-resolved paths under
# dist/linux-x86_64, while icons/config stay in the source tree. Cargo's
# compilation-only directories below are therefore not bundle inputs.
for intermediate in deps build .fingerprint incremental; do
  rm -rf -- "$release_dir/$intermediate"
done

[[ -f "$desktop_binary" && ! -L "$desktop_binary" && -x "$desktop_binary" ]] || {
  echo "Linux desktop executable was lost while pruning release intermediates" >&2
  exit 1
}
desktop_sha_after="$(sha256sum "$desktop_binary" | awk '{ print $1 }')"
[[ "$desktop_sha_after" == "$desktop_sha_before" ]] || {
  echo "Linux desktop executable changed while pruning release intermediates" >&2
  exit 1
}

print_release_disk_diagnostics "Post-prune disk diagnostics before AppImage free-space guard:"

df_output="$(df -Pk "$workspace")" || {
  echo "unable to determine pre-AppImage free disk space" >&2
  exit 1
}
available_kb="$(printf '%s\n' "$df_output" | awk 'NR == 2 { print $4 }')"
[[ "$available_kb" =~ ^[0-9]+$ ]] || {
  echo "unable to determine pre-AppImage free disk space: non-numeric available KiB '$available_kb'" >&2
  exit 1
}

echo "Pre-AppImage free disk space: ${available_kb} KiB; required minimum: ${minimum_free_kb} KiB (4 GiB)"
if (( available_kb < minimum_free_kb )); then
  echo "pre-AppImage free disk space is below the required 4 GiB guard: ${available_kb} KiB available" >&2
  exit 1
fi

echo "Linux AppImage disk preparation passed; preserved executable sha256: $desktop_sha_after"
