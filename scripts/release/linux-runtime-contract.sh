#!/usr/bin/env bash

require_exact_runpath() {
  local object="$1"
  local expected="$2"
  local dynamic runpath_count rpath_count actual_runpath actual_rpath

  if ! dynamic="$(readelf -d "$object" 2>&1)"; then
    printf '%s\n' "$dynamic" >&2
    echo "failed to inspect ELF dynamic section: object=$object" >&2
    return 1
  fi

  runpath_count="$(awk '/\(RUNPATH\)/ { count++ } END { print count + 0 }' <<<"$dynamic")"
  rpath_count="$(awk '/\(RPATH\)/ { count++ } END { print count + 0 }' <<<"$dynamic")"
  actual_runpath="$(awk '
    /\(RUNPATH\)/ {
      value = $0
      sub(/^.*\[/, "", value)
      sub(/\].*$/, "", value)
      output = output == "" ? value : output " | " value
    }
    END { print output }
  ' <<<"$dynamic")"
  actual_rpath="$(awk '
    /\(RPATH\)/ {
      value = $0
      sub(/^.*\[/, "", value)
      sub(/\].*$/, "", value)
      output = output == "" ? value : output " | " value
    }
    END { print output }
  ' <<<"$dynamic")"

  if [[ "$runpath_count" -ne 1 || "$rpath_count" -ne 0 || "$actual_runpath" != "$expected" ]]; then
    echo "ELF RUNPATH contract violation: object=$object expected RUNPATH=$expected actual RUNPATH=${actual_runpath:-<missing>} actual RPATH=${actual_rpath:-<missing>}" >&2
    return 1
  fi
}

clean_ldd() {
  local object="$1"
  env -u LD_LIBRARY_PATH -u NIAN_FFMPEG_LIB_DIR ldd "$object"
}

require_private_ffmpeg_closure() {
  local object="$1"
  local private_dir="$2"
  local closure name arrow resolved expected private_real resolved_real expected_real

  private_real="$(readlink -f "$private_dir" 2>/dev/null || true)"
  if [[ -z "$private_real" || ! -d "$private_real" ]]; then
    echo "private FFmpeg runtime directory is unavailable: object=$object private root=$private_dir canonical private root=${private_real:-<missing>}" >&2
    return 1
  fi

  if ! closure="$(clean_ldd "$object" 2>&1)"; then
    printf '%s\n' "$closure" >&2
    echo "ldd failed for ELF dependency closure: object=$object" >&2
    return 1
  fi
  printf '%s\n' "$closure"

  if grep -q 'not found' <<<"$closure"; then
    echo "runtime dependency closure is incomplete for $object" >&2
    return 1
  fi

  while read -r name arrow resolved _; do
    case "$name" in
      libavdevice.so*|libavfilter.so*|libpostproc.so*|libswresample.so*|libswscale.so*)
        echo "unexpected disabled FFmpeg runtime dependency for $object: $name" >&2
        return 1
        ;;
    esac

    [[ "$name" == libav*.so* ]] || continue

    case "$name" in
      libavformat.so.62|libavcodec.so.62|libavutil.so.60)
        ;;
      *)
        echo "unexpected FFmpeg runtime dependency for $object: $name" >&2
        return 1
        ;;
    esac

    if [[ "$arrow" != "=>" || -z "$resolved" || "$resolved" == "not" ]]; then
      echo "FFmpeg runtime dependency is unresolved for $object: $name => ${resolved:-<missing>}" >&2
      return 1
    fi

    expected="$private_dir/$name"
    resolved_real="$(readlink -f "$resolved" 2>/dev/null || true)"
    expected_real="$(readlink -f "$expected" 2>/dev/null || true)"
    if [[ -z "$expected_real" || "$expected_real" != "$private_real/"* ]]; then
      echo "expected FFmpeg dependency escaped the private runtime: object=$object dependency=$name private root=$private_real expected=$expected canonical expected=${expected_real:-<missing>}" >&2
      return 1
    fi
    if [[ -z "$resolved_real" || "$resolved_real" != "$private_real/"* ]]; then
      echo "resolved FFmpeg dependency escaped the private runtime: object=$object dependency=$name private root=$private_real resolved=${resolved:-<unresolved>} canonical resolved=${resolved_real:-<missing>}" >&2
      return 1
    fi
    if [[ "$resolved_real" != "$expected_real" ]]; then
      echo "FFmpeg dependency did not resolve to the expected private file: object=$object dependency=$name private root=$private_real resolved=$resolved_real expected=$expected_real" >&2
      return 1
    fi
  done <<<"$closure"
}
