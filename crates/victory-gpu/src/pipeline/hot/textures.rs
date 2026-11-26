use super::*;
use std::ops::Range;
type Texture = (usize, This, ID3D12Resource);

struct Patch {
    mip: u32,
    x: Range<u32>,
    y: Range<u32>,
}

pub(super) fn run(host: &Host, count: usize) {
    batch(host, count, false);
}

pub(super) fn batch(host: &Host, count: usize, check_eviction: bool) {
    exercise(host, count, 2048, 4, "texture", false, check_eviction);
}

#[test]
#[ignore = "paired sparse texture workloads with complete GPU readback"]
fn sparse_profiles() {
    let host = crate::host::host().expect("D3D12");
    exercise(host, 1, 4096, 1, "single", false, false);
    exercise(host, 8, 2048, 4, "eight", false, false);
    exercise(host, 64, 2048, 4, "large", true, false);
    // SAFETY: the host owns this live device through the query.
    assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    println!("BENCH verified=1");
}

fn exercise(host: &Host, count: usize, side: u32, mips: u32, prefix: &str, extra_patch: bool, check_eviction: bool) {
    let desc = [3, 0, 0, 0, side, 0, side, 1 | mips << 16, 28, 1, 0, 0, 0, 0];
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
    for k in 0..count {
        // Placed textures cache their address for the lifetime of this sample process.
        let base = pool((layout.size as usize).div_ceil(PAGE));
        // SAFETY: the committed pool has at least layout.size writable bytes.
        unsafe { std::ptr::write_bytes(base as *mut u8, k as u8 + 1, layout.size as usize) };
        for mip in 0..mips {
            let offset = layout.offset(mip, 73, 39, 0).unwrap();
            // SAFETY: layout.offset located a complete RGBA8 pixel in this mip.
            unsafe { ((base + offset as usize) as *mut u32).write_unaligned(0x1234abcd + mip) };
        }
        let guest = crate::resource::texture_at(base as u64, &desc).unwrap();
        let texture = crate::resource::host_resource(guest).unwrap();
        textures.push((base, guest, texture));
    }

    let patch = if mips == 1 {
        // Two small writes span four 32-row tiles of the 4096-square mip.
        vec![Patch { mip: 0, x: 31..63, y: 1024..1025 }, Patch { mip: 0, x: 31..63, y: 1151..1152 }]
    } else {
        vec![Patch { mip: 1, x: 31..63, y: 17..25 }]
    };
    submit(host, &textures, &layout, side, mips, &format!("{prefix}_full"), &[]);
    submit(host, &textures, &layout, side, mips, &format!("{prefix}_partial"), &patch);
    let changed: usize = patch.iter().map(|p| p.x.len() * p.y.len() * 4).sum();
    println!("BENCH {prefix}_title_patch_bytes={}", count * changed);
    if extra_patch {
        // The shipped large gradient writes 64 KiB in total. Also measure
        // 64 KiB per texture using the same mip; this covers the first patch.
        let wide = [Patch { mip: 1, x: 0..128, y: 0..128 }];
        submit(host, &textures, &layout, side, mips, "large_64k_partial", &wide);
        println!("BENCH large_64k_title_patch_bytes={}", count * 65536);
    }
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
        verify(host, &active, side, mips, &patch);
    }
}

fn submit(host: &Host, textures: &[Texture], layout: &victory_formats::texture::Layout, side: u32, mips: u32, name: &str, patches: &[Patch]) {
    let list = host.new_list().unwrap();
    let sums = || textures.iter().fold((0, 0), |(copies, bytes), &(_, guest, _)| {
        (copies + crate::texture::copied(guest), bytes + crate::texture::copied_bytes(guest))
    });
    let (before_copies, before_bytes) = sums();
    for &(base, guest, ref texture) in textures {
        for p in patches {
            for y in p.y.clone() {
                for x in p.x.clone() {
                    let off = layout.offset(p.mip, x, y, 0).unwrap() as usize;
                    // SAFETY: offset names a complete RGBA8 pixel in this allocation.
                    unsafe { ((base + off) as *mut u32).write_unaligned(0xabcd1234) };
                }
            }
        }
        crate::texture::upload(&list, guest, texture);
    }
    list.close();
    let reserved = crate::texture::staging_requested();
    timed(name, || host.execute(&[&list]));
    let requested = crate::texture::staging_requested() - reserved;
    let (copies, bytes) = sums();
    let copies = copies - before_copies;
    let bytes = bytes - before_bytes;
    let count = textures.len() as u64;
    assert_eq!(copies, count * if patches.is_empty() { u64::from(mips) } else { 1 });
    if let Some(first) = patches.first() {
        // All patches in these tracked RGBA8 scenarios target one mip. A
        // full-width band may include the other pixels of its block rows.
        let mip = layout.mips[first.mip as usize];
        let block_height = layout.block[1];
        let first_row = patches.iter().map(|p| p.y.start / block_height).min().unwrap();
        let last_row = patches.iter().map(|p| p.y.end.div_ceil(block_height)).max().unwrap();
        let height = (last_row * block_height).min(mip.height) - first_row * block_height;
        let limit = count * u64::from(mip.width) * u64::from(height) * 4;
        assert!(bytes <= limit, "{name}: copied {bytes} bytes, expected at most {limit}");
    } else {
        assert_eq!(bytes, count * (0..mips).map(|m| 4 * u64::from(side >> m).pow(2)).sum::<u64>());
    }
    assert!(bytes > 0 && requested >= bytes);
    println!("BENCH {name}_bytes={bytes}");
    println!("BENCH {name}_subresources={copies}");
    println!("BENCH {name}_staging_requested_bytes={requested}");
    println!("BENCH {name}_staging_backing_bytes={}", crate::texture::staging_backing());
    verify(host, textures, side, mips, patches);
}

// Actual D3D12 readback of every mip after each upload. Validation runs outside timings.
fn verify(host: &Host, textures: &[Texture], side: u32, mips: u32, patches: &[Patch]) {
    for (k, &(_, guest, ref texture)) in textures.iter().enumerate() {
        let list = host.new_list().unwrap();
        crate::list::ensure_state(&list, guest, D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32);
        let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); mips as usize];
        let mut total = 0;
        // SAFETY: this texture has mips subresources and one output slot per mip.
        unsafe {
            host.device.GetCopyableFootprints(
                &texture.GetDesc(),
                0,
                mips,
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
            let side = (side >> m) as usize;
            let plain = vec![u32::from_le_bytes([k as u8 + 1; 4]); side];
            for y in 0..side {
                let pos = f.Offset as usize + y * f.Footprint.RowPitch as usize;
                // SAFETY: the completed readback owns this row of RGBA8 texels.
                let row = unsafe { std::slice::from_raw_parts(data.add(pos).cast::<u32>(), side) };
                if y == 39 || patches.iter().any(|p| p.mip as usize == m && p.y.contains(&(y as u32))) {
                    let mut expect = plain.clone();
                    if y == 39 {
                        expect[73] = 0x1234abcd + m as u32;
                    }
                    for p in patches.iter().filter(|p| p.mip as usize == m && p.y.contains(&(y as u32))) {
                        expect[p.x.start as usize..p.x.end as usize].fill(0xabcd1234);
                    }
                    assert!(row == expect, "texture {k} mip {m} row {y}");
                } else {
                    assert!(row == plain, "texture {k} mip {m} row {y}");
                }
            }
        }
    }
}
