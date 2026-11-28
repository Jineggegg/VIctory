# Run from an x64 Developer PowerShell. All measurements take place inside
# the Windows test binary. Cargo, process launch and nvidia-smi are outside them.
[CmdletBinding()]
param(
    [ValidateRange(2,1000)][int]$Runs = 100,
    [ValidateRange(0,100)][int]$MaxGpuUtilization = 10,
    [int]$GpuIndex = 0,
    [switch]$CheckOnly,
    [switch]$SkipValidation,
    [string]$OutputDirectory
)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
if (!$OutputDirectory) {
    $OutputDirectory = Join-Path $root ("docs\bench\logs\" + (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
}
$out = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path "$out\run.txt") { throw "Run log already exists in $out" }
[IO.Directory]::CreateDirectory($out) | Out-Null
$log = Join-Path $out 'run.txt'
$encoding = New-Object Text.UTF8Encoding($false)
function Log([string]$line) { [IO.File]::AppendAllText($log, "$line`n", $encoding) }
function Save {
    [IO.File]::WriteAllText("$out\results.json", ($report | ConvertTo-Json -Depth 8), $encoding)
}
# Capture both streams without a pipe buffer deadlock and keep the child's
# original exit code (including Windows exception codes).
function Native([string]$exe, [string]$arguments) {
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = $exe
    $info.Arguments = $arguments
    $info.WorkingDirectory = $root
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $proc = New-Object Diagnostics.Process
    $proc.StartInfo = $info
    Log "> $exe $arguments"
    try {
        [void]$proc.Start()
        $stdout = $proc.StandardOutput.ReadToEndAsync()
        $stderr = $proc.StandardError.ReadToEndAsync()
        $proc.WaitForExit()
        $result = @{ stdout = $stdout.Result; stderr = $stderr.Result; code = $proc.ExitCode }
        Log $result.stdout
        Log $result.stderr
        Log "exit $($result.code)"
        return $result
    } finally { $proc.Dispose() }
}
function Require($result) {
    if ($result.code -ne 0) { throw "Exit code $($result.code). See $log" }
    return $result.stdout.Trim()
}
function Stats([double[]]$values) {
    $sorted = @($values | Sort-Object)
    $n = $sorted.Count
    $mean = ($sorted | Measure-Object -Average).Average
    $sum = 0.0
    foreach ($x in $sorted) { $sum += ($x - $mean) * ($x - $mean) }
    [ordered]@{
        median = ($sorted[[int][Math]::Floor(($n-1)/2)] + $sorted[[int][Math]::Floor($n/2)]) / 2
        p95 = $sorted[[int][Math]::Ceiling(.95 * $n) - 1]
        p99 = $sorted[[int][Math]::Ceiling(.99 * $n) - 1]
        sample_stddev = [Math]::Sqrt($sum / ($n - 1))
    }
}
$saved = @{}
$settings = @('VICTORY_BENCH_MODE','VICTORY_BENCH_ADAPTER','VICTORY_D3D12_DEBUG','VICTORY_D3D12_GBV','VICTORY_D3D12_WARP')
foreach ($key in $settings) { $saved[$key] = [Environment]::GetEnvironmentVariable($key) }
Push-Location $root
try {
    $cargo = (Get-Command cargo.exe).Source
    $smi = (Get-Command nvidia-smi.exe).Source
    $gpuInfo = (Require (Native $smi "-i $GpuIndex --query-gpu=name,uuid,driver_version,memory.total --format=csv,noheader,nounits")) | ConvertFrom-Csv -Header name,uuid,driver,memory_mib
    $env:VICTORY_BENCH_ADAPTER = $gpuInfo.name.Trim()
    $env:VICTORY_D3D12_DEBUG = '0'
    $env:VICTORY_D3D12_GBV = '0'
    $env:VICTORY_D3D12_WARP = '0'
    $report = [ordered]@{
        started_utc = [DateTime]::UtcNow.ToString('o')
        commit = (Require (Native 'git.exe' 'rev-parse HEAD'))
        source_changes = (Require (Native 'git.exe' 'status --porcelain -- . ":(exclude)docs/bench"'))
        rustc = (Require (Native 'rustc.exe' '-Vv'))
        profile = 'diag (release code generation), diagnostics; debug layer off during timings'
        cpu = (Get-CimInstance Win32_Processor).Name
        ram_bytes = (Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory
        os = (Get-CimInstance Win32_OperatingSystem).Version
        gpu = $gpuInfo
        max_pre_sample_gpu_utilization_pct = $MaxGpuUtilization
        requested_samples = $(if ($CheckOnly) { 0 } else { $Runs })
        validation_status = 'pending'; process_crashes = 0; failed_runs = 0; runs = @(); summary = @{}; complete = $false
    }
    $build = Native $cargo 'test --locked --profile diag -p victory-gpu --lib --features diagnostics --no-run --message-format=json'
    if ($build.code -ne 0) { throw "Benchmark build failed. See $log" }
    $exe = @($build.stdout -split '\r?\n' | Where-Object { $_ } | ForEach-Object { ConvertFrom-Json $_ } |
        Where-Object { $_.reason -eq 'compiler-artifact' -and $_.target.name -eq 'victory_gpu' -and $_.executable } |
        ForEach-Object { $_.executable })
    if ($exe.Count -ne 1) { throw "Expected exactly one victory_gpu test executable" }

    function RunScene([string]$mode, [int]$index) {
        # A utilization reading can still describe the previous process.
        # Retain every poll and wait between samples if the adapter is busy.
        $idleDeadline = [DateTime]::UtcNow.AddSeconds(30)
        do {
            $pre = (Require (Native $smi "-i $GpuIndex --query-gpu=uuid,utilization.gpu,memory.used,temperature.gpu --format=csv,noheader,nounits")) | ConvertFrom-Csv -Header uuid,gpu_pct,used_mib,temp_c
            if ($mode -ne 'sample' -or [int]$pre.gpu_pct -le $MaxGpuUtilization) { break }
            Start-Sleep -Seconds 1
        } while ([DateTime]::UtcNow -lt $idleDeadline)
        $row = [ordered]@{ mode = $mode; index = $index; utc = [DateTime]::UtcNow.ToString('o'); pre_run = $pre; values = [ordered]@{} }
        $report.runs += $row
        if ($pre.uuid.Trim() -ne $gpuInfo.uuid.Trim()) { throw "GPU identity changed" }
        if ($mode -eq 'sample' -and [int]$pre.gpu_pct -gt $MaxGpuUtilization) {
            $row.status = 'busy'
            throw "GPU at $($pre.gpu_pct)% before sample $index. See $log"
        }
        $env:VICTORY_BENCH_MODE = $mode
        $result = Native $exe[0] '--exact pipeline::baseline::synthetic_gpu --ignored --nocapture --test-threads=1'
        $row.exit_code = $result.code
        foreach ($line in ($result.stdout -split '\r?\n')) {
            if ($line -match 'BENCH (\w+)=(.*)$') {
                $key = $Matches[1]; $value = $Matches[2]
                if ($key -match '(_us|_mb|_bytes|_errors)$') { $value = [double]::Parse($value, [Globalization.CultureInfo]::InvariantCulture) }
                elseif ($key -in @('depth','rgba')) { $value = (ConvertFrom-Json $value).PSObject.BaseObject }
                $row.values[$key] = $value
            }
        }
        $row.status = 'passed'
        if ($result.code -ne 0 -or !$row.values.Contains('rgba') -or !$row.values.Contains('depth') -or
            ($mode -eq 'validate' -and !$row.values.Contains('validation_errors'))) {
            $report.failed_runs++
            if ($result.code -lt 0 -or $result.code -notin @(0,101)) { $report.process_crashes++ }
            $row.status = 'failed'
            $row.reason = $result.stderr.Trim()
        }
        Save
        return $row
    }
    $check = RunScene 'check' 0
    if ($check.status -ne 'passed') { throw "Pixel or adapter check failed. See $log" }
    if ($SkipValidation) {
        $report.validation_status = 'skipped by request'
        Log 'D3D12 Debug Layer and GPU-based validation skipped by request.'
    } else {
        $validation = RunScene 'validate' 0
        $report.validation_status = $validation.status
    }
    if (!$CheckOnly) {
        for ($i = 1; $i -le $Runs; $i++) {
            $row = RunScene 'sample' $i
            if ($row.status -ne 'passed') { throw "Sample $i failed. See $log" }
        }
        $samples = @($report.runs | Where-Object mode -eq 'sample')
        foreach ($key in $samples[0].values.Keys) {
            if ($key -match '(_us|_mb|_bytes)$') {
                $report.summary[$key] = Stats @($samples | ForEach-Object { $_.values[$key] })
            }
        }
    }
    if (!$SkipValidation -and $validation.status -ne 'passed') { throw "D3D12 validation failed. See $log" }
    $report.complete = !$SkipValidation
    $report.requested_run_complete = $true
} finally {
    if ($report) { Save }
    foreach ($key in $settings) { [Environment]::SetEnvironmentVariable($key, $saved[$key]) }
    Pop-Location
    Write-Host "Results: $out"
}
