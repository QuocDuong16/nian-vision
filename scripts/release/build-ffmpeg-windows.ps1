$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot "windows-native.ps1")
. (Join-Path $PSScriptRoot "windows-bounded-process.ps1")
. (Join-Path $PSScriptRoot "windows-msvc-toolchain.ps1")

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

function Require-Command([string]$Name) {
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) { throw "required Windows release tool is unavailable: $Name" }
}

function Convert-ToMsysPath([string]$Path, [string]$Bash) {
    $escaped = $Path.Replace("'", "'\''")
    return (Invoke-NianNative { & $Bash --noprofile --norc -lc "/usr/bin/cygpath -u '$escaped'" } 'cygpath path conversion' | Out-String).Trim()
}

function Convert-ToBashSingleQuoted([string]$Value) {
    return "'" + $Value.Replace("'", "'\''") + "'"
}

if ($Config.windowsTarget -ne "x86_64-pc-windows-msvc") { throw "Windows FFmpeg release target contract drifted" }
if ($Contract.toolchain -ne "msvc" -or $Contract.architecture -ne "x86_64") { throw "Windows FFmpeg toolchain contract drifted" }
$flags = @($Contract.configureFlags)
if ($flags.Count -eq 0) { throw "Windows FFmpeg configure flag contract is empty" }

$Msvc = Resolve-NianMsvcToolchain
$Bash = "C:\msys64\usr\bin\bash.exe"
$Tar = "C:\msys64\usr\bin\tar.exe"
$Xz = "C:\msys64\usr\bin\xz.exe"
$Make = "C:\msys64\usr\bin\make.exe"
$Awk = "C:\msys64\usr\bin\awk.exe"
$Sed = "C:\msys64\usr\bin\sed.exe"
$Grep = "C:\msys64\usr\bin\grep.exe"
$Cygpath = "C:\msys64\usr\bin\cygpath.exe"
foreach ($tool in @($Bash, $Tar, $Xz, $Make, $Awk, $Sed, $Grep, $Cygpath)) {
    if (-not (Test-Path $tool -PathType Leaf)) { throw "required deterministic MSYS2 release tool is unavailable: $tool" }
}

foreach ($tool in @("dumpbin.exe", "node.exe", "curl.exe")) { Require-Command $tool }
foreach ($entry in @(
    @{ Name = 'cl.exe'; Path = $Msvc.ClPath },
    @{ Name = 'lib.exe'; Path = $Msvc.LibPath },
    @{ Name = 'link.exe'; Path = $Msvc.LinkPath }
)) {
    Write-Host ("FFmpeg MSVC tool {0}: {1}" -f $entry.Name, $entry.Path)
}
$MsvcBinUnix = Convert-ToMsysPath $Msvc.MsvcBin $Bash
Write-Host "FFmpeg MSVC tool directory inside MSYS: $MsvcBinUnix"

$expectedMsysTools = [ordered]@{
    bash = "/usr/bin/bash"
    make = "/usr/bin/make"
    awk = "/usr/bin/awk"
    sed = "/usr/bin/sed"
    grep = "/usr/bin/grep"
    cygpath = "/usr/bin/cygpath"
}
foreach ($name in $expectedMsysTools.Keys) {
    if ($Contract.msysBuildTools.$name -ne $expectedMsysTools[$name]) {
        throw "Windows FFmpeg MSYS build-tool contract drifted for $name"
    }
}
$MsysBuildPreamble = @'
set -Eeuo pipefail
export PATH=__MSVC_BIN__:/usr/bin:"$PATH"
hash -r
'@.Replace('__MSVC_BIN__', (Convert-ToBashSingleQuoted $MsvcBinUnix))

