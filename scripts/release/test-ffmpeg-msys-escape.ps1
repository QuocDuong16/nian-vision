$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'windows-native.ps1')

if (-not $IsWindows) { throw 'FFmpeg MSYS build-tool probe requires Windows' }

$Bash = 'C:\msys64\usr\bin\bash.exe'
if (-not (Test-Path $Bash -PathType Leaf)) { throw "required deterministic MSYS2 Bash is unavailable: $Bash" }

$probe = @'
set -Eeuo pipefail
export PATH="/usr/bin:$PATH"
hash -r

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

Invoke-NianNative { & $Bash --noprofile --norc -lc $probe } 'FFmpeg MSYS build-tool and AWK escaping probe'
