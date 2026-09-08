param(
    [string]$VsInstall = '',
    [string]$ControlledPath = '',
    [string]$ExpectedClWindows = '',
    [string]$ExpectedLibWindows = '',
    [string]$ExpectedLinkWindows = '',
    [string]$ExpectedDumpbinWindows = '',
    [string]$ExpectedWindowsSdkBin = '',
    [string]$ExpectedRcWindows = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'windows-native.ps1')
. (Join-Path $PSScriptRoot 'windows-msvc-toolchain.ps1')
. (Join-Path $PSScriptRoot 'windows-ffmpeg-msys-environment.ps1')
. (Join-Path $PSScriptRoot 'windows-bash-script.ps1')

if (-not $IsWindows) { throw 'FFmpeg MSYS build-tool probe requires Windows' }

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$Contract = Get-Content (Join-Path $RepoRoot 'scripts/release/ffmpeg-windows-contract.json') -Raw | ConvertFrom-Json
$Bash = 'C:\msys64\usr\bin\bash.exe'
if (-not (Test-Path -LiteralPath $Bash -PathType Leaf)) { throw "required deterministic MSYS2 Bash is unavailable: $Bash" }

$providedAuthority = @(@($VsInstall, $ExpectedClWindows, $ExpectedLibWindows, $ExpectedLinkWindows, $ExpectedDumpbinWindows, $ExpectedWindowsSdkBin, $ExpectedRcWindows) | Where-Object { $_ })
if ($providedAuthority.Count -ne 0 -and $providedAuthority.Count -ne 7) {
    throw 'FFmpeg MSYS probe requires either all Windows tool authority arguments or none of them'
}

if ($providedAuthority.Count -eq 0) {
    $Msvc = Resolve-NianMsvcToolchain
}
else {
    $Msvc = Assert-NianFfmpegWindowsToolAuthority `
        -VsInstall $VsInstall `
        -ClPath $ExpectedClWindows `
        -LibPath $ExpectedLibWindows `
        -LinkPath $ExpectedLinkWindows `
        -DumpbinPath $ExpectedDumpbinWindows `
        -WindowsSdkBin $ExpectedWindowsSdkBin `
        -RcPath $ExpectedRcWindows
}

$MsysEnvironment = New-NianFfmpegMsysEnvironment -MsvcBinWindows $Msvc.MsvcBin
if ($ControlledPath -and $ControlledPath -ne $MsysEnvironment.PathText) {
    throw 'FFmpeg MSYS probe received a controlled PATH that differs from the shared environment authority'
}
$ControlledPath = $MsysEnvironment.PathText

Write-Host '---- FFmpeg Windows toolchain resolution ----'
Write-Host "controlled PATH: $ControlledPath"
if ($MsysEnvironment.Removed.Count -eq 0) {
    Write-Host 'forbidden inherited PATH entries removed: <none>'
}
else {
    foreach ($entry in $MsysEnvironment.Removed) {
        Write-Host "forbidden inherited PATH entry removed: $entry"
    }
}

$msysPairs = @($Contract.msysBuildTools.PSObject.Properties | ForEach-Object {
    "$($_.Name) $([string]$_.Value)"
}) -join "`n"
$forbiddenRoots = @((Get-NianForbiddenMsysToolRoots) | ForEach-Object { ConvertTo-NianBashSingleQuoted $_ }) -join ' '

$probe = @'
set -Eeuo pipefail
export PATH=__CONTROLLED_PATH__
hash -r
mismatch_count=0
failures=()

record_failure() {
  mismatch_count=$((mismatch_count + 1))
  failures+=("$1")
}

canonical_windows() {
  local path="$1"
  if [[ -z "$path" ]]; then
    printf '<missing>'
    return
  fi
  /usr/bin/cygpath -aw "$path" 2>/dev/null || printf '<cygpath-failed>'
}

first_version_line() {
  local name="$1"
  local expected="$2"
  case "$name" in
    bash|make|awk|sed|grep|tar|xz)
      "$expected" --version 2>&1 | /usr/bin/head -n 1 || true
      ;;
    *)
      printf '<not-requested>'
      ;;
  esac
}

report_msvc() {
  local logical="$1"
  local expected="$2"
  local resolved actual_windows actual_compare expected_compare status
  resolved="$(command -v "$logical" 2>/dev/null || true)"
  actual_windows="$(canonical_windows "$resolved")"
  actual_compare="${actual_windows,,}"
  expected_compare="${expected,,}"
  actual_compare="${actual_compare%.exe}"
  expected_compare="${expected_compare%.exe}"
  status=PASS
  if [[ -z "$resolved" || "$actual_compare" != "$expected_compare" ]]; then
    status=FAIL
    record_failure "MSVC $logical expected $expected, resolved $actual_windows"
  fi
  if [[ -n "$resolved" ]] && is_forbidden_resolution "$resolved"; then
    status=FAIL
    record_failure "MSVC $logical resolved inside forbidden toolchain: $resolved ($actual_windows)"
  fi
  printf '%-10s -> %-85s | %-85s | %-15s | %s\n' "$logical" "${resolved:-<missing>}" "$actual_windows" '<MSVC>' "$status"
}

