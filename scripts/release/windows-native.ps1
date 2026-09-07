$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# PowerShell 7.3+ can translate a non-zero native exit code into an ErrorRecord.
# Keep the explicit LASTEXITCODE check below as the compatibility/fail-closed
# contract for older PowerShell hosts and for clarity in release scripts.
if (Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $true
}

function Invoke-NianNative {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true, Position = 0)][scriptblock]$Command,
        [Parameter(Position = 1)][string]$Label = 'native command'
    )

    & $Command
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        throw ("required native command failed with exit code {0}: {1}" -f $exitCode, $Label)
    }
}
