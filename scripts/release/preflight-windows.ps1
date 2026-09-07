$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot "windows-native.ps1")

if (-not $IsWindows) { throw "M8 Windows release requires a Windows runner" }
if ($env:PROCESSOR_ARCHITECTURE -notin @('AMD64', 'x86_64')) {
    throw "M8 Windows release requires an x86_64 Windows runner"
}

foreach ($tool in @('cargo.exe','rustc.exe','node.exe','pnpm.cmd','git.exe','curl.exe')) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "required Windows release tool is unavailable: $tool"
    }
}

$rust = (Invoke-NianNative { rustc.exe --version } | Out-String).Trim()
$node = (Invoke-NianNative { node.exe --version } | Out-String).Trim()
$pnpm = (Invoke-NianNative { pnpm.cmd --version } | Out-String).Trim()
if (-not $rust.StartsWith('rustc 1.98.0 ')) { throw "release Rust toolchain mismatch: $rust" }
if ($node -ne 'v26.7.0') { throw "release Node.js mismatch: $node" }
if ($pnpm -ne '11.22.0') { throw "release pnpm mismatch: $pnpm" }

$bash = 'C:\msys64\usr\bin\bash.exe'
$tar = 'C:\msys64\usr\bin\tar.exe'
$xz = 'C:\msys64\usr\bin\xz.exe'
foreach ($tool in @($bash, $tar, $xz)) {
    if (-not (Test-Path $tool -PathType Leaf)) { throw "required MSYS2 release tool is unavailable: $tool" }
}
& (Join-Path $PSScriptRoot 'test-ffmpeg-msys-escape.ps1')
Invoke-NianNative { & $bash --noprofile --norc -lc 'test -x /usr/bin/tar; test -x /usr/bin/xz' } 'deterministic MSYS2 extraction tools'

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) { throw "vswhere.exe is unavailable" }
$install = (Invoke-NianNative { & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath } | Out-String).Trim()
if (-not $install) { throw "Visual Studio 2022 C++ tools are unavailable" }
$vsdev = Join-Path $install 'Common7\Tools\VsDevCmd.bat'
if (-not (Test-Path $vsdev)) { throw "VsDevCmd.bat is unavailable" }

$sdkRoot = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
$signtool = Get-ChildItem $sdkRoot -Recurse -File -Filter signtool.exe -ErrorAction SilentlyContinue |
    Where-Object FullName -Match '[\\/]x64[\\/]signtool\.exe$' |
    Sort-Object FullName -Descending |
    Select-Object -First 1
if (-not $signtool) { throw "Windows SDK signtool.exe is unavailable" }

Write-Host "Windows release preflight passed: $rust; $node; pnpm $pnpm; $($install.Trim())"
