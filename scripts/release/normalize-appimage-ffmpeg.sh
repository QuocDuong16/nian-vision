#!/usr/bin/env bash
set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
default_bundle_dir="$repo_root/target/release/bundle/appimage"
ffmpeg_sonames=(libavformat.so.62 libavcodec.so.62 libavutil.so.60)
normalize_work_dir=""

cleanup_normalize_work_dir() {
  if [[ -n "${normalize_work_dir:-}" ]]; then
    rm -rf -- "$normalize_work_dir"
    normalize_work_dir=""
  fi
}

normalize_legacy_ffmpeg_layout() {
  local appdir="$1"
  local private_lib_dir="$appdir/usr/lib/nian-vision"

  [[ -d "$private_lib_dir" && ! -L "$private_lib_dir" ]] || {
    echo "AppImage private FFmpeg runtime directory is missing or unsafe: $private_lib_dir" >&2
    return 1
  }

  local name private_object private_real legacy_object legacy_real
  for name in "${ffmpeg_sonames[@]}"; do
    private_object="$private_lib_dir/$name"
    [[ -f "$private_object" && ! -L "$private_object" ]] || {
      echo "AppImage private FFmpeg runtime entry is missing, non-regular, or symlinked: $private_object" >&2
      return 1
    }

    legacy_object="$appdir/usr/lib/$name"
    if [[ -e "$legacy_object" || -L "$legacy_object" ]]; then
      private_real="$(readlink -f "$private_object" 2>/dev/null || true)"
      legacy_real="$(readlink -f "$legacy_object" 2>/dev/null || true)"
      [[ -n "$private_real" && -f "$private_real" ]] || {
        echo "Unable to resolve private FFmpeg runtime entry: $private_object" >&2
        return 1
      }
      [[ -n "$legacy_real" && -f "$legacy_real" ]] || {
        echo "Legacy FFmpeg runtime entry is broken or non-regular: $legacy_object" >&2
        return 1
      }
      if ! cmp --silent "$private_real" "$legacy_real"; then
        echo "Refusing to delete non-identical legacy FFmpeg runtime entry: $legacy_object" >&2
        return 1
      fi
      rm -f -- "$legacy_object"
    fi

    if [[ -e "$legacy_object" || -L "$legacy_object" ]]; then
      echo "Failed to remove legacy FFmpeg runtime entry: $legacy_object" >&2
      return 1
    fi
  done
}

find_tauri_appimage_output_plugin() {
  local cache_dir="${TAURI_TOOLS_PATH:-$HOME/.cache/tauri}"
  local explicit="${NIAN_TAURI_APPIMAGE_OUTPUT_PLUGIN:-}"
  if [[ -n "$explicit" ]]; then
    [[ -f "$explicit" && ! -L "$explicit" && -x "$explicit" ]] || {
      echo "Configured Tauri AppImage output plugin is unavailable or unsafe: $explicit" >&2
      return 1
    }
    printf '%s\n' "$explicit"
    return 0
  fi

  mapfile -t plugins < <(find "$cache_dir" -maxdepth 1 -type f -name 'linuxdeploy-plugin-appimage*.AppImage' -print 2>/dev/null | sort)
  if [[ "${#plugins[@]}" -ne 1 ]]; then
    echo "Expected exactly one cached Tauri AppImage output plugin in $cache_dir, found ${#plugins[@]}" >&2
    return 1
  fi
  [[ -x "${plugins[0]}" ]] || {
    echo "Cached Tauri AppImage output plugin is not executable: ${plugins[0]}" >&2
    return 1
  }
  printf '%s\n' "${plugins[0]}"
}

main() {
  local bundle_dir="${1:-$default_bundle_dir}"
  [[ -d "$bundle_dir" && ! -L "$bundle_dir" ]] || {
    echo "AppImage bundle directory is missing or unsafe: $bundle_dir" >&2
    exit 1
  }
  bundle_dir="$(cd "$bundle_dir" && pwd -P)"

  mapfile -t images < <(find "$bundle_dir" -maxdepth 1 -type f -name '*.AppImage' -print | sort)
  if [[ "${#images[@]}" -ne 1 ]]; then
    echo "Expected exactly one AppImage in $bundle_dir, found ${#images[@]}" >&2
    exit 1
  fi
  local appimage="${images[0]}"
  [[ ! -L "$appimage" && -x "$appimage" ]] || {
    echo "AppImage is not a regular executable file: $appimage" >&2
    exit 1
  }

  local plugin
  plugin="$(find_tauri_appimage_output_plugin)"
  normalize_work_dir="$(mktemp -d)"
  trap cleanup_normalize_work_dir EXIT

  local runtime_offset
  runtime_offset="$("$appimage" --appimage-offset)"
  [[ "$runtime_offset" =~ ^[1-9][0-9]*$ ]] || {
    echo "Unable to determine AppImage runtime offset: $runtime_offset" >&2
    exit 1
  }
  dd if="$appimage" of="$normalize_work_dir/runtime" bs=1 count="$runtime_offset" status=none

  mkdir "$normalize_work_dir/image" "$normalize_work_dir/plugin"
  (cd "$normalize_work_dir/image" && "$appimage" --appimage-extract >/dev/null)
  local appdir="$normalize_work_dir/image/squashfs-root"
  normalize_legacy_ffmpeg_layout "$appdir"

  (cd "$normalize_work_dir/plugin" && "$plugin" --appimage-extract >/dev/null)
  local appimagetool="$normalize_work_dir/plugin/squashfs-root/usr/bin/appimagetool"
  [[ -f "$appimagetool" && ! -L "$appimagetool" && -x "$appimagetool" ]] || {
    echo "Tauri AppImage output plugin does not contain a usable appimagetool: $appimagetool" >&2
    exit 1
  }

  local repacked="$normalize_work_dir/repacked.AppImage"
  ARCH=x86_64 "$appimagetool" --runtime-file "$normalize_work_dir/runtime" "$appdir" "$repacked"
  [[ -f "$repacked" && ! -L "$repacked" && -x "$repacked" ]] || {
    echo "AppImage repack did not produce a regular executable: $repacked" >&2
    exit 1
  }
  mv -f -- "$repacked" "$appimage"
  echo "Normalized AppImage FFmpeg runtime layout: $appimage"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
