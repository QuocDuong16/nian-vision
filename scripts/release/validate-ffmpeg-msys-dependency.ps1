param(
    [Parameter(Mandatory = $true)][string]$Source,
    [Parameter(Mandatory = $true)][string]$Bash
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'windows-native.ps1')

function Convert-ToMsysPath([string]$Path) {
    $escaped = $Path.Replace("'", "'\''")
    return (Invoke-NianNative { & $Bash --noprofile --norc -lc "/usr/bin/cygpath -u '$escaped'" } 'cygpath path conversion' | Out-String).Trim()
}

function Get-BoundedText([string]$Text, [int]$Limit = 1600) {
    if ($Text.Length -le $Limit) { return $Text }
    return $Text.Substring(0, $Limit) + ' ...[truncated]'
}

$ConfigMak = Join-Path $Source 'ffbuild/config.mak'
if (-not (Test-Path $ConfigMak -PathType Leaf)) { throw "FFmpeg configure did not produce ffbuild/config.mak: $ConfigMak" }

$ccdepLines = @(Get-Content $ConfigMak | Where-Object { $_.StartsWith('CCDEP=') })
if ($ccdepLines.Count -ne 1) { throw "FFmpeg generated dependency contract requires exactly one CCDEP line, found $($ccdepLines.Count)" }

$expectedAwk = 'gsub(/\\/, "/")'
$ccdep = $ccdepLines[0]
Write-Host ("FFmpeg generated CCDEP on disk: {0}" -f (Get-BoundedText $ccdep))
if (-not $ccdep.Contains($expectedAwk)) {
    throw 'FFmpeg generated CCDEP is malformed on disk; expected the MSVC AWK backslash conversion gsub(/\\/, "/")'
}

$SourceUnix = Convert-ToMsysPath $Source
$probePath = Join-Path $env:RUNNER_TEMP ("nian-ffmpeg-depcmd-probe-{0}.mak" -f [Guid]::NewGuid().ToString('N'))
try {
    @'
include ffbuild/config.mak
$(info NIAN_CCDEP_BEGIN)
$(info $(call CCDEP,CC))
$(info NIAN_CCDEP_END)
.PHONY: nian-dependency-probe
nian-dependency-probe:
	@:
'@ | Set-Content -Path $probePath -NoNewline

    $probeUnix = Convert-ToMsysPath $probePath
    $command = "set -Eeuo pipefail; export PATH=\"/usr/bin:`$PATH\"; hash -r; cd '$SourceUnix'; /usr/bin/make --no-print-directory -f '$probeUnix' -n nian-dependency-probe"
    $expanded = (Invoke-NianNative { & $Bash --noprofile --norc -lc $command } 'FFmpeg generated dependency make-expansion probe' | Out-String).Trim()
    Write-Host ("FFmpeg CCDEP after /usr/bin/make expansion: {0}" -f (Get-BoundedText $expanded))
    if (-not $expanded.Contains($expectedAwk)) {
        throw 'FFmpeg CCDEP is correct on disk but is corrupted during GNU make expansion; compile is blocked'
    }
}
finally {
    Remove-Item -Force $probePath -ErrorAction SilentlyContinue
}

Write-Host 'FFmpeg generated MSVC dependency command is sane on disk and after /usr/bin/make expansion'
