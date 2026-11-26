use super::*;
const SIDE: u32 = 2048;
const MIPS: u32 = 4;

pub(super) fn run(host: &Host, count_textures: usize) {
    batch(host, count_textures, false);
}

pub(super) fn batch(host: &Host, count_textures: usize, check_eviction: bool) {
    let desc = [3, 0, 0, 0, SIDE, 0, SIDE, 1 | MIPS << 16, 28, 1, 0, 0, 0, 0];
    // SAFETY: resource description in the title ABI, with both padding words initialized.
    let d = unsafe {
        desc.as_ptr()
            .cast::<crate::resource::ResourceDesc>()
            .read_unaligned()
    };
    let layout = crate::resource::texture_layout(&d).unwrap().planes[0]
        .layout
        .clone();
    let mut textures = Vec::new();
    for k in 0..count_textures {
        // Placed textures cache their address for the lifetime of this sample process.
        let base = pool((layout.size as usize).div_ceil(PAGE));
        unsafe { std::ptr::write_bytes(base as *mut u8, k as u8 + 1, layout.size as usize) };
        for mip in 0..MIPS {
            let offset = layout.offset(mip, 73, 39, 0).unwrap();
            unsafe { ((base + offset as usize) as *mut u32).write_unaligned(0x1234abcd + mip) };
        }
        let guest = crate::resource::texture_at(base as u64, &desc).unwrap();
        let texture = crate::resource::host_resource(guest).unwrap();
        textures.push((base, guest, texture));
    }
    let mut previous_bytes = 0;
    for (partial, name) in [(false, "texture_full"), (true, "texture_partial")] {
        let list = host.new_list().unwrap();
        let mut copied = 0;
        for &(base, guest, ref texture) in &textures {
            if partial {
                // Change just 256 texels in mip 1. All other subresources should survive.
                for y in 17..25 {
                    for x in 31..63 {
                        let offset = layout.offset(1, x, y, 0).unwrap();
                        unsafe {
                            ((base + offset as usize) as *mut u32).write_unaligned(0xabcd1234)
                        };
                    }
                }
            }
            copied += crate::texture::copied(guest);
            crate::texture::upload(&list, guest, texture);
        }
        list.close();
        timed(name, || host.execute(&[&list]));
        let after: u64 = textures
            .iter()
            .map(|&(_, r, _)| crate::texture::copied(r))
            .sum();
        let count = if partial {
            count_textures
        } else {
            count_textures * MIPS as usize
        };
        assert_eq!(after - copied, count as u64, "{name} subresources");
        let total_bytes: u64 = textures
            .iter()
            .map(|&(_, r, _)| crate::texture::copied_bytes(r))
            .sum();
        let bytes = total_bytes - previous_bytes;
        previous_bytes = total_bytes;
        if !partial {
            assert_eq!(bytes, count_textures as u64 * 21_760 * 1024);
        }
        println!("BENCH {name}_bytes={bytes}");
        println!("BENCH {name}_subresources={count}");
        verify(host, &textures, partial);
    }
    println!(
        "BENCH texture_title_patch_bytes={}",
        count_textures * 32 * 8 * 4
    );
    if check_eviction {
        let addresses: Vec<_> = textures.into_iter().map(|(a, g, _)| (a, g)).collect();
        // Advance exactly one guest frame. The next Host::execute queries
        // WDDM and runs the real LRU policy on the now-idle textures.
        let q = super::contention::make_queue(0);
        let present = if crate::device::newer() { 0x100 } else { 0xf0 };
        let call = slot::<extern "system" fn(This, u32, *const u8, *const u8) -> i32>(q.0, present);
        assert_eq!(call(q.0, 0, std::ptr::null(), std::ptr::null()), 0);
        let list = host.new_list().unwrap();
        list.close();
        host.execute(&[&list]);
        let mut active = Vec::new();
        let list = host.new_list().unwrap();
        timed("pressure_reopen", || {
            for (a, g) in addresses {
                let resource = crate::resource::host_resource(g).expect("texture after eviction");
                crate::texture::upload(&list, g, &resource);
                active.push((a, g, resource));
            }
            list.close();
            host.execute(&[&list]);
        });
        verify(host, &active, true);
    }
}

// Actual D3D12 readback of every mip after each upload. Validation runs outside timings.
fn verify(host: &Host, textures: &[(usize, This, ID3D12Resource)], partial: bool) {
    for (k, &(_, guest, ref texture)) in textures.iter().enumerate() {
        let list = host.new_list().unwrap();
        crate::list::ensure_state(&list, guest, D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32);
        let mut footprints = [D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); MIPS as usize];
        let mut total = 0;
        unsafe {
            host.device.GetCopyableFootprints(
                &texture.GetDesc(),
                0,
                MIPS,
                0,
                Some(footprints.as_mut_ptr()),
                None,
                None,
                Some(&mut total),
            );
        }
        let (back, data) = list.readback(host, total).unwrap();
        for (m, &fp) in footprints.iter().enumerate() {
            // SAFETY: footprints were queried for this resource, buffers stay live to the fence.
            unsafe {
                let dst = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&back),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: fp,
                    },
                };
                let src = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(texture),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        SubresourceIndex: m as u32,
                    },
                };
                list.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            }
        }
        list.close();
        host.execute(&[&list]);
        for (m, f) in footprints.iter().enumerate() {
            let side = (SIDE >> m) as usize;
            let plain = vec![u32::from_le_bytes([k as u8 + 1; 4]); side];
            for y in 0..side {
                let pos = f.Offset as usize + y * f.Footprint.RowPitch as usize;
                // SAFETY: the completed readback owns this row of RGBA8 texels.
                let row = unsafe { std::slice::from_raw_parts(data.add(pos).cast::<u32>(), side) };
                if y == 39 || partial && m == 1 && (17..25).contains(&y) {
                    let mut expect = plain.clone();
                    if y == 39 {
                        expect[73] = 0x1234abcd + m as u32;
                    }
                    if partial && m == 1 && (17..25).contains(&y) {
                        expect[31..63].fill(0xabcd1234);
                    }
                    assert!(row == expect, "texture {k} mip {m} row {y}");
                } else {
                    assert!(row == plain, "texture {k} mip {m} row {y}");
                }
            }
        }
    }
}
