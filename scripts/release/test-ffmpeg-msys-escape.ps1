param(
    [string]$VsInstall = '',
    [string]$MsvcBinUnix = '',
    [string]$ExpectedClWindows = '',
    [string]$ExpectedLibWindows = '',
    [string]$ExpectedLinkWindows = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'windows-native.ps1')
. (Join-Path $PSScriptRoot 'windows-msvc-toolchain.ps1')

if (-not $IsWindows) { throw 'FFmpeg MSYS build-tool probe requires Windows' }

$Bash = 'C:\msys64\usr\bin\bash.exe'
if (-not (Test-Path $Bash -PathType Leaf)) { throw "required deterministic MSYS2 Bash is unavailable: $Bash" }

function Convert-ToBashSingleQuoted([string]$Value) {
    return "'" + $Value.Replace("'", "'\''") + "'"
}

function Convert-ToMsysPath([string]$Path) {
    $escaped = $Path.Replace("'", "'\''")
    return (Invoke-NianNative { & $Bash --noprofile --norc -lc "/usr/bin/cygpath -u '$escaped'" } 'probe cygpath path conversion' | Out-String).Trim()
}

$providedAuthority = @(@($VsInstall, $ExpectedClWindows, $ExpectedLibWindows, $ExpectedLinkWindows) | Where-Object { $_ })
if ($providedAuthority.Count -ne 0 -and $providedAuthority.Count -ne 4) {
    throw 'FFmpeg MSYS probe requires either all MSVC authority arguments or none of them'
}

if ($providedAuthority.Count -eq 0) {
    $Msvc = Resolve-NianMsvcToolchain
}
else {
    $Msvc = Assert-NianMsvcToolAuthority `
        -VsInstall $VsInstall `
        -ClPath $ExpectedClWindows `
        -LibPath $ExpectedLibWindows `
        -LinkPath $ExpectedLinkWindows
}

$expectedMsvcBinUnix = Convert-ToMsysPath $Msvc.MsvcBin
if (-not $MsvcBinUnix) { $MsvcBinUnix = $expectedMsvcBinUnix }
if ($MsvcBinUnix -ne $expectedMsvcBinUnix) {
    throw "FFmpeg MSYS probe MSVC path authority drifted: expected $expectedMsvcBinUnix, received $MsvcBinUnix"
}

$probe = @'
set -Eeuo pipefail
export PATH=__MSVC_BIN__:/usr/bin:"$PATH"
hash -r

assert_msvc_command() {
  name="$1"
  expected="$2"
  resolved="$(command -v "$name")"
  if [[ -z "$resolved" ]]; then
    printf 'required MSVC command is not visible inside MSYS: %s\n' "$name" >&2
    exit 1
  fi
  actual_windows="$(/usr/bin/cygpath -aw "$resolved")"
  actual_compare="${actual_windows,,}"
  expected_compare="${expected,,}"
  actual_compare="${actual_compare%.exe}"
  expected_compare="${expected_compare%.exe}"
  printf 'FFmpeg MSVC command %s: %s -> %s\n' "$name" "$resolved" "$actual_windows"
  if [[ "$actual_compare" != "$expected_compare" ]]; then
    printf 'expected MSVC command %s at %s, resolved %s\n' "$name" "$expected" "$actual_windows" >&2
    exit 1
  fi
}

assert_msvc_command cl.exe __CL__
assert_msvc_command cl __CL__
assert_msvc_command lib.exe __LIB__
assert_msvc_command link.exe __LINK__
assert_msvc_command link __LINK__

for pair in \
  make:/usr/bin/make \
  awk:/usr/bin/awk \
  sed:/usr/bin/sed \
  grep:/usr/bin/grep \
  cygpath:/usr/bin/cygpath
do
  name="${pair%%:*}"
  expected="${pair#*:}"
  actual="$(command -v "$name")"
  printf 'FFmpeg MSYS tool %s: %s\n' "$name" "$actual"
  if [[ "$actual" != "$expected" ]]; then
    printf 'expected deterministic MSYS2 tool %s at %s, resolved %s\n' "$name" "$expected" "$actual" >&2
    exit 1
  fi
done

printf 'FFmpeg MSYS make version: '
/usr/bin/make --version | /usr/bin/sed -n '1p'

awk_version="$(/usr/bin/awk --version 2>&1 | /usr/bin/sed -n '1p' || true)"
if [[ -z "$awk_version" ]]; then
  awk_version="$(/usr/bin/awk -W version 2>&1 | /usr/bin/sed -n '1p' || true)"
fi
if [[ -z "$awk_version" ]]; then
  printf 'unable to determine /usr/bin/awk version\n' >&2
  exit 1
fi
printf 'FFmpeg MSYS awk version: %s\n' "$awk_version"

printf 'FFmpeg MSYS sed version: '
/usr/bin/sed --version | /usr/bin/sed -n '1p'
printf 'FFmpeg MSYS grep version: '
/usr/bin/grep --version | /usr/bin/sed -n '1p'

actual="$(printf '%s\n' 'C:\foo\bar.h' | /usr/bin/awk '{ gsub(/\\/, "/"); print }')"
if [[ "$actual" != 'C:/foo/bar.h' ]]; then
  printf 'FFmpeg MSYS AWK backslash conversion failed: %s\n' "$actual" >&2
  exit 1
fi
printf 'FFmpeg MSYS AWK backslash probe: %s\n' "$actual"
'@

$probe = $probe.Replace('__MSVC_BIN__', (Convert-ToBashSingleQuoted $MsvcBinUnix))
$probe = $probe.Replace('__CL__', (Convert-ToBashSingleQuoted $Msvc.ClPath))
$probe = $probe.Replace('__LIB__', (Convert-ToBashSingleQuoted $Msvc.LibPath))
$probe = $probe.Replace('__LINK__', (Convert-ToBashSingleQuoted $Msvc.LinkPath))

Invoke-NianNative { & $Bash --noprofile --norc -lc $probe } 'FFmpeg MSYS build-tool, MSVC precedence, and AWK escaping probe'
