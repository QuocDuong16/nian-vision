$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'windows-configure-diagnostics.ps1')

$Root = Join-Path ([IO.Path]::GetTempPath()) ("nian-configure-diagnostics-" + [Guid]::NewGuid().ToString('N'))
$encoding = [Text.UTF8Encoding]::new($false)
New-Item -ItemType Directory -Force -Path $Root | Out-Null

function Get-FixtureDiagnostics([string]$Path) {
    [string[]]$diagnostics = @(
        Get-NianConfigureDiagnostics -LiteralPath $Path
    )
    return ,$diagnostics
}

function Get-FixtureSyntaxFailures([string[]]$Diagnostics) {
    [string[]]$failures = @(
        Get-NianConfigureSyntaxFailures -Diagnostics $Diagnostics
    )
    return ,$failures
}

try {
    $absentPath = Join-Path $Root 'absent.stderr.log'
    [string[]]$absent = Get-FixtureDiagnostics $absentPath
    if ($absent.Count -ne 0) { throw "absent diagnostics file must produce Count 0, got $($absent.Count)" }
    Write-Host 'configure diagnostics fixture PASS: absent file -> Count 0'

    $emptyPath = Join-Path $Root 'empty.stderr.log'
    [IO.File]::WriteAllText($emptyPath, '', $encoding)
    [string[]]$empty = Get-FixtureDiagnostics $emptyPath
    if ($empty.Count -ne 0) { throw "empty diagnostics file must produce Count 0, got $($empty.Count)" }
    Write-Host 'configure diagnostics fixture PASS: empty file -> Count 0'

    $onePath = Join-Path $Root 'one.stderr.log'
    [IO.File]::WriteAllText($onePath, "single diagnostic line`n", $encoding)
    [string[]]$one = Get-FixtureDiagnostics $onePath
    if ($one.Count -ne 1 -or $one[0] -ne 'single diagnostic line') {
        throw "one-line diagnostics file lost collection semantics or contents"
    }
    Write-Host 'configure diagnostics fixture PASS: one line -> Count 1 and index 0 preserved'

    $manyPath = Join-Path $Root 'many.stderr.log'
    [IO.File]::WriteAllText($manyPath, "first diagnostic`nsecond diagnostic`nthird diagnostic`n", $encoding)
    [string[]]$many = Get-FixtureDiagnostics $manyPath
    if ($many.Count -ne 3) { throw "many-line diagnostics file must produce Count 3, got $($many.Count)" }
    if (($many -join '|') -ne 'first diagnostic|second diagnostic|third diagnostic') {
        throw 'many-line diagnostics file did not preserve ordering'
    }
    Write-Host 'configure diagnostics fixture PASS: many lines -> exact Count and ordering preserved'

    [string[]]$syntaxZero = Get-FixtureSyntaxFailures @('configure: harmless note', 'warning: unrelated')
    if ($syntaxZero.Count -ne 0) { throw "syntax failure zero-match fixture expected Count 0, got $($syntaxZero.Count)" }
    Write-Host 'configure syntax fixture PASS: zero matches -> Count 0'

    [string[]]$syntaxOne = Get-FixtureSyntaxFailures @('sed: syntax error near unexpected token')
    if ($syntaxOne.Count -ne 1 -or $syntaxOne[0] -notmatch '^sed:') {
        throw 'syntax failure one-match fixture lost collection semantics or contents'
    }
    Write-Host 'configure syntax fixture PASS: one match -> Count 1 and index 0 preserved'

    [string[]]$syntaxMany = Get-FixtureSyntaxFailures @(
        'awk: syntax error at source line 1',
        'configure: harmless note',
        'sed: expression #2 failed'
    )
    if ($syntaxMany.Count -ne 2) { throw "syntax failure many-match fixture expected Count 2, got $($syntaxMany.Count)" }
    if (($syntaxMany -join '|') -ne 'awk: syntax error at source line 1|sed: expression #2 failed') {
        throw 'syntax failure many-match fixture did not preserve match ordering'
    }
    Write-Host 'configure syntax fixture PASS: many matches -> exact Count and ordering preserved'
}
finally {
    Remove-Item -Recurse -Force -LiteralPath $Root -ErrorAction SilentlyContinue
}
