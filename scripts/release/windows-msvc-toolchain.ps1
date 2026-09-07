function ConvertTo-NianNormalizedWindowsPath {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)][string]$Path)

    if ([string]::IsNullOrWhiteSpace($Path)) {
        throw 'Windows path must not be empty'
    }

    $normalized = $Path.Trim().Replace('/', '\')
    while ($normalized.Length -gt 3 -and $normalized.EndsWith('\')) {
        $normalized = $normalized.Substring(0, $normalized.Length - 1)
    }
    return $normalized
}

function Get-NianWindowsPathParent {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)][string]$Path)

    $normalized = ConvertTo-NianNormalizedWindowsPath $Path
    $separator = $normalized.LastIndexOf('\')
    if ($separator -le 0) {
        throw "Windows path has no parent directory: $Path"
    }
    return $normalized.Substring(0, $separator)
}

function Get-NianWindowsPathLeaf {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)][string]$Path)

    $normalized = ConvertTo-NianNormalizedWindowsPath $Path
    $separator = $normalized.LastIndexOf('\')
    if ($separator -lt 0 -or $separator -eq ($normalized.Length - 1)) {
        throw "Windows path has no leaf name: $Path"
    }
    return $normalized.Substring($separator + 1)
}

function Assert-NianMsvcToolchainPath {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string]$VsInstall,
        [Parameter(Mandatory = $true)][string]$MsvcBin
    )

    $normalizedVs = ConvertTo-NianNormalizedWindowsPath $VsInstall
    $normalizedBin = ConvertTo-NianNormalizedWindowsPath $MsvcBin
    $vcRoot = ConvertTo-NianNormalizedWindowsPath ($normalizedVs + '\VC\Tools\MSVC')
    $vcPrefix = $vcRoot + '\'

    if (-not $normalizedBin.StartsWith($vcPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "MSVC tool directory is outside the selected Visual Studio VC toolchain: $normalizedBin"
    }

    $relative = $normalizedBin.Substring($vcPrefix.Length)
    $segments = @($relative.Split([char]'\', [System.StringSplitOptions]::None))
    if ($segments.Count -ne 4) {
        throw "MSVC tool directory must have <toolset-version>\\bin\\HostX64\\x64 beneath VC\\Tools\\MSVC: $normalizedBin"
    }
    if ([string]::IsNullOrWhiteSpace($segments[0]) -or $segments[0] -eq '.' -or $segments[0] -eq '..') {
        throw "MSVC toolset version directory is invalid: $normalizedBin"
    }

    $equals = [System.StringComparer]::OrdinalIgnoreCase
    if (-not $equals.Equals($segments[1], 'bin')) {
        throw "MSVC tool directory is not beneath the bin segment: $normalizedBin"
    }
    if (-not $equals.Equals($segments[2], 'HostX64')) {
        throw "MSVC host tool architecture must be HostX64: $normalizedBin"
    }
    if (-not $equals.Equals($segments[3], 'x64')) {
        throw "MSVC target architecture must be x64: $normalizedBin"
    }

    return $normalizedBin
}

function Assert-NianMsvcToolAuthority {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string]$VsInstall,
        [Parameter(Mandatory = $true)][string]$ClPath,
        [Parameter(Mandatory = $true)][string]$LibPath,
        [Parameter(Mandatory = $true)][string]$LinkPath
    )

    $cl = ConvertTo-NianNormalizedWindowsPath $ClPath
    $lib = ConvertTo-NianNormalizedWindowsPath $LibPath
    $link = ConvertTo-NianNormalizedWindowsPath $LinkPath
    $equals = [System.StringComparer]::OrdinalIgnoreCase

    foreach ($entry in @(
        @{ Name = 'cl.exe'; Path = $cl },
        @{ Name = 'lib.exe'; Path = $lib },
        @{ Name = 'link.exe'; Path = $link }
    )) {
        if (-not $equals.Equals((Get-NianWindowsPathLeaf $entry.Path), $entry.Name)) {
            throw "MSVC tool path does not end in $($entry.Name): $($entry.Path)"
        }
    }

    $msvcBin = Get-NianWindowsPathParent $cl
    foreach ($path in @($lib, $link)) {
        if (-not $equals.Equals((Get-NianWindowsPathParent $path), $msvcBin)) {
            throw "MSVC cl.exe, lib.exe, and link.exe must resolve from the same directory: $cl; $lib; $link"
        }
    }

    $validatedBin = Assert-NianMsvcToolchainPath -VsInstall $VsInstall -MsvcBin $msvcBin
    return [pscustomobject]@{
        VsInstall = ConvertTo-NianNormalizedWindowsPath $VsInstall
        MsvcBin = $validatedBin
        ClPath = $cl
        LibPath = $lib
        LinkPath = $link
    }
}

function Import-NianVsDevEnvironment {
    [CmdletBinding()]
    param()

    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) {
        throw 'vswhere.exe is unavailable'
    }

    $install = (Invoke-NianNative { & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath } 'Visual Studio discovery' | Out-String).Trim()
    if (-not $install) {
        throw 'Visual Studio C++ build tools are unavailable'
    }
    $install = [IO.Path]::GetFullPath($install)

    $vsdev = [IO.Path]::GetFullPath((Join-Path $install 'Common7\Tools\VsDevCmd.bat'))
    if (-not (Test-Path -LiteralPath $vsdev -PathType Leaf)) {
        throw 'VsDevCmd.bat is unavailable'
    }

    $lines = Invoke-NianNative { cmd.exe /d /s /c "`"$vsdev`" -arch=amd64 -host_arch=amd64 >nul && set" } 'Visual Studio amd64 developer environment'
    foreach ($line in $lines) {
        $split = $line.IndexOf('=')
        if ($split -gt 0) {
            [Environment]::SetEnvironmentVariable($line.Substring(0, $split), $line.Substring($split + 1), 'Process')
        }
    }

    return $install
}

function Resolve-NianMsvcToolchain {
    [CmdletBinding()]
    param()

    $vsInstall = Import-NianVsDevEnvironment
    $tools = [ordered]@{}
    foreach ($name in @('cl.exe', 'lib.exe', 'link.exe')) {
        $command = Get-Command $name -CommandType Application -ErrorAction Stop | Select-Object -First 1
        $path = [IO.Path]::GetFullPath($command.Source)
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "resolved MSVC tool does not exist: $name -> $path"
        }
        $tools[$name] = $path
    }

    return Assert-NianMsvcToolAuthority `
        -VsInstall $vsInstall `
        -ClPath $tools['cl.exe'] `
        -LibPath $tools['lib.exe'] `
        -LinkPath $tools['link.exe']
}
