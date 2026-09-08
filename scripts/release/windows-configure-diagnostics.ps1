function Get-NianConfigureDiagnostics {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)][string]$LiteralPath)

    if (Test-Path -LiteralPath $LiteralPath -PathType Leaf) {
        Get-Content -LiteralPath $LiteralPath
    }
}

function Get-NianConfigureSyntaxFailures {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)][AllowEmptyCollection()][string[]]$Diagnostics)

    $Diagnostics | Where-Object {
        $_ -match '(?i)(?:sed|awk):.*(?:unterminated|syntax error|expression #[0-9]+)'
    }
}
