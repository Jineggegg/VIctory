use super::*;

pub(super) fn run(host: &Host) {
    scenarios(host, false);
}

/// Run in a fresh process so every sample includes fresh host bitmaps.
#[test]
#[ignore = "dedicated GPU timing window"]
fn wave_scenarios() {
    let host = crate::host::host().unwrap();
    // SAFETY: this is the DXGI adapter that created the test device.
    let desc = unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let adapter: IDXGIAdapter1 = factory.EnumAdapterByLuid(host.device.GetAdapterLuid()).unwrap();
        adapter.GetDesc1().unwrap()
    };
    println!("BENCH adapter={}", String::from_utf16_lossy(&desc.Description).trim_end_matches('\0'));
    scenarios(host, true);
    super::super::view_marks_tests::pixel_helper_marks(host, |name, list, draw| {
        let elapsed = time_commands(host, list, draw);
        println!("BENCH pixel_helper_{name}_gpu_us={elapsed:.3}");
    });
    println!("BENCH verified=1");
}

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
    // A shader without a bitmap access keeps byte-for-byte identical DXIL.
    let vertex = super::gfx10::fullscreen();
    let plain = |wave_marks| translate_with(
        &vertex, Options { wave_marks, view_marks: true, view_pages: true },
    ).unwrap().dxil;
    assert_eq!(plain(false), plain(true));
}

fn scenarios(host: &Host, extended: bool) {
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
    use super::gfx10::WaveCase::*;
    let cases = [
        ("wave_page_marks", View { scatter: 0 }),
        ("loop_page_lookup", Lookup),
        ("wave_two_pages", View { scatter: 1 }),
        ("wave_sixteen_pages", View { scatter: 4 }),
        ("wave_straddling_pairs", StraddlingPair),
        ("loop_early_exit", VariableLoop),
        ("title_odd_lanes", Title { scatter: 0 }),
        ("title_scattered_odd_lanes", Title { scatter: 4 }),
    ];
    let cases = if extended { &cases[..] } else { &cases[..2] };
    for &(name, case) in cases {
        let scatter = case.scatter();
        let cross = matches!(case, StraddlingPair);
        let title_store = matches!(case, Title { .. });
        let source = matches!(case, Lookup | VariableLoop | Title { .. }).then_some(src as u64);
        let count = if cross { 2 * WORDS + 1 } else { WORDS };
        let dst = pool((count * 4).div_ceil(PAGE));
        if title_store {
            // Start each title-write fixture with a known sentinel; the
            // readback checks that no inactive lane overwrote it.
            unsafe { std::ptr::write_bytes(src as *mut u32, 0, WORDS) };
        }
        let resource = [
            1,
            0,
            0,
            0,
            (count * 4) as u32,
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
        let bounds = crate::shadow::find(dst as u64, (count * 4) as u64).unwrap();
        assert!(bounds.marked());
        let h = heap(16, 0);
        let table = crate::descriptor::heap(h.0).start;
        let raw = [39, 1, 0, 0, count as u32, 0, 0, 0, 1, 0];
        crate::descriptor::create_uav(std::ptr::null(), r.0, std::ptr::null(), &raw, table);
        let shader = super::super::shader("compute", &super::gfx10::wave_scenario(source, case));
        let translated = &shader.translated().expect("translate synthetic wave").host;
        if let Some(dir) = std::env::var_os("VICTORY_EMIT_DUMP") {
            std::fs::write(
                std::path::Path::new(&dir).join(format!("{name}.dxil")),
                &translated.dxil,
            )
            .unwrap();
        }
        assert_eq!(translated.view_marks, !title_store);
        if title_store { assert!(translated.memory_writes); }
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
        let elapsed = time_commands(host, &list, || state.dispatch(&list, GROUPS as u32, 1, 1));
        println!("BENCH {name}_gpu_us={elapsed:.3}");
        println!("BENCH {name}_lanes={WORDS}");
        if matches!(case, Lookup) {
            println!("BENCH loop_global_loads={}", 8 * WORDS);
        }
        let out = unsafe { std::slice::from_raw_parts(dst as *const u32, count) };
        if cross {
            assert_eq!(out[0], 0);
        }
        for i in 0..WORDS {
            let steps = match case {
                VariableLoop => 1 + (i / 64) % 64,
                Lookup => 8,
                _ => 0,
            };
            let offset = ((i & ((1 << scatter) - 1)) << (18 - scatter)) | (i >> scatter);
            let expected = (4 * ((i + steps) % WORDS) + 1) as u32;
            let expected = if title_store && i & 1 == 0 {
                0
            } else {
                expected
            };
            if title_store {
                assert_eq!(
                    unsafe { (src as *const u32).add(offset).read() },
                    expected,
                    "{name} title lane {i}"
                );
                continue;
            }
            if cross {
                assert_eq!(out[2 * offset + 1], expected, "{name} lane {i}");
                assert_eq!(out[2 * offset + 2], expected + 1, "{name} lane {i} high");
            } else {
                assert_eq!(out[offset], expected, "{name} lane {i}");
            }
        }
        drop((state, list, h, bounds, r));
        crate::watch::tests::free(dst);
    }
    crate::shadow::unregister(src, src as u64);
    drop(src_copy);
    crate::watch::tests::free(src);
}

fn time_commands(host: &Host, list: &crate::host::HostList, commands: impl FnOnce()) -> f64 {
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
    commands();
    unsafe {
        list.list.EndQuery(&query, D3D12_QUERY_TYPE_TIMESTAMP, 1);
        list.list
            .ResolveQueryData(&query, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &readback, 0);
    }
    list.close();
    // One dispatch between timestamps; the sampler warms the device separately.
    host.execute(&[list]);
    let times = unsafe { data.cast::<[u64; 2]>().read_unaligned() };
    assert!(times[1] > times[0]);
    let freq = crate::host::bench_timestamp_frequency();
    (times[1] - times[0]) as f64 * 1e6 / freq as f64
}
