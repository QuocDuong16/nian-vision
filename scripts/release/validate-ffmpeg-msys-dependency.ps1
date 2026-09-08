param(
    [Parameter(Mandatory = $true)][string]$Source,
    [Parameter(Mandatory = $true)][string]$Bash,
    [Parameter(Mandatory = $true)][string]$ControlledPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'windows-native.ps1')
. (Join-Path $PSScriptRoot 'windows-ffmpeg-msys-environment.ps1')
. (Join-Path $PSScriptRoot 'windows-bash-script.ps1')

function Get-BoundedText([string]$Text, [int]$Limit = 1600) {
    if ($Text.Length -le $Limit) { return $Text }
    return $Text.Substring(0, $Limit) + ' ...[truncated]'
}

$ConfigMak = Join-Path $Source 'ffbuild/config.mak'
if (-not (Test-Path -LiteralPath $ConfigMak -PathType Leaf)) { throw "FFmpeg configure did not produce ffbuild/config.mak: $ConfigMak" }

$ccdepLines = @(Get-Content $ConfigMak | Where-Object { $_.StartsWith('CCDEP=') })
if ($ccdepLines.Count -ne 1) { throw "FFmpeg generated dependency contract requires exactly one CCDEP line, found $($ccdepLines.Count)" }

$expectedAwk = 'gsub(/\\/, "/")'
$ccdep = $ccdepLines[0]
Write-Host ("FFmpeg generated CCDEP on disk: {0}" -f (Get-BoundedText $ccdep))
if (-not $ccdep.Contains($expectedAwk)) {
    throw 'FFmpeg generated CCDEP is malformed on disk; expected the MSVC AWK backslash conversion gsub(/\\/, "/")'
}

$lowerCcdep = $ccdep.ToLowerInvariant()
foreach ($forbidden in @('mingw64', 'mingw32', 'ucrt64/bin', 'clang64/bin', 'clangarm64/bin')) {
    if ($lowerCcdep.Contains($forbidden)) {
        throw "FFmpeg generated CCDEP contains a forbidden toolchain authority on disk: $forbidden"
    }
}

$Cygpath = 'C:\msys64\usr\bin\cygpath.exe'
$SourceUnix = ConvertTo-NianMsysPath -Path $Source -Cygpath $Cygpath
$probePath = Join-Path $env:RUNNER_TEMP ("nian-ffmpeg-depcmd-probe-{0}.mak" -f [Guid]::NewGuid().ToString('N'))
try {
    @'
include ffbuild/config.mak
$(info NIAN_CCDEP_BEGIN)
$(info $(call CCDEP,CC))
$(info NIAN_CCDEP_END)
.PHONY: nian-dependency-probe
nian-dependency-probe:
'@ | Set-Content -Path $probePath -NoNewline

    $probeUnix = ConvertTo-NianMsysPath -Path $probePath -Cygpath $Cygpath
    $command = @'
set -Eeuo pipefail
export PATH=__CONTROLLED_PATH__
hash -r
cd __SOURCE__
/usr/bin/make --no-print-directory -f __PROBE__ -n nian-dependency-probe
'@
    $command = $command.Replace('__CONTROLLED_PATH__', (ConvertTo-NianBashSingleQuoted $ControlledPath))
    $command = $command.Replace('__SOURCE__', (ConvertTo-NianBashSingleQuoted $SourceUnix))
    $command = $command.Replace('__PROBE__', (ConvertTo-NianBashSingleQuoted $probeUnix))
    $expanded = (Invoke-NianBashScript `
        -Bash $Bash `
        -Script $command `
        -FileName 'nian-ffmpeg-ccdep-probe.sh' `
        -Label 'FFmpeg generated dependency make-expansion probe' | Out-String).Trim()
    Write-Host ("FFmpeg CCDEP after /usr/bin/make expansion: {0}" -f (Get-BoundedText $expanded))
    if (-not $expanded.Contains($expectedAwk)) {
        throw 'FFmpeg CCDEP is correct on disk but is corrupted during GNU make expansion; compile is blocked'
    }
    $lowerExpanded = $expanded.ToLowerInvariant()
    foreach ($forbidden in @('mingw64', 'mingw32', 'ucrt64/bin', 'clang64/bin', 'clangarm64/bin')) {
        if ($lowerExpanded.Contains($forbidden)) {
            throw "FFmpeg CCDEP make expansion selected or exposed a forbidden toolchain authority: $forbidden"
        }
    }
}
finally {
    Remove-Item -Force $probePath -ErrorAction SilentlyContinue
}

Write-Host 'FFmpeg generated MSVC dependency command is sane on disk and after /usr/bin/make expansion under the controlled PATH'
