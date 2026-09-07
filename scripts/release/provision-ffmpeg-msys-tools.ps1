$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'windows-native.ps1')
. (Join-Path $PSScriptRoot 'windows-bounded-process.ps1')
. (Join-Path $PSScriptRoot 'windows-ffmpeg-msys-environment.ps1')

if (-not $IsWindows) { throw 'FFmpeg MSYS provisioning requires a Windows runner' }

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$Contract = Get-Content (Join-Path $RepoRoot 'scripts/release/ffmpeg-windows-contract.json') -Raw | ConvertFrom-Json

$ExpectedPackages = @(
    [pscustomobject]@{
        Key = 'make'
        Name = 'make'
        Version = '4.4.1-3'
        Url = 'https://repo.msys2.org/msys/x86_64/make-4.4.1-3-x86_64.pkg.tar.zst'
        Sha256 = 'af0bdba17f06fe037f0194069adaa31a8fe45f1a11381501896aea1fae37bd5d'
        ArchiveName = 'make-4.4.1-3-x86_64.pkg.tar.zst'
        InstalledExecutable = 'C:\msys64\usr\bin\make.exe'
        MsysExecutable = '/usr/bin/make'
        VersionLine = 'GNU Make 4.4.1'
    },
    [pscustomobject]@{
        Key = 'diffutils'
        Name = 'diffutils'
        Version = '3.12-1'
        Url = 'https://mirror.msys2.org/msys/x86_64/diffutils-3.12-1-x86_64.pkg.tar.zst'
        Sha256 = '7902c8ce3d4dd69a0f5e98dc9d5c83c17b23314ba486169db57ef6e2835ce3b6'
        ArchiveName = 'diffutils-3.12-1-x86_64.pkg.tar.zst'
        InstalledExecutable = 'C:\msys64\usr\bin\cmp.exe'
        MsysExecutable = '/usr/bin/cmp'
        VersionLine = $null
    }
)

foreach ($expected in $ExpectedPackages) {
    $package = $Contract.provisionedMsysPackages.($expected.Key)
    if ($null -eq $package) { throw "pinned MSYS package contract is missing: $($expected.Key)" }
    foreach ($check in @(
        @{ Name = 'name'; Actual = $package.name; Expected = $expected.Name },
        @{ Name = 'version'; Actual = $package.version; Expected = $expected.Version },
        @{ Name = 'url'; Actual = $package.url; Expected = $expected.Url },
        @{ Name = 'sha256'; Actual = $package.sha256; Expected = $expected.Sha256 },
        @{ Name = 'installedExecutable'; Actual = $package.installedExecutable; Expected = $expected.InstalledExecutable },
        @{ Name = 'msysExecutable'; Actual = $package.msysExecutable; Expected = $expected.MsysExecutable }
    )) {
        if ($check.Actual -ne $check.Expected) {
            throw "pinned MSYS $($expected.Key) package contract drifted for $($check.Name)"
        }
    }
    if ($expected.VersionLine -and $package.versionLine -ne $expected.VersionLine) {
        throw "pinned MSYS $($expected.Key) package contract drifted for versionLine"
    }
}

$Bash = 'C:\msys64\usr\bin\bash.exe'
$Pacman = 'C:\msys64\usr\bin\pacman.exe'
$Cygpath = 'C:\msys64\usr\bin\cygpath.exe'
$Curl = Join-Path $env:SystemRoot 'System32\curl.exe'
foreach ($tool in @($Bash, $Pacman, $Cygpath, $Curl)) {
    if (-not (Test-Path -LiteralPath $tool -PathType Leaf)) {
        throw "required provisioning tool is unavailable: $tool"
    }
}

# Authoritative package metadata:
#   make 4.4.1-3      -> libintl, sh
#   diffutils 3.12-1  -> libiconv, libintl, sh
# Validate the union before any local package install so pacman -U never becomes
# an uncontrolled dependency-resolution path.
$dependencyFailures = [Collections.Generic.List[string]]::new()
foreach ($dependency in @('libiconv', 'libintl')) {
    try {
        $identity = (Invoke-NianNative { & $Pacman -Q $dependency } "query installed MSYS dependency $dependency" | Out-String).Trim()
        Write-Host "MSYS FFmpeg package dependency present: $identity"
    }
    catch {
        $dependencyFailures.Add($dependency)
    }
}
try {
    $bashIdentity = (Invoke-NianNative { & $Pacman -Q bash } 'query installed MSYS sh provider bash' | Out-String).Trim()
    Write-Host "MSYS FFmpeg sh provider present: $bashIdentity"
}
catch {
    $dependencyFailures.Add('bash (sh provider)')
}
if (-not (Test-Path -LiteralPath 'C:\msys64\usr\bin\sh.exe' -PathType Leaf)) {
    $dependencyFailures.Add('C:\msys64\usr\bin\sh.exe')
}
if ($dependencyFailures.Count -gt 0) {
    throw ("pinned MSYS package dependencies are missing; refusing package-manager dependency resolution: {0}" -f ($dependencyFailures -join ', '))
}

