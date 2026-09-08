$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'windows-bash-script.ps1')

$bashCommand = Get-Command bash -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $bashCommand) {
    Write-Host 'Bash unavailable; generated Bash execution fixture skipped'
    exit 0
}
$Bash = $bashCommand.Source
$Root = Join-Path ([IO.Path]::GetTempPath()) ("nian-bash-helper-fixture-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $Root | Out-Null
try {
    $writeRoot = Join-Path $Root 'write-check'
    $written = Write-NianBashScriptFile `
        -Directory $writeRoot `
        -FileName 'nian-encoding-check.sh' `
        -Script "printf '%s\n' encoding-ok`r`nprintf '%s\n' lf-ok`r`n"
    $bytes = [IO.File]::ReadAllBytes($written)
    if ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF) {
        throw 'UTF-8 BOM fixture failure'
    }
    if ($bytes -contains 0x0D) { throw 'LF normalization fixture failure' }
    Write-Host 'Generated Bash encoding fixture PASS: UTF-8 no BOM and LF only'
    Remove-Item -Recurse -Force -LiteralPath $writeRoot

    $longSegment = '/c/Program Files/Microsoft Visual Studio/2022/Enterprise/VC/Tools/MSVC/14.99.99999/bin/HostX64/x64:/c/Program Files (x86)/Windows Kits/10/bin/10.0.99999.0/x64'
    $LongControlledPath = ((1..700 | ForEach-Object { $longSegment }) -join ':') + "/c/Users/O'Brien/bin:/usr/bin:/c/Windows/System32"
    $quotedLongPath = ConvertTo-NianBashSingleQuoted $LongControlledPath
    $longScript = @"
set -Eeuo pipefail
controlled=$quotedLongPath
if (( `${#controlled} < 60000 )); then
  printf 'long controlled PATH fixture unexpectedly short: %s\n' "`${#controlled}" >&2
  exit 1
fi
printf 'generated Bash long PATH fixture PASS: %s bytes\n' "`${#controlled}"
"@
    Invoke-NianBashScript `
        -Bash $Bash `
        -Script $longScript `
        -FileName 'nian-long-controlled-path.sh' `
        -Label 'generated Bash long PATH fixture' `
        -TempRoot $Root | Out-Host
    if (@(Get-ChildItem -LiteralPath $Root -Directory -Filter 'nian-bash-script-*').Count -ne 0) {
        throw 'generated Bash success cleanup fixture left a temp directory'
    }

    $marker = Join-Path $Root 'syntax-should-not-run.txt'
    $markerQuoted = ConvertTo-NianBashSingleQuoted $marker
    $malformed = "set -Eeuo pipefail`nif true; then`nprintf '%s\n' should-not-run > $markerQuoted`n"
    $syntaxFailed = $false
    try {
        Invoke-NianBashScript `
            -Bash $Bash `
            -Script $malformed `
            -FileName 'nian-malformed.sh' `
            -Label 'generated Bash syntax failure fixture' `
            -TempRoot $Root | Out-Host
    }
    catch {
        $syntaxFailed = $true
    }
    if (-not $syntaxFailed) { throw 'malformed generated Bash script unexpectedly executed' }
    if (Test-Path -LiteralPath $marker) { throw 'syntax failure reached behavioral execution' }
    if (@(Get-ChildItem -LiteralPath $Root -Directory -Filter 'nian-bash-script-*').Count -ne 0) {
        throw 'generated Bash syntax failure cleanup fixture left a temp directory'
    }
    Write-Host 'Generated Bash syntax gate fixture PASS: bash -n blocked execution and cleanup ran'

    $placeholderFailed = $false
    try {
        Invoke-NianBashScript `
            -Bash $Bash `
            -Script "printf '%s\n' __NIAN_UNRESOLVED__" `
            -FileName 'nian-placeholder.sh' `
            -Label 'generated Bash unresolved placeholder fixture' `
            -TempRoot $Root | Out-Host
    }
    catch {
        $placeholderFailed = $true
    }
    if (-not $placeholderFailed) { throw 'unresolved generated Bash placeholder was accepted' }
    if (@(Get-ChildItem -LiteralPath $Root -Directory -Filter 'nian-bash-script-*').Count -ne 0) {
        throw 'generated Bash placeholder failure cleanup fixture left a temp directory'
    }
    Write-Host 'Generated Bash unresolved-placeholder fixture PASS'
}
finally {
    Remove-Item -Recurse -Force -LiteralPath $Root -ErrorAction SilentlyContinue
}
