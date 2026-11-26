//! Driver reuse across two processes with fresh VIctory caches.
//! The first process uses a fresh salt. The second sees its exact same DXIL.
use crate::pipeline::{Prebuild, gfx10, pipeline_counts};
use std::collections::{HashSet, VecDeque};
use std::hash::{BuildHasher, RandomState};
use std::process::Command;
use std::sync::Mutex;
use std::time::Instant;
use windows_sys::Win32::System::{ProcessStatus::*, Threading::GetCurrentProcess};

const CHILD: &str = "pipeline::hot::driver_cache::compile_one_process";

fn number(name: &str, default: usize) -> usize {
    std::env::var(name).map_or(default, |s| s.parse().unwrap())
}

#[test]
#[ignore = "child of fresh_and_replayed, requires a hardware D3D12 device"]
fn compile_one_process() {
    let seed: u32 = std::env::var("VICTORY_DRIVER_CHILD_SEED").unwrap().parse().unwrap();
    let count = number("VICTORY_BENCH_DRIVER_COUNT", 1996);
    let workers = number("VICTORY_BENCH_DRIVER_WORKERS", 24);
    assert!((1..=4096).contains(&count) && (1..=64).contains(&workers));
    let dir = tempfile::tempdir().unwrap();
    crate::pipeline::library::use_pipeline_cache(dir.path().to_owned());
    let host = crate::host::host().expect("hardware D3D12");
    let mut hashes = HashSet::new();
    let mut digest = blake3::Hasher::new();
    let mut jobs = VecDeque::with_capacity(count);
    let mut bytes = 0;
    for i in 1..=count as u32 {
        let width = if i <= 16 { 128 * i } else { 2048 + i };
        let container = gfx10::salted_compute_container(width, seed);
        let job = Prebuild::compute(&container);
        let dxil = &job.shaders[0].translated().unwrap().host.dxil;
        if i == 1 {
            let alternate = Prebuild::compute(&gfx10::salted_compute_container(width, !seed));
            assert_ne!(*dxil, alternate.shaders[0].translated().unwrap().host.dxil);
            let plain = Prebuild::compute(&gfx10::compute_container(width));
            assert_ne!(*dxil, plain.shaders[0].translated().unwrap().host.dxil);
        }
        assert!(hashes.insert(blake3::hash(dxil)));
        bytes += dxil.len();
        digest.update(dxil);
        jobs.push_back(job);
    }
    let jobs = Mutex::new(jobs);
    assert_eq!(pipeline_counts(), (0, 0));
    let start = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..workers.min(count) {
            scope.spawn(|| loop {
                let Some(job) = jobs.lock().unwrap().pop_front() else { break };
                job.make().unwrap();
            });
        }
    });
    let elapsed = start.elapsed().as_micros();
    assert_eq!(pipeline_counts(), (count as u64, 0));
    let mut memory = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: the writable structure has the size requested for this process.
    assert_ne!(unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut memory, memory.cb) }, 0);
    let pixels = crate::pipeline::placed_uav_tests::run_store_with_salt(host, Some(seed), |_| {}).unwrap();
    assert_eq!(pixels, [[1., 0.5, -0.25, 2.], [0.; 4]]);
    println!("DRIVER_RESULT {} {bytes} {elapsed} {} {count} {workers}",
        digest.finalize().to_hex(), memory.PeakWorkingSetSize);
}

#[test]
#[ignore = "fresh executable DXIL then an exact replay in a separate Windows process"]
fn fresh_and_replayed() {
    let seed = std::env::var("VICTORY_BENCH_DRIVER_SEED").map_or_else(
        |_| RandomState::new().hash_one(std::process::id()) as u32,
        |s| s.parse().unwrap(),
    );
    let sample = || {
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD, "--ignored", "--nocapture", "--test-threads=1"])
            .env("VICTORY_DRIVER_CHILD_SEED", seed.to_string())
            .output().unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
        let (_, fields) = stdout.split_once("DRIVER_RESULT ").expect(&stdout);
        fields.lines().next().unwrap().split_whitespace().map(str::to_owned).collect::<Vec<_>>()
    };
    let fresh = sample();
    let replay = sample();
    assert_eq!(fresh.len(), 6);
    assert_eq!(replay.len(), 6);
    assert_eq!(fresh[..2], replay[..2], "both processes must compile identical bytes");
    assert_eq!(fresh[4..], replay[4..], "workload and worker count");
    println!("driver seed={seed} digest={} DXIL bytes={} workers={}", fresh[0], fresh[1], fresh[5]);
    println!("BENCH driver_seed={seed}");
    println!("BENCH driver_fresh_us={}", fresh[2]);
    println!("BENCH driver_replay_us={}", replay[2]);
    println!("BENCH driver_fresh_peak_bytes={}", fresh[3]);
    println!("BENCH driver_replay_peak_bytes={}", replay[3]);
    println!("BENCH driver_psos={}", fresh[4]);
    println!("BENCH verified=1");
}
