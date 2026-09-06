param(
    [Parameter(Mandatory = $true)][string[]]$Path,
    [Parameter(Mandatory = $true)][string]$StateFile
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot "windows-native.ps1")

function Find-SignTool {
    $sdkRoot = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
    $tool = Get-ChildItem $sdkRoot -Recurse -File -Filter signtool.exe -ErrorAction SilentlyContinue |
        Where-Object FullName -Match '[\\/]x64[\\/]signtool\.exe$' |
        Sort-Object FullName -Descending |
        Select-Object -First 1
    if (-not $tool) { throw "Windows SDK signtool.exe is unavailable" }
    return $tool.FullName
}

$required = $env:REQUIRE_WINDOWS_AUTHENTICODE -eq 'true'
$hasPfx = -not [string]::IsNullOrWhiteSpace($env:WINDOWS_SIGNING_PFX_BASE64)
$hasPassword = -not [string]::IsNullOrWhiteSpace($env:WINDOWS_SIGNING_PFX_PASSWORD)
$timestamp = $env:WINDOWS_SIGNING_TIMESTAMP_URL

if ($required -and (-not $hasPfx -or -not $hasPassword)) {
    throw "Windows Authenticode is required but signing credentials are unavailable"
}
if ($hasPfx -xor $hasPassword) {
    throw "Windows Authenticode PFX and password must be configured together"
}
if (-not $hasPfx) {
    New-Item -ItemType Directory -Force (Split-Path $StateFile -Parent) | Out-Null
    '{"authenticode_signed":false}' | Set-Content -NoNewline $StateFile
    Write-Host "Windows Authenticode credentials are not configured; candidate remains explicitly unsigned"
    exit 0
}
if ([string]::IsNullOrWhiteSpace($timestamp) -or -not $timestamp.StartsWith('https://')) {
    throw "WINDOWS_SIGNING_TIMESTAMP_URL must be an HTTPS timestamp authority when Authenticode is enabled"
}

$signTool = Find-SignTool
$pfx = Join-Path $env:RUNNER_TEMP ("nian-authenticode-" + [Guid]::NewGuid().ToString('N') + '.pfx')
try {
    [IO.File]::WriteAllBytes($pfx, [Convert]::FromBase64String($env:WINDOWS_SIGNING_PFX_BASE64))
    foreach ($raw in $Path) {
        $file = (Resolve-Path $raw).Path
        Invoke-NianNative { & $signTool sign /fd SHA256 /td SHA256 /tr $timestamp /f $pfx /p $env:WINDOWS_SIGNING_PFX_PASSWORD $file }
        Invoke-NianNative { & $signTool verify /pa /all /v $file }
    }
    New-Item -ItemType Directory -Force (Split-Path $StateFile -Parent) | Out-Null
    '{"authenticode_signed":true}' | Set-Content -NoNewline $StateFile
}
finally {
    Remove-Item -Force $pfx -ErrorAction SilentlyContinue
}
