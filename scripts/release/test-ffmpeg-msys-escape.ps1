param(
    [string]$MsvcBinUnix = '',
    [string]$ExpectedClWindows = '',
    [string]$ExpectedLibWindows = '',
    [string]$ExpectedLinkWindows = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'windows-native.ps1')

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

function Import-ProbeVsDevEnvironment {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere -PathType Leaf)) { throw 'vswhere.exe is unavailable for the FFmpeg MSYS probe' }
    $install = (Invoke-NianNative { & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath } | Out-String).Trim()
    if (-not $install) { throw 'Visual Studio C++ build tools are unavailable for the FFmpeg MSYS probe' }
    $vsdev = Join-Path $install 'Common7\Tools\VsDevCmd.bat'
    if (-not (Test-Path $vsdev -PathType Leaf)) { throw 'VsDevCmd.bat is unavailable for the FFmpeg MSYS probe' }
    $lines = Invoke-NianNative { cmd.exe /d /s /c "`"$vsdev`" -arch=amd64 -host_arch=amd64 >nul && set" }
    foreach ($line in $lines) {
        $split = $line.IndexOf('=')
        if ($split -gt 0) {
            [Environment]::SetEnvironmentVariable($line.Substring(0, $split), $line.Substring($split + 1), 'Process')
        }
    }
    return $install
}

$provided = @(@($MsvcBinUnix, $ExpectedClWindows, $ExpectedLibWindows, $ExpectedLinkWindows) | Where-Object { $_ })
if ($provided.Count -ne 0 -and $provided.Count -ne 4) {
    throw 'FFmpeg MSYS probe requires either all MSVC authority arguments or none of them'
}

$vsInstall = $null
if ($provided.Count -eq 0) {
    $vsInstall = Import-ProbeVsDevEnvironment
    $ExpectedClWindows = [IO.Path]::GetFullPath((Get-Command cl.exe -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source)
    $ExpectedLibWindows = [IO.Path]::GetFullPath((Get-Command lib.exe -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source)
    $ExpectedLinkWindows = [IO.Path]::GetFullPath((Get-Command link.exe -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source)
}

$msvcBinWindows = Split-Path -Parent $ExpectedClWindows
foreach ($path in @($ExpectedLibWindows, $ExpectedLinkWindows)) {
    if ((Split-Path -Parent $path) -ne $msvcBinWindows) {
        throw 'FFmpeg MSYS probe resolved split MSVC tool directories'
    }
}
if ($vsInstall) {
    $vcToolsRoot = [IO.Path]::GetFullPath((Join-Path $vsInstall 'VC\Tools\MSVC'))
    $vcToolsPrefix = $vcToolsRoot.TrimEnd('\') + '\'
    if (-not $msvcBinWindows.StartsWith($vcToolsPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "FFmpeg MSYS probe did not resolve the selected Visual Studio VC toolchain: $msvcBinWindows"
    }
    if ($msvcBinWindows -notmatch '(?i)[\/]bin[\/]Hostx64[\/]x64$') {
        throw "FFmpeg MSYS probe did not resolve the expected amd64 Hostx64/x64 directory: $msvcBinWindows"
    }
}

$expectedMsvcBinUnix = Convert-ToMsysPath $msvcBinWindows
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
$probe = $probe.Replace('__CL__', (Convert-ToBashSingleQuoted $ExpectedClWindows))
$probe = $probe.Replace('__LIB__', (Convert-ToBashSingleQuoted $ExpectedLibWindows))
$probe = $probe.Replace('__LINK__', (Convert-ToBashSingleQuoted $ExpectedLinkWindows))

Invoke-NianNative { & $Bash --noprofile --norc -lc $probe } 'FFmpeg MSYS build-tool, MSVC precedence, and AWK escaping probe'
