//! Actual raster helper lanes: make one pixel in a quad cover a triangle,
//! store its neighbour's x coordinate and check the raw write bitmap.
use super::*;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_TYPELESS;

#[test]
#[ignore = "dedicated hardware or WARP process"]
fn pixel_helper_marks() {
    let host = crate::host::host().unwrap();
    let graphics = victory_shader::emit::host::Host::from_device(host.device.clone());
    let vs = victory_shader::emit::translate(&super::super::gfx10::fullscreen()).unwrap();
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
            &super::super::gfx10::pixel_marks(shift),
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
        let (timing, ticks) = list.readback(host, 16).unwrap();
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
                        NumElements: (bytes / 4) as u32,
                        Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                        ..Default::default()
                    },
                },
            };
            host.device.CreateUnorderedAccessView(
                &out,
                None,
                Some(&uav),
                heap.GetCPUDescriptorHandleForHeapStart(),
            );
            let mut query: Option<ID3D12QueryHeap> = None;
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
            let query = query.unwrap();
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
            l.EndQuery(&query, D3D12_QUERY_TYPE_TIMESTAMP, 0);
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
            l.EndQuery(&query, D3D12_QUERY_TYPE_TIMESTAMP, 1);
            l.ResolveQueryData(&query, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &timing, 0);
            list.close();
            host.execute(&[&list]);
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
            let [start, end] = ticks.cast::<[u64; 2]>().read_unaligned();
            assert!(end > start);
            let name = if paged {
                "descriptor_page"
            } else if shift == 2 {
                "shared_view_page"
            } else {
                "scattered_view_pages"
            };
            println!(
                "BENCH pixel_helper_{name}_gpu_us={:.3}",
                (end - start) as f64 * 1e6 / crate::host::bench_timestamp_frequency() as f64
            );
        }
    }
}
