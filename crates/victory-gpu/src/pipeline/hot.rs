//! Each sample is a fresh Windows process, with isolated application caches.
use super::gfx10;
mod textures;
mod waves;

use crate::copystat::{C, bench_count};
use crate::host::Host;
use crate::object::{This, guid, slot};
use crate::watch::{PAGE, tests::pool};
use std::sync::{Arc, atomic::Ordering::Relaxed};
use std::time::Instant;
use windows::Win32::Graphics::{Direct3D12::*, Dxgi::*};
use windows::core::Interface;

fn timed<T>(key: &str, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let value = f();
    println!("BENCH {key}_us={:.3}", start.elapsed().as_secs_f64() * 1e6);
    value
}

struct Guest(This);
impl Drop for Guest {
    fn drop(&mut self) {
        slot::<extern "system" fn(This) -> u32>(self.0, 0x10)(self.0);
    }
}

fn heap(count: u32, address: usize) -> Guest {
    #[repr(C)]
    struct Desc([u32; 4], [u64; 2]);
    let desc = Desc([0, count, 1, 0], [address as u64, u64::from(count) * 32]);
    let id = ID3D12DescriptorHeap::IID;
    let iid = guid(id.data1, id.data2, id.data3, id.data4);
    let mut h = std::ptr::null();
    assert_eq!(
        crate::descriptor::create_heap(h, std::ptr::from_ref(&desc).cast(), &iid, &mut h),
        0
    );
    Guest(h)
}

fn descriptors(count: usize) {
    let capacity = 32768;
    let address = pool(capacity * 32 / PAGE);
    let h = heap(capacity as u32, address);
    let host = crate::descriptor::heap(h.0).host().unwrap();
    crate::descriptor::sync_heaps();
    let constants = pool(1);
    crate::shadow::register(constants, constants as u64, PAGE as u64);
    let copy = crate::shadow::find(constants as u64, PAGE as u64).unwrap();
    let mut records = [[0u32; 8]; 256];
    for (i, r) in records.iter_mut().enumerate() {
        let a = (constants + 256 * i) as u64;
        unsafe { (a as *mut u32).write(0x12340000 + i as u32) };
        crate::descriptor::create_cbv(
            std::ptr::null(),
            &[a as u32, (a >> 32) as u32, 256, 0],
            r.as_mut_ptr() as u64,
        );
    }
    copy.update(constants, PAGE);
    let mut expected = vec![0u64; capacity];
    for i in 0..count {
        let index = i * 17 % capacity;
        let r = records[i % records.len()];
        expected[index] = (u64::from(r[2]) << 32) | u64::from(r[1]);
        assert_ne!(expected[index], 0);
        // SAFETY: distinct aligned 32-byte slots in our tracked allocation.
        unsafe { ((address + 32 * index) as *mut [u32; 8]).write(r) };
    }
    let scanned = bench_count(C::SyncSlots);
    timed(
        &format!("descriptors_{count}"),
        crate::descriptor::sync_heaps,
    );
    assert_eq!(&host.bench_slot_keys()[..capacity], expected);
    println!(
        "BENCH descriptors_{count}_scanned={}",
        bench_count(C::SyncSlots) - scanned
    );
    println!("BENCH descriptors_{count}_changed={count}");
    // D3D12 descriptors are opaque. Read every changed raw SRV on the GPU.
    let device = crate::host::host().unwrap();
    // A shader indexes the physical heap. On paged hosts, follow the same
    // GPU-visible page table as translated shaders, after the timed sync.
    let paged = crate::descriptor::view_pages();
    let page_slots = victory_shader::emit::VIEW_PAGE_SLOTS;
    // SAFETY: the view heap stays live through the readback.
    let host_slots = unsafe { host.heap.GetDesc().NumDescriptors };
    let source = format!(
        r#"
        ByteAddressBuffer slots[{host_slots}] : register(t0);
        ByteAddressBuffer pages : register(t0, space1);
        RWByteAddressBuffer dst : register(u0);
        [RootSignature("DescriptorTable(SRV(t0,numDescriptors={host_slots})), UAV(u0), SRV(t0,space=1)")]
        [numthreads(1,1,1)]
        void main(uint3 id : SV_GroupID) {{
            uint slot = (id.x * 17) % {capacity};
            #if {paged}
            slot = pages.Load(4 * (slot / {page_slots})) * {page_slots} + slot % {page_slots};
            #endif
            dst.Store(4 * id.x, slots[slot].Load(0));
        }}"#,
        paged = u32::from(paged),
    );
    let (root, pso) = crate::draw::tests::compile_cs(&source);
    let (output, data) = device.shared_buffer((4 * count) as u64).unwrap();
    let list = device.new_list().unwrap();
    unsafe {
        list.list.SetDescriptorHeaps(&[Some(host.heap.clone())]);
        list.list.SetComputeRootSignature(&root);
        list.list.SetPipelineState(&pso);
        list.list.SetComputeRootDescriptorTable(0, host.heap.GetGPUDescriptorHandleForHeapStart());
        let strides = host.strides().unwrap().GetGPUVirtualAddress();
        let offset = if paged { victory_shader::emit::VIEW_PAGE_TABLE } else { 0 };
        list.list.SetComputeRootShaderResourceView(2, strides + u64::from(offset));
        list.list
            .SetComputeRootUnorderedAccessView(1, output.GetGPUVirtualAddress());
        list.list.Dispatch(count as u32, 1, 1);
    }
    list.close();
    device.execute(&[&list]);
    for i in 0..count {
        assert_eq!(
            unsafe { data.cast::<u32>().add(i).read() },
            0x12340000 + (i % 256) as u32
        );
    }
    drop(h);
    crate::shadow::unregister(constants, constants as u64);
    drop(copy);
    crate::watch::tests::free(constants);
    crate::watch::tests::free(address);
}

