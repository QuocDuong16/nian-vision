param(
    [Parameter(Mandatory = $true)][string]$Installer,
    [Parameter(Mandatory = $true)][string]$ExpectedVersion
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot "windows-native.ps1")
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
$Installer = (Resolve-Path $Installer).Path
$Isolation = Join-Path $env:RUNNER_TEMP ("nian-windows-install-smoke-" + [Guid]::NewGuid().ToString("N"))
$InstallRoot = Join-Path $Isolation "install"
$AppData = Join-Path $Isolation "appdata"
$LocalAppData = Join-Path $Isolation "localappdata"
$WebViewData = Join-Path $LocalAppData "webview2"
$Footage = Join-Path $Isolation "recordings"
$Node = (Get-Command node.exe).Source
New-Item -ItemType Directory -Force $AppData, $LocalAppData, $WebViewData, $Footage | Out-Null

function Wait-Exit([Diagnostics.Process]$Process, [int]$Seconds, [string]$Label) {
    if (-not $Process.WaitForExit($Seconds * 1000)) {
        try { $Process.Kill($true) } catch {}
        throw "$Label timed out"
    }
    if ($Process.ExitCode -ne 0) { throw "$Label exited with code $($Process.ExitCode)" }
}

function Run-Installer([string]$Label = 'NSIS silent install') {
    $process = Start-Process -FilePath $Installer -ArgumentList @('/S', "/D=$InstallRoot") -PassThru
    Wait-Exit $process 30 $Label
}

function Wait-ProcessGone([int]$ProcessId, [int]$Seconds, [string]$Label) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    while (Get-Process -Id $ProcessId -ErrorAction SilentlyContinue) {
        if ([DateTime]::UtcNow -ge $deadline) { throw "$Label timed out" }
        Start-Sleep -Milliseconds 100
    }
}

function Start-DesktopContainmentSmoke([string]$Desktop) {
    $marker = Join-Path $Isolation ("desktop-ready-" + [Guid]::NewGuid().ToString("N") + ".txt")
    $containment = Join-Path $Isolation ("containment-worker-" + [Guid]::NewGuid().ToString("N") + ".txt")
    $power = Join-Path $Isolation ("power-subscription-" + [Guid]::NewGuid().ToString("N") + ".txt")
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Desktop
    $info.UseShellExecute = $false
    $info.Environment['APPDATA'] = $AppData
    $info.Environment['LOCALAPPDATA'] = $LocalAppData
    $info.Environment['USERPROFILE'] = $Isolation
    $info.Environment['WEBVIEW2_USER_DATA_FOLDER'] = $WebViewData
    $info.Environment['NIAN_DESKTOP_STARTUP_SMOKE_FILE'] = $marker
    $info.Environment['NIAN_DESKTOP_CONTAINMENT_SMOKE_FILE'] = $containment
    $info.Environment['NIAN_DESKTOP_POWER_SMOKE_FILE'] = $power
    [void]$info.Environment.Remove('NIAN_FFMPEG_LIB_DIR')
    [void]$info.Environment.Remove('LD_LIBRARY_PATH')
    $process = [Diagnostics.Process]::Start($info)
    # Hosted Windows can cold-start the evergreen WebView2 runtime substantially
    # slower than a warm developer machine. Keep the smoke bounded, but do not
    # make release correctness depend on an unrealistically tight 15 second race.
    $deadline = [DateTime]::UtcNow.AddSeconds(45)
    while (-not (Test-Path $marker) -or -not (Test-Path $containment) -or -not (Test-Path $power)) {
        if ($process.HasExited) { throw "installed desktop exited before startup readiness (code $($process.ExitCode))" }
        if ([DateTime]::UtcNow -ge $deadline) {
            $startupReady = Test-Path $marker
            $containmentReady = Test-Path $containment
            $powerReady = Test-Path $power
            try { $process.Kill($true) } catch {}
            throw "installed desktop startup readiness timed out after 45s (startup=$startupReady, power=$powerReady, containment=$containmentReady)"
        }
        Start-Sleep -Milliseconds 100
    }
    if ((Get-Content $marker -Raw).Trim() -ne 'desktop_startup_ready') {
        try { $process.Kill($true) } catch {}
        throw "installed desktop wrote an invalid readiness marker"
    }
    if ((Get-Content $power -Raw).Trim() -ne 'windows_power_subscription_ready') {
        try { $process.Kill($true) } catch {}
        throw "installed desktop did not prove the native Windows power subscription"
    }
    $workerProcessId = [int](Get-Content $containment -Raw).Trim()
    if (-not (Get-Process -Id $workerProcessId -ErrorAction SilentlyContinue)) {
        try { $process.Kill($true) } catch {}
        throw "installed desktop containment smoke worker is not alive"
    }
    if ($process.HasExited) { throw "installed desktop crashed immediately after readiness" }
    return [pscustomobject]@{ Process = $process; WorkerProcessId = $workerProcessId }
}