# FFmpeg 8.0.3 --toolchain=msvc sets LD=$source_path/compat/windows/mslink.
# That wrapper first resolves dirname "$(command -v cl)"/link and falls back to
# link.exe, so the controlled shell proves both bare and .exe spellings use the
# selected Visual Studio toolchain before configure or compile can start.
& (Join-Path $RepoRoot "scripts/release/test-ffmpeg-msys-escape.ps1") `
    -VsInstall $Msvc.VsInstall `
    -MsvcBinUnix $MsvcBinUnix `
    -ExpectedClWindows $Msvc.ClPath `
    -ExpectedLibWindows $Msvc.LibPath `
    -ExpectedLinkWindows $Msvc.LinkPath

$reportedCpuCount = [Environment]::ProcessorCount
$buildJobs = [Math]::Max(2, [Math]::Min($reportedCpuCount, 8))
Write-Host "FFmpeg Windows build parallelism: reported logical processors=$reportedCpuCount; make jobs=$buildJobs; cap=8"

$Work = Join-Path $env:RUNNER_TEMP ("nian-ffmpeg-windows-" + [Guid]::NewGuid().ToString("N"))
$Tarball = Join-Path $Work ("ffmpeg-{0}.tar.xz" -f $Config.ffmpegVersion)
$Source = Join-Path $Work ("ffmpeg-{0}" -f $Config.ffmpegVersion)
$DestRoot = Join-Path $Work "install-root"
$ConfigureStderr = Join-Path $Work "configure.stderr.log"
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
        $archive = Get-Item $Tarball
        $tarVersion = ((Invoke-NianNative { & $Tar --version }) | Select-Object -First 1).Trim()
        $xzVersion = ((Invoke-NianNative { & $Xz --version }) | Select-Object -First 1).Trim()
        $tarballUnix = Convert-ToMsysPath $Tarball $Bash
        $workUnix = Convert-ToMsysPath $Work $Bash
        $tarballQuoted = "'" + $tarballUnix.Replace("'", "'\''") + "'"
        $workQuoted = "'" + $workUnix.Replace("'", "'\''") + "'"
        $extractCommand = "set -Eeuo pipefail; export PATH=/usr/bin; /usr/bin/xz --decompress --stdout $tarballQuoted | /usr/bin/tar --extract --file - --directory $workQuoted"

        Write-Host "FFmpeg extraction tar: $Tar"
        Write-Host "FFmpeg extraction tar version: $tarVersion"
        Write-Host "FFmpeg extraction xz: $Xz"
        Write-Host "FFmpeg extraction xz version: $xzVersion"
        Write-Host "FFmpeg extraction archive: $Tarball"
        Write-Host ("FFmpeg extraction archive size: {0} bytes" -f $archive.Length)
        Write-Host "FFmpeg extraction destination: $Work"
        Write-Host ("FFmpeg extraction start UTC: {0:o}" -f [DateTime]::UtcNow)

        $result = Invoke-NianBoundedProcess -FilePath $Bash `
            -ArgumentList @("--noprofile", "--norc", "-lc", $extractCommand) `
            -TimeoutSeconds 600 `
            -Label "FFmpeg source extraction"
        Write-Host ("FFmpeg extraction command elapsed {0:n1}s" -f $result.ElapsedSeconds)

        if (-not (Test-Path $Source -PathType Container)) { throw "FFmpeg source extraction did not produce the expected source directory" }
        $sourceFiles = @(Get-ChildItem $Source -Recurse -File)
        $sourceBytes = ($sourceFiles | Measure-Object -Property Length -Sum).Sum
        Write-Host ("FFmpeg extracted source: {0} files; {1} bytes" -f $sourceFiles.Count, $sourceBytes)
    }

    Set-Content -Path (Join-Path $CandidateDir "FFMPEG_BUILD_FLAGS.txt") -Value ($flags -join "`n") -NoNewline

    $sourceUnix = Convert-ToMsysPath $Source $Bash
    $destUnix = Convert-ToMsysPath $DestRoot $Bash
    $configureStderrUnix = Convert-ToMsysPath $ConfigureStderr $Bash
    $flagText = ($flags | ForEach-Object { "'" + $_.Replace("'", "'\''") + "'" }) -join " "

    Invoke-FfmpegPhase "configure" {
        $configureCommand = @'
{0}
cd '{1}'
set +e
./configure {2} 2>'{3}'
status=$?
set -e
if (( status != 0 )); then
  printf 'FFmpeg configure failed; bounded stderr tail follows\n' >&2
  /usr/bin/tail -n 120 '{3}' >&2
  exit "$status"
fi
'@ -f $MsysBuildPreamble, $sourceUnix, $flagText, $configureStderrUnix
        Invoke-NianNative { & $Bash --noprofile --norc -lc $configureCommand } 'FFmpeg configure'
        $configHeader = Join-Path $Source "config.h"
        $componentHeader = Join-Path $Source "config_components.h"
        if (-not (Test-Path $configHeader)) { throw "FFmpeg configure did not produce config.h" }
        if (-not (Test-Path $componentHeader)) { throw "FFmpeg configure did not produce config_components.h" }
        Copy-Item -Force $configHeader (Join-Path $CandidateDir "FFMPEG_CONFIG.h")
        Copy-Item -Force $componentHeader (Join-Path $CandidateDir "FFMPEG_CONFIG_COMPONENTS.h")
    }

    Invoke-FfmpegPhase "post-configure MSYS dependency validation" {
        $configureDiagnostics = if (Test-Path $ConfigureStderr) { @(Get-Content $ConfigureStderr) } else { @() }
        if ($configureDiagnostics.Count -gt 0) {
            Write-Host "FFmpeg configure stderr (first 40 lines):"
            $configureDiagnostics | Select-Object -First 40 | ForEach-Object { Write-Host $_ }
        }
        $syntaxFailures = @($configureDiagnostics | Where-Object {
            $_ -match '(?i)(?:sed|awk):.*(?:unterminated|syntax error|expression #[0-9]+)'
        })
        if ($syntaxFailures.Count -gt 0) {
            Write-Host "FFmpeg configure shell-syntax failures (bounded):"
            $syntaxFailures | Select-Object -First 20 | ForEach-Object { Write-Host $_ }
            throw "FFmpeg configure returned success but emitted sed/awk syntax errors; compile is blocked"
        }
        & (Join-Path $RepoRoot "scripts/release/validate-ffmpeg-msys-dependency.ps1") `
            -Source $Source `
            -Bash $Bash `
            -MsvcBinUnix $MsvcBinUnix
    }

    Invoke-FfmpegPhase "compile" {
        Invoke-NianNative { & $Bash --noprofile --norc -lc "$MsysBuildPreamble; cd '$sourceUnix'; /usr/bin/make -j$buildJobs" } 'FFmpeg compile with /usr/bin/make'
    }

    Invoke-FfmpegPhase "install" {
        Invoke-NianNative { & $Bash --noprofile --norc -lc "$MsysBuildPreamble; cd '$sourceUnix'; /usr/bin/make install DESTDIR='$destUnix'" } 'FFmpeg install with /usr/bin/make'
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
