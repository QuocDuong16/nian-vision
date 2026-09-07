$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'windows-native.ps1')

function ConvertTo-NianBashSingleQuoted {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)][string]$Value)

    return "'" + $Value.Replace("'", "'\''") + "'"
}

function Normalize-NianMsysPathEntry {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)][string]$Path)

    $normalized = $Path.Trim().Trim('"').Replace('\', '/')
    if ($normalized.Length -ge 3 -and
        [char]::IsLetter($normalized[0]) -and
        $normalized[1] -eq ':' -and
        $normalized[2] -eq '/') {
        $drive = [char]::ToLowerInvariant($normalized[0])
        $normalized = "/$drive" + $normalized.Substring(2)
    }
    while ($normalized.Contains('//')) {
        $normalized = $normalized.Replace('//', '/')
    }
    while ($normalized.Length -gt 1 -and $normalized.EndsWith('/')) {
        $normalized = $normalized.Substring(0, $normalized.Length - 1)
    }
    return $normalized
}

function Get-NianForbiddenMsysToolRoots {
    [CmdletBinding()]
    param()

    return @(
        '/mingw64/bin',
        '/mingw32/bin',
        '/ucrt64/bin',
        '/clang64/bin',
        '/clang32/bin',
        '/clangarm64/bin',
        '/c/mingw64/bin',
        '/c/mingw32/bin',
        '/c/ucrt64/bin',
        '/c/clang64/bin',
        '/c/clang32/bin',
        '/c/clangarm64/bin',
        '/c/msys64/mingw64/bin',
        '/c/msys64/mingw32/bin',
        '/c/msys64/ucrt64/bin',
        '/c/msys64/clang64/bin',
        '/c/msys64/clang32/bin',
        '/c/msys64/clangarm64/bin'
    )
}

function Test-NianForbiddenMsysPathEntry {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)][string]$Path)

    $candidate = (Normalize-NianMsysPathEntry $Path).ToLowerInvariant()
    foreach ($root in Get-NianForbiddenMsysToolRoots) {
        $normalizedRoot = (Normalize-NianMsysPathEntry $root).ToLowerInvariant()
        if ($candidate -eq $normalizedRoot -or $candidate.StartsWith($normalizedRoot + '/', [System.StringComparison]::OrdinalIgnoreCase)) {
            return $true
        }
    }
    return $false
}

function Get-NianSanitizedMsysPathEntries {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string]$MsvcBinUnix,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][string[]]$InheritedEntries
    )

    $msvc = Normalize-NianMsysPathEntry $MsvcBinUnix
    if ([string]::IsNullOrWhiteSpace($msvc)) {
        throw 'selected MSVC path must not be empty'
    }

    $entries = [Collections.Generic.List[string]]::new()
    $removed = [Collections.Generic.List[string]]::new()
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)

    foreach ($entry in @($msvc, '/usr/bin')) {
        if ($seen.Add($entry)) { $entries.Add($entry) }
    }

    foreach ($raw in $InheritedEntries) {
        if ([string]::IsNullOrWhiteSpace($raw)) { continue }
        $entry = Normalize-NianMsysPathEntry $raw
        if ([string]::IsNullOrWhiteSpace($entry)) { continue }
        if (Test-NianForbiddenMsysPathEntry $entry) {
            $removed.Add($entry)
            continue
        }
        if ($seen.Add($entry)) { $entries.Add($entry) }
    }

    if ($entries.Count -lt 2 -or $entries[0] -ne $msvc -or $entries[1] -ne '/usr/bin') {
        throw 'controlled FFmpeg MSYS PATH lost selected MSVC or /usr/bin precedence'
    }

    return [pscustomobject]@{
        Entries = @($entries)
        Removed = @($removed)
        PathText = (@($entries) -join ':')
    }
}

function ConvertTo-NianMsysPath {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [string]$Cygpath = 'C:\msys64\usr\bin\cygpath.exe'
    )

    if (-not (Test-Path -LiteralPath $Cygpath -PathType Leaf)) {
        throw "required deterministic MSYS2 cygpath is unavailable: $Cygpath"
    }
    $converted = (Invoke-NianNative { & $Cygpath -u $Path } 'MSYS path conversion' | Out-String).Trim()
    if ([string]::IsNullOrWhiteSpace($converted)) {
        throw "cygpath returned an empty path for: $Path"
    }
    return Normalize-NianMsysPathEntry $converted
}

function New-NianFfmpegMsysEnvironment {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string]$MsvcBinWindows,
        [string]$InheritedWindowsPath = $env:PATH,
        [string]$Cygpath = 'C:\msys64\usr\bin\cygpath.exe'
    )

    $msvcUnix = ConvertTo-NianMsysPath -Path $MsvcBinWindows -Cygpath $Cygpath
    $converted = [Collections.Generic.List[string]]::new()
    foreach ($entry in @($InheritedWindowsPath -split [Regex]::Escape([IO.Path]::PathSeparator))) {
        if ([string]::IsNullOrWhiteSpace($entry)) { continue }
        $converted.Add((ConvertTo-NianMsysPath -Path $entry -Cygpath $Cygpath))
    }

    $sanitized = Get-NianSanitizedMsysPathEntries -MsvcBinUnix $msvcUnix -InheritedEntries @($converted)
    $quotedPath = ConvertTo-NianBashSingleQuoted $sanitized.PathText
    $preamble = "set -Eeuo pipefail`nexport PATH=$quotedPath`nhash -r"

    return [pscustomobject]@{
        MsvcBinUnix = $msvcUnix
        Entries = $sanitized.Entries
        Removed = $sanitized.Removed
        PathText = $sanitized.PathText
        BashPreamble = $preamble
    }
}
