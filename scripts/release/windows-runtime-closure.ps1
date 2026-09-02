function Test-VcRedistributableDependency([string]$Name) {
    return $Name -match '^(VCRUNTIME|MSVCP|CONCRT)[0-9A-Z_]*\.DLL$'
}

function Find-VcRuntimeDependency([string]$Name, [string]$VcRedistRoot) {
    if ([string]::IsNullOrWhiteSpace($VcRedistRoot) -or -not (Test-Path $VcRedistRoot)) {
        return $null
    }
    $matches = @(Get-ChildItem $VcRedistRoot -Recurse -File -Filter $Name |
        Where-Object FullName -Match '[\\/]x64[\\/]' |
        Sort-Object FullName)
    if ($matches.Count -eq 0) { return $null }
    return $matches[0].FullName
}

function Resolve-WindowsDependency(
    [string]$Name,
    [string]$Runtime,
    [string]$System32,
    [string]$VcRedistRoot
) {
    $local = Join-Path $Runtime $Name
    if (Test-Path $local) {
        return [pscustomobject]@{ Kind = 'ApplicationLocal'; Source = $local }
    }

    # VC redistributables are application-owned for the per-user installer even
    # when the build runner also happens to expose a copy in System32.
    if (Test-VcRedistributableDependency $Name) {
        return [pscustomobject]@{
            Kind = 'Redistributable'
            Source = Find-VcRuntimeDependency $Name $VcRedistRoot
        }
    }

    $upper = $Name.ToUpperInvariant()
    if ($upper -match '^(API-MS-WIN-|EXT-MS-WIN-)') {
        return [pscustomobject]@{ Kind = 'System'; Source = $null }
    }
    if (Test-Path (Join-Path $System32 $Name)) {
        return [pscustomobject]@{ Kind = 'System'; Source = $null }
    }

    return [pscustomobject]@{ Kind = 'Unknown'; Source = $null }
}

function Stage-WindowsDependency(
    [string]$Name,
    [string]$RequiredBy,
    [string]$Runtime,
    [string]$System32,
    [string]$VcRedistRoot
) {
    $resolution = Resolve-WindowsDependency $Name $Runtime $System32 $VcRedistRoot
    switch ($resolution.Kind) {
        'ApplicationLocal' {
            return $resolution.Source
        }
        'Redistributable' {
            if (-not $resolution.Source) {
                throw "required Visual C++ runtime dependency is unavailable: $Name"
            }
            $local = Join-Path $Runtime $Name
            Copy-Item -Force $resolution.Source $local
            return $local
        }
        'System' {
            return $null
        }
        default {
            throw "application-owned Windows dependency is missing from stage: $Name required by $RequiredBy"
        }
    }
}
