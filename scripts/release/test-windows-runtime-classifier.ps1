$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'windows-runtime-closure.ps1')

$root = Join-Path ([IO.Path]::GetTempPath()) ("nian-vc-classifier-" + [Guid]::NewGuid().ToString('N'))
$runtime = Join-Path $root 'runtime'
$system32 = Join-Path $root 'System32'
$redist = Join-Path $root 'VC/Redist/MSVC/14.0/x64/Microsoft.VC143.CRT'
New-Item -ItemType Directory -Force $runtime, $system32, $redist | Out-Null

try {
    foreach ($name in @('VCRUNTIME140.dll', 'MSVCP140.dll', 'VCRUNTIME140_1.dll', 'CONCRT140.dll')) {
        Set-Content -NoNewline -Path (Join-Path $system32 $name) -Value 'runner-system-copy'
        Set-Content -NoNewline -Path (Join-Path $redist $name) -Value "redist-$name"

        $resolution = Resolve-WindowsDependency $name $runtime $system32 $redist
        if ($resolution.Kind -ne 'Redistributable') {
            throw "$name was not classified as a VC redistributable before System32"
        }
        if ($resolution.Source -ne (Join-Path $redist $name)) {
            throw "$name did not resolve from the configured VC redist source"
        }

        $staged = Stage-WindowsDependency $name 'nian-media-worker.exe' $runtime $system32 $redist
        if ($staged -ne (Join-Path $runtime $name) -or -not (Test-Path $staged)) {
            throw "$name was not copied application-local"
        }
        if ((Get-Content $staged -Raw) -ne "redist-$name") {
            throw "$name staged bytes did not come from the VC redist source"
        }
    }

    $apiSet = Resolve-WindowsDependency 'API-MS-WIN-CORE-FILE-L1-1-0.DLL' $runtime $system32 $redist
    if ($apiSet.Kind -ne 'System') { throw 'API-MS-WIN dependency was not classified as OS-provided' }

    $unknown = Resolve-WindowsDependency 'NOT-A-SYSTEM-RUNTIME.DLL' $runtime $system32 $redist
    if ($unknown.Kind -ne 'Unknown') { throw 'unknown dependency was not rejected by classification' }

    Write-Host 'Windows VC runtime classifier/staging regression passed'
}
finally {
    Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
}
