param(
    [Parameter(Mandatory = $true)]
    [string]$Spec,

    [Parameter(Mandatory = $true)]
    [string]$CsvIn,

    [string]$Offline = "relative",

    [string]$Oorv = "",

    [string]$OutDir = "",

    [string]$Label = "",

    [int]$PollMs = 50,

    [string[]]$ExtraArgs = @()
)

$ErrorActionPreference = "Stop"

if ($PollMs -lt 1) {
    throw "PollMs must be a positive integer."
}

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$toolDir = Resolve-Path (Join-Path $scriptDir "../..")

if ([string]::IsNullOrWhiteSpace($Oorv)) {
    $Oorv = Join-Path $toolDir "target/release/oorv.exe"
}

if ([string]::IsNullOrWhiteSpace($OutDir)) {
    $OutDir = Join-Path $toolDir "bench/results/rq2/windows_memory"
}

if ([string]::IsNullOrWhiteSpace($Label)) {
    $Label = "windows_peak_private_{0:yyyyMMdd_HHmmss}" -f (Get-Date)
}

$Spec = (Resolve-Path $Spec).Path
$CsvIn = (Resolve-Path $CsvIn).Path
$Oorv = (Resolve-Path $Oorv).Path

$runDir = Join-Path $OutDir $Label
New-Item -ItemType Directory -Force -Path $runDir | Out-Null

$stdoutPath = Join-Path $runDir "run.stdout.log"
$stderrPath = Join-Path $runDir "run.stderr.log"
$metricsPath = Join-Path $runDir "memory_metrics.tsv"

$argsList = @(
    $Spec,
    "--offline",
    $Offline,
    "--csv-in",
    $CsvIn,
    "--verbosity",
    "silent",
    "--statistics",
    "all"
) + $ExtraArgs

$psi = [System.Diagnostics.ProcessStartInfo]::new()
$psi.FileName = $Oorv
$psi.UseShellExecute = $false
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
foreach ($arg in $argsList) {
    [void]$psi.ArgumentList.Add([string]$arg)
}

$stdoutWriter = [System.IO.StreamWriter]::new($stdoutPath, $false)
$stderrWriter = [System.IO.StreamWriter]::new($stderrPath, $false)
$process = [System.Diagnostics.Process]::new()
$process.StartInfo = $psi

$process.add_OutputDataReceived({
    param($sender, $eventArgs)
    if ($null -ne $eventArgs.Data) {
        $stdoutWriter.WriteLine($eventArgs.Data)
    }
})

$process.add_ErrorDataReceived({
    param($sender, $eventArgs)
    if ($null -ne $eventArgs.Data) {
        $stderrWriter.WriteLine($eventArgs.Data)
    }
})

$started = Get-Date
$peakPrivateBytes = 0L
$sampleCount = 0

try {
    if (-not $process.Start()) {
        throw "failed to start process: $Oorv"
    }

    $process.BeginOutputReadLine()
    $process.BeginErrorReadLine()

    while (-not $process.HasExited) {
        try {
            $process.Refresh()
            $sampleCount += 1
            if ($process.PrivateMemorySize64 -gt $peakPrivateBytes) {
                $peakPrivateBytes = $process.PrivateMemorySize64
            }
        } catch {
            # The process may exit between HasExited and Refresh; keep the final WaitForExit path authoritative.
        }
        Start-Sleep -Milliseconds $PollMs
    }

    $process.WaitForExit()
    $process.Refresh()
    if ($process.PrivateMemorySize64 -gt $peakPrivateBytes) {
        $peakPrivateBytes = $process.PrivateMemorySize64
    }
} finally {
    $stdoutWriter.Flush()
    $stderrWriter.Flush()
    $stdoutWriter.Dispose()
    $stderrWriter.Dispose()
}

$ended = Get-Date
$durationMs = ($ended - $started).TotalMilliseconds
$commandText = @($Oorv) + $argsList
$escapedCommand = ($commandText | ForEach-Object {
    if ($_ -match '\s') {
        '"' + ($_ -replace '"', '\"') + '"'
    } else {
        $_
    }
}) -join " "

@(
    "label`tstarted`tended`tduration_ms`texit_code`tpeak_private_bytes`tpoll_ms`tsamples`tstdout`tstderr`tcommand",
    ("{0}`t{1:o}`t{2:o}`t{3:N3}`t{4}`t{5}`t{6}`t{7}`t{8}`t{9}`t{10}" -f `
        $Label, `
        $started, `
        $ended, `
        $durationMs, `
        $process.ExitCode, `
        $peakPrivateBytes, `
        $PollMs, `
        $sampleCount, `
        $stdoutPath, `
        $stderrPath, `
        $escapedCommand)
) | Set-Content -Path $metricsPath -Encoding UTF8

Write-Host "wrote $metricsPath"
Write-Host "peak_private_bytes=$peakPrivateBytes"
exit $process.ExitCode