report_msys() {
  local logical="$1"
  local expected="$2"
  local resolved actual_windows version status
  resolved="$(command -v "$logical" 2>/dev/null || true)"
  actual_windows="$(canonical_windows "$resolved")"
  version="$(first_version_line "$logical" "$expected")"
  status=PASS
  if [[ "$resolved" != "$expected" ]]; then
    status=FAIL
    record_failure "MSYS $logical expected $expected, resolved ${resolved:-<missing>} ($actual_windows)"
  fi
  printf '%-10s -> %-85s | %-85s | %-40s | %s\n' "$logical" "${resolved:-<missing>}" "$actual_windows" "$version" "$status"
}

report_windows_sdk() {
  local logical="$1"
  local expected="$2"
  local resolved actual_windows actual_compare expected_compare status
  resolved="$(command -v "$logical" 2>/dev/null || true)"
  actual_windows="$(canonical_windows "$resolved")"
  actual_compare="${actual_windows,,}"
  expected_compare="${expected,,}"
  status=PASS
  if [[ -z "$resolved" || "$actual_compare" != "$expected_compare" ]]; then
    status=FAIL
    record_failure "Windows SDK $logical expected $expected, resolved $actual_windows"
  fi
  if [[ -n "$resolved" ]] && is_forbidden_resolution "$resolved"; then
    status=FAIL
    record_failure "Windows SDK $logical resolved inside forbidden toolchain: $resolved ($actual_windows)"
  fi
  printf '%-10s -> %-85s | %-85s | %-40s | %s\n' "$logical" "${resolved:-<missing>}" "$actual_windows" '<Windows SDK>' "$status"
}

