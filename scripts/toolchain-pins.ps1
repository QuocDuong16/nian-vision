param(
    [Parameter(Position = 0)]
    [string]$Field
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# Resolve toolchain versions and verified artifacts from mise.toml/mise.lock.
# mise.toml is authoritative; mise.lock supplies the per-platform URLs and
# checksums. Release jobs call this after checkout so the workflows do not
# duplicate version literals.

$script:ToolchainRoot = Split-Path -Parent $PSScriptRoot
$script:MiseTomlPath = Join-Path $script:ToolchainRoot "mise.toml"
$script:MiseLockPath = Join-Path $script:ToolchainRoot "mise.lock"

function Get-ToolchainPin {
    param([Parameter(Mandatory = $true)][string]$Tool)

    $pattern = '^\s*"?' + [regex]::Escape($Tool) + '"?\s*=\s*"([^"]+)"'
    $inTools = $false
    foreach ($line in Get-Content -LiteralPath $script:MiseTomlPath) {
        if ($line -match '^\s*\[') {
            $inTools = ($line -match '^\s*\[tools\]\s*$')
            continue
        }
        if ($inTools -and $line -match $pattern) { return $Matches[1] }
    }
    throw "missing $Tool pin in mise.toml"
}

function Get-ToolchainLockValue {
    param(
        [Parameter(Mandatory = $true)][string]$Tool,
        [Parameter(Mandatory = $true)][string]$Platform,
        [Parameter(Mandatory = $true)][string]$Key
    )

    $section = '[tools.' + $Tool + '."platforms.' + $Platform + '"]'
    $pattern = '^' + [regex]::Escape($Key) + '\s*=\s*"([^"]+)"'
    $inSection = $false
    foreach ($line in Get-Content -LiteralPath $script:MiseLockPath) {
        if ($line -eq $section) {
            $inSection = $true
            continue
        }
        if ($line -match '^\[') { $inSection = $false }
        if ($inSection -and $line -match $pattern) { return $Matches[1] }
    }
    throw "missing $Tool.$Platform.$Key in mise.lock"
}

function Get-ToolchainSha256 {
    param(
        [Parameter(Mandatory = $true)][string]$Tool,
        [Parameter(Mandatory = $true)][string]$Platform
    )

    return ((Get-ToolchainLockValue -Tool $Tool -Platform $Platform -Key 'checksum') -replace '^sha256:', '')
}

if ($Field) {
    switch -Regex ($Field) {
        '^(rust|node|pnpm)$' { Get-ToolchainPin -Tool $Field; break }
        '^cargo-(audit|deny|llvm-cov|machete|outdated)$' { Get-ToolchainPin -Tool "cargo:$Field"; break }
        '^(node|pnpm)-url-(.+)$' { Get-ToolchainLockValue -Tool $Matches[1] -Platform $Matches[2] -Key 'url'; break }
        '^(node|pnpm)-sha256-(.+)$' { Get-ToolchainSha256 -Tool $Matches[1] -Platform $Matches[2]; break }
        default { throw "unknown field: $Field" }
    }
}
