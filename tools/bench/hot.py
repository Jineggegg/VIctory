"""WSL entry point for the native Windows hotspot benchmark. No Python dependencies."""
import argparse
import csv
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone

ROOT = Path(__file__).resolve().parents[2]
PS = "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe"
PROFILE = "diag"
TEST = "pipeline::hot::hot_paths"
MANIFEST = ROOT / "target" / "hot-bench.json"
METRICS = {
    "pso_cold", "pso_hot", "prebuild_16_cold", "prebuild_16_hot", "shader_batch_translate",
    "shader_batch_cold", "shader_batch_hot", "descriptors_1300", "descriptors_14400",
    "submit_32", "submit_fence_wait", "submit_prepare", "submit_queue_and_fence", "texture_full", "texture_partial",
    "wave_page_marks_gpu", "loop_page_lookup_gpu", "shadow_refresh", "marked_writeback_frame",
}
SMI_FIELDS = [
    "name", "driver_version", "memory.total", "utilization.gpu", "memory.used",
    "temperature.gpu", "clocks.gr", "clocks.mem", "power.draw", "pstate",
]


def output(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def quote_ps(value):
    return "'" + str(value).replace("'", "''") + "'"


def clean(text):
    text = re.sub(r"(?i)[A-Z]:[\\/]Users[\\/][^\\/\s\"']+", "%USERPROFILE%", text)
    return re.sub(r"(?i)GPU-[a-f0-9-]{32,}", "GPU-REDACTED", text)


def windows(script):
    env_file = Path(os.getenv("VICTORY_BENCH_ENV", ROOT.parent / "toolchain" / "env.ps1"))
    env = quote_ps(output("wslpath", "-w", str(env_file.resolve())))
    root = quote_ps(output("wslpath", "-w", str(ROOT)))
    # Isolate the timing profile from inherited emulator diagnostics and copy-mode overrides.
    setup = (f"$ErrorActionPreference='Stop'; Get-ChildItem Env:VICTORY_* | Remove-Item; "
             f". {env}; Set-Location -LiteralPath {root}; $env:CARGO_TARGET_DIR=Join-Path (Get-Location) target; ")
    return [PS, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", setup + script]


def identity():
    return output("git", "rev-parse", "HEAD")


def source_delta():
    paths = ["--", ".", ":(exclude)docs/bench"]
    diff = subprocess.check_output(["git", "diff", "--binary", "HEAD", *paths], cwd=ROOT)
    files = subprocess.check_output(["git", "ls-files", "-z", "--others", "--exclude-standard", *paths], cwd=ROOT)
    if not diff and not files:
        return None
    digest = hashlib.sha256(diff)
    for name in sorted(filter(None, files.split(b"\0"))):
        digest.update(name + b"\0")
        digest.update(hashlib.sha256((ROOT / os.fsdecode(name)).read_bytes()).digest())
    return digest.hexdigest()


def save(path, data):
    temp = path.with_suffix(".tmp")
    temp.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")
    temp.replace(path)


def build():
    commit, delta = identity(), source_delta()
    command = ("cargo test --locked --profile diag -p victory-gpu --lib --features diagnostics "
               "--no-run --message-format=json; exit $LASTEXITCODE")
    p = subprocess.run(windows(command), cwd=ROOT, capture_output=True, text=True)
    print(clean(p.stderr), end="", flush=True)
    if p.returncode:
        raise RuntimeError(clean(p.stdout[-6000:]))
    artifacts = [json.loads(line) for line in p.stdout.splitlines() if line.startswith("{")]
    exe, = [a["executable"] for a in artifacts if a.get("target", {}).get("name") == "victory_gpu"
            and a.get("executable")]
    path = Path(output("wslpath", "-u", exe))
    manifest = dict(commit=commit, source_delta_sha256=delta, executable=str(path.relative_to(ROOT)),
                    sha256=hashlib.sha256(path.read_bytes()).hexdigest(),
                    rustc=output(*windows("rustc -Vv; exit $LASTEXITCODE")))
    if source_delta() != delta or identity() != commit:
        raise RuntimeError("Source changed during build")
    save(MANIFEST, manifest)


def gpu():
    smi = shutil.which("nvidia-smi") or "/usr/lib/wsl/lib/nvidia-smi"
    row, = csv.reader(output(smi, "-i", "0", "--query-gpu=" + ",".join(SMI_FIELDS),
                             "--format=csv,noheader,nounits").splitlines(), skipinitialspace=True)
    return dict(zip(SMI_FIELDS, row, strict=True))


def stats(values):
    seq = sorted(values)
    return dict(n=len(seq), median=statistics.median(seq), p95=seq[math.ceil(.95 * len(seq)) - 1],
                p99=seq[math.ceil(.99 * len(seq)) - 1],
                sample_stddev=statistics.stdev(seq) if len(seq) > 1 else None)


def sample(out, runs, informal):
    binary = json.loads(MANIFEST.read_text())
    path = ROOT / binary["executable"]
    if source_delta() != binary["source_delta_sha256"] or identity() != binary["commit"] or hashlib.sha256(path.read_bytes()).hexdigest() != binary["sha256"]:
        raise ValueError("Measured source or executable differs from build manifest")
    out.mkdir(parents=True, exist_ok=True)
    report_path = out / "results.json"
    system = json.loads(output(*windows(
        "[PSCustomObject]@{cpu=(Get-CimInstance Win32_Processor).Name; "
        "ram_bytes=(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory; "
        "os=(Get-CimInstance Win32_OperatingSystem).Version} | ConvertTo-Json -Compress")))
    report = dict(system=system, schema=1, suite="victory-hot-v1", formal=not informal, complete=False,
                  debug_layer="pending: Windows Graphics Tools unavailable", binary=binary,
                  requested=runs, utc=datetime.now(timezone.utc).isoformat(), gpu=gpu(),
                  profile=PROFILE, runs=[], summary={})
    with (out / "run.txt").open("x", buffering=1) as log:
        exe = quote_ps(output("wslpath", "-w", str(path)))
        command = windows(f"& {exe} --exact {TEST} --ignored --nocapture --test-threads=1; exit $LASTEXITCODE")
        for i in range(runs):
            row = dict(index=i + 1, utc=datetime.now(timezone.utc).isoformat(), ok=False, polls=[])
            report["runs"].append(row)
            try:
                until = time.monotonic() + 30
                while True:
                    pre = gpu()
                    row["polls"].append(pre)
                    if informal or float(pre["utilization.gpu"]) <= 10:
                        break
                    if time.monotonic() > until:
                        raise RuntimeError("Busy adapter before sample")
                    time.sleep(.5)
                print(f"Sample {i + 1}/{runs}, GPU {pre['utilization.gpu']}%", flush=True)
                child = subprocess.run(command, cwd=ROOT, stdout=subprocess.PIPE,
                                       stderr=subprocess.STDOUT, text=True, errors="replace", timeout=180)
                print(f"\nSample {i + 1} | {json.dumps(pre)}\n{clean(child.stdout)}\nexit {child.returncode}", file=log)
                row["exit"] = child.returncode
                values = {}
                for key, value in re.findall(r"BENCH (\w+)=(.*)", child.stdout):
                    if key in values:
                        raise RuntimeError(f"Duplicate metric {key}")
                    values[key] = value.strip() if key in ("adapter", "shader_digest") else float(value)
                row["values"] = values
                if (child.returncode or values.get("verified") != 1 or
                        values.get("adapter") != pre["name"] or
                        not {"shader_digest", "peak_working_set_bytes", "sampled_local_peak_mb"} <= values.keys() or
                        {k + "_us" for k in METRICS} != {k for k in values if k.endswith("_us")}):
                    raise RuntimeError(f"Validation failed in sample {i + 1}; see {out / 'run.txt'}")
                if i and values["shader_digest"] != report["runs"][0]["values"]["shader_digest"]:
                    raise RuntimeError("DXIL content changed within this sample set")
                row["ok"] = True
            finally:
                save(report_path, report)
    report["summary"] = {k: stats([row["values"][k] for row in report["runs"]])
                         for k, v in values.items() if isinstance(v, float) and k != "verified"}
    report["complete"] = True
    save(report_path, report)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["build", "run", "smoke", "_sample"])
    parser.add_argument("--runs", type=int)
    parser.add_argument("--out", type=Path)
    a = parser.parse_args()
    if a.mode == "build":
        build()
        return
    runs = a.runs if a.runs is not None else (3 if a.mode == "smoke" else 30)
    if runs < (1 if a.mode == "smoke" else 30):
        parser.error("Formal sampling requires at least 30 runs")
    out = (a.out or ROOT / "docs/bench/logs" / datetime.now().strftime("hot-%Y%m%d-%H%M%S")).resolve()
    if a.mode != "run":
        sample(out, runs, informal=a.mode == "smoke")
        return
    build()  # Always outside the GPU reservation.
    out.mkdir(parents=True, exist_ok=True)
    child = ["gpu-quiet", "run", "--", sys.executable, __file__, "_sample",
             "--runs", str(runs), "--out", str(out)]
    with (out / "window.txt").open("x", buffering=1) as log:
        with subprocess.Popen(child, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True) as proc:
            for line in proc.stdout:
                print(clean(line), end="", file=log)
                print(clean(line), end="", flush=True)
            if proc.wait():
                raise SystemExit(proc.returncode)


if __name__ == "__main__":
    main()