is_forbidden_resolution() {
  local resolved="$1"
  local root
  for root in __FORBIDDEN_ROOTS__; do
    case "${resolved,,}" in
      "${root,,}"|"${root,,}"/*) return 0 ;;
    esac
  done
  return 1
}

report_unexpected_toolchain_command() {
  local logical="$1"
  local resolved actual_windows status
  resolved="$(command -v "$logical" 2>/dev/null || true)"
  actual_windows="$(canonical_windows "$resolved")"
  status=PASS
  if [[ -n "$resolved" ]] && is_forbidden_resolution "$resolved"; then
    status=FAIL
    record_failure "unexpected $logical resolves inside forbidden toolchain: $resolved ($actual_windows)"
  fi
  printf '%-10s -> %-85s | %-85s | %-40s | %s\n' "$logical" "${resolved:-<absent>}" "$actual_windows" '<forbidden-root-check>' "$status"
}

report_msvc cl.exe __CL__
report_msvc cl __CL__
report_msvc lib.exe __LIB__
report_msvc link.exe __LINK__
report_msvc link __LINK__
report_msvc dumpbin.exe __DUMPBIN__
report_windows_sdk rc.exe __RC__

while IFS=' ' read -r logical expected; do
  [[ -n "$logical" ]] || continue
  report_msys "$logical" "$expected"
done <<'NIAN_MSYS_TOOLS'
__MSYS_PAIRS__
NIAN_MSYS_TOOLS

for name in gcc cc ld ar; do
  report_unexpected_toolchain_command "$name"
done

make_resolved="$(command -v make 2>/dev/null || true)"
if [[ "$make_resolved" != '/usr/bin/make' ]]; then
  record_failure "bare make does not resolve to /usr/bin/make: ${make_resolved:-<missing>}"
fi
if [[ -x /usr/bin/make && -x /usr/bin/head ]]; then
  make_version="$(/usr/bin/make --version 2>&1 | /usr/bin/head -n 1 || true)"
  if [[ "$make_version" != '__MAKE_VERSION_LINE__' ]]; then
    record_failure "GNU make version mismatch: $make_version"
  fi
fi

if [[ -x /usr/bin/awk ]]; then
  awk_actual="$(printf '%s\n' 'C:\foo\bar.h' | /usr/bin/awk '{ gsub(/\\/, "/"); print }' || true)"
  if [[ "$awk_actual" != 'C:/foo/bar.h' ]]; then
    record_failure "AWK backslash conversion failed: $awk_actual"
  fi
  printf 'FFmpeg MSYS AWK backslash probe: %s\n' "$awk_actual"
fi


if [[ -x /usr/bin/sort && -x /usr/bin/uniq ]]; then
  uniq_actual=''
  if ! uniq_actual="$(printf '%s\n' a a b | /usr/bin/sort | /usr/bin/uniq)"; then
    record_failure 'uniq behavioral probe pipeline exited non-zero'
  elif [[ "$uniq_actual" != $'a\nb' ]]; then
    record_failure "uniq behavioral probe produced unexpected output: $uniq_actual"
  fi
  uniq_report="$(printf '%s' "$uniq_actual" | /usr/bin/tr '\n' ',')"
  printf 'FFmpeg MSYS uniq behavioral probe: %s\n' "$uniq_report"
fi

printf '%s\n' '--------------------------------------------'
if (( mismatch_count > 0 )); then
  printf 'FFmpeg Windows toolchain resolution failures: %d\n' "$mismatch_count" >&2
  limit=0
  for failure in "${failures[@]}"; do
    printf '%s\n' "$failure" >&2
    limit=$((limit + 1))
    if (( limit >= 40 )); then break; fi
  done
  exit 1
fi

behavior_probe_dir="$(/usr/bin/mktemp -d -t nian-ffmpeg-msys-probe.XXXXXX)"
cleanup() { /usr/bin/rm -rf "$behavior_probe_dir"; }
trap cleanup EXIT

cmp_left="$behavior_probe_dir/cmp-left.txt"
cmp_right="$behavior_probe_dir/cmp-right.txt"
printf '%s\n' 'nian-cmp-probe' > "$cmp_left"
printf '%s\n' 'nian-cmp-probe' > "$cmp_right"
if ! /usr/bin/cmp -s "$cmp_left" "$cmp_right"; then
  record_failure 'cmp behavioral probe rejected identical files'
fi
printf '%s\n' 'different' >> "$cmp_right"
set +e
/usr/bin/cmp -s "$cmp_left" "$cmp_right"
cmp_different_status=$?
set -e
if (( cmp_different_status != 1 )); then
  record_failure "cmp behavioral probe expected exit 1 for different files, got $cmp_different_status"
fi
printf 'FFmpeg MSYS cmp behavioral probe: identical=0 different=%d\n' "$cmp_different_status"

install_source="$behavior_probe_dir/install-source.txt"
install_destination="$behavior_probe_dir/install-destination.txt"
printf '%s\n' 'nian-install-probe' > "$install_source"
if ! /usr/bin/install -m 644 "$install_source" "$install_destination"; then
  record_failure 'install behavioral probe exited non-zero'
elif [[ ! -f "$install_destination" ]]; then
  record_failure 'install behavioral probe did not create the destination file'
else
  install_contents="$(/usr/bin/cat "$install_destination")"
  if [[ "$install_contents" != 'nian-install-probe' ]]; then
    record_failure "install behavioral probe copied unexpected contents: $install_contents"
  fi
fi
printf 'FFmpeg MSYS install behavioral probe: %s\n' "${install_contents:-<missing>}"

/usr/bin/cat > "$behavior_probe_dir/Makefile" <<'NIAN_MAKEFILE'
.RECIPEPREFIX := >
.PHONY: nian-make-probe
nian-make-probe:
>@printf '%s\n' 'C:\foo\bar.h' | /usr/bin/awk '{ gsub(/\\/, "/"); print }'
NIAN_MAKEFILE
make_probe_output="$(/usr/bin/make --no-print-directory -f "$behavior_probe_dir/Makefile" nian-make-probe 2>&1)" || {
  record_failure "GNU make behavioral probe exited non-zero: $make_probe_output"
  make_probe_output='<failed>'
}
if [[ "$make_probe_output" != 'C:/foo/bar.h' ]]; then
  record_failure "GNU make/AWK recipe expansion produced unexpected output: $make_probe_output"
fi
case "${make_probe_output,,}" in
  *mingw64*|*mingw32*|*ucrt64*|*clang64*|*clangarm64*)
    record_failure "GNU make behavioral probe exposed a forbidden toolchain path: $make_probe_output"
    ;;
esac
printf 'FFmpeg MSYS GNU make behavioral probe: %s\n' "$make_probe_output"
if (( mismatch_count > 0 )); then
  printf 'FFmpeg MSYS behavioral failures: %d\n' "$mismatch_count" >&2
  for failure in "${failures[@]}"; do printf '%s\n' "$failure" >&2; done
  exit 1
fi
printf 'FFmpeg Windows toolchain resolution: PASS\n'
'@

$probe = $probe.Replace('__CONTROLLED_PATH__', (ConvertTo-NianBashSingleQuoted $ControlledPath))
$probe = $probe.Replace('__FORBIDDEN_ROOTS__', $forbiddenRoots)
$probe = $probe.Replace('__MSYS_PAIRS__', $msysPairs)
$probe = $probe.Replace('__CL__', (ConvertTo-NianBashSingleQuoted $Msvc.ClPath))
$probe = $probe.Replace('__LIB__', (ConvertTo-NianBashSingleQuoted $Msvc.LibPath))
$probe = $probe.Replace('__LINK__', (ConvertTo-NianBashSingleQuoted $Msvc.LinkPath))
$probe = $probe.Replace('__DUMPBIN__', (ConvertTo-NianBashSingleQuoted $Msvc.DumpbinPath))
$probe = $probe.Replace('__RC__', (ConvertTo-NianBashSingleQuoted $Msvc.RcPath))
$probe = $probe.Replace('__MAKE_VERSION_LINE__', [string]$Contract.provisionedMsysPackages.make.versionLine)

Invoke-NianBashScript `
    -Bash $Bash `
    -Script $probe `
    -FileName 'nian-ffmpeg-msys-toolchain-probe.sh' `
    -Label 'aggregate FFmpeg MSYS/MSVC toolchain and GNU make behavioral probe' | Out-Host
