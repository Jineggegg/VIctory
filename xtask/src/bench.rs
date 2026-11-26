//! The driver supplies one tab-separated series per metric (two for comparison).
//! Both report generation and historical re-analysis use this numeric backend.
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use clap::ValueEnum;

#[derive(Clone, Copy, ValueEnum)]
pub enum Mode {
    Build,
    Smoke,
    Run,
    Ab,
}

fn driver(root: &Path) -> Command {
    let mut cmd = Command::new("bash");
    cmd.arg(root.join("tools/bench/hot.sh"));
    cmd
}

pub fn hot(
    root: &Path,
    mode: Mode,
    runs: Option<usize>,
    out: Option<PathBuf>,
    arms: Option<(PathBuf, PathBuf)>,
) -> Result<()> {
    // A Windows -> WSL round trip loses the original build-queue ancestor.
    // Reserving every slot from there can deadlock against the calling wrapper.
    ensure!(
        !cfg!(windows),
        "start tools/bench/hot.sh in WSL for GPU sampling"
    );
    let mut cmd = driver(root);
    cmd.arg(mode.to_possible_value().unwrap().get_name());
    if let Some((a, b)) = arms {
        cmd.arg("--a")
            .arg(std::path::absolute(a)?)
            .arg("--b")
            .arg(std::path::absolute(b)?);
    }
    if let Some(count) = runs {
        cmd.arg("--runs").arg(count.to_string());
    }
    if let Some(path) = out {
        cmd.arg("--out").arg(std::path::absolute(path)?);
    }
    ensure!(cmd.status()?.success(), "hotspot benchmark failed");
    Ok(())
}

pub fn compare(root: &Path, before: &Path, after: &Path) -> Result<()> {
    let mut cmd = if cfg!(windows) {
        let mut cmd = Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(root.join("tools/bench/hot.ps1"))
            .arg("compare");
        cmd
    } else {
        let mut cmd = driver(root);
        cmd.arg("compare");
        cmd
    };
    cmd.arg(std::path::absolute(before)?)
        .arg(std::path::absolute(after)?);
    // The PS driver calls this executable back for pure statistics.
    if cfg!(windows) {
        cmd.arg("-StatsExe").arg(std::env::current_exe()?);
    }
    ensure!(cmd.status()?.success(), "hotspot comparison failed");
    Ok(())
}

fn sorted_median(sorted: &[f64]) -> f64 {
    let half = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[half]
    } else {
        (sorted[half - 1] + sorted[half]) / 2.0
    }
}

// Accumulate the variance with a second limb. Ordinary sums lose an ulp even
// on small historical batches; the reference stdev rounds the exact variance.
#[derive(Clone, Copy, Default)]
struct Wide(f64, f64);

impl Wide {
    fn add(self, other: Self) -> Self {
        let s = self.0 + other.0;
        let b = s - self.0;
        let e = (self.0 - (s - b)) + (other.0 - b) + (self.1 + other.1);
        let t = s + e;
        Self(t, e - (t - s))
    }
    fn sub(self, other: Self) -> Self {
        self.add(Self(-other.0, -other.1))
    }
    fn mul(self, other: Self) -> Self {
        let p = self.0 * other.0;
        Self(
            p,
            self.0.mul_add(other.0, -p) + self.0 * other.1 + self.1 * other.0,
        )
    }
    fn div(self, n: f64) -> Self {
        let q = Self(self.0 / n, 0.0);
        let rem = self.sub(q.mul(Self(n, 0.0)));
        q.add(Self((rem.0 + rem.1) / n, 0.0))
    }
}

