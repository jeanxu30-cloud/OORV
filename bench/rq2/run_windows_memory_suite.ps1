param(
    [string]$Manifest = "",

    [string]$ResultsRoot = "",

    [string]$OutDir = "",

    [string]$SuiteLabel = "",

    [string[]]$Labels = @(),

    [string[]]$Families = @(),

    [switch]$All,

    [switch]$GenerateMissing,

    [switch]$SkipBuild,

    [string]$Oorv = "",

    [int]$PollMs = 50,

    [switch]$ContinueOnError,

    [string[]]$ExtraArgs = @()
)

$ErrorActionPreference = "Stop"

if ($PollMs -lt 1) {
    throw "PollMs must be a positive integer."
}

if ($All -and $Labels.Count -gt 0) {
    throw "Use either -All or -Labels, not both."
}

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$toolDir = Resolve-Path (Join-Path $scriptDir "../..")
$singleRunScript = Resolve-Path (Join-Path $scriptDir "run_windows_peak_private_memory.ps1")

if ([string]::IsNullOrWhiteSpace($ResultsRoot)) {
    $ResultsRoot = Join-Path $toolDir "bench/results/rq2"
}

if ([string]::IsNullOrWhiteSpace($Manifest)) {
    $Manifest = Join-Path $ResultsRoot "showcase_manifest.tsv"
}

if ([string]::IsNullOrWhiteSpace($OutDir)) {
    $OutDir = Join-Path $ResultsRoot "windows_memory"
}

if ([string]::IsNullOrWhiteSpace($SuiteLabel)) {
    $SuiteLabel = "windows_memory_suite_{0:yyyyMMdd_HHmmss}" -f (Get-Date)
}

if ([string]::IsNullOrWhiteSpace($Oorv)) {
    $Oorv = Join-Path $toolDir "target/release/oorv.exe"
}

$ResultsRoot = (Resolve-Path $ResultsRoot).Path
$Manifest = (Resolve-Path $Manifest).Path
$OutDir = (New-Item -ItemType Directory -Force -Path $OutDir).FullName
$suiteDir = Join-Path $OutDir $SuiteLabel
$runRoot = Join-Path $suiteDir "runs"
New-Item -ItemType Directory -Force -Path $runRoot | Out-Null

$aggregateMetrics = Join-Path $suiteDir "memory_metrics.tsv"
$suiteManifest = Join-Path $suiteDir "suite_manifest.tsv"

function Get-Number {
    param(
        [Parameter(Mandatory = $true)]$Row,
        [Parameter(Mandatory = $true)][string]$Name
    )

    $raw = [string]$Row.$Name
    if ([string]::IsNullOrWhiteSpace($raw) -or $raw -eq "NA") {
        return 0.0
    }
    return [double]::Parse(
        $raw,
        [System.Globalization.CultureInfo]::InvariantCulture
    )
}

function Get-StressScore {
    param([Parameter(Mandatory = $true)]$Row)

    return (
        (Get-Number $Row "objects") * 1000000.0 +
        (Get-Number $Row "constraints") * 10000.0 +
        (Get-Number $Row "history_depth") * 1000.0 +
        (Get-Number $Row "periodic_constraints") * 1000.0 +
        (Get-Number $Row "hotset_size") * 100.0 +
        (Get-Number $Row "burst_size") * 10.0 +
        (Get-Number $Row "events_requested")
    )
}

