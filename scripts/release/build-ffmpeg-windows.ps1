$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot "windows-native.ps1")

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
$Config = Get-Content (Join-Path $RepoRoot "scripts/release/release-config.json") -Raw | ConvertFrom-Json
$Contract = Get-Content (Join-Path $RepoRoot "scripts/release/ffmpeg-windows-contract.json") -Raw | ConvertFrom-Json
$OutputDir = if ($args.Count -gt 0) { [IO.Path]::GetFullPath($args[0]) } else { Join-Path $RepoRoot "dist/ffmpeg-windows-x86_64" }

function Invoke-FfmpegPhase([string]$Name, [scriptblock]$Action) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    Write-Host "::group::FFmpeg Windows phase: $Name"
    try {
        & $Action
    }
    finally {
        $timer.Stop()
        Write-Host ("FFmpeg Windows phase '{0}' elapsed {1:n1}s" -f $Name, $timer.Elapsed.TotalSeconds)
        Write-Host "::endgroup::"
    }
}

function Import-VsDevEnvironment {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { throw "vswhere.exe is unavailable" }
    $install = (Invoke-NianNative { & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath } | Out-String).Trim()
    if (-not $install) { throw "Visual Studio C++ build tools are unavailable" }
    $vsdev = Join-Path $install "Common7\Tools\VsDevCmd.bat"
    if (-not (Test-Path $vsdev)) { throw "VsDevCmd.bat is unavailable" }
    $lines = Invoke-NianNative { cmd.exe /d /s /c "`"$vsdev`" -arch=amd64 -host_arch=amd64 >nul && set" }
    foreach ($line in $lines) {
        $split = $line.IndexOf('=')
        if ($split -gt 0) {
            [Environment]::SetEnvironmentVariable($line.Substring(0, $split), $line.Substring($split + 1), 'Process')
        }
    }
}

function Require-Command([string]$Name) {
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) { throw "required Windows release tool is unavailable: $Name" }
}

function Convert-ToMsysPath([string]$Path, [string]$Bash) {
    $escaped = $Path.Replace("'", "'\''")
    return (Invoke-NianNative { & $Bash --noprofile --norc -lc "cygpath -u '$escaped'" } | Out-String).Trim()
}

if ($Config.windowsTarget -ne "x86_64-pc-windows-msvc") { throw "Windows FFmpeg release target contract drifted" }
if ($Contract.toolchain -ne "msvc" -or $Contract.architecture -ne "x86_64") { throw "Windows FFmpeg toolchain contract drifted" }
$flags = @($Contract.configureFlags)
if ($flags.Count -eq 0) { throw "Windows FFmpeg configure flag contract is empty" }

Import-VsDevEnvironment
foreach ($tool in @("cl.exe", "lib.exe", "dumpbin.exe", "node.exe", "curl.exe", "tar.exe")) { Require-Command $tool }

$Bash = "C:\msys64\usr\bin\bash.exe"
if (-not (Test-Path $Bash)) { throw "MSYS2 bash is unavailable at $Bash" }

$reportedCpuCount = [Environment]::ProcessorCount
$buildJobs = [Math]::Max(2, [Math]::Min($reportedCpuCount, 8))
Write-Host "FFmpeg Windows build parallelism: reported logical processors=$reportedCpuCount; make jobs=$buildJobs; cap=8"

$Work = Join-Path $env:RUNNER_TEMP ("nian-ffmpeg-windows-" + [Guid]::NewGuid().ToString("N"))
$Tarball = Join-Path $Work ("ffmpeg-{0}.tar.xz" -f $Config.ffmpegVersion)
$Source = Join-Path $Work ("ffmpeg-{0}" -f $Config.ffmpegVersion)
$DestRoot = Join-Path $Work "install-root"
$OutputParent = Split-Path -Parent $OutputDir
New-Item -ItemType Directory -Force $Work, $OutputParent | Out-Null
$CandidateDir = Join-Path $OutputParent (".ffmpeg-windows-x86_64.candidate-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force $CandidateDir | Out-Null
try {
    Invoke-FfmpegPhase "download" {
        Invoke-NianNative { curl.exe --fail --silent --show-error --location --output $Tarball $Config.ffmpegSourceUrl }
    }

    Invoke-FfmpegPhase "SHA-256 verification" {
        $actual = (Get-FileHash -Algorithm SHA256 $Tarball).Hash.ToLowerInvariant()
        if ($actual -ne $Config.ffmpegSourceSha256.ToLowerInvariant()) {
            throw "FFmpeg source SHA-256 mismatch"
        }
        Write-Host "FFmpeg source SHA-256 verified"
    }

    Invoke-FfmpegPhase "extraction" {
        Invoke-NianNative { tar.exe -xf $Tarball -C $Work }
        if (-not (Test-Path $Source)) { throw "FFmpeg source extraction did not produce the expected source directory" }
    }

    Set-Content -Path (Join-Path $CandidateDir "FFMPEG_BUILD_FLAGS.txt") -Value ($flags -join "`n") -NoNewline

    $sourceUnix = Convert-ToMsysPath $Source $Bash
    $destUnix = Convert-ToMsysPath $DestRoot $Bash
    $flagText = ($flags | ForEach-Object { "'" + $_.Replace("'", "'\''") + "'" }) -join " "

    Invoke-FfmpegPhase "configure" {
        Invoke-NianNative { & $Bash --noprofile --norc -lc "set -Eeuo pipefail; cd '$sourceUnix'; ./configure $flagText" }
        $configHeader = Join-Path $Source "config.h"
        $componentHeader = Join-Path $Source "config_components.h"
        if (-not (Test-Path $configHeader)) { throw "FFmpeg configure did not produce config.h" }
        if (-not (Test-Path $componentHeader)) { throw "FFmpeg configure did not produce config_components.h" }
        Copy-Item -Force $configHeader (Join-Path $CandidateDir "FFMPEG_CONFIG.h")
        Copy-Item -Force $componentHeader (Join-Path $CandidateDir "FFMPEG_CONFIG_COMPONENTS.h")
    }

    Invoke-FfmpegPhase "compile" {
        Invoke-NianNative { & $Bash --noprofile --norc -lc "set -Eeuo pipefail; cd '$sourceUnix'; make -j$buildJobs" }
    }

    Invoke-FfmpegPhase "install" {
        Invoke-NianNative { & $Bash --noprofile --norc -lc "set -Eeuo pipefail; cd '$sourceUnix'; make install DESTDIR='$destUnix'" }
    }

    Invoke-FfmpegPhase "configuration and license validation" {
        Invoke-NianNative { node.exe (Join-Path $RepoRoot "scripts/release/validate-ffmpeg-config.mjs") `
            --config-header (Join-Path $CandidateDir "FFMPEG_CONFIG.h") `
            --flags (Join-Path $CandidateDir "FFMPEG_BUILD_FLAGS.txt") }
    }

    Invoke-FfmpegPhase "runtime staging and validation" {
        $InstallRoot = Join-Path $DestRoot "opt/nian-vision-ffmpeg"
        $BinOut = Join-Path $CandidateDir "bin"
        $LibOut = Join-Path $CandidateDir "lib"
        New-Item -ItemType Directory -Force $BinOut, $LibOut | Out-Null
        Copy-Item -Recurse -Force (Join-Path $InstallRoot "include") (Join-Path $CandidateDir "include")

        $requiredDlls = @(
            "avformat-$($Config.libavformatMajor).dll",
            "avcodec-$($Config.libavcodecMajor).dll",
            "avutil-$($Config.libavutilMajor).dll"
        )
        $requiredLibs = @("avformat.lib", "avcodec.lib", "avutil.lib")
        foreach ($name in $requiredDlls) {
            $match = @(Get-ChildItem $InstallRoot -Recurse -File -Filter $name)
            if ($match.Count -ne 1) { throw "required FFmpeg runtime DLL missing or ambiguous: $name" }
            Copy-Item -Force $match[0].FullName (Join-Path $BinOut $name)
        }
        foreach ($name in $requiredLibs) {
            $match = @(Get-ChildItem $InstallRoot -Recurse -File -Filter $name)
            if ($match.Count -ne 1) { throw "required MSVC FFmpeg import library missing or ambiguous: $name" }
            Copy-Item -Force $match[0].FullName (Join-Path $LibOut $name)
        }
        Copy-Item -Force (Join-Path $Source "COPYING.LGPLv2.1") (Join-Path $CandidateDir "FFMPEG-LGPL-2.1.txt")

        if (Get-ChildItem $InstallRoot -Recurse -File | Where-Object Name -Match '^ff(mpeg|probe|play)\.exe$') {
            throw "FFmpeg CLI program unexpectedly present in Windows release runtime"
        }

        Invoke-NianNative { node.exe (Join-Path $RepoRoot "scripts/release/ffmpeg-cache-key.mjs") `
            --write-metadata (Join-Path $CandidateDir "FFMPEG_BUILD_METADATA.json") }
        & (Join-Path $RepoRoot "scripts/release/validate-ffmpeg-windows.ps1") -OutputDir $CandidateDir
    }

    Invoke-FfmpegPhase "publish validated output" {
        Remove-Item -Recurse -Force $OutputDir -ErrorAction SilentlyContinue
        Move-Item -Path $CandidateDir -Destination $OutputDir
    }

    Write-Host "FFmpeg $($Config.ffmpegVersion) Windows MSVC shared runtime built and validated at $OutputDir"
}
finally {
    Remove-Item -Recurse -Force $CandidateDir -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $Work -ErrorAction SilentlyContinue
}
