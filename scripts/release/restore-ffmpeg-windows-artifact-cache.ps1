$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Restore-NianWindowsFfmpegArtifactCache {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ArtifactName,
        [Parameter(Mandatory = $true)]
        [string]$OutputDir
    )

    if (-not $env:GH_TOKEN) { throw "GH_TOKEN is required for cross-tag FFmpeg artifact cache lookup" }
    if (-not $env:GITHUB_REPOSITORY) { throw "GITHUB_REPOSITORY is required for cross-tag FFmpeg artifact cache lookup" }
    if (-not $env:RUNNER_TEMP) { throw "RUNNER_TEMP is required for cross-tag FFmpeg artifact cache lookup" }

    $api = "https://api.github.com"
    $headers = @{
        Accept = "application/vnd.github+json"
        Authorization = "Bearer $($env:GH_TOKEN)"
        "X-GitHub-Api-Version" = "2022-11-28"
        "User-Agent" = "nian-vision-release-cache"
    }
    $repo = $env:GITHUB_REPOSITORY
    $workflow = Invoke-RestMethod -Headers $headers -Uri "$api/repos/$repo/actions/workflows/release.yml"
    if (-not $workflow.id) { throw "unable to resolve Production Release workflow identity" }

    $encodedName = [Uri]::EscapeDataString($ArtifactName)
    $response = Invoke-RestMethod -Headers $headers -Uri "$api/repos/$repo/actions/artifacts?name=$encodedName&per_page=100"
    $artifacts = @($response.artifacts | Where-Object { -not $_.expired } | Sort-Object created_at -Descending)
    if ($artifacts.Count -eq 0) {
        Write-Host "FFmpeg cross-tag artifact cache hit: false"
        return $false
    }

    $attempt = 0
    foreach ($artifact in $artifacts) {
        $attempt += 1
        if (-not $artifact.workflow_run -or -not $artifact.workflow_run.id) { continue }
        $run = Invoke-RestMethod -Headers $headers -Uri "$api/repos/$repo/actions/runs/$($artifact.workflow_run.id)"
        if ($run.workflow_id -ne $workflow.id) { continue }
        if ($run.event -ne "push" -or $run.conclusion -ne "success") { continue }
        if (-not $run.head_repository -or $run.head_repository.full_name -ne $repo) { continue }
        if ($run.id -eq [int64]$env:GITHUB_RUN_ID) { continue }

        $tempRoot = Join-Path $env:RUNNER_TEMP ("nian-ffmpeg-artifact-cache-" + [Guid]::NewGuid().ToString("N"))
        $zip = Join-Path $tempRoot "cache.zip"
        $extractRoot = Join-Path $tempRoot "extract"
        New-Item -ItemType Directory -Force $extractRoot | Out-Null
        try {
            Write-Host "Trying validated FFmpeg cross-tag artifact cache from successful Production Release run $($run.id)"
            Invoke-WebRequest -Headers $headers -Uri "$api/repos/$repo/actions/artifacts/$($artifact.id)/zip" -OutFile $zip
            Expand-Archive -Path $zip -DestinationPath $extractRoot -Force
            Remove-Item -Recurse -Force $OutputDir -ErrorAction SilentlyContinue
            New-Item -ItemType Directory -Force $OutputDir | Out-Null
            Get-ChildItem -Force $extractRoot | Copy-Item -Destination $OutputDir -Recurse -Force
            & (Join-Path $PSScriptRoot "validate-ffmpeg-windows.ps1") -OutputDir $OutputDir | ForEach-Object { Write-Host $_ }
            Write-Host "FFmpeg cross-tag artifact cache hit: true"
            return $true
        }
        catch {
            Write-Warning "cross-tag FFmpeg artifact cache candidate failed validation and was discarded: $($_.Exception.Message)"
            Remove-Item -Recurse -Force $OutputDir -ErrorAction SilentlyContinue
        }
        finally {
            Remove-Item -Recurse -Force $tempRoot -ErrorAction SilentlyContinue
        }
    }

    Write-Host "FFmpeg cross-tag artifact cache hit: false"
    return $false
}
