use super::*;
use std::ptr::null;
use std::sync::Barrier;

fn cbv(table: usize, constants: usize, index: usize, value: usize) {
    let a = (constants + value * 256) as u64;
    crate::descriptor::create_cbv(
        null(),
        &[a as u32, (a >> 32) as u32, 256, 0],
        (table + 32 * index) as u64,
    );
}

#[test]
#[ignore = "one thread count per process"]
fn recording() {
    const JOBS: usize = 256;
    let threads = threads();
    let host = crate::host::host().unwrap();
    let table = pool(1);
    let h = heap(JOBS as u32, table);
    let heap = crate::descriptor::heap(h.0).host().unwrap();
    let constants = pool(1);
    crate::shadow::register(constants, constants as u64, PAGE as u64);
    let copy = crate::shadow::find(constants as u64, PAGE as u64).unwrap();
    for i in 0..JOBS {
        // SAFETY: Each aligned word is inside the writable page returned by pool.
        unsafe { ((constants + 256 * i) as *mut u32).write(0xcafe0000 + i as u32) };
        cbv(table, constants, i, (i + 1) % JOBS);
    }
    copy.update(constants, PAGE);
    let (root, pso) = crate::draw::tests::compile_cs(
        r#"
        ByteAddressBuffer src : register(t0); RWByteAddressBuffer dst : register(u0);
        [RootSignature("DescriptorTable(SRV(t0)), UAV(u0)")] [numthreads(1,1,1)]
        void main() { dst.Store(0,src.Load(0)); }
    "#,
    );
    let sync = Barrier::new(threads);
    let before = [
        bench_count(C::MirrorWaitUs),
        bench_count(C::GuardWaitUs),
        bench_count(C::Submissions),
    ];
    let start = Instant::now();
    std::thread::scope(|s| {
        for lane in 0..threads {
            let (root, pso, sync) = (&root, &pso, &sync);
            s.spawn(move || {
                let mut jobs = Vec::new();
                sync.wait();
                for index in (lane..JOBS).step_by(threads) {
                    let list = Arc::new(host.new_list().unwrap());
                    let (out, data) = host.shared_buffer(4).unwrap();
                    // SAFETY: Each recorder owns its open list and retains its output, heap and
                    // matching pipeline through submission.
                    unsafe {
                        list.list.SetDescriptorHeaps(&[Some(heap.heap.clone())]);
                        list.list.SetComputeRootSignature(root);
                        list.list.SetPipelineState(pso);
                        list.list
                            .SetComputeRootDescriptorTable(0, heap.gpu(index as u32));
                        list.list
                            .SetComputeRootUnorderedAccessView(1, out.GetGPUVirtualAddress());
                        list.list.Dispatch(1, 1, 1);
                    }
                    let next = (index + 1) % JOBS;
                    cbv(table, constants, next, next);
                    list.close();
                    jobs.push((list, out, data, index));
                }
                // All edits finish before any GPU dereference. Each recorded list
                // sees the replacement, including edits from a different recorder.
                sync.wait();
                for (list, _out, data, index) in jobs {
                    crate::queue::bench_submit(list);
                    assert_eq!(
                        // SAFETY: bench_submit has finished writing the mapped four-byte output,
                        // still held by _out.
                        unsafe { data.cast::<u32>().read() },
                        0xcafe0000 + index as u32
                    );
                }
            });
        }
    });
    let elapsed = start.elapsed();
    assert_eq!(bench_count(C::Submissions) - before[2], JOBS as u64);
    println!("BENCH record_submit_us={:.3}", elapsed.as_secs_f64() * 1e6);
    println!(
        "BENCH lists_per_second={:.3}",
        JOBS as f64 / elapsed.as_secs_f64()
    );
    println!(
        "BENCH mirror_lock_us={}",
        bench_count(C::MirrorWaitUs) - before[0]
    );
    println!(
        "BENCH guard_lock_us={}",
        bench_count(C::GuardWaitUs) - before[1]
    );
    println!("BENCH threads={threads}");
    drop(h);
    crate::shadow::unregister(constants, constants as u64);
    drop(copy);
    crate::watch::tests::free(constants);
    crate::watch::tests::free(table);
    println!("BENCH verified=1");
}