function Stop-DesktopAndVerifyContainment($Session) {
    $Session.Process.Kill($true)
    $Session.Process.WaitForExit()
    try {
        Wait-ProcessGone $Session.WorkerProcessId 5 'Windows Job Object worker reap'
    }
    catch {
        Stop-Process -Id $Session.WorkerProcessId -Force -ErrorAction SilentlyContinue
        throw "Windows Job Object did not reap the installed media worker after hard desktop death"
    }
}

function Run-DesktopSmoke([string]$Desktop) {
    $session = Start-DesktopContainmentSmoke $Desktop
    Stop-DesktopAndVerifyContainment $session
}

function Find-SettingsDatabase {
    $matches = @(
        Get-ChildItem $AppData, $LocalAppData -Recurse -File -Filter settings.sqlite3 -ErrorAction SilentlyContinue
    )
    if ($matches.Count -ne 1) { throw "expected exactly one isolated settings.sqlite3 after desktop startup, found $($matches.Count)" }
    return $matches[0].FullName
}

function Verify-InstalledLayout {
    $required = @(
        "nian-desktop.exe",
        "nian-media-worker.exe",
        "avformat-62.dll",
        "avcodec-62.dll",
        "avutil-60.dll",
        "release-evidence\THIRD_PARTY_NOTICES.txt",
        "release-evidence\FFMPEG-LGPL-2.1.txt",
        "release-evidence\FFMPEG_BUILD_FLAGS.txt",
        "release-evidence\FFMPEG_CONFIG.h",
        "release-evidence\BUILD_METADATA.json"
    )
    foreach ($relative in $required) {
        if (-not (Test-Path (Join-Path $InstallRoot $relative))) { throw "installed Windows layout is missing: $relative" }
    }

    $desktopSource = Join-Path $RepoRoot 'target/x86_64-pc-windows-msvc/release/nian-desktop.exe'
    if ((Get-FileHash -Algorithm SHA256 $desktopSource).Hash -ne (Get-FileHash -Algorithm SHA256 (Join-Path $InstallRoot 'nian-desktop.exe')).Hash) {
        throw "installed desktop bytes differ from the exact signed bundle input"
    }
    $stageRuntime = Join-Path $RepoRoot 'dist/windows-x86_64/runtime'
    foreach ($source in Get-ChildItem $stageRuntime -File | Where-Object { $_.Extension -ieq '.dll' -or $_.Name -ieq 'nian-media-worker.exe' }) {
        $installed = Join-Path $InstallRoot $source.Name
        if (-not (Test-Path $installed)) { throw "installed runtime closure is missing: $($source.Name)" }
        if ((Get-FileHash -Algorithm SHA256 $source.FullName).Hash -ne (Get-FileHash -Algorithm SHA256 $installed).Hash) {
            throw "installed runtime bytes differ from staged release input: $($source.Name)"
        }
    }
}

function Scan-InstalledSecrets {
    if (-not [string]::IsNullOrEmpty($env:NIAN_RELEASE_SECRET_SENTINEL)) {
        Invoke-NianNative { & $Node (Join-Path $RepoRoot "scripts/release/scan-release-secrets.mjs") $InstallRoot }
    }
}

