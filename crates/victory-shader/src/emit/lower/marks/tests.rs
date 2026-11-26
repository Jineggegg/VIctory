//! Adapted from Marvisak/VIctory PR #2755 (brainbudt). Check the whole
//! bitmap on hardware and WARP, including writes under a narrowed EXEC.

use crate::dxbc::Container;
use crate::emit::{
    HostShader, Options, emit_with,
    host::{Dispatch, Host, Root, View},
};
use crate::native::{ComputeIo, HwStage, NativeProgram, RootType, StageIo};
use crate::{io, ir, resource, structure};

/// Write to a title address offset by the local thread id shifted by s2.
/// A preceding store to page zero leaves the first bit already set.
fn dirty_mark_program(
    divergent: bool,
    threads: u32,
    wave_marks: bool,
    fixed_width: bool,
) -> HostShader {
    let mut words: Vec<u32> = vec![
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0xDC70_8000,
        0x0000_0001, // global_store_dword v1, v0, s[0:1]
        0x3404_0002, // v_lshlrev_b32 v2, s2, v0
        0xDC70_8000,
        0x0000_0002, // global_store_dword v2, v0, s[0:1]
        0xBF8C_0000,
        0xBF81_0000, // s_waitcnt; s_endpgm
    ];
    if divergent {
        // (thread id & 1) != 0 narrows EXEC for the second store. In
        // particular the physically lowest lane cannot lead this mark.
        words.splice(4..4, [0x3606_0081, 0x7daa_0680]);
    }
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: fixed_width && threads == 64,
        vgprs: 4,
        layout: vec![crate::native::RootParam {
            root: RootType::Constants,
            table_offset_dwords: 0,
            sgprs: 3,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            // A single-wave group with an LDS allocation requests the native
            // wave size in DXIL, even without a cross-lane guest instruction.
            rsrc2: 3 << 1 | u32::from(fixed_width) << 15,
            threads: [threads, 1, 1],
        }),
        code: words.into_iter().flat_map(u32::to_le_bytes).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let bytes = crate::dxbc::build(&[]);
    emit_with(
        &Container::parse(&bytes).unwrap(),
        &native,
        &p,
        &st,
        &io,
        &res,
        Options {
            wave_marks,
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn title_page_marks_keep_every_lanes_bit() {
    let _one = crate::emit::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use windows::Win32::Graphics::{Direct3D::D3D_FEATURE_LEVEL_12_0, Direct3D12::*, Dxgi::*};
    // Exercise every installed hardware adapter (including integrated GPUs).
    // WARP is selected separately and may occur in the DXGI list as well.
    let factory: IDXGIFactory4 =
        unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }.unwrap();
    for i in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        let desc = unsafe { adapter.GetDesc1() }.unwrap();
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        let mut device: Option<ID3D12Device> = None;
        if unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device) }.is_err() {
            continue;
        }
        let name = String::from_utf16_lossy(&desc.Description);
        verify_adapter(
            &Host::from_device(device.unwrap()),
            name.trim_end_matches('\0'),
        );
    }
    if let Ok(warp) = Host::new(true) {
        verify_adapter(&warp, "WARP");
    }
}

fn verify_adapter(host: &Host, name: &str) {
    use windows::Win32::Graphics::Direct3D12::*;
    let mut sm = D3D12_FEATURE_DATA_SHADER_MODEL {
        HighestShaderModel: D3D_SHADER_MODEL_6_6,
    };
    let mut caps = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
    // SAFETY: both queries write to their corresponding sized structures.
    unsafe {
        if host
            .device
            .CheckFeatureSupport(
                D3D12_FEATURE_SHADER_MODEL,
                (&raw mut sm).cast(),
                size_of_val(&sm) as u32,
            )
            .is_err()
            || sm.HighestShaderModel.0 < D3D_SHADER_MODEL_6_6.0
            || host
                .device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS1,
                    (&raw mut caps).cast(),
                    size_of_val(&caps) as u32,
                )
                .is_err()
        {
            eprintln!("{name}: SM 6.6 wave operations unavailable, skipped");
            return;
        }
    }
    for threads in [32, 64] {
        for divergent in [false, true] {
            verify_marks(host, divergent, threads, false, false);
            if caps.WaveOps.as_bool() {
                verify_marks(host, divergent, threads, true, false);
                if (caps.WaveLaneCountMin..=caps.WaveLaneCountMax).contains(&threads) {
                    verify_marks(host, divergent, threads, true, true);
                }
            }
        }
    }
    eprintln!(
        "{name}: marks verified, reported wave widths {}..={}",
        caps.WaveLaneCountMin, caps.WaveLaneCountMax
    );
}

fn verify_marks(host: &Host, divergent: bool, threads: u32, wave_marks: bool, fixed_width: bool) {
    let shader = dirty_mark_program(divergent, threads, wave_marks, fixed_width);
    let (root, pso) = host.compute_pipeline(&shader).unwrap();
    // Region zero uses block 1. Its first page points to the real copy,
    // the rest to the null pair. Writes and misses both owe a mark.
    let mut table = vec![0; 4096 + 2 * 2 * 4096];
    table[0] = 1;
    table[4096 + 2 * 4096] = 2;
    for shift in [2, 16, 21] {
        let mut constants = vec![0; shader.user_data_dwords() as usize];
        for u in &shader.user_data {
            constants[u.index as usize] = [0, 0, shift][u.dword as usize];
        }
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![
                        (0, Root::Constants(constants)),
                        (
                            1,
                            Root::View {
                                srv: true,
                                data: table.clone(),
                            },
                        ),
                        (2, Root::Uav(8192 / 32)),
                    ],
                    views: &[
                        View::NullPair,
                        View::Pair,
                        View::Memory(&vec![0; threads as usize]),
                        View::Pair,
                    ],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap();
        let mut expected = vec![0u32; 8192 / 32];
        expected[4096 / 32] = 1; // first, unguarded store
        for lane in (0..threads as usize).filter(|lane| !divergent || lane & 1 != 0) {
            let page = 4096 + ((lane << shift) >> 16);
            expected[page / 32] |= 1 << (page % 32);
        }
        assert_eq!(
            out[0],
            (0..threads).collect::<Vec<_>>(),
            "title data, shift {shift}"
        );
        assert_eq!(out[1], expected, "dirty pages, shift {shift}");
    }
}
