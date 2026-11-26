//! Compare the mapped view bitmap with the guest stores, including raster helpers.
use super::gfx10;
use crate::host::{Host, HostList};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_TYPELESS;

pub(super) fn pixel_helper_marks(host: &Host, execute: impl Fn(&str, &HostList, &dyn Fn())) {
    let graphics = victory_shader::emit::host::Host::from_device(host.device.clone());
    let vs = victory_shader::emit::translate(&gfx10::fullscreen()).unwrap();
    use victory_shader::emit::{VIEW_MARK_NONE, VIEW_PAGE_READS, VIEW_PAGE_SLOTS, VIEW_PAGE_TABLE};
    for (shift, paged) in [(2, false), (16, false), (2, true)] {
        // Virtual page two goes to physical page zero. Disable the view write
        // mark in this case, leaving only its virtual page read to measure.
        let virtual_slot = if paged { 2 * VIEW_PAGE_SLOTS } else { 0 };
        let bit_words = if paged {
            ((VIEW_PAGE_READS + 32) / 32) as usize
        } else {
            64
        };
        let meta_bytes = (8 * (virtual_slot + 1)).max(256) as usize;
        let mut ps = victory_shader::emit::translate_with(
            &gfx10::pixel_marks(shift),
            victory_shader::emit::Options {
                view_marks: true,
                view_pages: paged,
                ..crate::shader_cache::options()
            },
        )
        .unwrap();
        assert!(ps.view_marks);
        assert!(!ps.memory);
        if let Some(dir) = std::env::var_os("VICTORY_EMIT_DUMP") {
            std::fs::write(
                std::path::Path::new(&dir).join(format!("pixel-{shift}-{paged}.dxil")),
                &ps.dxil,
            )
            .unwrap();
        }
        use victory_shader::emit::validator::Validator;
        if let Ok(mut validator) = Validator::load(Validator::default_path().as_deref()) {
            validator.validate(&mut ps.dxil).unwrap();
        }
        let root = graphics
            .root_signature_for(&[(&ps, D3D12_SHADER_VISIBILITY_PIXEL)])
            .unwrap();
        let pso = graphics.pipeline_with(&root, &vs.dxil, &ps.dxil).unwrap();
        let bytes = 64u64 << shift;
        let (out, data) = host.shared_buffer(bytes).unwrap();
        let (meta, m) = host.shared_buffer(meta_bytes as u64).unwrap();
        let (strides, stride_data) = host
            .shared_buffer(if paged {
                u64::from(VIEW_PAGE_TABLE) + 256
            } else {
                256
            })
            .unwrap();
        let (bits, b) = host.shared_buffer((bit_words * 4) as u64).unwrap();
        let list = host.new_list().unwrap();
        // SAFETY: these mapped buffers and descriptors outlive execution.
        unsafe {
            data.write_bytes(0, bytes as usize);
            m.write_bytes(0, meta_bytes);
            stride_data.write_bytes(
                0,
                if paged {
                    VIEW_PAGE_TABLE as usize + 256
                } else {
                    256
                },
            );
            if paged {
                m.cast::<u32>()
                    .add(2 * virtual_slot as usize)
                    .write(VIEW_MARK_NONE);
            }
            b.write_bytes(0, bit_words * 4);
            let heap = raw_uav_heap(host, &out, (bytes / 4) as u32);
            let l = &list.list;
            l.SetDescriptorHeaps(&[Some(heap.clone())]);
            l.SetGraphicsRootSignature(&root);
            l.SetPipelineState(&pso);
            let constants = vec![virtual_slot * 32; ps.user_data_dwords().max(1) as usize];
            l.SetGraphicsRoot32BitConstants(
                0,
                constants.len() as u32,
                constants.as_ptr().cast(),
                0,
            );
            let mut slot = 1;
            for &srv in &ps.root_srvs {
                assert_eq!(srv, victory_shader::emit::VIEW_STRIDES);
                l.SetGraphicsRootShaderResourceView(slot, strides.GetGPUVirtualAddress());
                slot += 1;
            }
            l.SetGraphicsRootShaderResourceView(slot, meta.GetGPUVirtualAddress());
            l.SetGraphicsRootUnorderedAccessView(slot + 1, bits.GetGPUVirtualAddress());
            l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            l.RSSetViewports(&[D3D12_VIEWPORT {
                Width: 64.,
                Height: 2.,
                MaxDepth: 1.,
                ..Default::default()
            }]);
            let name = if paged {
                "descriptor_page"
            } else if shift == 2 {
                "shared_view_page"
            } else {
                "scattered_view_pages"
            };
            execute(name, &list, &|| {
                // Only an odd-x pixel of row 1 survives each scissor. The pixel
                // to its left is a helper, and its page must remain clear.
                for x in (1..64).step_by(2) {
                    l.RSSetScissorRects(&[RECT {
                        left: x,
                        top: 1,
                        right: x + 1,
                        bottom: 2,
                    }]);
                    l.DrawInstanced(3, 1, 0, 0);
                }
            });
            let mut expected = vec![0u32; bit_words];
            if paged {
                expected[(VIEW_PAGE_READS / 32) as usize] = 1 << 2;
            } else if shift == 2 {
                expected[0] = 1;
            } else {
                expected[..2].fill(0xaaaa_aaaa);
            }
            for (word, (&actual, wanted)) in std::slice::from_raw_parts(b.cast::<u32>(), bit_words)
                .iter()
                .zip(expected)
                .enumerate()
            {
                assert_eq!(
                    actual, wanted,
                    "bitmap word {word}, shift {shift}, paged {paged}"
                );
            }
            for x in 0..64 {
                let value = data.add(x << shift).cast::<u32>().read();
                assert_eq!(
                    value,
                    if x % 2 == 1 { x as u32 } else { 0 },
                    "pixel {x}, shift {shift}"
                );
            }
        }
    }
}

