$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
$Config = Get-Content (Join-Path $RepoRoot "scripts/release/release-config.json") -Raw | ConvertFrom-Json
$Ffmpeg = if ($args.Count -gt 0) { [IO.Path]::GetFullPath($args[0]) } else { Join-Path $RepoRoot "dist/ffmpeg-windows-x86_64" }
$Stage = Join-Path $RepoRoot "dist/windows-x86_64"
$Runtime = Join-Path $Stage "runtime"
$Tauri = Join-Path $Stage "tauri"
$Target = $Config.windowsTarget
$Node = (Get-Command node.exe).Source
$WorkerSource = Join-Path $RepoRoot "target/$Target/release/nian-media-worker.exe"
$DesktopSource = Join-Path $RepoRoot "target/$Target/release/nian-desktop.exe"

function Import-VsDevEnvironment {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { throw "vswhere.exe is unavailable" }
    $install = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if (-not $install) { throw "Visual Studio C++ build tools are unavailable" }
    $vsdev = Join-Path $install "Common7\Tools\VsDevCmd.bat"
    $lines = & cmd.exe /d /s /c "`"$vsdev`" -arch=amd64 -host_arch=amd64 >nul && set"
    foreach ($line in $lines) {
        $split = $line.IndexOf('=')
        if ($split -gt 0) {
            [Environment]::SetEnvironmentVariable($line.Substring(0, $split), $line.Substring($split + 1), 'Process')
        }
    }
}

Import-VsDevEnvironment
if (-not (Get-Command dumpbin.exe -ErrorAction SilentlyContinue)) {
    throw "dumpbin.exe is unavailable after importing the Visual Studio environment"
}

function Get-Dependencies([string]$Path) {
    $lines = & dumpbin.exe /nologo /dependents $Path
    if ($LASTEXITCODE -ne 0) { throw "dumpbin failed for $Path" }
    $names = @()
    foreach ($line in $lines) {
        if ($line -match '^\s+([A-Za-z0-9_.-]+\.dll)\s*$') { $names += $Matches[1] }
    }
    return $names | Sort-Object -Unique
}

function Is-SystemDependency([string]$Name) {
    $upper = $Name.ToUpperInvariant()
    if ($upper -match '^(API-MS-WIN-|EXT-MS-WIN-)') { return $true }
    return Test-Path (Join-Path $env:SystemRoot "System32\$Name")
}

function Find-VcRuntime([string]$Name) {
    if (-not $env:VCToolsRedistDir -or -not (Test-Path $env:VCToolsRedistDir)) { return $null }
    $matches = Get-ChildItem $env:VCToolsRedistDir -Recurse -File -Filter $Name |
        Where-Object FullName -Match '[\\/]x64[\\/]' |
        Sort-Object FullName
    if ($matches.Count -eq 0) { return $null }
    return $matches[0].FullName
}

function Ensure-DependencyClosure([string[]]$Roots) {
    $queue = [Collections.Generic.Queue[string]]::new()
    foreach ($root in $Roots) { $queue.Enqueue($root) }
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    while ($queue.Count -gt 0) {
        $object = $queue.Dequeue()
        foreach ($dep in Get-Dependencies $object) {
            if (-not $seen.Add($dep)) { continue }
            $local = Join-Path $Runtime $dep
            if (Test-Path $local) {
                $queue.Enqueue($local)
                continue
            }
            if (Is-SystemDependency $dep) { continue }
            if ($dep -match '^(VCRUNTIME|MSVCP|CONCRT)[0-9A-Z_]*\.DLL$') {
                $vc = Find-VcRuntime $dep
                if (-not $vc) { throw "required Visual C++ runtime dependency is unavailable: $dep" }
                Copy-Item -Force $vc $local
                $queue.Enqueue($local)
                continue
            }
            throw "application-owned Windows dependency is missing from stage: $dep required by $object"
        }
    }
}

foreach ($path in @(
    $WorkerSource,
    (Join-Path $Ffmpeg "bin/avformat-62.dll"),
    (Join-Path $Ffmpeg "bin/avcodec-62.dll"),
    (Join-Path $Ffmpeg "bin/avutil-60.dll"),
    (Join-Path $Ffmpeg "FFMPEG_CONFIG.h"),
    (Join-Path $Ffmpeg "FFMPEG_BUILD_FLAGS.txt"),
    (Join-Path $Ffmpeg "FFMPEG-LGPL-2.1.txt"),
    (Join-Path $RepoRoot "THIRD_PARTY_NOTICES.txt")
)) {
    if (-not (Test-Path $path)) { throw "Windows staging prerequisite missing: $path" }
}

& $Node (Join-Path $RepoRoot "scripts/release/validate-ffmpeg-config.mjs") `
    --config-header (Join-Path $Ffmpeg "FFMPEG_CONFIG.h") `
    --flags (Join-Path $Ffmpeg "FFMPEG_BUILD_FLAGS.txt")
if ($LASTEXITCODE -ne 0) { throw "Windows FFmpeg configuration validation failed" }

Remove-Item -Recurse -Force $Stage -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $Runtime, $Tauri | Out-Null
Copy-Item -Force $WorkerSource (Join-Path $Runtime "nian-media-worker.exe")
Copy-Item -Force $WorkerSource (Join-Path $Tauri "nian-media-worker-$Target.exe")
foreach ($dll in @("avformat-62.dll", "avcodec-62.dll", "avutil-60.dll")) {
    Copy-Item -Force (Join-Path $Ffmpeg "bin/$dll") (Join-Path $Runtime $dll)
}
foreach ($name in @("THIRD_PARTY_NOTICES.txt", "FFMPEG-LGPL-2.1.txt", "FFMPEG_BUILD_FLAGS.txt", "FFMPEG_CONFIG.h")) {
    $source = if ($name -eq "THIRD_PARTY_NOTICES.txt") { Join-Path $RepoRoot $name } else { Join-Path $Ffmpeg $name }
    Copy-Item -Force $source (Join-Path $Stage $name)
}

& $Node (Join-Path $RepoRoot "scripts/release/build-metadata.mjs") `
    --output (Join-Path $Stage "BUILD_METADATA.json") --target $Target
if ($LASTEXITCODE -ne 0) { throw "Windows build metadata generation failed" }

$closureRoots = @(
    (Join-Path $Runtime "nian-media-worker.exe"),
    (Join-Path $Runtime "avformat-62.dll"),
    (Join-Path $Runtime "avcodec-62.dll"),
    (Join-Path $Runtime "avutil-60.dll")
)
if (Test-Path $DesktopSource) { $closureRoots += $DesktopSource }
Ensure-DependencyClosure $closureRoots

$forbidden = @('msys64', 'vcpkg\\installed', 'nian-vision\\target', 'nian-vision/target')
foreach ($file in Get-ChildItem $Stage -Recurse -File) {
    if ($file.Extension -in @('.json','.txt','.h')) {
        $text = Get-Content $file.FullName -Raw
        foreach ($needle in $forbidden) {
            if ($text -match [Regex]::Escape($needle)) { throw "Windows stage contains a developer/runtime path: $($file.FullName)" }
        }
    }
}

$oldPath = $env:PATH
$oldOverride = $env:NIAN_FFMPEG_LIB_DIR
try {
    $env:PATH = $Runtime
    Remove-Item Env:NIAN_FFMPEG_LIB_DIR -ErrorAction SilentlyContinue
    & $Node (Join-Path $RepoRoot "scripts/release/stage-runtime-smoke.mjs") `
        (Join-Path $Runtime "nian-media-worker.exe") (Join-Path $Stage "smoke")
    if ($LASTEXITCODE -ne 0) { throw "clean Windows staged worker smoke failed" }
}
finally {
    $env:PATH = $oldPath
    if ($null -ne $oldOverride) { $env:NIAN_FFMPEG_LIB_DIR = $oldOverride }
}

Write-Host "Windows x86_64 release runtime staged at $Stage"
