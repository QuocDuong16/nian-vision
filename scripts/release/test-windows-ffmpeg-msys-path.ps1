$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'windows-ffmpeg-msys-environment.ps1')

$msvc = '/c/Program Files/Microsoft Visual Studio/2022/Enterprise/VC/Tools/MSVC/14.44.35207/bin/HostX64/x64'
$input = @(
    '/c/mingw64/bin',
    '/c/Windows/System32',
    '/usr/bin',
    '/c/Program Files/nodejs',
    $msvc,
    '/c/Windows',
    '/c/Program Files/Git/cmd'
)
$result = Get-NianSanitizedMsysPathEntries -MsvcBinUnix $msvc -InheritedEntries $input

if ($result.Entries[0] -ne $msvc) { throw 'selected MSVC path is not first' }
if ($result.Entries[1] -ne '/usr/bin') { throw '/usr/bin is not second' }
if ($result.Entries -contains '/c/mingw64/bin') { throw 'MinGW64 path survived sanitization' }
foreach ($required in @('/c/Windows/System32', '/c/Program Files/nodejs', '/c/Windows', '/c/Program Files/Git/cmd')) {
    if ($result.Entries -notcontains $required) { throw "required inherited path was removed: $required" }
}

foreach ($forbidden in @(
    '/mingw64/bin',
    '/mingw32/bin',
    '/c/mingw32/bin',
    '/ucrt64/bin',
    '/clang64/bin',
    '/clang32/bin',
    '/clangarm64/bin',
    '/c/ucrt64/bin',
    '/c/clang64/bin',
    '/c/clangarm64/bin',
    'C:\msys64\mingw64\bin\',
    'C:\msys64\ucrt64\bin'
)) {
    if (-not (Test-NianForbiddenMsysPathEntry $forbidden)) {
        throw "forbidden FFmpeg toolchain path fixture was accepted: $forbidden"
    }
}

foreach ($allowed in @(
    '/c/Program Files/Microsoft Visual Studio/2022/Community/VC/Tools/MSVC/99.1/bin/HostX64/x64',
    '/c/Windows/System32',
    '/c/tools/not-mingw/bin',
    '/usr/bin'
)) {
    if (Test-NianForbiddenMsysPathEntry $allowed) {
        throw "allowed FFmpeg PATH fixture was rejected: $allowed"
    }
}

Write-Host 'FFmpeg MSYS PATH fixture PASS: selected MSVC first, /usr/bin second, forbidden toolchain roots removed'
