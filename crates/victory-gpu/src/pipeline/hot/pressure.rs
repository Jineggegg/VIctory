use super::*;
use std::ptr::null;
use std::time::Duration;

fn headroom_mib() -> f64 {
    let (used, budget) = crate::host::video_memory().unwrap()[0];
    (budget - used) * 1e6 / 1048576.
}

// DXGI can resize this process's budget by several GiB at once. Use board
// free memory for feedback and log the separate per-process WDDM headroom.
fn board_free_mib(device: &Host) -> f64 {
    // SAFETY: The factory and device are live and the device LUID selects the adapter being
    // queried.
    let name = unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let adapter: IDXGIAdapter1 = factory
            .EnumAdapterByLuid(device.device.GetAdapterLuid())
            .unwrap();
        String::from_utf16_lossy(&adapter.GetDesc1().unwrap().Description)
            .trim_end_matches('\0')
            .to_owned()
    };
    let output = std::process::Command::new("nvidia-smi.exe")
        .args([
            "--query-gpu=name,memory.free",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .expect("pressure test requires NVIDIA board telemetry");
    assert!(output.status.success());
    let csv = String::from_utf8(output.stdout).unwrap();
    let rows = csv
        .lines()
        .filter_map(|line| line.rsplit_once(','))
        .filter(|(board, _)| board.trim() == name)
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1, "identify exactly one measured board");
    rows[0].1.trim().parse().unwrap()
}

