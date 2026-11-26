use super::*;

/// Two hosts may share the same disk cache. The key and the pipeline name
/// must change only when the generated shader changes.
#[test]
fn wave_mark_capability_selects_its_cached_dxil() {
    use crate::shader_cache::{cached, key};
    use victory_shader::emit::{Options, translate_with};
    let dir = tempfile::tempdir().unwrap();
    let cache = victory_pipeline::Cache::open(dir.path()).unwrap();
    let container = super::gfx10::wave_container(None);
    let mut outputs = Vec::new();
    for wave_marks in [false, true] {
        let build = key(
            "test",
            b"test",
            victory_shader::emit::DEFAULT_LOOP_LIMIT,
            true,
            wave_marks,
        );
        let translate = |bytes: &[u8]| {
            translate_with(
                bytes,
                Options {
                    view_marks: true,
                    view_pages: true,
                    wave_marks,
                },
            )
            .map_err(|e| e.to_string())
        };
        let (first, hit) = cached(&cache, &build, &container, translate);
        assert!(!hit);
        let (again, hit) = cached(&cache, &build, &container, |_| {
            panic!("cached translation missed")
        });
        assert!(hit);
        assert_eq!(first, again);
        let bytes = first.unwrap().dxil;
        let pso_name = super::super::library::name("compute", &[], &[&bytes], "");
        outputs.push((bytes, pso_name));
    }
    assert_ne!(outputs[0].0, outputs[1].0);
    assert_ne!(outputs[0].1, outputs[1].1);
}

pub(super) fn run(host: &Host) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    };
    const GROUPS: usize = 4096;
    const WORDS: usize = GROUPS * 64;
    const BYTES: usize = WORDS * 4;
    // VIctory's guest VA space is 40-bit. Reserve an available aligned range within it.
    let src = (32..64usize)
        .find_map(|i| {
            let addr = i << 32;
            let p = unsafe {
                VirtualAlloc(
                    addr as *const _,
                    BYTES,
                    MEM_RESERVE | MEM_COMMIT | crate::watch::tests::MEM_WRITE_WATCH,
                    PAGE_READWRITE,
                )
            };
            (!p.is_null()).then_some(p as usize)
        })
        .unwrap();
    for i in 0..WORDS {
        unsafe {
            (src as *mut u32)
                .add(i)
                .write((4 * ((i + 1) % WORDS)) as u32)
        };
    }
    crate::shadow::register(src, src as u64, BYTES as u64);
    let src_copy = crate::shadow::find(src as u64, BYTES as u64).unwrap();
    for (source, name) in [
        (None, "wave_page_marks"),
        (Some(src as u64), "loop_page_lookup"),
    ] {
        let dst = pool(BYTES / PAGE);
        let resource = [
            1,
            0,
            0,
            0,
            BYTES as u32,
            0,
            1,
            1 | 1 << 16,
            0,
            1,
            0,
            1,
            4,
            0,
        ];
        let id = ID3D12Resource::IID;
        let iid = guid(id.data1, id.data2, id.data3, id.data4);
        let mut r = std::ptr::null();
        assert_eq!(
            crate::resource::create_at_address(
                r,
                dst as u64,
                resource.as_ptr().cast(),
                0,
                std::ptr::null(),
                &iid,
                &mut r
            ),
            0
        );
        let r = Guest(r);
        let bounds = crate::shadow::find(dst as u64, BYTES as u64).unwrap();
        assert!(bounds.marked());
        let h = heap(16, 0);
        let table = crate::descriptor::heap(h.0).start;
        let raw = [39, 1, 0, 0, WORDS as u32, 0, 0, 0, 1, 0];
        crate::descriptor::create_uav(std::ptr::null(), r.0, std::ptr::null(), &raw, table);
        let shader = super::super::shader("compute", &super::gfx10::wave_container(source));
        let translated = &shader.translated().expect("translate synthetic wave").host;
        assert!(translated.view_marks);
        assert_eq!(translated.memory, source.is_some());
        let mut pso = std::ptr::null();
        assert_eq!(
            super::super::hand_out(
                vec![shader],
                None,
                &super::super::PIPELINE_IIDS[0],
                &mut pso
            ),
            0
        );
        let pso = Guest(pso);
        assert!(super::super::host_pipeline(pso.0).is_some());
        let list = host.new_list().unwrap();
        let mut state = crate::draw::State::default();
        state.set_pipeline(pso.0);
        state.set_heaps(&[h.0]);
        state.set_table(true, 0, table);
        let mut query: Option<ID3D12QueryHeap> = None;
        let (readback, data) = list.readback(host, 16).unwrap();
        unsafe {
            host.device
                .CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC {
                        Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                        Count: 2,
                        NodeMask: 0,
                    },
                    &mut query,
                )
                .unwrap();
        }
        let query = query.unwrap();
        unsafe { list.list.EndQuery(&query, D3D12_QUERY_TYPE_TIMESTAMP, 0) };
        state.dispatch(&list, GROUPS as u32, 1, 1);
        unsafe {
            list.list.EndQuery(&query, D3D12_QUERY_TYPE_TIMESTAMP, 1);
            list.list
                .ResolveQueryData(&query, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &readback, 0);
        }
        list.close();
        host.execute(&[&list]);
        let times = unsafe { data.cast::<[u64; 2]>().read_unaligned() };
        assert!(times[1] > times[0]);
        let freq = crate::host::bench_timestamp_frequency();
        println!(
            "BENCH {name}_gpu_us={:.3}",
            (times[1] - times[0]) as f64 * 1e6 / freq as f64
        );
        println!("BENCH {name}_lanes={WORDS}");
        if source.is_some() {
            println!("BENCH loop_global_loads={}", 8 * WORDS);
        }
        let out = unsafe { std::slice::from_raw_parts(dst as *const u32, WORDS) };
        let steps = if source.is_some() { 8 } else { 0 };
        for (i, &v) in out.iter().enumerate() {
            assert_eq!(v, (4 * ((i + steps) % WORDS) + 1) as u32, "{name} lane {i}");
        }
        drop((state, list, h, bounds, r));
        crate::watch::tests::free(dst);
    }
    crate::shadow::unregister(src, src as u64);
    drop(src_copy);
    crate::watch::tests::free(src);
}
