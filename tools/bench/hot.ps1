# Windows half of hot.sh. Use the WSL entry point for sampling.
param(
    [Parameter(Mandatory, Position=0)][ValidateSet('build','smoke','sample','ab','compare')][string]$Mode,
    [Parameter(Position=1)][string]$Before,
    [Parameter(Position=2)][string]$After,
    [string]$StatsExe, [string]$SuiteFile, [string]$EnvFile, [string]$Distro, [int]$Runs, [string]$OutDir, [string]$QuietState
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$utf8 = [Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = $OutputEncoding = $utf8
$culture = [Globalization.CultureInfo]::InvariantCulture
$root = (Get-Item -LiteralPath "$PSScriptRoot/../..").FullName
Set-Location -LiteralPath $root
$manifest = Join-Path $root 'target/hot-bench.json'
$xtask = $StatsExe
if ($Mode -ne 'compare' -or !$xtask) {
    Get-ChildItem Env:VICTORY_* | Remove-Item
    . $EnvFile
    $env:CARGO_TARGET_DIR = Join-Path $root target
    $xtask = Join-Path $env:CARGO_TARGET_DIR 'debug/xtask.exe'
}
function Protect-Log([string]$text) {
    ($text -ireplace '[A-Z]:[\\/]Users[\\/][^\\/\s"'']+', '%USERPROFILE%') -ireplace 'GPU-[a-f0-9-]{32,}', 'GPU-REDACTED'
}
function Utc-Now { [datetime]::UtcNow.ToString("yyyy-MM-dd'T'HH:mm:ss.ffffff'+00:00'", $culture) }
function Read-Json($path) { [IO.File]::ReadAllText($path) | ConvertFrom-Json }
function Save-Json($path, $value) {
    # Windows PowerShell's native pretty printer uses inconsistent indentation.
    # Re-indent the compact JSON without interpreting dates or changing numbers.
    $compact = ConvertTo-Json -InputObject $value -Depth 16 -Compress
    Add-Type -AssemblyName System.Runtime.Serialization
    $reader = [Runtime.Serialization.Json.JsonReaderWriterFactory]::CreateJsonReader($utf8.GetBytes($compact), [Xml.XmlDictionaryReaderQuotas]::Max)
    $stream = [IO.MemoryStream]::new()
    $writer = [Runtime.Serialization.Json.JsonReaderWriterFactory]::CreateJsonWriter($stream, $utf8, $false, $true, '  ')
    try {
        $writer.WriteNode($reader, $true)
        $writer.Flush()
        $text = $utf8.GetString($stream.ToArray()).Replace("`r`n", "`n")
    } finally { $writer.Close(); $reader.Close(); $stream.Dispose() }
    $temp = "$path.$PID.tmp"
    try {
        [IO.File]::WriteAllText($temp, $text + "`n", $utf8)
        # A WSL output directory can be a Linux filesystem (e.g. /tmp). Its
        # Windows UNC provider does not implement File.Replace. Rename on WSL
        # for atomic snapshots on both Linux and drive-backed directories.
        wsl.exe --distribution $Distro --cd ([IO.Path]::GetDirectoryName($path)) --exec mv -fT -- ([IO.Path]::GetFileName($temp)) ([IO.Path]::GetFileName($path))
        if ($LASTEXITCODE) { throw "Cannot publish $path" }
    } finally { if ([IO.File]::Exists($temp)) { [IO.File]::Delete($temp) } }
}
function Source-Stamp($checkout = $root) {
    $scriptPath = wsl.exe --distribution $Distro --exec wslpath -u "$PSScriptRoot/hot.sh"
    if ($LASTEXITCODE) { throw 'Cannot locate fingerprint tool' }
    $stamp = @(wsl.exe --distribution $Distro --cd $checkout --exec bash $scriptPath _stamp .)
    if ($LASTEXITCODE -or $stamp.Count -lt 1 -or $stamp.Count -gt 2) { throw 'Cannot fingerprint measured sources' }
    $delta = $null
    if ($stamp.Count -eq 2) { $delta = $stamp[1] }
    [ordered]@{commit=$stamp[0]; source_delta_sha256=$delta}
}
function Same-Source($a, $b) {
    ($a.commit -ceq $b.commit) -and ($a.source_delta_sha256 -ceq $b.source_delta_sha256)
}
function File-Hash($path) { (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() }

# Stream the build on stderr while capturing only Cargo's JSON artifacts on stdout.
function Build-Stats {
    & cargo build --locked -p xtask
    if ($LASTEXITCODE) { throw 'Cannot build xtask' }
}
function Build-Benchmark {
    $stamp = Source-Stamp
    Build-Stats
    $artifacts = @(cargo test --locked --profile diag -p victory-gpu --lib --features diagnostics --no-run --message-format=json |
        ForEach-Object { if ($_.StartsWith('{')) {
            $entry = ConvertFrom-Json $_
            if ($entry.reason -eq 'compiler-artifact' -and $entry.target.name -eq 'victory_gpu' -and $entry.executable) { $entry.executable }
        } })
    if ($LASTEXITCODE -or $artifacts.Count -ne 1) { throw 'Expected one victory-gpu test executable' }
    $exe = $artifacts[0]
    $target = $env:CARGO_TARGET_DIR + '\'
    if (!$exe.StartsWith($target, [StringComparison]::OrdinalIgnoreCase)) { throw 'Unexpected artifact directory' }
    $version = (rustc -Vv) -join "`n"
    if ($LASTEXITCODE) { throw 'Cannot identify compiler' }
    $stamp.executable = $exe.Substring($root.Length + 1).Replace('\','/')
    $stamp.sha256 = File-Hash $exe
    $stamp.rustc = $version
    if (!(Same-Source $stamp (Source-Stamp))) { throw 'Source changed during build' }
    Save-Json $manifest $stamp
}
$gpuFields = 'name','driver_version','memory.total','utilization.gpu','memory.used','temperature.gpu','clocks.gr','clocks.mem','power.draw','pstate',
    'clocks.max.gr','clocks.max.mem','pcie.link.gen.current','pcie.link.gen.max','pcie.link.width.current','pcie.link.width.max'
function Get-Gpu {
    $csv = @(nvidia-smi -i 0 "--query-gpu=$($gpuFields -join ',')" --format=csv,noheader,nounits)
    if ($LASTEXITCODE -or $csv.Count -ne 1) { throw 'Cannot query adapter 0' }
    $row = $csv[0] | ConvertFrom-Csv -Header $gpuFields
    $gpu = [ordered]@{utc=Utc-Now}
    foreach ($field in $gpuFields) { $gpu[$field] = $row.$field.Trim() }
    $gpu
}
$timings = ('pso_cold pso_hot prebuild_16_cold prebuild_16_hot shader_batch_translate shader_batch_cold shader_batch_hot ' +
    'descriptors_1300 descriptors_14400 submit_32 submit_fence_wait submit_prepare submit_queue_and_fence texture_full ' +
    'texture_partial wave_page_marks_gpu loop_page_lookup_gpu shadow_refresh marked_writeback_frame').Split(' ') |
    ForEach-Object { $_ + '_us' }

$workload = 'shader_count','prebuild_count','submit_count','shadow_refresh_bytes','descriptors_1300_changed',
    'descriptors_14400_changed','wave_page_marks_lanes','loop_global_loads','texture_title_patch_bytes'
$suiteName = 'victory-hot-v1'
$testName = 'pipeline::hot::hot_paths'
$parameters = [ordered]@{}
$timeoutSeconds = 180
if ($SuiteFile) {
    $spec = Read-Json $SuiteFile
    $suiteName = [string]$spec.name
    $testName = [string]$spec.test
    if (!$suiteName -or $testName -notmatch '^\w+(::\w+)*$') { throw 'Expected a suite name and an exact Rust test name' }
    $timings = @($spec.timings)
    if (!$timings.Count -or @($timings | Where-Object { $_ -notmatch '^\w+$' }).Count -or
        @($timings | Select-Object -Unique).Count -ne $timings.Count) { throw 'Expected distinct numeric timing keys' }
    if ($spec.PSObject.Properties['timeout_seconds']) {
        $timeoutSeconds = [int]$spec.timeout_seconds
        if ($timeoutSeconds -lt 1 -or $timeoutSeconds -gt 2147483) { throw 'Invalid process timeout' }
    }
    $workload = @()
    if ($spec.PSObject.Properties['workload']) { $workload = @($spec.workload) }
    if ($spec.PSObject.Properties['env']) {
        foreach ($prop in $spec.env.PSObject.Properties) {
            if ($prop.Name -cnotmatch '^VICTORY_BENCH_[A-Z0-9_]+$') { throw "Unexpected benchmark parameter $($prop.Name)" }
            $parameters[$prop.Name] = [string]$prop.Value
        }
    }
}

function Assert-Keys($expected, $actual, $context) {
    $missing = @($expected | Where-Object { $_ -cnotin $actual })
    $extra = @($actual | Where-Object { $_ -cnotin $expected })
    if ($missing.Count -or $extra.Count) {
        throw "$context metric mismatch; missing=[$($missing -join ', ')], extra=[$($extra -join ', ')]"
    }
}

# Older default and scale gradient binaries predate the four texture staging counters.
# Keep them in each arm's report and compare them when both binaries emit them.
function Get-ComparableKeys($left, $right, $suite, $context) {
    if ($suite -cin 'victory-hot-v1','victory-gradient-small-v1','victory-gradient-medium-v1','victory-gradient-large-v1') {
        $staging = 'texture_full_staging_requested_bytes','texture_full_staging_backing_bytes',
                   'texture_partial_staging_requested_bytes','texture_partial_staging_backing_bytes'
        $leftCount = @($staging | Where-Object { $_ -cin $left }).Count
        $rightCount = @($staging | Where-Object { $_ -cin $right }).Count
        if ($leftCount -eq 0 -and $rightCount -eq $staging.Count) {
            $right = @($right | Where-Object { $_ -cnotin $staging })
        } elseif ($rightCount -eq 0 -and $leftCount -eq $staging.Count) {
            $left = @($left | Where-Object { $_ -cnotin $staging })
        }
    }
    Assert-Keys $left $right $context
    $left
}

function Read-Metrics($text) {
    $values = [ordered]@{}
    foreach ($match in [regex]::Matches($text, 'BENCH (\w+)=(.*)')) {
        $key = $match.Groups[1].Value
        $raw = $match.Groups[2].Value.Trim()
        if ($values.Contains($key)) { throw "Duplicate metric $key" }
        $values[$key] = if ($key -cin 'adapter','shader_digest') { $raw } else {
            $number = [double]::Parse($raw, $culture)
            if ([double]::IsNaN($number) -or [double]::IsInfinity($number)) { throw "Non-finite metric $key" }
            $number
        }
    }
    $values
}
function New-Child($exe, $cwd, $test) {
    $process = [Diagnostics.Process]::new()
    $start = $process.StartInfo
    $start.FileName = $exe
    $start.WorkingDirectory = $cwd
    $start.Arguments = "--exact $test --ignored --nocapture --test-threads=1"
    $start.UseShellExecute = $false
    $start.RedirectStandardOutput = $start.RedirectStandardError = $true
    $start.StandardOutputEncoding = $start.StandardErrorEncoding = $utf8
    $process
}
function Run-Child($exe, $cwd, $test) {
    $process = New-Child $exe $cwd $test
    foreach ($k in $parameters.Keys) { $process.StartInfo.EnvironmentVariables[$k] = $parameters[$k] }
    try {
        [void]$process.Start()
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (!$process.WaitForExit($timeoutSeconds * 1000)) {
            $process.Kill()
            $process.WaitForExit()
            throw "Hotspot test timed out after ${timeoutSeconds}s"
        }
        @{exit=$process.ExitCode; text=$stdout.Result + $stderr.Result}
    } finally { $process.Dispose() }
}
function Series($rows, $key) {
    ($rows | ForEach-Object { ([double]$_.values.$key).ToString('G17', $culture) }) -join ','
}
function Summarize($rows) {
    $summary = [ordered]@{}
    $keys = $rows[-1].values.Keys | Where-Object { $_ -cnotin 'verified','adapter','shader_digest' }
    $stats = $keys | ForEach-Object { "$_`t$(Series $rows $_)" } | & $xtask bench-stats
    if ($LASTEXITCODE) { throw 'Cannot summarize samples' }
    foreach ($line in $stats) {
        $key,$n,$median,$p95,$p99,$sd = $line.Split("`t")
        $deviation = $null
        if ($sd) { $deviation = [double]$sd }
        $summary[$key] = [ordered]@{n=[int]$n; median=[double]$median; p95=[double]$p95; p99=[double]$p99;
            sample_stddev=$deviation}
    }
    $summary
}
function Get-Target($dir) {
    $dir = (Get-Item -LiteralPath $dir).FullName
    if (Test-Path -LiteralPath (Join-Path $dir hot-bench.json)) { $dir = Split-Path $dir }
    $binary = Read-Json (Join-Path $dir target/hot-bench.json)
    $exe = Join-Path $dir $binary.executable
    if (!(Same-Source $binary (Source-Stamp $dir)) -or (File-Hash $exe) -cne $binary.sha256) {
        throw "Source or executable differs from build manifest in $dir"
    }
    @{dir=$dir; exe=$exe; binary=$binary}
}

function Read-QuietWindow {
    # gpu-quiet emits its accepted idle snapshot before starting this driver.
    # The WSL tee may still be flushing that line across the Windows mount.
    # Newer gpu-quiet versions also print the admitted samples and the desktop
    # background they allowed on an "Idle window:" line; keep it when present.
    $snapshot = [ordered]@{status_line=$null; tolerated_background=$null; admission=$null}
    for ($i=0; $i -lt 10; $i++) {
        # The Linux writer holds this file open. Windows ReadAllLines opens
        # with FileShare.Read, which denies that writer on a drive mount.
        $lines = @(wsl.exe --distribution $Distro --cd $OutDir --exec head -n 50 window.txt)
        if ($LASTEXITCODE) { throw 'Cannot read gpu-quiet window log' }
        $line = $lines | Where-Object { $_ -match '\| 3D/Compute=' } | Select-Object -Last 1
        if ($line) {
            $snapshot.status_line = Protect-Log $line.Trim()
            if ($snapshot.status_line -match '\| tolerated background=([^|]*)') {
                $snapshot.tolerated_background = $Matches[1].Trim()
            }
            $admitted = $lines | Where-Object { $_ -match '^Idle window: ' } | Select-Object -Last 1
            if ($admitted) { $snapshot.admission = Protect-Log $admitted.Trim() }
            break
        }
        Start-Sleep -Milliseconds 100
    }
    $snapshot
}

# Query the debug interface without enabling validation in timing processes.
function Get-DebugLayerStatus {
    if (!('VictoryBench.DebugLayer' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace VictoryBench {
    public static class DebugLayer {
        [DllImport("d3d12.dll", ExactSpelling = true)]
        public static extern int D3D12GetDebugInterface(ref Guid iid, out IntPtr result);
    }
}
'@
    }
    $iid = [Guid]'affaa4ca-63fe-4d8e-b8ad-159000af4304' # ID3D12Debug1 supports GBV
    $pointer = [IntPtr]::Zero
    $hr = [VictoryBench.DebugLayer]::D3D12GetDebugInterface([ref]$iid, [ref]$pointer)
    if ($pointer -ne [IntPtr]::Zero) { [void][Runtime.InteropServices.Marshal]::Release($pointer) }
    if ($hr -eq 0 -and $pointer -ne [IntPtr]::Zero) { return 'available: ID3D12Debug1; disabled during timings' }
    'unavailable: ID3D12Debug1 (HRESULT 0x{0:X8})' -f $hr
}

function New-Report($target, $system, $gpu) {
    [ordered]@{system=$system; schema=1; suite=$suiteName; test=$testName; parameters=$parameters; timing_keys=$timings; workload_keys=$workload; timeout_seconds=$timeoutSeconds; formal=($Mode -ne 'smoke'); complete=$false;
        debug_layer=$debugLayerStatus; binary=$target.binary; requested=$Runs; utc=Utc-Now;
        gpu=$gpu; profile='diag'; runs=[Collections.ArrayList]::new(); summary=@{}; invalid_count=0; max_invalid=[int][Math]::Floor($Runs / 10); invalid_runs=[Collections.ArrayList]::new()}
}

function Warm-Ready($gpu) {
    foreach ($key in 'clocks.gr','clocks.mem','pcie.link.gen.current','pcie.link.width.current') {
        if ($gpu[$key] -notmatch '^\d+$') { return $false }
    }
    ($gpu.pstate -in 'P0','P1' -and [int]$gpu.'clocks.gr' -ge 2500 -and [int]$gpu.'clocks.mem' -ge 13000 -and
        [int]$gpu.'pcie.link.gen.current' -eq 5 -and [int]$gpu.'pcie.link.width.current' -eq 16)
}

function Invoke-Warmup($target, $row) {
    $process = New-Child $target.exe $target.dir 'pipeline::hot::warm_adapter'
    $process.StartInfo.RedirectStandardInput = $true
    $row.warm_trace = [Collections.ArrayList]::new()
    $started = $false
    try {
        $started = $process.Start()
        $errors = $process.StandardError.ReadToEndAsync()
        do {
            $read = $process.StandardOutput.ReadLineAsync()
            if (!$read.Wait(10000) -or $null -eq $read.Result) { throw 'Warmup dispatch failed to start' }
        } until ($read.Result.Contains('WARMUP active'))
        $output = $process.StandardOutput.ReadToEndAsync()
        $timer = [Diagnostics.Stopwatch]::StartNew()
        $since = $null
        $row.warm_stable_ms = 0
        do {
            $g = Get-Gpu
            $now = $timer.ElapsedMilliseconds
            [void]$row.warm_trace.Add(@($now,$g.pstate,$g.'clocks.gr',$g.'clocks.mem',$g.'pcie.link.gen.current',$g.'pcie.link.width.current'))
            if (Warm-Ready $g) {
                if ($null -eq $since) { $since = $now }
                $row.warm_stable_ms = $now - $since
                if ($row.warm_stable_ms -ge 3000) { break }
            } else { $since = $null; $row.warm_stable_ms = 0 }
            Start-Sleep -Milliseconds 200
        } while ($now -lt 12000 -and !$process.HasExited)
        $process.StandardInput.Close()
        if (!$process.WaitForExit(5000)) { throw 'Warmup process did not release the GPU' }
        if ($process.ExitCode -ne 0 -or $output.Result -notmatch 'WARMUP completed') {
            throw (Protect-Log "Warmup dispatch failed: $($errors.Result)")
        }
    } finally {
        if ($started -and !$process.HasExited) { $process.Kill(); $process.WaitForExit() }
        $process.Dispose()
    }
}

function Measure-One($target, $warmer, $report, $i, $position, $label, $log, $thermal) {
    $row = [ordered]@{index=$i; position=$position; utc=Utc-Now; ok=$false; polls=[Collections.ArrayList]::new()}
    [void]$report.runs.Add($row)
    try {
        if (!$report.formal -and [IO.File]::Exists($QuietState)) { throw 'GPU reserved for formal sampling' }
        $row.pre = Get-Gpu
        [void]$row.polls.Add($row.pre)
        $row.warm_reused = $false
        $stable = $true
        if ($report.formal) {
            $row.warm_reused = $thermal.ready -and (Warm-Ready $row.pre)
            if (!$row.warm_reused) {
                Invoke-Warmup $warmer $row
                $stable = $row.warm_stable_ms -ge 3000
                $row.pre = Get-Gpu
                [void]$row.polls.Add($row.pre)
            }
        }
        if ($report.formal -and (!$stable -or !(Warm-Ready $row.pre))) {
            $thermal.ready = $false
            $row.error = 'GPU did not hold the required clocks and link before the sample'
            $row.post = Get-Gpu
            $report.invalid_count++
            $report.runs.RemoveAt($report.runs.Count - 1)
            [void]$report.invalid_runs.Add($row)
            Write-Host "Invalid warmup for $label pair $i ($($report.invalid_count)/$($report.max_invalid) allowed)"
            return $false
        }
        Write-Host "Sample $label $i/$Runs $($row.pre.pstate) $($row.pre.'clocks.gr')/$($row.pre.'clocks.mem') MHz"
        $child = Run-Child $target.exe $target.dir $testName
        $row.post = Get-Gpu
        $log.WriteLine((Protect-Log "`nSample $label $i, position $position`n$($child.text)`nexit $($child.exit)"))
        $row.exit = $child.exit
        $v = Read-Metrics $child.text
        $row.values = $v
        if ($row.exit -ne 0 -or $v['verified'] -ne 1) { throw "Exit/verification failed for $testName" }
        if ($v.Contains('adapter') -and $v['adapter'] -cne $row.pre.name) { throw 'Adapter identity differs' }
        $required = @($timings) + @($workload)
        if (!$SuiteFile) {
            $required += 'adapter','shader_digest','peak_working_set_bytes','sampled_local_peak_mb'
            Assert-Keys $timings @($v.Keys | Where-Object { $_.EndsWith('_us') }) 'Default suite'
        }
        $missing = @($required | Where-Object { !$v.Contains($_) })
        if ($missing.Count) { throw "Missing required metrics: $($missing -join ', ')" }
        foreach ($key in $timings) {
            if ($v[$key] -isnot [double]) { throw "Nonnumeric timing $key" }
        }
        Assert-Keys $report.runs[0].values.Keys $v.Keys "Arm $label sample $i"
        if ($v.Contains('shader_digest') -and $v.shader_digest -cne $report.runs[0].values.shader_digest) {
            throw 'DXIL changed within arm'
        }
        $row.ok = $true
        if ($report.formal) { $thermal.ready = Warm-Ready $row.post }
        return $true
    } catch {
        $row.error = Protect-Log $_.Exception.Message
        $report.invalid_count++
        if (!$row.Contains('post')) { $row.post = Get-Gpu }
        throw
    }
}

function Telemetry-Range($rows, $phase) {
    $range = [ordered]@{}
    $range.pstate = @($rows | ForEach-Object { $_[$phase].pstate } | Sort-Object -Unique)
    foreach ($key in 'clocks.gr','clocks.mem','pcie.link.gen.current','pcie.link.width.current','temperature.gpu','power.draw') {
        $v = $rows | ForEach-Object { [double]$_[$phase][$key] } | Measure-Object -Minimum -Maximum
        $range[$key] = @($v.Minimum,$v.Maximum)
    }
    $range
}

function Paired-Report($a, $b) {
    # Each array is indexed by chronological pair, even on BA rounds.
    $null = Get-ComparableKeys $a.runs[0].values.Keys $b.runs[0].values.Keys $suiteName 'A/B'
    foreach ($key in $workload) {
        $base = $a.runs[0].values[$key]
        foreach ($row in (@($a.runs) + @($b.runs))) {
            if ($null -eq $base -or $row.values[$key] -ne $base) { throw "Workload mismatch: $key" }
        }
    }
    $table = [ordered]@{}
    [string[]]$keys = @(Get-ComparableKeys $a.summary.Keys $b.summary.Keys $suiteName 'A/B summary')
    [array]::Sort($keys, [StringComparer]::Ordinal)
    # A zero baseline makes a pair's percentage undefined. Keep the two
    # medians and mark the entire metric inestimable instead of dropping pairs.
    $undefined = @($keys | Where-Object {
        $k = $_
        @($a.runs | Where-Object { $_.values[$k] -le 0 }).Count -gt 0
    })
    foreach ($k in $undefined) {
        $table[$k] = [ordered]@{a_median=$a.summary[$k].median; b_median=$b.summary[$k].median;
            change_pct=$null; ci95_pct=$null; family95_pct=$null}
    }
    $keys = @($keys | Where-Object { $_ -cnotin $undefined })
    $lines = @($keys | ForEach-Object { "$_`t$(Series $a.runs $_)`t$(Series $b.runs $_)" } | & $xtask bench-stats --paired --family-size $timings.Count)
    if ($LASTEXITCODE -ne 0 -or $lines.Count -ne $keys.Count) { throw 'Paired bootstrap failed' }
    foreach ($line in $lines) {
        $k,$m,$n,$d,$lo,$hi,$flo,$fhi = $line.Split("`t")
        $table[$k] = [ordered]@{a_median=[double]$m; b_median=[double]$n; change_pct=[double]$d;
            ci95_pct=@([double]$lo,[double]$hi); family95_pct=@([double]$flo,[double]$fhi)}
    }
    $estimable = @($timings | Where-Object { $_ -cnotin $undefined })
    $outside = @($estimable | Where-Object { $table[$_].family95_pct[0] -gt 0 -or $table[$_].family95_pct[1] -lt 0 })
    $nominal = @($estimable | Where-Object { $table[$_].ci95_pct[0] -gt 0 -or $table[$_].ci95_pct[1] -lt 0 })
    $aa = ($a.binary.sha256 -ceq $b.binary.sha256)
    [ordered]@{schema=1; complete=$true; suite=$suiteName; test=$testName; parameters=$parameters; family_size=$timings.Count; pairs=$Runs; a_commit=$a.binary.commit; b_commit=$b.binary.commit; a_correct=$a.complete;
        b_correct=$b.complete; quiet_window=$a.quiet_window; same_binary=$aa; order='alternating AB / BA by pair'; bootstrap_draws=5000; seed=5090;
        undefined_percentages=$undefined; pointwise_excludes_zero=$nominal; family_excludes_zero=$outside; family_pass=(!$aa -or ($estimable.Count -eq $timings.Count -and !$outside.Count)); pass=(!$aa -or ($estimable.Count -eq $timings.Count -and !$nominal.Count)); metrics=$table}
}

function Sample {
    $paired = $Mode -eq 'ab'
    if ($Mode -eq 'smoke' -and [IO.File]::Exists($QuietState)) { throw 'GPU reserved for formal sampling' }
    $debugLayerStatus = Get-DebugLayerStatus
    $warmer = Get-Target $root
    $targets = [ordered]@{A=$warmer}
    if ($paired) { $targets.A = Get-Target $Before; $targets.B = Get-Target $After }
    $system = [ordered]@{cpu=(Get-CimInstance Win32_Processor).Name; ram_bytes=(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory;
        os=(Get-CimInstance Win32_OperatingSystem).Version}
    $gpu = Get-Gpu
    [void][IO.Directory]::CreateDirectory($OutDir)
    $file = [IO.File]::Open((Join-Path $OutDir run.txt), 'CreateNew', 'Write', 'Read')
    $log = [IO.StreamWriter]::new($file, $utf8)
    $log.AutoFlush = $true
    $quietWindow = $null
    if ($Mode -ne 'smoke') { $quietWindow = Read-QuietWindow }
    $thermal = @{ready=$false}
    $reports = @{}
    $paths = @{}
    try {
        foreach ($label in $targets.Keys) {
            $reports[$label] = New-Report $targets[$label] $system $gpu
            if ($Mode -ne 'smoke') {
                $reports[$label].quiet_window = $quietWindow
                $reports[$label].warmup = [ordered]@{binary=$warmer.binary; reuse='previous post and current pre both qualified'; stable_ms=3000; timeout_ms=12000;
                    pstate=@('P0','P1'); min_graphics_mhz=2500; min_memory_mhz=13000; pcie_gen=5; pcie_width=16;
                    trace_fields=@('elapsed_ms','pstate','graphics_mhz','memory_mhz','pcie_gen','pcie_width')}
            }
            $dir = if ($paired) { Join-Path $OutDir $label } else { $OutDir }
            [void][IO.Directory]::CreateDirectory($dir)
            $paths[$label] = Join-Path $dir results.json
            Save-Json $paths[$label] $reports[$label]
        }
        for ($i=1; $i -le $Runs; $i++) {
            $order = @($targets.Keys)
            if ($paired -and $i % 2 -eq 0) { [array]::Reverse($order) }
            $position = 0
            foreach ($label in $order) {
                $position++
                do {
                    try {
                        $valid = Measure-One $targets[$label] $warmer $reports[$label] $i $position $label $log $thermal
                        if ($paired -and $valid -and $position -eq 2) {
                            $null = Get-ComparableKeys $reports.A.runs[-1].values.Keys $reports.B.runs[-1].values.Keys $suiteName "A/B pair $i"
                        }
                    }
                    finally { Save-Json $paths[$label] $reports[$label] }
                    if ($reports[$label].invalid_count -gt $reports[$label].max_invalid) { throw "Invalid-sample limit exceeded for $label" }
                } until ($valid)
            }
        }
        foreach ($label in $targets.Keys) {
            $r = $reports[$label]
            $r.summary = Summarize $r.runs
            $r.telemetry = [ordered]@{pre=Telemetry-Range $r.runs pre; post=Telemetry-Range $r.runs post}
            $r.complete = $true
            Save-Json $paths[$label] $r
        }
        if ($paired) {
            $result = Paired-Report $reports.A $reports.B
            Save-Json (Join-Path $OutDir paired.json) $result
            Write-Host "Correctness: A $($result.a_correct), B $($result.b_correct). Median paired change [pointwise 95%]:"
            foreach ($k in $result.metrics.Keys) {
                $v = $result.metrics[$k]
                if ($null -eq $v.change_pct) { Write-Host "${k}: undefined percentage (nonpositive baseline)"; continue }
                Write-Host ('{0}: {1:+0.00;-0.00;0.00}% [{2:F2}%, {3:F2}%]' -f $k,$v.change_pct,$v.ci95_pct[0],$v.ci95_pct[1])
            }
            if (!$result.pass) { throw 'A/A timing interval excludes zero or is undefined; inspect paired.json' }
            Write-Host "Paired bootstrap: $Runs pairs; see paired.json"
        }
    } catch {
        if ($paired -and !(Test-Path -LiteralPath (Join-Path $OutDir paired.json))) {
            $failure = [ordered]@{schema=1; complete=$false; pass=$false; requested_pairs=$Runs;
                error=Protect-Log $_.Exception.Message; arms=[ordered]@{}}
            foreach ($label in $reports.Keys) {
                $r=$reports[$label]
                $failure.arms[$label] = @{completed=@($r.runs | Where-Object { $_.ok }).Count; invalid=$r.invalid_count}
            }
            Save-Json (Join-Path $OutDir paired.json) $failure
        }
        throw
    } finally { $log.Dispose() }
}
function Passed($report) {
    if (!$report.complete -or $report.runs.Count -ne $report.requested -or !$report.runs.Count) { return $false }
    foreach ($row in $report.runs) {
        if (!$row.ok -or $row.exit -ne 0 -or $row.values.verified -ne 1) { return $false }
    }
    return $true
}
function Compare-Reports {
    $a = Read-Json $Before
    $b = Read-Json $After
    $labels = '基线','对比'
    $i = 0
    foreach ($report in $a,$b) {
        $ok = if (Passed $report) { '全过' } else { '未全过' }
        $commit = $report.binary.commit.Substring(0, [Math]::Min(12,$report.binary.commit.Length))
        Write-Output "$($labels[$i++]) ${commit}，正确性${ok}，$($report.runs.Count)/$($report.requested) 次，正式采样=$($report.formal)，$($report.debug_layer)"
    }
    if (!(Passed $a) -or !(Passed $b)) { exit 1 }
    if ($a.schema -ne $b.schema -or $a.suite -cne $b.suite) { throw '基准格式或工作负载版本不同' }
    [string[]]$keys = @(Get-ComparableKeys @($a.summary.PSObject.Properties.Name) @($b.summary.PSObject.Properties.Name) $a.suite 'Comparison')
    [array]::Sort($keys, [StringComparer]::Ordinal)
    if ($a.PSObject.Properties['workload_keys'] -and $b.PSObject.Properties['workload_keys']) {
        Assert-Keys $a.workload_keys $b.workload_keys 'Workload'
        Assert-Keys $a.timing_keys $b.timing_keys 'Timing'
        if ($a.test -cne $b.test -or (ConvertTo-Json $a.parameters -Compress) -cne (ConvertTo-Json $b.parameters -Compress)) {
            throw 'Different test or benchmark parameters'
        }
        $workload = $a.workload_keys
    }
    foreach ($key in $workload) {
        $base = $a.runs[0].values.$key
        foreach ($row in ($a.runs + $b.runs)) {
            if ($row.values.$key -ne $base) { throw "工作量不同 $key" }
        }
    }
    if ($a.gpu.name -cne $b.gpu.name -or $a.gpu.driver_version -cne $b.gpu.driver_version -or $a.formal -ne $b.formal) {
        Write-Output '两份数据的 GPU、驱动或空闲条件不同。'
    }
    if ($a.runs[0].values.PSObject.Properties['shader_digest'] -and $b.runs[0].values.PSObject.Properties['shader_digest']) {
        $digest = $a.runs[0].values.shader_digest
        $changed = @($a.runs + $b.runs | Where-Object { $_.values.shader_digest -cne $digest }).Count
        Write-Output $(if ($changed) { 'DXIL 摘要有变化，请结合输出校验解读' } else { 'DXIL 摘要一致' })
    }
    Write-Output '| 指标 | 基线中位数 | 对比中位数 | 变化 | 95% 区间 | 超出噪声 |'
    Write-Output '|---|---:|---:|---:|---:|---|'
    $keys | ForEach-Object { "$_`t$(Series $a.runs $_)`t$(Series $b.runs $_)" } | & $xtask bench-stats --compare
    if ($LASTEXITCODE) { throw 'Cannot compare samples' }
}
switch ($Mode) {
    'build' { Build-Benchmark }
    'compare' {
        if (!$StatsExe) { Build-Stats }
        Compare-Reports
    }
    default { Sample }
}
