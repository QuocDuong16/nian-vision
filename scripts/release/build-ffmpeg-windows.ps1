$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
$Config = Get-Content (Join-Path $RepoRoot "scripts/release/release-config.json") -Raw | ConvertFrom-Json
$OutputDir = if ($args.Count -gt 0) { [IO.Path]::GetFullPath($args[0]) } else { Join-Path $RepoRoot "dist/ffmpeg-windows-x86_64" }

function Import-VsDevEnvironment {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { throw "vswhere.exe is unavailable" }
    $install = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if (-not $install) { throw "Visual Studio C++ build tools are unavailable" }
    $vsdev = Join-Path $install "Common7\Tools\VsDevCmd.bat"
    if (-not (Test-Path $vsdev)) { throw "VsDevCmd.bat is unavailable" }
    $lines = & cmd.exe /d /s /c "`"$vsdev`" -arch=amd64 -host_arch=amd64 >nul && set"
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

Import-VsDevEnvironment
foreach ($tool in @("cl.exe", "lib.exe", "dumpbin.exe", "node.exe", "curl.exe", "tar.exe")) { Require-Command $tool }

$Bash = "C:\msys64\usr\bin\bash.exe"
if (-not (Test-Path $Bash)) { throw "MSYS2 bash is unavailable at $Bash" }

$Work = Join-Path $env:RUNNER_TEMP ("nian-ffmpeg-windows-" + [Guid]::NewGuid().ToString("N"))
$Tarball = Join-Path $Work ("ffmpeg-{0}.tar.xz" -f $Config.ffmpegVersion)
$Source = Join-Path $Work ("ffmpeg-{0}" -f $Config.ffmpegVersion)
$DestRoot = Join-Path $Work "install-root"
New-Item -ItemType Directory -Force $Work | Out-Null
try {
    & curl.exe --fail --silent --show-error --location --output $Tarball $Config.ffmpegSourceUrl
    if ($LASTEXITCODE -ne 0) { throw "FFmpeg source download failed" }
    $actual = (Get-FileHash -Algorithm SHA256 $Tarball).Hash.ToLowerInvariant()
    if ($actual -ne $Config.ffmpegSourceSha256.ToLowerInvariant()) {
        throw "FFmpeg source SHA-256 mismatch"
    }
    & tar.exe -xf $Tarball -C $Work
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path $Source)) { throw "FFmpeg source extraction failed" }

    Remove-Item -Recurse -Force $OutputDir -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force $OutputDir | Out-Null

    $flags = @(
        "--prefix=/opt/nian-vision-ffmpeg",
        "--toolchain=msvc",
        "--arch=x86_64",
        "--disable-doc",
        "--disable-programs",
        "--disable-static",
        "--enable-shared",
        "--disable-gpl",
        "--disable-nonfree",
        "--disable-autodetect",
        "--disable-everything",
        "--disable-x86asm",
        "--disable-avdevice",
        "--disable-avfilter",
        "--disable-swresample",
        "--disable-swscale",
        "--enable-network",
        "--enable-protocol=file,tcp,rtsp,rtp,udp",
        "--enable-demuxer=matroska,mov,rtsp",
        "--enable-muxer=matroska,mov,mp4",
        "--enable-parser=h264,mpeg4video,mpegaudio,aac",
        "--enable-decoder=mpeg4,aac"
    )
    Set-Content -Path (Join-Path $OutputDir "FFMPEG_BUILD_FLAGS.txt") -Value ($flags -join "`n") -NoNewline

    $sourceUnix = (& $Bash --noprofile --norc -lc "cygpath -u '$($Source.Replace("'", "'\''"))'").Trim()
    $destUnix = (& $Bash --noprofile --norc -lc "cygpath -u '$($DestRoot.Replace("'", "'\''"))'").Trim()
    $flagText = ($flags | ForEach-Object { "'" + $_.Replace("'", "'\''") + "'" }) -join " "
    $script = @"
set -Eeuo pipefail
cd '$sourceUnix'
./configure $flagText
cp config.h '$((& $Bash --noprofile --norc -lc "cygpath -u '$((Join-Path $OutputDir 'FFMPEG_CONFIG.h').Replace("'", "'\''"))'").Trim())'
make -j2
make install DESTDIR='$destUnix'
"@
    & $Bash --noprofile --norc -lc $script
    if ($LASTEXITCODE -ne 0) { throw "FFmpeg Windows MSVC build failed" }

    & node.exe (Join-Path $RepoRoot "scripts/release/validate-ffmpeg-config.mjs") `
        --config-header (Join-Path $OutputDir "FFMPEG_CONFIG.h") `
        --flags (Join-Path $OutputDir "FFMPEG_BUILD_FLAGS.txt")
    if ($LASTEXITCODE -ne 0) { throw "FFmpeg Windows license/config validation failed" }

    $InstallRoot = Join-Path $DestRoot "opt/nian-vision-ffmpeg"
    $BinOut = Join-Path $OutputDir "bin"
    $LibOut = Join-Path $OutputDir "lib"
    New-Item -ItemType Directory -Force $BinOut, $LibOut | Out-Null
    Copy-Item -Recurse -Force (Join-Path $InstallRoot "include") (Join-Path $OutputDir "include")

    $requiredDlls = @("avformat-62.dll", "avcodec-62.dll", "avutil-60.dll")
    $requiredLibs = @("avformat.lib", "avcodec.lib", "avutil.lib")
    foreach ($name in $requiredDlls) {
        $match = Get-ChildItem $InstallRoot -Recurse -File -Filter $name
        if ($match.Count -ne 1) { throw "required FFmpeg runtime DLL missing or ambiguous: $name" }
        Copy-Item -Force $match[0].FullName (Join-Path $BinOut $name)
    }
    foreach ($name in $requiredLibs) {
        $match = Get-ChildItem $InstallRoot -Recurse -File -Filter $name
        if ($match.Count -ne 1) { throw "required MSVC FFmpeg import library missing or ambiguous: $name" }
        Copy-Item -Force $match[0].FullName (Join-Path $LibOut $name)
    }
    Copy-Item -Force (Join-Path $Source "COPYING.LGPLv2.1") (Join-Path $OutputDir "FFMPEG-LGPL-2.1.txt")

    if (Get-ChildItem $InstallRoot -Recurse -File | Where-Object Name -Match '^ff(mpeg|probe|play)\.exe$') {
        throw "FFmpeg CLI program unexpectedly present in Windows release runtime"
    }
    if (Get-ChildItem $OutputDir -Recurse -File | Select-String -SimpleMatch $env:GITHUB_WORKSPACE -Quiet -ErrorAction SilentlyContinue) {
        throw "Windows FFmpeg release evidence contains the repository build path"
    }
    Write-Host "FFmpeg $($Config.ffmpegVersion) Windows MSVC shared runtime built at $OutputDir"
}
finally {
    Remove-Item -Recurse -Force $Work -ErrorAction SilentlyContinue
}
