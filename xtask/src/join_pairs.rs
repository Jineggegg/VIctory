//! Join already-paired samples without pairing observations across windows.
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Number, Value, json};

use crate::bench::{paired_interval, sorted_median, stdev};

#[derive(Deserialize)]
struct Run {
    index: usize,
    position: u8,
    ok: bool,
    values: Map<String, Value>,
}

#[derive(Deserialize)]
struct Binary {
    sha256: String,
}

#[derive(Deserialize)]
struct Arm {
    complete: bool,
    formal: bool,
    requested: usize,
    binary: Binary,
    runs: Vec<Run>,
    summary: Map<String, Value>,
}

#[derive(Deserialize)]
struct Interval {
    change_pct: f64,
    ci95_pct: [f64; 2],
    family95_pct: [f64; 2],
}

#[derive(Deserialize)]
struct Pairing {
    complete: bool,
    pairs: usize,
    suite: String,
    parameters: Value,
    family_size: usize,
    quiet_window: Value,
    a_commit: String,
    b_commit: String,
    metrics: Map<String, Value>,
}

fn load<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes))
        .with_context(|| format!("parse {}", path.display()))
}

fn series(arm: &Arm, key: &str) -> Result<Vec<Number>> {
    arm.runs
        .iter()
        .map(|run| {
            run.values
                .get(key)
                .and_then(Value::as_number)
                .cloned()
                .with_context(|| format!("missing numeric {key} in sample {}", run.index))
        })
        .collect()
}

fn floats(numbers: &[Number]) -> Vec<f64> {
    numbers
        .iter()
        .map(|v| v.as_f64().expect("finite JSON number"))
        .collect()
}

fn read_window(path: &Path) -> Result<(Pairing, Arm, Arm)> {
    let p: Pairing = load(&path.join("paired.json"))?;
    ensure!(
        p.complete && p.pairs >= 2 && p.family_size > 0,
        "incomplete window"
    );
    let a: Arm = load(&path.join("A/results.json"))?;
    let b: Arm = load(&path.join("B/results.json"))?;
    for arm in [&a, &b] {
        ensure!(
            arm.complete && arm.formal && arm.requested == p.pairs && arm.runs.len() == p.pairs,
            "incomplete formal arm"
        );
        ensure!(
            arm.runs
                .iter()
                .all(|r| r.ok && r.values.get("verified").and_then(Value::as_f64) == Some(1.0)),
            "unverified sample"
        );
    }
    for (a, b) in a.runs.iter().zip(&b.runs) {
        ensure!(
            a.index == b.index && matches!((a.position, b.position), (1, 2) | (2, 1)),
            "mismatched pair"
        );
    }
    for (key, value) in &p.metrics {
        let x = floats(&series(&a, key)?);
        if x.iter().any(|&v| v <= 0.0) {
            continue;
        }
        let stored: Interval = serde_json::from_value(value.clone())?;
        let [lo, hi] = stored.ci95_pct;
        let [fl, fh] = stored.family95_pct;
        let actual = paired_interval(&x, &floats(&series(&b, key)?), p.family_size)?;
        ensure!(
            actual
                .into_iter()
                .zip([stored.change_pct, lo, hi, fl, fh])
                .all(|(x, y)| (x - y).abs() < 1e-6),
            "stored interval mismatch for {key}"
        );
    }
    Ok((p, a, b))
}

fn summary(values: &[Number]) -> Value {
    let mut sorted = values.to_vec();
    sorted.sort_by(|x, y| x.as_f64().unwrap().total_cmp(&y.as_f64().unwrap()));
    let nums = floats(&sorted);
    let n = nums.len();
    // Odd medians and nearest ranks retain the original sample's JSON type.
    let median = if n % 2 == 1 {
        json!(sorted[n / 2])
    } else {
        json!(sorted_median(&nums))
    };
    json!({
        "n": n, "median": median,
        "p95": sorted[(95 * n).div_ceil(100) - 1],
        "p99": sorted[(99 * n).div_ceil(100) - 1],
        "sample_stddev": stdev(&nums)
    })
}

pub fn run(first: &Path, second: &Path, output: &Path) -> Result<()> {
    let (p, a, b) = read_window(first).with_context(|| first.display().to_string())?;
    let (q, mut x, mut y) = read_window(second).with_context(|| second.display().to_string())?;
    ensure!(
        p.suite == q.suite && p.parameters == q.parameters && p.family_size == q.family_size,
        "incompatible benchmark windows"
    );
    ensure!(p.quiet_window != q.quiet_window, "same sampling window");
    if a.binary.sha256 != x.binary.sha256 {
        std::mem::swap(&mut x, &mut y);
    }
    ensure!(
        a.binary.sha256 == x.binary.sha256 && b.binary.sha256 == y.binary.sha256,
        "executable hashes differ"
    );
    let mut metrics = Map::new();
    for key in a.summary.keys() {
        let mut original = series(&a, key)?;
        original.extend(series(&x, key)?);
        let mut updated = series(&b, key)?;
        updated.extend(series(&y, key)?);
        let mut cell = json!({"A":summary(&original), "B":summary(&updated)});
        let numbers = floats(&original);
        if numbers.iter().all(|&v| v > 0.0) {
            let [change, low, high, family_low, family_high] =
                paired_interval(&numbers, &floats(&updated), p.family_size)?;
            cell["change_pct"] = json!(change);
            cell["ci95_pct"] = json!([low, high]);
            cell["family95_pct"] = json!([family_low, family_high]);
        }
        metrics.insert(key.clone(), cell);
    }
    let report = json!({
        "suite": p.suite, "pairs": p.pairs + q.pairs,
        "windows": [first.file_name().context("first window name")?.to_str(),
                    second.file_name().context("second window name")?.to_str()],
        "a_commit": p.a_commit, "b_commit": p.b_commit,
        "family_size": p.family_size, "metrics": metrics
    });
    let mut text = serde_json::to_string_pretty(&report)?;
    text.push('\n');
    // Match the reference writer's native text-file line endings.
    if cfg!(windows) {
        text = text.replace('\n', "\r\n");
    }
    fs::write(output, text).with_context(|| format!("write {}", output.display()))
}