#[test]
#[ignore = "allocates real local memory close to a requested DXGI headroom"]
fn video_memory() {
    let host = crate::host::host().unwrap();
    let target: u64 = std::env::var("VICTORY_BENCH_HEADROOM_MIB")
        .unwrap_or_else(|_| "8192".into())
        .parse()
        .unwrap();
    assert!(matches!(target, 512 | 8192 | 12288));
    let available = board_free_mib(host);
    println!("BENCH initial_dxgi_headroom_mib={}", headroom_mib());
    assert!(
        available > target as f64,
        "insufficient initial budget for this profile"
    );
    let initial = crate::host::video_memory().unwrap();
    let mut filler = Vec::new();
    const CHUNK: u64 = 128 << 20;
    // WDDM can change Budget and CurrentUsage as allocations accumulate.
    // Approach the target from either side and require five settled readings.
    let limit = (headroom_mib().max(available) / 128.).ceil() as usize;
    let mut refused = false;
    timed("pressure_fill", || {
        let mut settled = 0;
        for _ in 0..100 {
            let delta = board_free_mib(host) - target as f64;
            if delta.abs() <= 128. {
                settled += 1;
                if settled == 3 {
                    break;
                }
            } else {
                settled = 0;
                let chunks = ((delta.abs() / 128.).round() as usize).clamp(1, 16);
                if delta < 0. {
                    filler.truncate(filler.len().saturating_sub(chunks));
                } else {
                    for _ in 0..chunks.min(limit - filler.len()) {
                        let Some(resource) = host.default_buffer(CHUNK) else {
                            refused = true;
                            break;
                        };
                        filler.push(resource);
                    }
                    if refused || filler.len() == limit {
                        break;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    });
    let reached = board_free_mib(host);
    println!("BENCH workload_dxgi_headroom_mib={}", headroom_mib());
    println!("BENCH initial_headroom_mib={available:.3}");
    println!("BENCH target_headroom_mib={target}");
    println!("BENCH measured_headroom_mib={reached:.3}");
    println!("BENCH filler_bytes={}", filler.len() as u64 * CHUNK);
    println!("BENCH filler_refused={}", u8::from(refused));
    assert!(
        (reached - target as f64).abs() < 384.
            || (target == 512 && reached < 2048. && {
                let (used, budget) = crate::host::video_memory().unwrap()[0];
                used > 0.90 * budget
            }),
        "failed to reach profile"
    );
    let before = crate::resource::EVICTIONS
        .each_ref()
        .map(|x| x.load(Relaxed));
    textures::batch(host, 8, true);
    let after = crate::resource::EVICTIONS
        .each_ref()
        .map(|x| x.load(Relaxed));
    let current = crate::host::video_memory().unwrap();
    println!("BENCH final_board_free_mib={}", board_free_mib(host));
    println!("BENCH texture_evictions={}", after[0] - before[0]);
    println!("BENCH texture_remakes={}", after[2] - before[2]);
    for (i, name) in ["local", "nonlocal"].iter().enumerate() {
        println!("BENCH initial_{name}_mb={}", initial[i].0);
        println!("BENCH final_{name}_mb={}", current[i].0);
        println!("BENCH final_{name}_budget_mb={}", current[i].1);
    }
    // SAFETY: The device is live for this status query.
    assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    drop(filler);
    println!("BENCH verified=1");
}

#[test]
#[ignore = "cold placed texture before eviction, at miss, and after restoration"]
fn heap_capacity() {
    let mode = std::env::var("VICTORY_BENCH_HEAP").unwrap();
    assert!(matches!(mode.as_str(), "flat" | "small"));
    let device = crate::host::host().unwrap();
    let page = victory_shader::emit::VIEW_PAGE_SLOTS as usize;
    let count = 144 * page;
    let table = pool((32 * count).div_ceil(PAGE));
    let owner = heap(count as u32, table);
    let h = crate::descriptor::heap(owner.0).host().unwrap();
    let tex = pool(1);
    // SAFETY: pool allocated one writable page at tex.
    unsafe { std::ptr::write_bytes(tex as *mut u8, 0x7b, PAGE) };
    let words: [u32; 14] = [3, 0, 0, 0, 4, 0, 1, 1 | 1 << 16, 28, 1, 0, 0x100, 0, 0];
    let mut view = [0u32; 16];
    view[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
    (view[10], view[11]) = (tex as u32, ((tex as u64) >> 32) as u32);
    let cold = 5;
    let handle = (table + 32 * cold) as u64;
    crate::descriptor::create_placed_srv(null(), &words, &view, handle);
    h.mirror(cold as u32, handle, true).unwrap();
    let placed = crate::resource::texture_at(tex as u64, &words).unwrap();
    let texture = crate::resource::host_resource(placed).unwrap();
    let upload = device.new_list().unwrap();
    crate::texture::upload(&upload, placed, &texture);
    crate::list::ensure_state(
        &upload,
        placed,
        D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0 as u32,
    );
    upload.close();
    device.execute(&[&upload]);
    let (root, pso) = crate::draw::tests::compile_cs(
        r#"
        Texture2D<float4> src : register(t0); RWByteAddressBuffer dst : register(u0);
        [RootSignature("DescriptorTable(SRV(t0)), UAV(u0)")] [numthreads(1,1,1)]
        void main() { dst.Store(0,(uint)round(src.Load(int3(0,0,0)).x*255)); }
    "#,
    );
    let sample = || {
        let list = device.new_list().unwrap();
        let (out, data) = device.shared_buffer(4).unwrap();
        // SAFETY: The mapped output holds one aligned word. The open list binds the matching
        // heap, root and PSO.
        unsafe {
            data.cast::<u32>().write(u32::MAX);
            list.list.SetComputeRootSignature(&root);
            list.list.SetPipelineState(&pso);
            list.list.SetDescriptorHeaps(&[Some(h.heap.clone())]);
            list.list
                .SetComputeRootDescriptorTable(0, h.gpu(cold as u32));
            list.list
                .SetComputeRootUnorderedAccessView(1, out.GetGPUVirtualAddress());
            list.list.Dispatch(1, 1, 1);
        }
        list.close();
        device.execute(&[&list]);
        // SAFETY: execute completed the write to the mapped word while out is still live.
        unsafe { data.cast::<u32>().read() }
    };
    assert_eq!(sample(), 0x7b);
    let paged = mode == "small";
    // Writing a slot in every virtual page exhausts the small physical heap.
    timed("heap_fill", || {
        for i in 1..144 {
            crate::descriptor::create_placed_srv(
                null(),
                &words,
                &view,
                (table + 32 * i * page) as u64,
            );
        }
    });
    let start = Instant::now();
    timed("heap_age_and_evict", || {
        for i in 0..400 {
            crate::descriptor::turn_pages(&[], start + Duration::from_secs(i));
        }
    });
    let miss = sample();
    assert_eq!(miss, if paged { 0 } else { 0x7b });
    if paged {
        assert_eq!(h.page_table_entry(0), 0);
    }
    // Replay the same read marks that translated shaders return after the miss.
    timed("heap_restore", || {
        crate::descriptor::turn_pages(&[0], start + Duration::from_secs(400));
        crate::descriptor::make_read(&[cold as u32]);
    });
    let restored = sample();
    assert_eq!(restored, 0x7b);
    // SAFETY: The descriptor heap owner stays live through this query.
    println!("BENCH physical_slots={}", unsafe {
        h.heap.GetDesc().NumDescriptors
    });
    println!("BENCH paged={}", u8::from(paged));
    println!("BENCH first_reread={miss}");
    println!("BENCH restored_reread={restored}");
    // texture_at keeps the guest placement and its address until process exit.
    drop(owner);
    crate::watch::tests::free(table);
    println!("BENCH verified=1");
}
