$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'windows-native.ps1')
. (Join-Path $PSScriptRoot 'windows-bounded-process.ps1')
. (Join-Path $PSScriptRoot 'windows-ffmpeg-msys-environment.ps1')

function ConvertTo-NianLfText {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)][AllowEmptyString()][string]$Text)

    return $Text.Replace("`r`n", "`n").Replace("`r", "`n")
}

function Assert-NianBashScriptResolved {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)][AllowEmptyString()][string]$Script)

    $unresolved = @([Regex]::Matches($Script, '__[A-Z][A-Z0-9_]*__') | ForEach-Object { $_.Value } | Sort-Object -Unique)
    if ($unresolved.Count -gt 0) {
        throw ("generated Bash script contains unresolved placeholders: {0}" -f ($unresolved -join ', '))
    }
}

function Write-NianBashScriptFile {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string]$Directory,
        [Parameter(Mandatory = $true)][string]$FileName,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Script
    )

    if ([IO.Path]::GetFileName($FileName) -ne $FileName -or -not $FileName.EndsWith('.sh', [StringComparison]::OrdinalIgnoreCase)) {
        throw "generated Bash script filename must be one deterministic .sh leaf name: $FileName"
    }

    $normalized = ConvertTo-NianLfText $Script
    Assert-NianBashScriptResolved $normalized
    New-Item -ItemType Directory -Force -Path $Directory | Out-Null
    $path = Join-Path $Directory $FileName
    $encoding = [Text.UTF8Encoding]::new($false)
    [IO.File]::WriteAllText($path, $normalized, $encoding)

    $bytes = [IO.File]::ReadAllBytes($path)
    if ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF) {
        throw "generated Bash script unexpectedly contains a UTF-8 BOM: $path"
    }
    return $path
}

function Invoke-NianBashScript {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string]$Bash,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Script,
        [Parameter(Mandatory = $true)][string]$FileName,
        [Parameter(Mandatory = $true)][string]$Label,
        [string]$TempRoot = $env:RUNNER_TEMP,
        [string]$Cygpath = 'C:\msys64\usr\bin\cygpath.exe',
        [ValidateRange(0, 86400)][int]$TimeoutSeconds = 0
    )

    if ([string]::IsNullOrWhiteSpace($TempRoot)) {
        throw "generated Bash script requires a bounded temporary root: $Label"
    }
    if (-not (Test-Path -LiteralPath $Bash -PathType Leaf)) {
        throw "required Bash executable is unavailable: $Bash"
    }

    $directory = Join-Path $TempRoot ("nian-bash-script-" + [Guid]::NewGuid().ToString('N'))
    $scriptPath = $null
    try {
        $scriptPath = Write-NianBashScriptFile -Directory $directory -FileName $FileName -Script $Script
        $scriptInvocationPath = if ($IsWindows) {
            ConvertTo-NianMsysPath -Path $scriptPath -Cygpath $Cygpath
        }
        else {
            $scriptPath
        }
        Write-Host ("generated Bash script: {0}" -f $scriptPath)

        try {
            $null = Invoke-NianBoundedProcess -FilePath $Bash `
                -ArgumentList @('--noprofile', '--norc', '-n', $scriptInvocationPath) `
                -TimeoutSeconds 30 `
                -Label "$Label syntax validation"
        }
        catch {
            Write-Host ("Bash syntax validation failed for generated script: {0}" -f $scriptPath)
            throw
        }

        if ($TimeoutSeconds -gt 0) {
            return Invoke-NianBoundedProcess -FilePath $Bash `
                -ArgumentList @('--noprofile', '--norc', $scriptInvocationPath) `
                -TimeoutSeconds $TimeoutSeconds `
                -Label $Label
        }

        Invoke-NianNative { & $Bash --noprofile --norc $scriptInvocationPath } $Label
    }
    finally {
        Remove-Item -Recurse -Force -LiteralPath $directory -ErrorAction SilentlyContinue
    }
}