fn raw_uav_heap(host: &Host, out: &ID3D12Resource, words: u32) -> ID3D12DescriptorHeap {
    // SAFETY: the raw buffer covers the described words and outlives the heap.
    unsafe {
        let heap: ID3D12DescriptorHeap = host
            .device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })
            .unwrap();
        let uav = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    NumElements: words,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                    ..Default::default()
                },
            },
        };
        host.device.CreateUnorderedAccessView(
            out,
            None,
            Some(&uav),
            heap.GetCPUDescriptorHandleForHeapStart(),
        );
        heap
    }
}

#[test]
fn raster_helpers_and_compute_stores_mark_exact_view_pages() {
    let host = crate::host::host().expect("D3D12 device for the view-mark tests");
    pixel_helper_marks(host, |_, list, draw| {
        draw();
        list.close();
        host.execute(&[list]);
    });

    use gfx10::WaveCase::{StraddlingPair, View};
    let compiler = victory_shader::emit::host::Host::from_device(host.device.clone());
    let cases = [
        // The 64 lanes write four dwords on each of 16 pages.
        (View { scatter: 4 }, 0, 16 * 16_384, [1 << 31, 0x7fff, 0]),
        // The last lane writes at 65532..65540 relative to the first page.
        // Its second dword is the only access that sets the next bitmap word.
        (StraddlingPair, 65_024, 129, [1 << 31, 1, 0]),
    ];
    for (case, phase, words, expected) in cases {
        let shader = victory_shader::emit::translate_with(
            &gfx10::wave_scenario(None, case),
            victory_shader::emit::Options {
                view_marks: true,
                view_pages: false,
                ..crate::shader_cache::options()
            },
        )
        .expect("translate view store");
        assert!(shader.view_marks && !shader.memory && shader.root_srvs.is_empty());
        let (root, pso) = compiler.compute_pipeline(&shader).expect("compute pipeline");
        let (out, data) = host.shared_buffer(u64::from(words) * 4).unwrap();
        let (meta, metadata) = host.shared_buffer(8).unwrap();
        let (bitmap, bits) = host.shared_buffer(12).unwrap();
        let heap = raw_uav_heap(host, &out, words);
        let list = host.new_list().unwrap();
        // SAFETY: resources and their mappings live through execution and the
        // fence. The metadata selects page 31 so a crossing changes two words.
        unsafe {
            data.write_bytes(0, words as usize * 4);
            metadata.cast::<[u32; 2]>().write([31, phase]);
            bits.cast::<[u32; 3]>().write([0; 3]);
            let l = &list.list;
            l.SetDescriptorHeaps(&[Some(heap.clone())]);
            l.SetComputeRootSignature(&root);
            l.SetPipelineState(&pso);
            let constants = vec![0u32; shader.user_data_dwords().max(1) as usize];
            l.SetComputeRoot32BitConstants(0, constants.len() as u32, constants.as_ptr().cast(), 0);
            l.SetComputeRootShaderResourceView(1, meta.GetGPUVirtualAddress());
            l.SetComputeRootUnorderedAccessView(2, bitmap.GetGPUVirtualAddress());
            l.Dispatch(1, 1, 1);
            list.close();
            host.execute(&[&list]);

            assert_eq!(bits.cast::<[u32; 3]>().read(), expected, "view phase {phase}");
            let result = std::slice::from_raw_parts(data.cast::<u32>(), words as usize);
            for lane in 0..64 {
                let value = (4 * lane + 1) as u32;
                match case {
                    View { .. } => assert_eq!(result[(lane % 16) * 16_384 + lane / 16], value),
                    StraddlingPair => assert_eq!(&result[2 * lane + 1..2 * lane + 3], &[value, value + 1]),
                    _ => unreachable!(),
                }
            }
        }
    }
}