$DownloadRoot = Join-Path $env:RUNNER_TEMP 'nian-msys-packages'
New-Item -ItemType Directory -Force $DownloadRoot | Out-Null

foreach ($expected in $ExpectedPackages) {
    $Archive = Join-Path $DownloadRoot $expected.ArchiveName
    Remove-Item -Force $Archive -ErrorAction SilentlyContinue

    Write-Host ("---- Provision pinned MSYS2 package: {0} ----" -f $expected.Name)
    Write-Host "package version: $($expected.Version)"
    Write-Host "package source: $($expected.Url)"
    Write-Host "expected SHA-256: $($expected.Sha256)"

    $downloadTimer = [Diagnostics.Stopwatch]::StartNew()
    Invoke-NianBoundedProcess -FilePath $Curl `
        -ArgumentList @('--fail', '--silent', '--show-error', '--location', '--output', $Archive, $expected.Url) `
        -TimeoutSeconds 180 -Label "pinned MSYS2 $($expected.Name) download" | Out-Null
    $downloadTimer.Stop()
    $archiveFile = Get-Item -LiteralPath $Archive -ErrorAction Stop
    Write-Host ("downloaded byte count: {0}" -f $archiveFile.Length)
    Write-Host ("download elapsed: {0:n1}s" -f $downloadTimer.Elapsed.TotalSeconds)

    $VerifiedSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $Archive).Hash.ToLowerInvariant()
    Write-Host "verified SHA-256: $VerifiedSha256"
    if ($VerifiedSha256 -ne $expected.Sha256) {
        throw "pinned MSYS2 $($expected.Name) package SHA-256 mismatch; installation is blocked"
    }
    Write-Host 'SHA-256 verification: PASS'

    $ArchiveUnix = ConvertTo-NianMsysPath -Path $Archive -Cygpath $Cygpath
    $installCommand = "/usr/bin/pacman --noconfirm --needed -U $(ConvertTo-NianBashSingleQuoted $ArchiveUnix)"
    $installTimer = [Diagnostics.Stopwatch]::StartNew()
    Write-Host "install start: pinned local $($expected.Name) package only; dependencies prevalidated; no system upgrade"
    Invoke-NianBoundedProcess -FilePath $Bash `
        -ArgumentList @('--noprofile', '--norc', '-lc', $installCommand) `
        -TimeoutSeconds 180 -Label "pinned MSYS2 $($expected.Name) installation" | Out-Null
    $installTimer.Stop()
    Write-Host ("install elapsed: {0:n1}s" -f $installTimer.Elapsed.TotalSeconds)

    $installedIdentity = (Invoke-NianNative { & $Pacman -Q $expected.Name } "query installed MSYS2 $($expected.Name) package" | Out-String).Trim()
    if ($installedIdentity -ne "$($expected.Name) $($expected.Version)") {
        throw "installed MSYS2 $($expected.Name) package identity mismatch: $installedIdentity"
    }
    if (-not (Test-Path -LiteralPath $expected.InstalledExecutable -PathType Leaf)) {
        throw "pinned MSYS2 $($expected.Name) installation did not create $($expected.InstalledExecutable)"
    }
    if ($expected.VersionLine) {
        $installedExecutable = [string]$expected.InstalledExecutable
        $versionLine = ((Invoke-NianNative { & $installedExecutable --version } "pinned MSYS2 $($expected.Name) version") | Select-Object -First 1).Trim()
        if ($versionLine -ne $expected.VersionLine) {
            throw "pinned MSYS2 $($expected.Name) executable version mismatch: $versionLine"
        }
        Write-Host "executable version: $versionLine"
    }

    Write-Host "installed package identity: $installedIdentity"
    Write-Host "resulting executable path: $($expected.InstalledExecutable)"
    Remove-Item -Force $Archive -ErrorAction SilentlyContinue
}

Write-Host 'Pinned MSYS2 FFmpeg build-tool provisioning passed'
