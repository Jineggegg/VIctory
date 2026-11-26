# Windows half of hot.sh. Use the WSL entry point for sampling.
param(
    [Parameter(Mandatory, Position=0)][ValidateSet('build','smoke','sample','ab','compare')][string]$Mode,
    [Parameter(Position=1)][string]$Before,
    [Parameter(Position=2)][string]$After,
    [string]$StatsExe, [string]$EnvFile, [string]$Distro, [int]$Runs, [string]$OutDir, [string]$QuietState
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
    $gpu = [ordered]@{}
    foreach ($field in $gpuFields) { $gpu[$field] = $row.$field.Trim() }
    $gpu
}
$timings = ('pso_cold pso_hot prebuild_16_cold prebuild_16_hot shader_batch_translate shader_batch_cold shader_batch_hot ' +
    'descriptors_1300 descriptors_14400 submit_32 submit_fence_wait submit_prepare submit_queue_and_fence texture_full ' +
    'texture_partial wave_page_marks_gpu loop_page_lookup_gpu shadow_refresh marked_writeback_frame').Split(' ') |
    ForEach-Object { $_ + '_us' }

function Read-Metrics($text) {
    $values = [ordered]@{}
    foreach ($match in [regex]::Matches($text, 'BENCH (\w+)=(.*)')) {
        $key = $match.Groups[1].Value
        $raw = $match.Groups[2].Value.Trim()
        if ($values.Contains($key)) { throw "Duplicate metric $key" }
        $values[$key] = if ($key -cin 'adapter','shader_digest') { $raw } else { [double]::Parse($raw, $culture) }
    }
    $values
}
function Run-Child($exe, $cwd, $test) {
    $process = [Diagnostics.Process]::new()
    $start = $process.StartInfo
    $start.FileName = $exe
    $start.WorkingDirectory = $cwd
    $start.Arguments = "--exact $test --ignored --nocapture --test-threads=1"
    $start.UseShellExecute = $false
    $start.RedirectStandardOutput = $start.RedirectStandardError = $true
    $start.StandardOutputEncoding = $start.StandardErrorEncoding = $utf8
    try {
        [void]$process.Start()
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (!$process.WaitForExit(180000)) {
            $process.Kill()
            $process.WaitForExit()
            throw 'Hotspot test timed out after 180s'
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

function New-Report($target, $system, $gpu) {
    [ordered]@{system=$system; schema=1; suite='victory-hot-v1'; formal=($Mode -ne 'smoke'); complete=$false;
        debug_layer='pending: Windows Graphics Tools unavailable'; binary=$target.binary; requested=$Runs; utc=Utc-Now;
        gpu=$gpu; profile='diag'; runs=[Collections.ArrayList]::new(); summary=@{}; invalid_count=0; max_invalid=0}
}

function Warm-Ready($gpu) {
    # GPU Boost changes individual bins with temperature. Require the high
    # performance P-state, the full negotiated link and near-boost clocks.
    foreach ($key in 'clocks.gr','clocks.mem','clocks.max.gr','clocks.max.mem','pcie.link.gen.current',
            'pcie.link.gen.max','pcie.link.width.current','pcie.link.width.max') {
        if ($gpu[$key] -notmatch '^\d+$' -or [double]$gpu[$key] -le 0) { return $false }
    }
    ($gpu.pstate -in 'P0','P2' -and [double]$gpu.'clocks.gr' -ge 0.80 * [double]$gpu.'clocks.max.gr' -and
        [double]$gpu.'clocks.mem' -ge 0.90 * [double]$gpu.'clocks.max.mem' -and
        $gpu.'pcie.link.gen.current' -eq $gpu.'pcie.link.gen.max' -and
        $gpu.'pcie.link.width.current' -eq $gpu.'pcie.link.width.max')
}

function Measure-One($target, $warmer, $report, $i, $position, $label, $log) {
    $row = [ordered]@{index=$i; position=$position; utc=Utc-Now; ok=$false; polls=[Collections.ArrayList]::new()}
    [void]$report.runs.Add($row)
    try {
        if (!$report.formal -and [IO.File]::Exists($QuietState)) { throw 'GPU reserved for formal sampling' }
        if ($report.formal) {
            for ($attempt=1; $attempt -le 2; $attempt++) {
                $warm = Run-Child $warmer.exe $warmer.dir 'pipeline::hot::warm_adapter'
                if ($warm.exit -ne 0 -or $warm.text -notmatch 'WARMUP completed') {
                    throw (Protect-Log "GPU warmup failed: $($warm.text)")
                }
                $row.pre = Get-Gpu
                [void]$row.polls.Add($row.pre)
                if (Warm-Ready $row.pre) { break }
            }
            if (!(Warm-Ready $row.pre)) { throw 'GPU clocks or PCIe link below the warmup gate' }
        } else {
            $row.pre = Get-Gpu
            [void]$row.polls.Add($row.pre)
        }
        Write-Host "Sample $label $i/$Runs $($row.pre.pstate) $($row.pre.'clocks.gr')/$($row.pre.'clocks.mem') MHz"
        $child = Run-Child $target.exe $target.dir 'pipeline::hot::hot_paths'
        $row.post = Get-Gpu
        $log.WriteLine((Protect-Log "`nSample $label $i, position $position`n$($child.text)`nexit $($child.exit)"))
        $row.exit = $child.exit
        $v = Read-Metrics $child.text
        $row.values = $v
        $times = @($v.Keys | Where-Object { $_.EndsWith('_us') })
        if ($row.exit -ne 0 -or $v['verified'] -ne 1 -or $v['adapter'] -cne $row.pre.name -or
            !$v.Contains('shader_digest') -or !$v.Contains('peak_working_set_bytes') -or
            !$v.Contains('sampled_local_peak_mb') -or $times.Count -ne $timings.Count -or
            @($timings | Where-Object { $_ -cnotin $times }).Count) { throw 'Hotspot correctness check failed' }
        if ($v.shader_digest -cne $report.runs[0].values.shader_digest) { throw 'DXIL changed within arm' }
        $row.ok = $true
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
    $workload = 'shader_count','prebuild_count','submit_count','shadow_refresh_bytes','descriptors_1300_changed',
        'descriptors_14400_changed','wave_page_marks_lanes','loop_global_loads','texture_title_patch_bytes'
    foreach ($key in $workload) {
        $base = $a.runs[0].values[$key]
        foreach ($row in (@($a.runs) + @($b.runs))) {
            if ($null -eq $base -or $row.values[$key] -ne $base) { throw "Workload mismatch: $key" }
        }
    }
    if (Compare-Object @($a.summary.Keys) @($b.summary.Keys) -CaseSensitive) { throw 'Metric sets differ' }
    $table = [ordered]@{}
    $keys = $timings + @('peak_working_set_bytes','sampled_local_peak_mb')
    $lines = $keys | ForEach-Object { "$_`t$(Series $a.runs $_)`t$(Series $b.runs $_)" } | & $xtask bench-stats --paired
    if ($LASTEXITCODE -ne 0 -or $lines.Count -ne $keys.Count) { throw 'Paired bootstrap failed' }
    foreach ($line in $lines) {
        $k,$m,$n,$d,$lo,$hi,$flo,$fhi = $line.Split("`t")
        $table[$k] = [ordered]@{a_median=[double]$m; b_median=[double]$n; change_pct=[double]$d;
            ci95_pct=@([double]$lo,[double]$hi); family95_pct=@([double]$flo,[double]$fhi)}
    }
    $outside = @($timings | Where-Object { $table[$_].family95_pct[0] -gt 0 -or $table[$_].family95_pct[1] -lt 0 })
    $nominal = @($timings | Where-Object { $table[$_].ci95_pct[0] -gt 0 -or $table[$_].ci95_pct[1] -lt 0 })
    $aa = ($a.binary.sha256 -ceq $b.binary.sha256)
    [ordered]@{schema=1; pairs=$Runs; a_commit=$a.binary.commit; b_commit=$b.binary.commit; a_correct=$a.complete;
        b_correct=$b.complete; same_binary=$aa; order='alternating AB / BA by pair'; bootstrap_draws=5000; seed=5090;
        pointwise_excludes_zero=$nominal; family_excludes_zero=$outside; pass=(!$aa -or $outside.Count -eq 0); metrics=$table}
}

function Sample {
    $paired = $Mode -eq 'ab'
    if ($Mode -eq 'smoke' -and [IO.File]::Exists($QuietState)) { throw 'GPU reserved for formal sampling' }
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
    $reports = @{}
    $paths = @{}
    try {
        foreach ($label in $targets.Keys) {
            $reports[$label] = New-Report $targets[$label] $system $gpu
            if ($Mode -ne 'smoke') {
                $reports[$label].warmup = [ordered]@{binary=$warmer.binary; seconds=2; max_attempts=2;
                    pstate=@('P0','P2'); min_graphics_fraction=0.80; min_memory_fraction=0.90; full_link=$true}
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
                try { Measure-One $targets[$label] $warmer $reports[$label] $i $position $label $log }
                finally { Save-Json $paths[$label] $reports[$label] }
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
            if (!$result.pass) { throw 'A/A family interval excludes zero; inspect paired.json' }
            Write-Host "Paired bootstrap: $Runs pairs; see paired.json"
        }
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
    [string[]]$keys = @($a.summary.PSObject.Properties.Name)
    [array]::Sort($keys, [StringComparer]::Ordinal)
    if (Compare-Object $keys @($b.summary.PSObject.Properties.Name) -CaseSensitive) { throw '指标集合不同，不能逐项比较' }
    $workload = ('shader_count prebuild_count submit_count shadow_refresh_bytes descriptors_1300_changed ' +
        'descriptors_14400_changed wave_page_marks_lanes loop_global_loads texture_title_patch_bytes').Split(' ')
    foreach ($key in $workload) {
        $base = $a.runs[0].values.$key
        foreach ($row in ($a.runs + $b.runs)) {
            if ($row.values.$key -ne $base) { throw "工作量不同 $key" }
        }
    }
    if ($a.gpu.name -cne $b.gpu.name -or $a.gpu.driver_version -cne $b.gpu.driver_version -or $a.formal -ne $b.formal) {
        Write-Output '两份数据的 GPU、驱动或空闲条件不同。'
    }
    $digest = $a.runs[0].values.shader_digest
    $changed = @($a.runs + $b.runs | Where-Object { $_.values.shader_digest -cne $digest }).Count
    Write-Output $(if ($changed) { 'DXIL 摘要有变化，请结合输出校验解读' } else { 'DXIL 摘要一致' })
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