fn writeback(host: &Host) {
    const PAGES: usize = 4096;
    let len = PAGES * PAGE;
    let addr = pool(PAGES);
    unsafe { std::ptr::write_bytes(addr as *mut u8, 0x35, len) };
    crate::shadow::register(addr, addr as u64, len as u64);
    let copy = crate::shadow::find(addr as u64, len as u64).unwrap();
    let (base, offset) = copy.mark_entry(addr as u64).unwrap();
    assert_eq!(offset, 0);
    assert_eq!(timed("shadow_refresh", || copy.update(addr, len)), len);
    for i in 0..PAGES {
        let page = copy.read_host((addr + i * PAGE) as u64, PAGE / 4);
        assert!(
            page.iter().all(|&word| word == 0x35353535),
            "refresh page {i}"
        );
    }
    let pattern: Vec<u8> = (0..PAGE).map(|i| (i % 251 + 1) as u8).collect();
    for i in 0..PAGES {
        copy.write_host((addr + i * PAGE) as u64, &pattern);
    }
    crate::shadow::bench_mark(base as usize, PAGES);
    let list = host.new_list().unwrap();
    // Duplicate declarations in every group, with an overlapping withheld
    // range in every 16th. This exercises Host::execute's actual dedup/overlap logic.
    const GROUP: usize = 8 * PAGE;
    for i in 0..len / GROUP {
        let p = (addr + i * GROUP) as u64;
        for _ in 0..2 {
            copy.gpu_writes(&list, p, GROUP as u64, true);
        }
        if i % 16 == 0 {
            let _writer = list.writer_uses_memory(true);
            copy.gpu_writes(&list, p, PAGE as u64, true);
        }
    }
    list.defer_missed(|| true);
    list.close();
    let before = bench_count(C::MarkedBytes);
    timed("marked_writeback_frame", || host.execute(&[&list]));
    let bytes = bench_count(C::MarkedBytes) - before;
    assert_eq!(bytes as usize, len - len / 16);
    let original = vec![0x35; PAGE];
    // SAFETY: the entire allocation is live and the submission has completed.
    let title = unsafe { std::slice::from_raw_parts(addr as *const u8, len) };
    for (i, page) in title.chunks_exact(PAGE).enumerate() {
        assert!(
            page == if i / 8 % 16 == 0 { &original } else { &pattern },
            "page {i}"
        );
    }
    println!("BENCH shadow_refresh_bytes={len}");
    println!("BENCH writeback_bytes={bytes}");
    println!("BENCH writeback_withheld_bytes={}", len as u64 - bytes);
    drop(list);
    crate::shadow::unregister(addr, addr as u64);
    drop(copy);
    crate::watch::tests::free(addr);
}

fn submissions(host: &Host) {
    let queue_before = bench_count(C::Submissions);
    let mut readbacks = Vec::new();
    let mut lists = Vec::new();
    for i in 0u64..32 {
        let list = Arc::new(host.new_list().unwrap());
        let (src, input) = list.upload(host, 8).unwrap();
        let (dst, output) = list.readback(host, 8).unwrap();
        // SAFETY: two live eight-byte buffers held by this closed list through validation.
        unsafe {
            input.cast::<u64>().write(i + 1);
            list.list.CopyBufferRegion(&dst, 0, &src, 0, 8);
        }
        list.close();
        lists.push(list);
        readbacks.push(output);
    }
    let fence_before = crate::host::BENCH_FENCE_NS.load(Relaxed);
    let before = crate::host::bench_submission_us();
    timed("submit_32", || {
        for list in &lists {
            crate::queue::bench_submit(list.clone());
        }
    });
    let after = crate::host::bench_submission_us();
    assert_eq!(bench_count(C::Submissions) - queue_before, 32);
    for (i, data) in readbacks.iter().enumerate() {
        // SAFETY: all fences completed and lists still own their readback buffers.
        assert_eq!(unsafe { data.cast::<u64>().read() }, i as u64 + 1);
    }
    println!("BENCH submit_count=32");
    println!(
        "BENCH submit_fence_wait_us={:.3}",
        (crate::host::BENCH_FENCE_NS.load(Relaxed) - fence_before) as f64 / 1000.0
    );
    println!("BENCH submit_prepare_us={}", after[0] - before[0]);
    println!("BENCH submit_queue_and_fence_us={}", after[1] - before[1]);
}

