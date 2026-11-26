"""Compare two hot.py result sets using independent-sample percentile bootstrap intervals."""
import argparse
import json
import math
from pathlib import Path
import random
from statistics import median
import sys


def passed(report):
    rows = report["runs"]
    return (report.get("complete") and len(rows) == report["requested"] and len(rows) > 0
            and all(r.get("ok") and r.get("exit") == 0 and r["values"].get("verified") == 1 for r in rows))


def interval(a, b, draws=5000):
    # Independent fresh processes in each run. Resample each set at its own original size.
    randoms = random.Random(5090)
    changes = []
    for _ in range(draws):
        before = median(randoms.choices(a, k=len(a)))
        after = median(randoms.choices(b, k=len(b)))
        if before == 0:
            return None
        changes.append(100 * (after / before - 1))
    changes.sort()
    return changes[math.ceil(.025 * draws) - 1], changes[math.ceil(.975 * draws) - 1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    args = parser.parse_args()
    a, b = (json.loads(p.read_text()) for p in (args.before, args.after))
    for label, result in [("基线", a), ("对比", b)]:
        print(f"{label} {result['binary']['commit'][:12]}，正确性{'全过' if passed(result) else '未全过'}，"
              f"{len(result['runs'])}/{result['requested']} 次，正式采样={result['formal']}，{result['debug_layer']}")
    if not all(passed(r) for r in (a, b)):
        return 1
    if (a["schema"], a["suite"]) != (b["schema"], b["suite"]):
        sys.exit("基准格式或工作负载版本不同")
    if a["summary"].keys() != b["summary"].keys():
        sys.exit("指标集合不同，不能逐项比较")
    for key in ["shader_count", "prebuild_count", "submit_count", "shadow_refresh_bytes", "descriptors_1300_changed",
                "descriptors_14400_changed", "wave_page_marks_lanes", "loop_global_loads", "texture_title_patch_bytes"]:
        if len({row["values"][key] for r in (a, b) for row in r["runs"]}) != 1:
            sys.exit(f"工作量不同 {key}")
    if any(a["gpu"][k] != b["gpu"][k] for k in ("name", "driver_version")) or a["formal"] != b["formal"]:
        print("两份数据的 GPU、驱动或空闲条件不同。")
    digests = {r["values"]["shader_digest"] for result in (a, b) for r in result["runs"]}
    print("DXIL 摘要" + ("一致" if len(digests) == 1 else "有变化，请结合输出校验解读"))
    print("| 指标 | 基线中位数 | 对比中位数 | 变化 | 95% 区间 | 超出噪声 |")
    print("|---|---:|---:|---:|---:|---|")
    for key in sorted(a["summary"]):
        x, y = ([r["values"][key] for r in report["runs"]] for report in (a, b))
        m, n = median(x), median(y)
        ci = interval(x, y) if len(x) >= 2 and len(y) >= 2 and m else None
        delta = f"{100 * (n / m - 1):+.2f}%" if m else "无基数"
        band = f"[{ci[0]:+.2f}%, {ci[1]:+.2f}%]" if ci else "无法估计"
        decision = ("是" if ci[0] > 0 or ci[1] < 0 else "否") if ci else "无法估计"
        print(f"| {key} | {m:.3f} | {n:.3f} | {delta} | {band} | {decision} |")
    return 0


if __name__ == "__main__":
    sys.exit(main())
