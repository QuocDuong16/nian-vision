$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Invoke-NianBoundedProcess {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter()][string[]]$ArgumentList = @(),
        [Parameter(Mandatory = $true)][ValidateRange(1, 86400)][int]$TimeoutSeconds,
        [Parameter()][string]$Label = 'native process'
    )

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FilePath
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    foreach ($argument in $ArgumentList) {
        $null = $startInfo.ArgumentList.Add($argument)
    }

    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $timer = [Diagnostics.Stopwatch]::StartNew()
    try {
        if (-not $process.Start()) {
            throw ("{0} failed to start: {1}" -f $Label, $FilePath)
        }

        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()

        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $killFailure = $null
            try {
                $process.Kill($true)
            }
            catch {
                $killFailure = $_.Exception.Message
            }
            try {
                $null = $process.WaitForExit(5000)
            }
            catch {}

            if ($process.HasExited) {
                $stdout = $stdoutTask.GetAwaiter().GetResult().Trim()
                $stderr = $stderrTask.GetAwaiter().GetResult().Trim()
                if ($stdout) { Write-Host ("{0} stdout before timeout:`n{1}" -f $Label, $stdout) }
                if ($stderr) { Write-Host ("{0} stderr before timeout:`n{1}" -f $Label, $stderr) }
            }
            if ($killFailure) {
                Write-Warning ("{0} process-tree termination reported: {1}" -f $Label, $killFailure)
            }
            throw ("{0} timed out after {1}s; process-tree termination was requested" -f $Label, $TimeoutSeconds)
        }

        $stdout = $stdoutTask.GetAwaiter().GetResult().Trim()
        $stderr = $stderrTask.GetAwaiter().GetResult().Trim()
        if ($process.ExitCode -ne 0) {
            if ($stdout) { Write-Host ("{0} stdout:`n{1}" -f $Label, $stdout) }
            if ($stderr) { Write-Host ("{0} stderr:`n{1}" -f $Label, $stderr) }
            throw ("{0} failed with exit code {1}" -f $Label, $process.ExitCode)
        }

        $timer.Stop()
        return [pscustomobject]@{
            ExitCode = $process.ExitCode
            Stdout = $stdout
            Stderr = $stderr
            ElapsedSeconds = $timer.Elapsed.TotalSeconds
        }
    }
    finally {
        $timer.Stop()
        $process.Dispose()
    }
}
