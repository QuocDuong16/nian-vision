$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'windows-native.ps1')
. (Join-Path $PSScriptRoot 'windows-bounded-process.ps1')
. (Join-Path $PSScriptRoot 'windows-ffmpeg-msys-environment.ps1')

if (-not $IsWindows) { throw 'FFmpeg MSYS provisioning requires a Windows runner' }

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$Contract = Get-Content (Join-Path $RepoRoot 'scripts/release/ffmpeg-windows-contract.json') -Raw | ConvertFrom-Json
$Package = $Contract.provisionedMsysPackages.make

$ExpectedName = 'make'
$ExpectedVersion = '4.4.1-3'
$ExpectedUrl = 'https://repo.msys2.org/msys/x86_64/make-4.4.1-3-x86_64.pkg.tar.zst'
$ExpectedSha256 = 'af0bdba17f06fe037f0194069adaa31a8fe45f1a11381501896aea1fae37bd5d'
$ExpectedExecutable = 'C:\msys64\usr\bin\make.exe'
$ExpectedMsysExecutable = '/usr/bin/make'
$ExpectedVersionLine = 'GNU Make 4.4.1'

foreach ($check in @(
    @{ Name = 'name'; Actual = $Package.name; Expected = $ExpectedName },
    @{ Name = 'version'; Actual = $Package.version; Expected = $ExpectedVersion },
    @{ Name = 'url'; Actual = $Package.url; Expected = $ExpectedUrl },
    @{ Name = 'sha256'; Actual = $Package.sha256; Expected = $ExpectedSha256 },
    @{ Name = 'installedExecutable'; Actual = $Package.installedExecutable; Expected = $ExpectedExecutable },
    @{ Name = 'msysExecutable'; Actual = $Package.msysExecutable; Expected = $ExpectedMsysExecutable },
    @{ Name = 'versionLine'; Actual = $Package.versionLine; Expected = $ExpectedVersionLine }
)) {
    if ($check.Actual -ne $check.Expected) {
        throw "pinned MSYS make package contract drifted for $($check.Name)"
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

$dependencyFailures = [Collections.Generic.List[string]]::new()
foreach ($dependency in @('bash', 'libintl')) {
    try {
        $identity = (Invoke-NianNative { & $Pacman -Q $dependency } "query installed MSYS dependency $dependency" | Out-String).Trim()
        Write-Host "MSYS make dependency present: $identity"
    }
    catch {
        $dependencyFailures.Add($dependency)
    }
}
if (-not (Test-Path -LiteralPath 'C:\msys64\usr\bin\sh.exe' -PathType Leaf)) { $dependencyFailures.Add('sh provider') }
if ($dependencyFailures.Count -gt 0) {
    throw ("pinned make dependencies are missing; refusing package-manager dependency resolution: {0}" -f ($dependencyFailures -join ', '))
}

$DownloadRoot = Join-Path $env:RUNNER_TEMP 'nian-msys-packages'
New-Item -ItemType Directory -Force $DownloadRoot | Out-Null
$Archive = Join-Path $DownloadRoot 'make-4.4.1-3-x86_64.pkg.tar.zst'
Remove-Item -Force $Archive -ErrorAction SilentlyContinue

Write-Host '---- Provision pinned MSYS2 GNU make ----'
Write-Host "package name: $ExpectedName"
Write-Host "package version: $ExpectedVersion"
Write-Host "package source: $ExpectedUrl"
Write-Host "expected SHA-256: $ExpectedSha256"

$downloadTimer = [Diagnostics.Stopwatch]::StartNew()
Invoke-NianBoundedProcess -FilePath $Curl `
    -ArgumentList @('--fail', '--silent', '--show-error', '--location', '--output', $Archive, $ExpectedUrl) `
    -TimeoutSeconds 180 -Label 'pinned MSYS2 GNU make download' | Out-Null
$downloadTimer.Stop()
$archiveFile = Get-Item -LiteralPath $Archive -ErrorAction Stop
Write-Host ("downloaded byte count: {0}" -f $archiveFile.Length)
Write-Host ("download elapsed: {0:n1}s" -f $downloadTimer.Elapsed.TotalSeconds)

$VerifiedSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $Archive).Hash.ToLowerInvariant()
Write-Host "verified SHA-256: $VerifiedSha256"
if ($VerifiedSha256 -ne $ExpectedSha256) {
    throw 'pinned MSYS2 GNU make package SHA-256 mismatch; installation is blocked'
}
Write-Host 'SHA-256 verification: PASS'

$ArchiveUnix = ConvertTo-NianMsysPath -Path $Archive -Cygpath $Cygpath
$installCommand = "/usr/bin/pacman --noconfirm --needed -U $(ConvertTo-NianBashSingleQuoted $ArchiveUnix)"
$installTimer = [Diagnostics.Stopwatch]::StartNew()
Write-Host 'install start: pinned local package only; dependencies prevalidated; no system upgrade'
Invoke-NianBoundedProcess -FilePath $Bash `
    -ArgumentList @('--noprofile', '--norc', '-lc', $installCommand) `
    -TimeoutSeconds 180 -Label 'pinned MSYS2 GNU make installation' | Out-Null
$installTimer.Stop()
Write-Host ("install elapsed: {0:n1}s" -f $installTimer.Elapsed.TotalSeconds)

$installedIdentity = (Invoke-NianNative { & $Pacman -Q make } 'query installed MSYS2 GNU make package' | Out-String).Trim()
if ($installedIdentity -ne "make $ExpectedVersion") { throw "installed MSYS2 GNU make package identity mismatch: $installedIdentity" }
if (-not (Test-Path -LiteralPath $ExpectedExecutable -PathType Leaf)) { throw "pinned GNU make installation did not create $ExpectedExecutable" }
$versionLine = ((Invoke-NianNative { & $ExpectedExecutable --version } 'pinned MSYS2 GNU make version') | Select-Object -First 1).Trim()
if ($versionLine -ne $ExpectedVersionLine) { throw "pinned GNU make executable version mismatch: $versionLine" }

Write-Host "installed package identity: $installedIdentity"
Write-Host "resulting executable path: $ExpectedExecutable"
Write-Host "executable version: $versionLine"
Remove-Item -Force $Archive -ErrorAction SilentlyContinue
Write-Host 'Pinned MSYS2 GNU make provisioning passed'