function Select-WorkloadRows {
    param(
        [Parameter(Mandatory = $true)]$Rows,
        [string[]]$WantedLabels,
        [string[]]$WantedFamilies,
        [bool]$SelectAll
    )

    $filtered = @($Rows)

    if ($WantedFamilies.Count -gt 0) {
        $familySet = @{}
        foreach ($family in $WantedFamilies) {
            $familySet[$family] = $true
        }
        $filtered = @($filtered | Where-Object { $familySet.ContainsKey($_.family) })
    }

    if ($WantedLabels.Count -gt 0) {
        $rowByLabel = @{}
        foreach ($row in $filtered) {
            $rowByLabel[$row.label] = $row
        }
        $selected = @()
        foreach ($label in $WantedLabels) {
            if (-not $rowByLabel.ContainsKey($label)) {
                throw "label not found in manifest: $label"
            }
            $selected += $rowByLabel[$label]
        }
        return $selected
    }

    if ($SelectAll) {
        return $filtered
    }

    $selectedByFamily = @()
    foreach ($group in ($filtered | Group-Object family)) {
        $selectedByFamily += @(
            $group.Group |
                Sort-Object `
                    @{ Expression = { Get-StressScore $_ }; Descending = $true },
                    @{ Expression = { Get-Number $_ "repetition" }; Descending = $false },
                    @{ Expression = { $_.label }; Descending = $false } |
                Select-Object -First 1
        )
    }
    return @($selectedByFamily | Sort-Object family, label)
}

function Ensure-WorkloadFiles {
    param([Parameter(Mandatory = $true)]$Row)

    $workloadDir = Join-Path $ResultsRoot $Row.label
    $specPath = Join-Path $workloadDir "synthetic.oorv"
    $csvPath = Join-Path $workloadDir "synthetic.csv"

    if ((Test-Path $specPath) -and (Test-Path $csvPath)) {
        return @{
            WorkloadDir = $workloadDir
            SpecPath = (Resolve-Path $specPath).Path
            CsvPath = (Resolve-Path $csvPath).Path
        }
    }

    if (-not $GenerateMissing) {
        throw "missing generated workload files for $($Row.label): run the showcase suite first, or pass -GenerateMissing with bash available"
    }

    $bash = Get-Command bash -ErrorAction SilentlyContinue
    if ($null -eq $bash) {
        throw "cannot generate missing workload $($Row.label): bash is not available"
    }

    $runner = Join-Path $scriptDir "run_single_workload.sh"
    $bashArgs = @(
        $runner,
        "--label", $Row.label,
        "--objects", $Row.objects,
        "--constraints", $Row.constraints,
        "--events", $Row.events_requested,
        "--history-depth", $Row.history_depth,
        "--periodic-constraints", $Row.periodic_constraints,
        "--periodic-hz", $Row.periodic_hz,
        "--burst-size", $Row.burst_size,
        "--hotset-size", $Row.hotset_size,
        "--phase-length", $Row.phase_length,
        "--time-step-ms", $Row.time_step_ms,
        "--burst-gap-ms", $Row.burst_gap_ms,
        "--family", $Row.family,
        "--repetition", $Row.repetition,
        "--skip-build"
    )

    & $bash.Source @bashArgs
    if ($LASTEXITCODE -ne 0) {
        throw "failed to generate workload $($Row.label) with run_single_workload.sh"
    }

    if (-not ((Test-Path $specPath) -and (Test-Path $csvPath))) {
        throw "run_single_workload.sh completed but workload files are still missing for $($Row.label)"
    }

    return @{
        WorkloadDir = $workloadDir
        SpecPath = (Resolve-Path $specPath).Path
        CsvPath = (Resolve-Path $csvPath).Path
    }
}

function Append-MetricsRows {
    param(
        [Parameter(Mandatory = $true)][string]$SourcePath,
        [Parameter(Mandatory = $true)][string]$DestinationPath
    )

    $lines = @(Get-Content -Path $SourcePath)
    if ($lines.Count -lt 2) {
        throw "metrics file has no data rows: $SourcePath"
    }

    if (-not (Test-Path $DestinationPath)) {
        $lines | Set-Content -Path $DestinationPath -Encoding UTF8
    } else {
        $lines[1..($lines.Count - 1)] | Add-Content -Path $DestinationPath -Encoding UTF8
    }
}

function Get-PowerShellExecutable {
    $current = (Get-Process -Id $PID).Path
    if (-not [string]::IsNullOrWhiteSpace($current)) {
        return $current
    }
    $cmd = Get-Command pwsh -ErrorAction SilentlyContinue
    if ($null -ne $cmd) {
        return $cmd.Source
    }
    return "powershell.exe"
}

if (-not $SkipBuild) {
    Push-Location $toolDir
    try {
        & cargo build --release --locked
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build --release --locked failed"
        }
    } finally {
        Pop-Location
    }
}

$Oorv = (Resolve-Path $Oorv).Path
$rows = @(Import-Csv -Path $Manifest -Delimiter "`t")
if ($rows.Count -eq 0) {
    throw "manifest has no rows: $Manifest"
}

$selectedRows = @(Select-WorkloadRows $rows $Labels $Families ([bool]$All))
if ($selectedRows.Count -eq 0) {
    throw "no workload rows selected"
}

@(
    "suite_label`tlabel`tfamily`tobjects`tconstraints`tevents_requested`tworkload_dir`tmetrics_path`thelper_exit_code"
) | Set-Content -Path $suiteManifest -Encoding UTF8

$powerShellExe = Get-PowerShellExecutable
$failures = @()

foreach ($row in $selectedRows) {
    $files = Ensure-WorkloadFiles $row
    $childLabel = $row.label

    $helperArgs = @(
        "-NoProfile",
        "-ExecutionPolicy", "Bypass",
        "-File", $singleRunScript.Path,
        "-Spec", $files.SpecPath,
        "-CsvIn", $files.CsvPath,
        "-Oorv", $Oorv,
        "-OutDir", $runRoot,
        "-Label", $childLabel,
        "-PollMs", $PollMs
    )
    if ($ExtraArgs.Count -gt 0) {
        $helperArgs += "-ExtraArgs"
        foreach ($arg in $ExtraArgs) {
            $helperArgs += $arg
        }
    }

    & $powerShellExe @helperArgs
    $helperExitCode = $LASTEXITCODE
    $childMetrics = Join-Path (Join-Path $runRoot $childLabel) "memory_metrics.tsv"

    if (Test-Path $childMetrics) {
        Append-MetricsRows $childMetrics $aggregateMetrics
    } else {
        $failures += "$childLabel: missing child metrics file"
    }

    @(
        "{0}`t{1}`t{2}`t{3}`t{4}`t{5}`t{6}`t{7}`t{8}" -f `
            $SuiteLabel,
            $row.label,
            $row.family,
            $row.objects,
            $row.constraints,
            $row.events_requested,
            $files.WorkloadDir,
            $childMetrics,
            $helperExitCode
    ) | Add-Content -Path $suiteManifest -Encoding UTF8

    if ($helperExitCode -ne 0) {
        $failures += "$childLabel: helper exit code $helperExitCode"
        if (-not $ContinueOnError) {
            break
        }
    }
}

if ($failures.Count -gt 0) {
    Write-Error ("Windows memory suite completed with failures: " + ($failures -join "; "))
    exit 1
}

Write-Host "wrote $aggregateMetrics"
Write-Host "wrote $suiteManifest"
Write-Host "selected_workloads=$($selectedRows.Count)"
