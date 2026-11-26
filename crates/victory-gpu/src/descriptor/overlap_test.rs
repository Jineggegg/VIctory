use super::*;
use crate::host::Host;
use victory_shader::emit::{HostShader, VIEW_STRIDES};
use victory_shader::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};

const LANES: usize = 32;
const ELEMENTS: usize = 36;
const FIELDS: usize = 3;
const BYTES: usize = ELEMENTS * FIELDS * 4;
const TITLE_VIEWS: u32 = 1_245_184;
// Exercise the first page boundary, a slot above the hardware cap, and the last guest slot.
const READERS: [u32; 5] = [1, 255, 256, 1_099_777, TITLE_VIEWS - 1];
const WRITER_SLOT: u32 = 1_099_778;

// Each lane addresses element v0 of the heap view at s0. Readers export the
// selected field through a separate raw output UAV in s2. Writers store the
// lane index, distinguishable from the initial values in every backing dword.
fn shader(write: bool, field: usize) -> HostShader {
    let mut code = vec![
        0x8F04_8500, // s_lshl_b32 s4, s0, 5
        0xBE86_0301,
        0xBE87_0380, // s[6:7] = resource heap
        0xF408_0203,
        0x0800_0000, // s_load_dwordx4 s[8:11], s[6:7], s4
    ];
    let byte = (field * 4) as u32;
    if write {
        code.extend([
            0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
            0xE070_2000 | byte,
            0x8002_0000, // store v0 to elem v0, s[8:11]
        ]);
    } else {
        code.extend([
            0xBE8C_0302,
            0xBE8D_0380, // s[12:13] = output table
            0xF408_0406,
            0xFA00_0000, // s_load_dwordx4 s[16:19], s[12:13], 0
            0x3402_0082, // v_lshlrev_b32 v1, 2, v0
            0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
            0xE030_2000 | byte,
            0x8002_0200, // load v2 from elem v0, s[8:11]
            0xBF8C_3F70, // s_waitcnt vmcnt(0)
            0xE070_1000,
            0x8004_0201, // store v2 to raw byte v1, s[16:19]
        ]);
    }
    code.push(0xBF81_0000); // s_endpgm
    let layout = [RootType::Constants, RootType::ResourceHeap, RootType::Table]
        .into_iter()
        .enumerate()
        .map(|(slot, root)| RootParam {
            root,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: slot as u8,
            slot: slot as u8,
        })
        .collect();
    let output = UsedResource {
        kind: ResourceType::Uav,
        root: RootType::Table,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride: 0,
        dwords: 4,
        user_data: 2,
        slot: 2,
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout,
        used: if write { vec![] } else { vec![output] },
        io: StageIo::Cs(ComputeIo {
            rsrc2: 3 << 1,
            threads: [LANES as u32, 1, 1],
        }),
        code: code.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let ir = victory_shader::ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let st = victory_shader::structure::structure(&ir).unwrap();
    let res =
        victory_shader::resource::track(&ir, &native.layout, native.stage, Some(&st)).unwrap();
    let io = victory_shader::io::raise(&ir, &native).unwrap();
    let signatures = victory_shader::Container {
        digest: [0; 16],
        version: (1, 0),
        parts: Vec::new(),
    };
    let options = victory_shader::emit::Options { view_marks: false, view_pages: view_pages(), ..Default::default() };
    let emitted = victory_shader::emit::emit_with(&signatures, &native, &ir, &st, &io, &res, options).unwrap();
    assert!(emitted.heap && emitted.heap_writes == write);
    assert_eq!(emitted.root_srvs, [VIEW_STRIDES]);
    emitted
}

fn mirror(heap: &HostHeap, slot: u32, address: u64, first: usize, write: bool) -> [u32; 8] {
    assert!(first + LANES <= ELEMENTS);
    let mut resource = [0; 14];
    resource[0] = 1; // D3D12_RESOURCE_DIMENSION_BUFFER
    resource[4] = BYTES as u32;
    let mut words = [0; 16];
    words[10] = address as u32;
    words[11] = (address >> 32) as u32;
    let mut record = [0u32; 8];
    let handle = record.as_mut_ptr() as u64;
    if write {
        words[1] = D3D12_UAV_DIMENSION_BUFFER.0 as u32;
        words[2] = first as u32;
        words[4] = LANES as u32;
        words[5] = (FIELDS * 4) as u32;
        crate::descriptor::create_placed_uav(std::ptr::null(), &resource, &words, handle);
    } else {
        words[1] = D3D12_SRV_DIMENSION_BUFFER.0 as u32;
        words[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        words[4] = first as u32;
        words[6] = LANES as u32;
        words[7] = (FIELDS * 4) as u32;
        crate::descriptor::create_placed_srv(std::ptr::null(), &resource, &words, handle);
    }
    let view = heap.mirror(slot, handle, true).unwrap();
    assert_eq!(
        buffer_range(&view),
        Some((
            address + (first * FIELDS * 4) as u64,
            (LANES * FIELDS * 4) as u64
        ))
    );
    record
}

struct Kernel {
    shader: HostShader,
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

impl Kernel {
    fn new(device: &victory_shader::emit::host::Host, write: bool, field: usize) -> Self {
        let shader = shader(write, field);
        let (root, pso) = device.compute_pipeline(&shader).expect("overlap kernel on SM 6.6");
        Self { shader, root, pso }
    }

    fn run(&self, host: &Host, heap: &HostHeap, slot: u32) {
        let list = host.new_list().unwrap();
        // Follow the heap-indexed dispatch path in draw::State::bind. Each
        // submission gets the chance to refresh the heap's backing buffers.
        list.defer_heap(&heap.viewed);
        let mut ud = vec![0; self.shader.user_data_dwords().max(1) as usize];
        for u in &self.shader.user_data {
            ud[u.index as usize] = match u.root {
                RootType::Constants => slot,
                RootType::Table => 0,
                other => panic!("unexpected root {other:?}"),
            };
        }
        // SAFETY: all descriptors, resources and root arguments live through
        // the waited submission. Each buffer implicitly promotes from COMMON.
        unsafe {
            list.list.SetDescriptorHeaps(&[Some(heap.heap.clone())]);
            list.list.SetComputeRootSignature(&self.root);
            list.list.SetPipelineState(&self.pso);
            list.list
                .SetComputeRoot32BitConstants(0, ud.len() as u32, ud.as_ptr().cast(), 0);
            list.list.SetComputeRootShaderResourceView(
                1,
                heap.strides().unwrap().GetGPUVirtualAddress(),
            );
            list.list.Dispatch(1, 1, 1);
        }
        list.close();
        // Fence completion and implicit buffer decay precede the next run.
        host.execute(&[&list]);
        // SAFETY: status query on the live device.
        unsafe { host.device.GetDeviceRemovedReason().unwrap() };
    }
}

#[test]
fn gpu_writes_visible_through_overlapping_views() {
    let Some(host) = host() else {
        return; // no host GPU here
    };
    let _ = host.log_messages();
    let mut sm = D3D12_FEATURE_DATA_SHADER_MODEL { HighestShaderModel: D3D_SHADER_MODEL_6_6 };
    // SAFETY: the feature query receives the corresponding structure and size.
    let queried = unsafe {
        host.device.CheckFeatureSupport(D3D12_FEATURE_SHADER_MODEL, (&raw mut sm).cast(),
            size_of_val(&sm) as u32)
    };
    if queried.is_err() || sm.HighestShaderModel.0 < D3D_SHADER_MODEL_6_6.0 {
        eprintln!("overlap fixture requires SM 6.6");
        return;
    }
    let heap = HostHeap::new(false, TITLE_VIEWS).expect("guest heap with high view indices");
    let device = victory_shader::emit::host::Host::from_device(host.device.clone());
    let writer = Kernel::new(&device, true, FIELDS - 1);
    let readers: Vec<_> = (0..FIELDS).map(|field| Kernel::new(&device, false, field)).collect();
    let (output, mapped) = host.shared_buffer((LANES * 4) as u64).unwrap();
    heap.raw_view_at(0, Some(&output), 0, (LANES * 4) as u64, true);

    let initial: [u32; ELEMENTS * FIELDS] = std::array::from_fn(|i| 0xa170_0000 + i as u32);
    // Title GPU memory participates in per-submit change tracking.
    let pool = crate::watch::tests::pool(5);
    let pages = (0..5).map(|i| pool + i * crate::watch::PAGE);
    let mut total_wrong = 0;
    for (writer_first, page) in pages.enumerate() {
        let address = page as u64;
        // SAFETY: this new page contains the whole 432-byte allocation.
        unsafe { std::ptr::copy_nonoverlapping(initial.as_ptr(), page as *mut u32, initial.len()) };
        crate::shadow::register(address as usize, address, BYTES as u64);
        for (reader_first, slot) in READERS.into_iter().enumerate() {
            mirror(&heap, slot, address, reader_first, false);
        }
        mirror(&heap, WRITER_SLOT, address, writer_first, true);
        writer.run(host, &heap, WRITER_SLOT);
        let mut expected = initial;
        for lane in 0..LANES {
            expected[(writer_first + lane) * FIELDS + FIELDS - 1] = lane as u32;
        }
        // The title makes no CPU-side changes between the writer and readers.
        for (reader_first, slot) in READERS.into_iter().enumerate() {
            for (field, reader) in readers.iter().enumerate() {
                // SAFETY: the previous submission completed; 32 mapped output dwords.
                unsafe { std::ptr::write_bytes(mapped, 0x55, LANES * 4) };
                reader.run(host, &heap, slot);
                // SAFETY: every lane's output is visible after the queue fence.
                let values = unsafe { std::slice::from_raw_parts(mapped.cast::<u32>(), LANES) };
                let want = (0..LANES)
                    .map(|lane| expected[(reader_first + lane) * FIELDS + field])
                    .collect::<Vec<_>>();
                let wrong = values.iter().zip(&want).filter(|(a, b)| a != b).count();
                if wrong > 0 {
                    let first = values
                        .iter()
                        .zip(&want)
                        .enumerate()
                        .find(|(_, (a, b))| a != b);
                    eprintln!(
                        "READ writer={writer_first} reader={reader_first} field={field} mismatches={wrong}/32 first_difference(lane, (actual, expected))={first:?}"
                    );
                }
                total_wrong += wrong;
            }
        }
        // SAFETY: the pool is live and every submitted reader has finished.
        let backing = unsafe { (page as *const [u32; ELEMENTS * FIELDS]).read() };
        assert_eq!(backing, initial);
        crate::shadow::unregister(address as usize, address);
    }
    drop(heap);
    crate::watch::tests::free(pool);
    assert_eq!(host.log_messages(), None, "D3D12 validation errors");
    assert_eq!(total_wrong, 0, "mismatches out of 2400");
    eprintln!("Verified 2400 words across five view origins and guest slots through {}.", TITLE_VIEWS - 1);
}

/// After eviction the slot's default Texture2D cannot be used as a buffer.
/// Changing only the dedicated null SRV distinguishes the guard's two routes.
#[test]
fn an_evicted_buffer_reads_through_the_dedicated_null_srv() {
    let Some(host) = host() else { return };
    if !view_pages() { return; }
    let heap = HostHeap::new(false, 2 * PAGE).unwrap();
    let slot = PAGE + 13;
    let bytes = (4 * LANES) as u64;
    let (canary, src) = host.shared_buffer(bytes).unwrap();
    let pool = crate::watch::tests::pool(1);
    let (output, dst) = host.shared_buffer(bytes).unwrap();
    // SAFETY: both mapped allocations hold LANES dwords; no pending submission.
    unsafe { std::ptr::write_bytes(src, 0x37, bytes as usize) };
    let words: [u32; ELEMENTS * FIELDS] = std::array::from_fn(|i| 100 + i as u32);
    // SAFETY: our live pool has a full page for the title's structured buffer.
    unsafe { std::ptr::copy_nonoverlapping(words.as_ptr(), pool as *mut u32, words.len()) };
    let field = |first: usize| std::array::from_fn(|i| words[(first + i) * FIELDS]);
    crate::shadow::register(pool, pool as u64, BYTES as u64);
    mirror(&heap, slot, pool as u64, 1, false);
    let data = heap.strides.as_ref().unwrap().1 as *const u32;
    // SAFETY: slot is within the mapped stride table of our private idle heap.
    let stride_word = || unsafe { data.add(slot as usize).read_volatile() };
    let biased = 12 | (3 << victory_shader::emit::VIEW_RAW_OFFSET_SHIFT);
    assert_eq!(stride_word(), biased);
    let device = victory_shader::emit::host::Host::from_device(host.device.clone());
    let reader = Kernel::new(&device, false, 0);
    let read = |expected: [u32; LANES]| {
        heap.raw_view_at(0, Some(&output), 0, bytes, true);
        // SAFETY: poison every output dword before dispatch; run waits for its fence.
        unsafe { std::ptr::write_bytes(dst, 0xEE, bytes as usize) };
        reader.run(host, &heap, slot);
        // SAFETY: the mapped result contains LANES initialized dwords.
        assert_eq!(unsafe { std::slice::from_raw_parts(dst.cast::<u32>(), LANES) }, expected);
    };
    read(field(1));
    crate::host::tests::without_submissions(|| {
        crate::host::tests::take_back_all(&heap, &mut std::time::Instant::now());
    });
    assert_eq!(heap.host_page_of(slot), Some(NULL_PAGE));
    assert_eq!(stride_word(), 0);
    read([0; LANES]);
    heap.raw_view_at(victory_shader::emit::NULL_BUFFER_VIEW, Some(&canary), 0, bytes, false);
    read([0x3737_3737; LANES]);
    assert_eq!(heap.host_page_of(slot), Some(NULL_PAGE));

    // A guest rewrite with no room goes into scratch. Rebuilding the page
    // from that record must restore its stride and nonzero prefix.
    heap.hold_page_room(true);
    let record = mirror(&heap, slot, pool as u64, 2, false);
    assert_eq!(heap.host_page_of(slot), Some(NULL_PAGE));
    assert_eq!(stride_word(), 0);
    heap.hold_page_room(false);
    heap.place_page(slot / PAGE);
    assert!(heap.remakes().contains(&(PAGE, 2 * PAGE)));
    heap.mirror(slot, record.as_ptr() as u64, true).unwrap();
    assert_eq!(stride_word(), 12 | (2 << victory_shader::emit::VIEW_RAW_OFFSET_SHIFT));
    read(field(2));
    drop(heap);
    crate::shadow::unregister(pool, pool as u64);
    crate::watch::tests::free(pool);
}