fn stdev(values: &[f64]) -> Option<f64> {
    if values.len() == 1 {
        return None;
    }
    let mut sum = Wide::default();
    let mut squares = Wide::default();
    for value in values {
        let x = Wide(*value, 0.0).sub(Wide(values[0], 0.0));
        sum = sum.add(x);
        squares = squares.add(x.mul(x));
    }
    let n = values.len() as f64;
    let variance = squares.sub(sum.mul(sum).div(n)).div(n - 1.0);
    let root = variance.0.sqrt();
    if root == 0.0 {
        return Some(root);
    }
    let root = Wide(root, 0.0);
    Some(root.add(variance.sub(root.mul(root)).div(2.0 * root.0)).0)
}

// MT19937 with the one-word init_by_array integer seed and 53-bit random()
// used by Python's Random(5090). choices() selects floor(random() * len).
struct BootstrapRng {
    state: [u32; 624],
    index: usize,
}
impl BootstrapRng {
    fn new() -> Self {
        let mut s = [0u32; 624];
        s[0] = 19650218;
        for i in 1..624 {
            s[i] = 1812433253u32
                .wrapping_mul(s[i - 1] ^ (s[i - 1] >> 30))
                .wrapping_add(i as u32);
        }
        let mut i = 1;
        for (rounds, mul, seeded) in [(624, 1664525u32, true), (623, 1566083941u32, false)] {
            for _ in 0..rounds {
                let mix = (s[i - 1] ^ (s[i - 1] >> 30)).wrapping_mul(mul);
                s[i] = (s[i] ^ mix).wrapping_add(if seeded {
                    5090
                } else {
                    0u32.wrapping_sub(i as u32)
                });
                i += 1;
                if i == 624 {
                    s[0] = s[623];
                    i = 1;
                }
            }
        }
        s[0] = 1 << 31;
        Self {
            state: s,
            index: 624,
        }
    }

