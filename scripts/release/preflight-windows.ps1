$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot "windows-native.ps1")
. (Join-Path $PSScriptRoot "windows-msvc-toolchain.ps1")
. (Join-Path $PSScriptRoot "windows-ffmpeg-msys-environment.ps1")

if (-not $IsWindows) { throw "M8 Windows release requires a Windows runner" }
if ($env:PROCESSOR_ARCHITECTURE -notin @('AMD64', 'x86_64')) {
    throw "M8 Windows release requires an x86_64 Windows runner"
}

foreach ($tool in @('cargo.exe','rustc.exe','node.exe','pnpm.cmd','git.exe')) {
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
$systemCurl = Join-Path $env:SystemRoot 'System32\curl.exe'
if (-not (Test-Path -LiteralPath $systemCurl -PathType Leaf)) { throw "required Windows system curl is unavailable: $systemCurl" }

$bash = 'C:\msys64\usr\bin\bash.exe'
$tar = 'C:\msys64\usr\bin\tar.exe'
$xz = 'C:\msys64\usr\bin\xz.exe'
$cygpath = 'C:\msys64\usr\bin\cygpath.exe'
foreach ($tool in @($bash, $tar, $xz, $cygpath)) {
    if (-not (Test-Path -LiteralPath $tool -PathType Leaf)) { throw "required MSYS2 release tool is unavailable: $tool" }
}
$msvc = Resolve-NianMsvcToolchain
$msysEnvironment = New-NianFfmpegMsysEnvironment -MsvcBinWindows $msvc.MsvcBin -Cygpath $cygpath
& (Join-Path $PSScriptRoot 'test-ffmpeg-msys-escape.ps1') `
    -VsInstall $msvc.VsInstall `
    -ControlledPath $msysEnvironment.PathText `
    -ExpectedClWindows $msvc.ClPath `
    -ExpectedLibWindows $msvc.LibPath `
    -ExpectedLinkWindows $msvc.LinkPath
Invoke-NianNative { & $bash --noprofile --norc -lc 'set -Eeuo pipefail; export PATH=/usr/bin; test -x /usr/bin/tar; test -x /usr/bin/xz' } 'deterministic MSYS2 extraction tools'

$sdkRoot = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
$signtool = Get-ChildItem $sdkRoot -Recurse -File -Filter signtool.exe -ErrorAction SilentlyContinue |
    Where-Object FullName -Match '[\\/]x64[\\/]signtool\.exe$' |
    Sort-Object FullName -Descending |
    Select-Object -First 1
if (-not $signtool) { throw "Windows SDK signtool.exe is unavailable" }

Write-Host "Windows release preflight passed: $rust; $node; pnpm $pnpm; $($msvc.VsInstall)"
