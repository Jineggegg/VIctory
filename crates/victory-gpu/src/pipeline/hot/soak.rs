//! One persistent process and shadow. Every round records fresh GPU work, refreshes
//! title bytes, dispatches, waits for the fence, merges the result and checks it.
use super::*;
use std::time::Duration;
use windows_sys::Win32::System::{ProcessStatus::*, Threading::*};

fn process_usage() -> (usize, u32) {
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    let mut handles = 0;
    // SAFETY: Both out-parameters are writable and cb matches the size of counters for the
    // current process.
    unsafe {
        let process = GetCurrentProcess();
        assert_ne!(
            K32GetProcessMemoryInfo(process, &mut counters, counters.cb),
            0
        );
        assert_ne!(GetProcessHandleCount(process, &mut handles), 0);
    }
    (counters.WorkingSetSize, handles)
}

const PROGRAM: &str = r#"
    RWByteAddressBuffer dst : register(u0);
    [RootSignature("UAV(u0)")] [numthreads(64,1,1)]
    void main(uint3 id : SV_DispatchThreadID) {
        uint at = 4*id.x;
        dst.Store(at, dst.Load(at) ^ (0xabc123 + id.x));
    }
"#;

#[test]
#[ignore = "persistent mixed workload, default 30 minutes"]
fn mixed() {
    let seconds = std::env::var("VICTORY_BENCH_SECONDS")
        .map(|s| s.parse::<u64>().unwrap())
        .unwrap_or(1800);
    assert!(seconds > 0);
    let host = crate::host::host().expect("D3D12");
    let (root, pso) = crate::draw::tests::compile_cs(PROGRAM);
    let bytes = 16 * PAGE;
    let addr = pool(16);
    crate::shadow::register(addr, addr as u64, bytes as u64);
    let copy = crate::shadow::find(addr as u64, bytes as u64).unwrap();
    let started = Instant::now();
    let duration = Duration::from_secs(seconds);
    let mut round = 0u32;
    let mut interval = Duration::from_secs(60).min(duration);
    let mut latencies = Vec::new();
    let mut failures = 0u64;
    let mut window = 0;
    let mut history = Vec::new();
    while started.elapsed() < duration {
        let begin = Instant::now();
        // The CPU owns title memory outside execute. The shader operates on its host copy.
        // SAFETY: The writable pool spans the full byte range, and no submission is in flight.
        unsafe { std::ptr::write_bytes(addr as *mut u8, round as u8, bytes) };
        let value = u32::from_ne_bytes([round as u8; 4]);
        let list = Arc::new(host.new_list().unwrap());
        copy.refresh(&list, addr as u64, bytes as u64);
        copy.gpu_writes(&list, addr as u64, bytes as u64, false);
        // SAFETY: The matching root and PSO bind the live shadow UAV spanning the dispatch.
        unsafe {
            list.list.SetComputeRootSignature(&root);
            list.list.SetPipelineState(&pso);
            list.list
                .SetComputeRootUnorderedAccessView(0, copy.gpu(addr as u64));
            list.list.Dispatch((bytes / (4 * 64)) as u32, 1, 1);
        }
        list.close();
        crate::queue::bench_submit(list);
        // SAFETY: bench_submit completed copy-back to this still-allocated, word-aligned pool.
        let result = unsafe { std::slice::from_raw_parts(addr as *const u32, bytes / 4) };
        failures += u64::from(
            !result
                .iter()
                .enumerate()
                .all(|(i, v)| *v == (value ^ (0xabc123 + i as u32))),
        );
        latencies.push(begin.elapsed().as_nanos() as u64);
        round += 1;
        if started.elapsed() >= interval || failures != 0 {
            window += 1;
            latencies.sort_unstable();
            let percentile =
                |p: usize| latencies[(latencies.len() * p).div_ceil(100) - 1] as f64 / 1000.;
            let (ws, handles) = process_usage();
            let [(local, budget), (system, _)] = crate::host::video_memory().unwrap();
            // SAFETY: The device is live for this status query.
            let removed = unsafe { host.device.GetDeviceRemovedReason() }
                .err()
                .map_or(0, |e| e.code().0);
            println!(
                "BENCH soak_{window}_elapsed_s={:.3}",
                started.elapsed().as_secs_f64()
            );
            println!("BENCH soak_{window}_rounds={}", latencies.len());
            for p in [50, 95, 99] {
                println!("BENCH soak_{window}_p{p}_us={:.3}", percentile(p));
            }
            println!("BENCH soak_{window}_ws_bytes={ws}");
            println!("BENCH soak_{window}_handles={handles}");
            println!("BENCH soak_{window}_local_mb={local}");
            println!("BENCH soak_{window}_budget_mb={budget}");
            println!("BENCH soak_{window}_nonlocal_mb={system}");
            println!("BENCH soak_{window}_failures={failures}");
            println!("BENCH soak_{window}_removed_hresult={removed}");
            if interval.as_secs().is_multiple_of(60) {
                history.push([ws as f64, local, system, f64::from(handles), percentile(50)]);
            }
            assert_eq!(failures, 0);
            assert_eq!(removed, 0);
            latencies.clear();
            interval = (interval + Duration::from_secs(60)).min(duration);
        }
    }
    drop(copy);
    crate::shadow::unregister(addr, addr as u64);
    crate::watch::tests::free(addr);
    // Exclude the first two minutes. Use two nonoverlapping five-minute
    // medians, so one OS trim or latency spike cannot decide the verdict.
    if history.len() >= 12 {
        let median = |rows: &[[f64; 5]], field: usize| {
            let mut v = rows.iter().map(|r| r[field]).collect::<Vec<_>>();
            v.sort_unstable_by(f64::total_cmp);
            v[2]
        };
        let mut growth = false;
        for (field, (name, tolerance)) in [
            ("ws_bytes", 32. * 1048576.),
            ("local_mb", 32.),
            ("nonlocal_mb", 32.),
            ("handles", 16.),
            ("p50_pct", 10.),
        ]
        .into_iter()
        .enumerate()
        {
            let first = median(&history[2..7], field);
            let last = median(&history[history.len() - 5..], field);
            let mut delta = last - first;
            if name == "p50_pct" {
                delta = 100. * delta / first;
                println!(
                    "BENCH soak_latency_drift={}",
                    u8::from(delta.abs() > tolerance)
                );
            } else {
                growth |= delta > tolerance;
            }
            println!("BENCH soak_delta_{name}={delta:.3}");
        }
        println!("BENCH soak_resource_growth={}", u8::from(growth));
    }
    println!("BENCH soak_total_rounds={round}");
    println!("BENCH verified=1");
}