    fn word(&mut self) -> u32 {
        if self.index == 624 {
            for i in 0..624 {
                let joined =
                    (self.state[i] & 0x80000000) | (self.state[(i + 1) % 624] & 0x7fffffff);
                self.state[i] = self.state[(i + 397) % 624]
                    ^ (joined >> 1)
                    ^ (if joined & 1 == 1 { 0x9908b0df } else { 0 });
            }
            self.index = 0;
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c5680;
        y ^= (y << 15) & 0xefc60000;
        y ^ (y >> 18)
    }

    fn resample(&mut self, values: &[f64], buf: &mut Vec<f64>) -> f64 {
        buf.clear();
        for _ in values {
            let bits = (u64::from(self.word() >> 5) << 26) + u64::from(self.word() >> 6);
            let fraction = bits as f64 / ((1u64 << 53) as f64);
            buf.push(values[(fraction * values.len() as f64) as usize]);
        }
        buf.sort_unstable_by(f64::total_cmp);
        sorted_median(buf)
    }
}

fn interval(a: &[f64], b: &[f64]) -> Option<(f64, f64)> {
    if a.len() < 2 || b.len() < 2 {
        return None;
    }
    let mut rng = BootstrapRng::new();
    let mut changes = Vec::with_capacity(5000);
    let mut scratch = Vec::new();
    for _ in 0..5000 {
        let x = rng.resample(a, &mut scratch);
        let y = rng.resample(b, &mut scratch);
        if x == 0.0 {
            return None;
        }
        changes.push(100.0 * (y / x - 1.0));
    }
    changes.sort_unstable_by(f64::total_cmp);
    Some((changes[124], changes[4874]))
}

/// Resampling these pairwise ratios is equivalent to drawing whole pairs.
fn paired_interval(a: &[f64], b: &[f64]) -> Result<[f64; 5]> {
    ensure!(a.len() == b.len() && a.len() >= 2, "incomplete pairs");
    ensure!(
        a.iter().all(|&x| x > 0.0),
        "paired percentage needs positive baselines"
    );
    let mut values: Vec<_> = a
        .iter()
        .zip(b)
        .map(|(x, y)| 100.0 * (y / x - 1.0))
        .collect();
    let mut rng = BootstrapRng::new();
    let mut scratch = Vec::new();
    let mut draws: Vec<_> = (0..5000)
        .map(|_| rng.resample(&values, &mut scratch))
        .collect();
    draws.sort_unstable_by(f64::total_cmp);
    values.sort_unstable_by(f64::total_cmp);
    // Nearest ranks for 2.5/97.5% and Bonferroni 0.025/19 on each tail.
    Ok([
        sorted_median(&values),
        draws[124],
        draws[4874],
        draws[6],
        draws[4993],
    ])
}

fn series(field: Option<&str>) -> Result<Vec<f64>> {
    let values = field
        .context("missing series")?
        .split(',')
        .map(str::parse)
        .collect::<Result<Vec<f64>, _>>()?;
    ensure!(values.iter().all(|v| v.is_finite()), "non-finite sample");
    Ok(values)
}

pub fn statistics(compare: bool, paired: bool) -> Result<()> {
    for line in io::stdin().lock().lines() {
        let line = line?;
        let mut fields = line.split('\t');
        let key = fields.next().context("missing metric name")?;
        let a = series(fields.next())?;
        let mut sorted = a.clone();
        sorted.sort_unstable_by(f64::total_cmp);
        let m = sorted_median(&sorted);
        if paired {
            let b = series(fields.next())?;
            let [change, low, high, family_low, family_high] = paired_interval(&a, &b)?;
            let mut sorted = b;
            sorted.sort_unstable_by(f64::total_cmp);
            println!(
                "{key}\t{m}\t{}\t{change}\t{low}\t{high}\t{family_low}\t{family_high}",
                sorted_median(&sorted)
            );
        } else if compare {
            let b = series(fields.next())?;
            let mut sorted = b.clone();
            sorted.sort_unstable_by(f64::total_cmp);
            let n = sorted_median(&sorted);
            let mut delta = String::from("无基数");
            let mut band = String::from("无法估计");
            let mut decision = "无法估计";
            if m != 0.0 {
                delta = format!("{:+.2}%", 100.0 * (n / m - 1.0));
                if let Some((low, high)) = interval(&a, &b) {
                    band = format!("[{low:+.2}%, {high:+.2}%]");
                    decision = if low > 0.0 || high < 0.0 {
                        "是"
                    } else {
                        "否"
                    };
                }
            }
            println!("| {key} | {m:.3} | {n:.3} | {delta} | {band} | {decision} |");
        } else {
            let n = a.len();
            let rank = |p: usize| sorted[(n * p).div_ceil(100usize) - 1];
            let stddev = stdev(&sorted).map(|x| x.to_string()).unwrap_or_default();
            println!(
                "{key}\t{n}\t{m}\t{}\t{}\t{stddev}",
                rank(95usize),
                rank(99usize)
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paired_resampling_keeps_the_matches() {
        assert_eq!(
            paired_interval(&[1., 10., 100.], &[2., 20., 200.]).unwrap(),
            [100.; 5]
        );
        assert_eq!(paired_interval(&[2., 1.], &[2., 1.]).unwrap(), [0.; 5]);
        assert!(paired_interval(&[1.], &[1., 2.]).is_err());
    }

    #[test]
    fn bootstrap_matches_the_reference_random_stream() {
        let mut rng = BootstrapRng::new();
        assert_eq!(
            [rng.word(), rng.word(), rng.word()],
            [4021333432, 2291698999, 3287329440]
        );
        assert_eq!(
            interval(&[13., 3., 10., 9., 16.], &[18., 4., 13., 5., 1., 17., 22.]),
            Some((-69.23076923076923, 333.3333333333333))
        );
        assert_eq!(interval(&[0., 0., 1.], &[2., 3.]), None);
    }

    #[test]
    fn sample_deviation_handles_singletons_and_small_differences() {
        assert_eq!(stdev(&[4.0]), None);
        assert_eq!(stdev(&[4.0, 4.0]), Some(0.0));
        assert_eq!(stdev(&[1e12, 1e12 + 1., 1e12 + 2.]), Some(1.0));
    }
}
