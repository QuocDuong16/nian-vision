param(
    [Parameter(Mandatory = $false)]
    [string]$OutputDir
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot "windows-native.ps1")

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
if (-not $OutputDir) {
    $OutputDir = Join-Path $RepoRoot "dist/ffmpeg-windows-x86_64"
}
$OutputDir = [IO.Path]::GetFullPath($OutputDir)

Invoke-NianNative {
    node.exe (Join-Path $RepoRoot "scripts/release/validate-ffmpeg-windows.mjs") --output-dir $OutputDir
}