fn threads() -> usize {
    let n = std::env::var("VICTORY_BENCH_THREADS")
        .map(|s| s.parse::<usize>().unwrap())
        .unwrap_or(1);
    assert!(matches!(n, 1 | 4 | 8 | 16));
    n
}

#[test]
#[ignore = "CPU writers share pages declared by a pending GPU submission"]
fn write_overlap() {
    let host = crate::host::host().unwrap();
    let threads = threads();
    let addr = pool(threads);
    let bytes = threads * PAGE;
    crate::shadow::register(addr, addr as u64, bytes as u64);
    let copy = crate::shadow::find(addr as u64, bytes as u64).unwrap();
    let list = Arc::new(host.new_list().unwrap());
    copy.refresh(&list, addr as u64, bytes as u64);
    copy.gpu_writes(&list, addr as u64, bytes as u64, false);
    let sync = Arc::new(Barrier::new(threads + 1));
    let edge = sync.clone();
    // Start CPU writers after the snapshot. Rendezvous just after the GPU
    // fence, then keep writing the title page throughout the copy-back.
    list.defer_host(move || {
        edge.wait();
    });
    let edge = sync.clone();
    list.defer_missed(move || {
        edge.wait();
        false
    });
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let final_edge = done.clone();
    list.defer_after(move || final_edge.store(true, std::sync::atomic::Ordering::Release));
    let writes = std::sync::atomic::AtomicU64::new(0);
    let (root, pso) = crate::draw::tests::compile_cs(
        r#"
        RWByteAddressBuffer dst : register(u0);
        [RootSignature("UAV(u0)")] [numthreads(64,1,1)]
        void main(uint3 id : SV_DispatchThreadID) { dst.Store(8*id.x,0x55555555); }
    "#,
    );
    // SAFETY: The root matches the PSO, and the live shadow UAV covers the dispatched byte range.
    unsafe {
        list.list.SetComputeRootSignature(&root);
        list.list.SetPipelineState(&pso);
        list.list
            .SetComputeRootUnorderedAccessView(0, copy.gpu(addr as u64));
        list.list.Dispatch((bytes / 512) as u32, 1, 1);
    }
    list.close();
    timed("overlap_merge", || {
        std::thread::scope(|s| {
            for lane in 0..threads {
                let sync = sync.clone();
                let (done, writes) = (&done, &writes);
                s.spawn(move || {
                    sync.wait();
                    // SAFETY: Each lane writes its own allocated page before the second
                    // rendezvous permits copy-back.
                    unsafe { std::ptr::write_bytes((addr + lane * PAGE) as *mut u8, 0xaa, PAGE) };
                    let word = (addr + lane * PAGE + 4) as *mut u8;
                    let value = [0xbb; 4];
                    crate::host::write_mapped(word, value.as_ptr(), 4);
                    sync.wait();
                    let mut n = 0;
                    // Use the OS-checked copy path on both threads, with no
                    // aliased Rust reference to bytes being merged.
                    while !done.load(std::sync::atomic::Ordering::Acquire) {
                        crate::host::write_mapped(word, value.as_ptr(), 4);
                        n += 1;
                    }
                    writes.fetch_add(n, Relaxed);
                });
            }
            crate::queue::bench_submit(list);
        })
    });
    // SAFETY: Submission and writers have finished. The page-aligned pool still owns all bytes.
    let words = unsafe { std::slice::from_raw_parts(addr as *const u32, bytes / 4) };
    for page in words.as_chunks::<{ PAGE / 4 }>().0 {
        assert_eq!(page[..2], [0x55555555, 0xbbbbbbbb]);
        assert!(
            page[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .all(|p| *p == [0x55555555, 0xaaaaaaaa])
        );
    }
    println!("BENCH overlap_cpu_writes={}", writes.load(Relaxed));
    crate::shadow::unregister(addr, addr as u64);
    drop(copy);
    crate::watch::tests::free(addr);
    println!("BENCH merged_bytes={bytes}");
    println!("BENCH threads={threads}");
    println!("BENCH verified=1");
}

pub(super) fn make_queue(kind: u32) -> Guest {
    let mut q = null();
    let id = ID3D12CommandQueue::IID;
    let desc = [kind, 0, 0, 0];
    assert_eq!(
        crate::queue::create(
            null(),
            desc.as_ptr().cast(),
            &guid(id.data1, id.data2, id.data3, id.data4),
            &mut q
        ),
        0
    );
    Guest(q)
}

fn chain(tag: u64) {
    let host = crate::host::host().unwrap();
    let mut f = null();
    assert_eq!(
        crate::fence::create(null(), 0, 0, &crate::fence::FENCE_IIDS[0], &mut f),
        0
    );
    let fence = Guest(f);
    let stages = [0, 2, 3].map(|kind| {
        let q = make_queue(kind);
        let mut a = null();
        let id = ID3D12CommandAllocator::IID;
        assert_eq!(
            crate::list::create_allocator(
                null(),
                kind,
                &guid(id.data1, id.data2, id.data3, id.data4),
                &mut a
            ),
            0
        );
        let allocator = Guest(a);
        let id = ID3D12GraphicsCommandList::IID;
        let mut l = null();
        assert_eq!(
            crate::list::create_list(
                null(),
                0,
                kind,
                a,
                null(),
                &guid(id.data1, id.data2, id.data3, id.data4),
                &mut l
            ),
            0
        );
        (q, allocator, Guest(l))
    });
    let list = |i: usize| crate::list::host_list(stages[i].2.0).unwrap();
    let (src, input) = list(0).upload(host, 8).unwrap();
    let a = host.default_buffer(8).unwrap();
    let b = host.default_buffer(8).unwrap();
    let (dst, output) = list(2).readback(host, 8).unwrap();
    // SAFETY: All buffers cover eight bytes. Input is mapped and the resources outlive the
    // ordered copies.
    unsafe {
        input.cast::<u64>().write(tag);
        list(0).list.CopyBufferRegion(&a, 0, &src, 0, 8);
        list(1).list.CopyBufferRegion(&b, 0, &a, 0, 8);
        list(2).list.CopyBufferRegion(&dst, 0, &b, 0, 8);
    }
    for (_, _, l) in &stages {
        assert_eq!(slot::<extern "system" fn(This) -> i32>(l.0, 0x48)(l.0), 0);
    }
    let signal = if crate::device::newer() { 0x80 } else { 0x70 };
    let edge = |i: usize, method, value| {
        let q = stages[i].0.0;
        assert_eq!(
            slot::<extern "system" fn(This, This, u64) -> i32>(q, method)(q, fence.0, value),
            0
        );
    };
    // Queue the consumers first. Their completion signals must remain held
    // until the direct producer signals and pumps both waiting queues.
    for i in (0..3).rev() {
        if i != 0 {
            edge(i, signal + 8, i as u64);
        }
        let (q, _, l) = &stages[i];
        slot::<extern "system" fn(This, u32, *const This)>(q.0, 0x50)(q.0, 1, &l.0);
        if i == 0 {
            assert_eq!(crate::fence::value(fence.0), 0);
        }
        edge(i, signal, i as u64 + 1);
    }
    assert_eq!(crate::fence::value(fence.0), 3);
    // SAFETY: The final fence has completed the eight-byte copy and dst still owns the mapped
    // output.
    assert_eq!(unsafe { output.cast::<u64>().read() }, tag);
}

#[test]
#[ignore = "guest direct, compute and copy queue fence dependencies"]
fn queue_waits() {
    let n = threads();
    let start = Instant::now();
    std::thread::scope(|s| {
        for worker in 0..n {
            s.spawn(move || {
                for i in (worker..32).step_by(n) {
                    chain(0xcafebabe00000000 + i as u64);
                }
            });
        }
    });
    let elapsed = start.elapsed().as_secs_f64();
    println!("BENCH queue_chain_us={:.3}", elapsed * 1e6);
    println!("BENCH chains_per_second={:.3}", 32. / elapsed);
    println!("BENCH threads={n}");
    println!("BENCH verified=1");
}
