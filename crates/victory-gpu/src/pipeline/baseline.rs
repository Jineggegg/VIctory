//! One invocation per process. See tools/bench/run.ps1 for sampling and statistics.
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, PCSTR};
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

static ERRORS: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn message(
    _: D3D12_MESSAGE_CATEGORY,
    severity: D3D12_MESSAGE_SEVERITY,
    id: D3D12_MESSAGE_ID,
    text: PCSTR,
    _: *mut c_void,
) {
    if severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 {
        ERRORS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the runtime supplies the message text for this callback.
        eprintln!("D3D12 error {}: {}", id.0, unsafe { text.display() });
    }
}

fn memory(scene: &str) {
    let [(local, _), _] = crate::host::video_memory().expect("query adapter memory");
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: current process and a sized, writable output buffer.
    assert_ne!(
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) },
        0
    );
    println!("BENCH {scene}_local_mb={local}");
    println!(
        "BENCH {scene}_working_set_bytes={}",
        counters.WorkingSetSize
    );
}

#[test]
#[ignore = "run explicitly in a fresh process via tools/bench/run.ps1"]
fn synthetic_gpu() {
    let mode = std::env::var("VICTORY_BENCH_MODE").expect("benchmark mode");
    let timing = mode == "sample";
    assert!(timing || mode == "validate" || mode == "check");
    if mode == "validate" {
        let mut debug: Option<ID3D12Debug1> = None;
        // SAFETY: enable the layer and GPU validation before creating the only device.
        unsafe {
            D3D12GetDebugInterface(&mut debug).expect("install Windows Graphics Tools");
            let debug = debug.unwrap();
            debug.EnableDebugLayer();
            debug.SetEnableGPUBasedValidation(true);
        }
    }
    let host = crate::host::host().expect("a hardware D3D12 device is required");
    // SAFETY: query the exact adapter of this device, with valid output structures.
    let adapter = unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let adapter: IDXGIAdapter1 = factory
            .EnumAdapterByLuid(host.device.GetAdapterLuid())
            .unwrap();
        adapter.GetDesc1().unwrap()
    };
    assert_eq!(adapter.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32, 0);
    let name = String::from_utf16_lossy(&adapter.Description);
    let name = name.trim_end_matches('\0');
    println!("BENCH adapter={name}");
    assert_eq!(
        name,
        std::env::var("VICTORY_BENCH_ADAPTER").unwrap(),
        "adapter mismatch"
    );
    let mut cookie = 0;
    let queue = (mode == "validate").then(|| {
        let queue: ID3D12InfoQueue1 = host.device.cast().expect("D3D12 debug device required");
        // SAFETY: static callback, no context, and registration retained until both submissions finish.
        unsafe {
            queue
                .RegisterMessageCallback(
                    Some(message),
                    D3D12_MESSAGE_CALLBACK_FLAG_NONE,
                    std::ptr::null_mut(),
                    &mut cookie,
                )
                .unwrap();
        }
        queue
    });
    let mut start = None;
    let mut last = None;
    let mut phases = Vec::new();
    let texels = super::placed_uav_tests::run_store(host, |phase| {
        if timing {
            let now = Instant::now();
            if let Some(previous) = last {
                phases.push((phase, now.duration_since(previous)));
            } else {
                start = Some(now);
            }
            last = Some(now);
        }
        if phase == "dispatch_readback_us" {
            memory("compute");
        }
    })
    .expect("compute scene requires shader model 6.6");
    println!("BENCH rgba={texels:?}");
    if let (Some(start), Some(end)) = (start, last) {
        println!(
            "BENCH compute_us={}",
            end.duration_since(start).as_secs_f64() * 1e6
        );
        for (phase, elapsed) in phases {
            println!("BENCH {phase}={}", elapsed.as_secs_f64() * 1e6);
        }
    }
    let start = timing.then(Instant::now);
    let depth = super::depth_bounds_test::depth_draw(host);
    if let Some(start) = start {
        println!("BENCH depth_us={}", start.elapsed().as_secs_f64() * 1e6);
    }
    memory("depth");
    println!("BENCH depth={depth:?}");
    if let Some(queue) = queue {
        // SAFETY: cookie came from this queue and both submissions are synchronous.
        unsafe { queue.UnregisterMessageCallback(cookie).unwrap() };
        let n = ERRORS.load(Ordering::Relaxed);
        println!("BENCH validation_errors={n}");
        assert_eq!(n, 0, "D3D12 validation errors");
    }
}