fn pipelines(host: &Host) {
    for (name, counter) in [
        ("pso_cold", &super::library::MADE),
        ("pso_hot", &super::library::LOADED),
    ] {
        let before = counter.load(Relaxed);
        let mut start = None;
        let pixels = super::placed_uav_tests::run_store(host, |phase| match phase {
            "shader_us" => start = Some(Instant::now()),
            "pso_ready_us" => println!(
                "BENCH {name}_us={:.3}",
                start.unwrap().elapsed().as_secs_f64() * 1e6
            ),
            _ => (),
        })
        .unwrap();
        assert_eq!(pixels[0], [1., 0.5, -0.25, 2.]);
        assert_eq!(counter.load(Relaxed) - before, 1, "{name}");
    }
}

fn shaders(dir: &std::path::Path) {
    let mut inputs = vec![gfx10::vertex_container(), gfx10::pixel_container()];
    inputs.extend((1..=16).map(|i| gfx10::compute_container(128 * i)));
    inputs.extend([
        gfx10::wave_container(None),
        gfx10::wave_container(Some(32 << 32)),
    ]);
    let translate = |expect_hit| {
        inputs
            .iter()
            .map(|c| {
                let (result, hit) = super::translate(c);
                assert_eq!(hit, expect_hit);
                let bytes = result.unwrap().host.dxil;
                assert!(bytes.starts_with(b"DXBC"));
                bytes
            })
            .collect::<Vec<_>>()
    };
    let fresh = timed("shader_batch_translate", || translate(false));
    crate::shader_cache::use_shader_cache(dir.to_owned()).unwrap();
    let cold = timed("shader_batch_cold", || translate(false));
    let hot = timed("shader_batch_hot", || translate(true));
    assert_eq!(fresh, cold);
    assert_eq!(cold, hot);
    let builds: Vec<_> = inputs[2..18]
        .iter()
        .map(|b| super::Prebuild::compute(b))
        .collect();
    let before = super::pipeline_counts();
    timed("prebuild_16_cold", || {
        for build in &builds {
            build.make().unwrap();
        }
    });
    let after = super::pipeline_counts();
    assert_eq!(after.0 - before.0, 16);
    timed("prebuild_16_hot", || {
        for build in &builds {
            build.make().unwrap();
        }
    });
    assert_eq!(super::pipeline_counts().1 - after.1, 16);
    assert!(builds.iter().all(|b| super::prebuilt(&b.key())));
    println!("BENCH prebuild_count=16");
    let mut digest = blake3::Hasher::new();
    for b in &cold {
        digest.update(b);
    }
    println!("BENCH shader_digest={}", digest.finalize().to_hex());
    println!("BENCH shader_count={}", inputs.len());
    println!(
        "BENCH shader_dxil_bytes={}",
        cold.iter().map(Vec::len).sum::<usize>()
    );
}

#[test]
#[ignore = "run alone in a fresh process via tools/bench/hot.py"]
fn hot_paths() {
    let cache = tempfile::tempdir().unwrap();
    super::library::use_pipeline_cache(cache.path().to_owned());
    let host = crate::host::host().expect("hardware D3D12 device");
    // SAFETY: query the adapter that owns this device.
    let desc = unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let adapter: IDXGIAdapter1 = factory
            .EnumAdapterByLuid(host.device.GetAdapterLuid())
            .unwrap();
        adapter.GetDesc1().unwrap()
    };
    assert_eq!(desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32, 0);
    println!(
        "BENCH adapter={}",
        String::from_utf16_lossy(&desc.Description).trim_end_matches('\0')
    );
    let sampler = crate::host::video_memory().unwrap();
    let monitor = std::thread::spawn(move || {
        let mut high = sampler[0].0;
        while !DONE.load(Relaxed) {
            high = high.max(crate::host::video_memory().unwrap()[0].0);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        high
    });
    pipelines(host);
    shaders(cache.path());
    descriptors(1300);
    descriptors(14400);
    submissions(host);
    textures::run(host);
    waves::run(host);
    writeback(host);
    DONE.store(true, Relaxed);
    println!("BENCH sampled_local_peak_mb={}", monitor.join().unwrap());
    use windows_sys::Win32::System::{ProcessStatus::*, Threading::GetCurrentProcess};
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: current process with a writable, sized buffer.
    assert_ne!(
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) },
        0
    );
    println!("BENCH peak_working_set_bytes={}", c.PeakWorkingSetSize);
    assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    println!("BENCH verified=1");
}
static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
