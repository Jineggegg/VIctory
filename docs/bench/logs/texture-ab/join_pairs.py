"""Combine two texture sampling windows, matching builds by executable hash."""

from argparse import ArgumentParser
import json
from math import ceil
from pathlib import Path
from random import Random
from statistics import median, stdev


def load(path):
    return json.loads(path.read_text(encoding="utf-8-sig"))


def interval(original, updated, family_size):
    changes = [100 * (b / a - 1) for a, b in zip(original, updated, strict=True)]
    rng = Random(5090)
    samples = sorted(median(rng.choices(changes, k=len(changes))) for _ in range(5000))
    return {
        "change_pct": median(changes),
        "ci95_pct": [samples[124], samples[4874]],
        "family95_pct": [samples[ceil(125 / family_size) - 1], samples[4999 - 125 // family_size]],
    }


def stats(values):
    ordered = sorted(values)
    n = len(values)
    return {
        "n": n,
        "median": median(ordered),
        "p95": ordered[ceil(0.95 * n) - 1],
        "p99": ordered[ceil(0.99 * n) - 1],
        "sample_stddev": stdev(ordered),
    }


def series(arm, key):
    return [run["values"][key] for run in arm["runs"]]


def read_window(directory):
    paired = load(directory / "paired.json")
    a, b = [load(directory / arm / "results.json") for arm in ("A", "B")]
    assert paired["complete"]
    for arm in (a, b):
        assert arm["complete"] and arm["formal"] and arm["requested"] == paired["pairs"]
        assert len(arm["runs"]) == paired["pairs"]
        assert all(run["ok"] and run["values"]["verified"] == 1 for run in arm["runs"])
    for x, y in zip(a["runs"], b["runs"], strict=True):
        assert x["index"] == y["index"] and {x["position"], y["position"]} == {1, 2}
    # Reproduce the native runner's intervals before combining any samples.
    for key, measurements in paired["metrics"].items():
        original, updated = series(a, key), series(b, key)
        if min(original) <= 0:
            continue
        result = interval(original, updated, paired["family_size"])
        assert abs(result["change_pct"] - measurements["change_pct"]) < 1e-6
        for name in ("ci95_pct", "family95_pct"):
            assert all(abs(x - y) < 1e-6 for x, y in zip(result[name], measurements[name], strict=True))
    return paired, a, b


def combine(first, second):
    p, a, b = read_window(first)
    q, x, y = read_window(second)
    assert p["suite"] == q["suite"] and p["parameters"] == q["parameters"]
    assert p["quiet_window"] != q["quiet_window"] and p["family_size"] == q["family_size"]
    if x["binary"]["sha256"] != a["binary"]["sha256"]:
        x, y = y, x
    assert a["binary"]["sha256"] == x["binary"]["sha256"]
    assert b["binary"]["sha256"] == y["binary"]["sha256"]
    metrics = {}
    for key in a["summary"]:
        original = series(a, key) + series(x, key)
        updated = series(b, key) + series(y, key)
        measurements = {"A": stats(original), "B": stats(updated)}
        if min(original) > 0:
            measurements.update(interval(original, updated, p["family_size"]))
        metrics[key] = measurements
    return {
        "suite": p["suite"],
        "pairs": p["pairs"] + q["pairs"],
        "windows": [first.name, second.name],
        "a_commit": p["a_commit"],
        "b_commit": p["b_commit"],
        "family_size": p["family_size"],
        "metrics": metrics,
    }


if __name__ == "__main__":
    parser = ArgumentParser(description=__doc__)
    parser.add_argument("first", type=Path)
    parser.add_argument("second", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    args.output.write_text(json.dumps(combine(args.first, args.second), indent=2) + "\n")
