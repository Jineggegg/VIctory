//! Driver reuse across two processes with fresh VIctory caches.
//! The first process uses a fresh salt. The second sees its exact same DXIL.
use crate::pipeline::{Prebuild, gfx10, pipeline_counts};
use std::collections::{HashSet, VecDeque};
use std::hash::{BuildHasher, RandomState};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex};
use windows::Win32::Graphics::Dxgi::*;
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
    let heavy_every = number("VICTORY_BENCH_DRIVER_HEAVY_EVERY", 0);
    let rounds = number("VICTORY_BENCH_DRIVER_HEAVY_ROUNDS", 1);
    let budget = number("VICTORY_BENCH_DRIVER_BUDGET_KIB", 0) * 1024;
    assert!((1..=4096).contains(&rounds));
    assert!((1..=4096).contains(&count) && (1..=64).contains(&workers));
    // The normal prebuild path can serialize a growing application library
    // every 20 seconds. Keep that I/O out of the driver-only comparison.
    // An optional fresh directory reproduces the original large scenario.
    let disk = number("VICTORY_BENCH_DRIVER_APP_DISK", 0);
    assert!(disk <= 1);
    let _dir = (disk == 1).then(|| {
        let dir = tempfile::tempdir().unwrap();
        crate::pipeline::library::use_pipeline_cache(dir.path().to_owned());
        dir
    });
    let host = crate::host::host().expect("hardware D3D12");
    // SAFETY: enumerate the live device's exact adapter before the timer.
    let adapter = unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let adapter: IDXGIAdapter1 = factory.EnumAdapterByLuid(host.device.GetAdapterLuid()).unwrap();
        adapter.GetDesc1().unwrap()
    };
    assert_eq!(adapter.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32, 0);
    let mut hashes = HashSet::new();
    let mut digest = blake3::Hasher::new();
    let mut jobs = VecDeque::with_capacity(count);
    let salts: Vec<_> = (0..rounds as u32)
        .map(|n| seed.wrapping_add(n.wrapping_mul(0x9e37_79b9)))
        .collect();
    let mut bytes = 0;
    let mut largest = 0;
    for i in 1..=count as u32 {
        let width = if i <= 16 { 128 * i } else { 2048 + i };
        let heavy = heavy_every != 0 && (i as usize).is_multiple_of(heavy_every);
        let container = gfx10::salted_compute_container(width, if heavy { &salts } else { &salts[..1] });
        let job = Prebuild::compute(&container);
        let dxil = &job.shaders[0].translated().unwrap().host.dxil;
        if i == 1 {
            let alternate = Prebuild::compute(&gfx10::salted_compute_container(width, &[!seed]));
            assert_ne!(*dxil, alternate.shaders[0].translated().unwrap().host.dxil);
            let plain = Prebuild::compute(&gfx10::compute_container(width));
            assert_ne!(*dxil, plain.shaders[0].translated().unwrap().host.dxil);
        }
        assert!(hashes.insert(blake3::hash(dxil)));
        bytes += dxil.len();
        largest = largest.max(dxil.len());
        digest.update(dxil);
        jobs.push_back((dxil.len(), job));
    }
    let queue = Mutex::new((jobs, 0usize));
    let finished = Condvar::new();
    assert_eq!(pipeline_counts(), (0, 0));
    let first = AtomicU64::new(u64::MAX);
    let start = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..workers.min(count) {
            scope.spawn(|| loop {
                let mut state = queue.lock().unwrap();
                let (cost, job) = loop {
                    if state.0.is_empty() { return; }
                    // A shader larger than the budget can proceed on its own.
                    // Small ready jobs can fill the remaining byte allowance.
                    let index = if budget == 0 || state.1 == 0 { Some(0) } else {
                        state.0.iter().position(|(cost, _)| state.1 + cost <= budget)
                    };
                    if let Some(i) = index {
                        let next = state.0.remove(i).unwrap();
                        if budget != 0 { state.1 += next.0; }
                        break next;
                    }
                    state = finished.wait(state).unwrap();
                };
                drop(state);
                let result = job.make();
                first.fetch_min(start.elapsed().as_micros() as u64, Relaxed);
                if budget != 0 {
                    let mut state = queue.lock().unwrap();
                    state.1 -= cost;
                    drop(state);
                    finished.notify_all();
                }
                result.unwrap();
            });
        }
    });
    let elapsed = start.elapsed().as_micros();
    let first = first.load(Relaxed);
    assert!(u128::from(first) <= elapsed);
    assert_eq!(pipeline_counts(), (count as u64, 0));
    let mut memory = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: the writable structure has the size requested for this process.
    assert_ne!(unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut memory, memory.cb) }, 0);
    let pixels = crate::pipeline::placed_uav_tests::run_store_with_salt(host, Some(seed), |_| {}).unwrap();
    assert_eq!(pixels, [[1., 0.5, -0.25, 2.], [0.; 4]]);
    let name = String::from_utf16_lossy(&adapter.Description);
    println!("DRIVER_ADAPTER {}", name.trim_end_matches('\0'));
    println!("DRIVER_RESULT {} {bytes} {elapsed} {} {count} {workers} {largest} {first}",
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
        let (_, adapter) = stdout.split_once("DRIVER_ADAPTER ").expect(&stdout);
        let (_, fields) = stdout.split_once("DRIVER_RESULT ").expect(&stdout);
        let values = fields.lines().next().unwrap().split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        (adapter.lines().next().unwrap().to_owned(), values)
    };
    let (adapter, fresh) = sample();
    let (replay_adapter, replay) = sample();
    assert_eq!(adapter, replay_adapter);
    assert_eq!(fresh.len(), 8);
    assert_eq!(replay.len(), 8);
    assert_eq!(fresh[..2], replay[..2], "both processes must compile identical bytes");
    assert_eq!(fresh[4..7], replay[4..7], "workload and worker count");
    println!("driver seed={seed} digest={} DXIL bytes={} workers={}", fresh[0], fresh[1], fresh[5]);
    println!("BENCH adapter={adapter}");
    println!("BENCH driver_seed={seed}");
    println!("BENCH driver_fresh_first_us={}", fresh[7]);
    println!("BENCH driver_replay_first_us={}", replay[7]);
    println!("BENCH driver_largest_dxil_bytes={}", fresh[6]);
    println!("BENCH driver_total_dxil_bytes={}", fresh[1]);
    println!("BENCH driver_fresh_us={}", fresh[2]);
    println!("BENCH driver_replay_us={}", replay[2]);
    println!("BENCH driver_fresh_peak_bytes={}", fresh[3]);
    println!("BENCH driver_replay_peak_bytes={}", replay[3]);
    println!("BENCH driver_psos={}", fresh[4]);
    println!("BENCH verified=1");
}