#[test]
#[ignore = "warm shader clocks before collecting samples"]
fn warm() {
    let host = crate::host::host().unwrap();
    let (root, pso) = crate::draw::tests::compile_cs(
        r#"
        RWByteAddressBuffer dst : register(u0);
        [RootSignature("UAV(u0)")] [numthreads(64,1,1)]
        void main(uint3 id : SV_DispatchThreadID) {
            uint v = id.x;
            [loop] for(uint i=0;i<4096;i++) v = (v ^ (v>>13))*1664525u+1013904223u;
            dst.Store(4*id.x,v);
        }
    "#,
    );
    let words = 1 << 20;
    let (out, data) = host.shared_buffer(words * 4).unwrap();
    let list = host.new_list().unwrap();
    // SAFETY: The matching root and PSO use the live output UAV sized for all dispatched words.
    unsafe {
        list.list.SetComputeRootSignature(&root);
        list.list.SetPipelineState(&pso);
        list.list
            .SetComputeRootUnorderedAccessView(0, out.GetGPUVirtualAddress());
        // Several back-to-back dispatches per fence keep the GPU occupied
        // through native CPU scheduling and the submission bookkeeping.
        for _ in 0..8 {
            list.list.Dispatch(words as u32 / 64, 1, 1);
        }
    }
    list.close();
    let start = Instant::now();
    let seconds = std::env::var("VICTORY_BENCH_WARM_SECONDS")
        .map(|s| s.parse::<u64>().unwrap())
        .unwrap_or(10);
    assert!(seconds > 0);
    while start.elapsed() < Duration::from_secs(seconds) {
        host.execute(&[&list]);
    }
    for i in [0, 1, words / 2, words - 1] {
        let mut v = i as u32;
        for _ in 0..4096 {
            v = (v ^ (v >> 13))
                .wrapping_mul(1664525)
                .wrapping_add(1013904223);
        }
        // SAFETY: The submissions have completed and i is within the mapped output of words u32s.
        assert_eq!(unsafe { data.cast::<u32>().add(i as usize).read() }, v);
    }
    println!("BENCH verified=1");
}