try {
    Run-Installer
    Verify-InstalledLayout
    Scan-InstalledSecrets

    $oldOverride = $env:NIAN_FFMPEG_LIB_DIR
    $oldPath = $env:PATH
    try {
        Remove-Item Env:NIAN_FFMPEG_LIB_DIR -ErrorAction SilentlyContinue
        $env:PATH = $InstallRoot
        Invoke-NianNative { & $Node (Join-Path $RepoRoot "scripts/release/stage-runtime-smoke.mjs") `
            (Join-Path $InstallRoot "nian-media-worker.exe") (Join-Path $Isolation "worker-smoke") }
    }
    finally {
        $env:PATH = $oldPath
        if ($null -ne $oldOverride) { $env:NIAN_FFMPEG_LIB_DIR = $oldOverride }
    }

    Run-DesktopSmoke (Join-Path $InstallRoot "nian-desktop.exe")
    $freshRun = Get-ItemPropertyValue -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'Nian Vision' -ErrorAction SilentlyContinue
    if ($freshRun) { throw "fresh install unexpectedly enabled launch-at-login" }

    $settings = Find-SettingsDatabase
    Invoke-NianNative { cargo.exe run --quiet -p nian-settings-fixture -- create $settings $Footage }

    $runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
    New-Item $runKey -Force | Out-Null
    Set-ItemProperty -Path $runKey -Name 'Nian Vision' -Value '"C:\stale-nian-vision\nian-desktop.exe" --startup-hidden'

    # Direct same-version reinstall while the installed desktop is running proves
    # the NSIS/Tauri app-running path. The desktop owns the worker through its Job
    # Object, so installer file replacement must succeed without a global worker kill.
    $upgradeSession = Start-DesktopContainmentSmoke (Join-Path $InstallRoot "nian-desktop.exe")
    $oldDesktopProcessId = $upgradeSession.Process.Id
    $oldWorkerProcessId = $upgradeSession.WorkerProcessId
    Run-Installer 'NSIS direct reinstall with running desktop'
    Wait-ProcessGone $oldDesktopProcessId 10 'old desktop shutdown during direct reinstall'
    Wait-ProcessGone $oldWorkerProcessId 10 'owned worker shutdown during direct reinstall'
    Verify-InstalledLayout
    Scan-InstalledSecrets
    if (Get-Process -Id $oldWorkerProcessId -ErrorAction SilentlyContinue) {
        throw "owned worker restarted or survived during installer file replacement"
    }
    Run-DesktopSmoke (Join-Path $InstallRoot "nian-desktop.exe")
    Invoke-NianNative { cargo.exe run --quiet -p nian-settings-fixture -- verify $settings $Footage }

    $runValue = Get-ItemPropertyValue `
        -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' `
        -Name 'Nian Vision' -ErrorAction SilentlyContinue
    if (-not $runValue) { throw "M7 launch-at-login reconciliation did not restore the Windows Run entry" }
    $desktopPath = Join-Path $InstallRoot "nian-desktop.exe"
    if ($runValue -notmatch [Regex]::Escape($desktopPath) -or $runValue -match 'stale-nian-vision') {
        throw "M7 launch-at-login reconciliation did not repair the executable path"
    }

    $uninstallers = @(Get-ChildItem $InstallRoot -File -Filter '*uninstall*.exe')
    if ($uninstallers.Count -ne 1) { throw "expected exactly one NSIS uninstaller, found $($uninstallers.Count)" }
    $uninstall = Start-Process -FilePath $uninstallers[0].FullName -ArgumentList '/S' -PassThru -Wait
    if ($uninstall.ExitCode -ne 0) { throw "NSIS silent uninstall failed with code $($uninstall.ExitCode)" }

    foreach ($binary in @("nian-desktop.exe", "nian-media-worker.exe", "avformat-62.dll", "avcodec-62.dll", "avutil-60.dll")) {
        if (Test-Path (Join-Path $InstallRoot $binary)) { throw "uninstall left application binary behind: $binary" }
    }
    if (-not (Test-Path $settings)) { throw "uninstall deleted authoritative settings.sqlite3" }
    if (-not (Test-Path (Join-Path $Footage "preserve-me.mkv"))) { throw "uninstall deleted recording footage" }
    $staleRun = Get-ItemPropertyValue `
        -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' `
        -Name 'Nian Vision' -ErrorAction SilentlyContinue
    if ($staleRun) { throw "uninstall left stale Nian Vision launch-at-login registration" }
    Write-Host "Windows NSIS install/upgrade/uninstall smoke passed for $ExpectedVersion"
}
finally {
    Remove-Item -Recurse -Force $Isolation -ErrorAction SilentlyContinue
}
