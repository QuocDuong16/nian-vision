$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'windows-msvc-toolchain.ps1')

$vs = 'C:\Program Files\Microsoft Visual Studio\2022\Enterprise'
$toolset = '14.44.35207'

function Expect-PathPass([string]$Name, [string]$VsInstall, [string]$MsvcBin) {
    Assert-NianMsvcToolchainPath -VsInstall $VsInstall -MsvcBin $MsvcBin | Out-Null
    Write-Host "MSVC path fixture PASS: $Name"
}

function Expect-PathFail([string]$Name, [string]$VsInstall, [string]$MsvcBin) {
    $failed = $false
    try {
        Assert-NianMsvcToolchainPath -VsInstall $VsInstall -MsvcBin $MsvcBin | Out-Null
    }
    catch {
        $failed = $true
    }
    if (-not $failed) {
        throw "MSVC path fixture unexpectedly passed: $Name -> $MsvcBin"
    }
    Write-Host "MSVC path fixture rejected as expected: $Name"
}

Expect-PathPass 'exact RC8 hosted-runner path' $vs 'C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Tools\MSVC\14.44.35207\bin\HostX64\x64'
Expect-PathPass 'Hostx64 casing variant' $vs 'C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64'
Expect-PathPass 'case slash and trailing-separator variants' 'c:/PROGRAM FILES/Microsoft Visual Studio/2022/ENTERPRISE/' 'C:/program files/MICROSOFT VISUAL STUDIO/2022/enterprise/VC/TOOLS/msvc/14.44.35207/BIN/hostx64/X64/'
Expect-PathPass 'dynamic toolset version' $vs "$vs\VC\Tools\MSVC\99.88.777\bin\HOSTX64\X64"
Expect-PathPass 'Community edition selected root' 'C:\Program Files\Microsoft Visual Studio\2022\Community' 'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\MSVC\17.99.preview\bin\HostX64\x64'

Expect-PathFail 'x86 host and target' $vs "$vs\VC\Tools\MSVC\$toolset\bin\HostX86\x86"
Expect-PathFail 'x64 host with x86 target' $vs "$vs\VC\Tools\MSVC\$toolset\bin\HostX64\x86"
Expect-PathFail 'x86 host with x64 target' $vs "$vs\VC\Tools\MSVC\$toolset\bin\HostX86\x64"
Expect-PathFail 'MSYS directory' $vs 'C:\msys64\usr\bin'
Expect-PathFail 'wrong compiler toolchain tree' $vs "$vs\VC\Tools\Llvm\x64\bin"
Expect-PathFail 'different Visual Studio tree' $vs 'C:\some-other-tree\VC\Tools\MSVC\14.44.35207\bin\HostX64\x64'
Expect-PathFail 'missing target segment' $vs "$vs\VC\Tools\MSVC\$toolset\bin\HostX64"
Expect-PathFail 'extra segment' $vs "$vs\VC\Tools\MSVC\$toolset\bin\HostX64\x64\extra"

$validBin = "$vs\VC\Tools\MSVC\$toolset\bin\HostX64\x64"
Assert-NianMsvcToolAuthority `
    -VsInstall $vs `
    -ClPath "$validBin\cl.exe" `
    -LibPath "$validBin\lib.exe" `
    -LinkPath "$validBin\link.exe" | Out-Null
Write-Host 'MSVC tool authority fixture PASS: cl/lib/link same validated directory'

$splitFailed = $false
try {
    Assert-NianMsvcToolAuthority `
        -VsInstall $vs `
        -ClPath "$validBin\cl.exe" `
        -LibPath "$vs\VC\Tools\MSVC\$toolset\bin\HostX64\x86\lib.exe" `
        -LinkPath "$validBin\link.exe" | Out-Null
}
catch {
    $splitFailed = $true
}
if (-not $splitFailed) {
    throw 'MSVC split-directory authority fixture unexpectedly passed'
}
Write-Host 'MSVC tool authority fixture rejected split cl/lib/link directories as expected'
