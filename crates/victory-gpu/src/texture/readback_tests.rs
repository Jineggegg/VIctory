use super::*;
use victory_formats::texture::Layout;

/// A resource can appear in two lists in the same submission. Both CPU
/// writes must reach its one refresh, including a write to the last row.
#[test]
fn array_mips_match_full_detile_after_repeated_updates() {
    let Some(host) = host() else { return };
    for format in [28, 71, 77] {
        // RGBA8, BC1, BC3
        const MIPS: u32 = 3;
        let words = [
            3,
            0,
            0,
            0,
            772,
            0,
            516,
            2 | MIPS << 16,
            format,
            1,
            0,
            0,
            0,
            0,
        ];
        // SAFETY: the test supplies the complete guest descriptor, including padding.
        let desc = unsafe {
            words
                .as_ptr()
                .cast::<crate::resource::ResourceDesc>()
                .read_unaligned()
        };
        let layout = crate::resource::texture_layout(&desc)
            .unwrap()
            .planes
            .remove(0)
            .layout;
        let mut tiled = vec![0u8; layout.size as usize];
        for (i, byte) in tiled.iter_mut().enumerate() {
            *byte = (i ^ (i >> 9) ^ (i >> 16)) as u8;
        }
        let memory = crate::watch::tests::pool(tiled.len().div_ceil(crate::watch::PAGE));
        // SAFETY: the tracked pool owns enough committed pages for the tiled image.
        unsafe { std::ptr::copy_nonoverlapping(tiled.as_ptr(), memory as *mut u8, tiled.len()) };
        let guest = crate::resource::texture_at(memory as u64, &words).unwrap();
        let texture = crate::resource::host_resource(guest).unwrap();
        let initial = host.new_list().unwrap();
        upload(&initial, guest, &texture);
        initial.close();
        host.execute(&[&initial]);
        check_readback(host, guest, &texture, &layout, &tiled);

        let before = uploads(guest);
        let lists = [host.new_list().unwrap(), host.new_list().unwrap()];
        let m = layout.mips[1];
        for (list, (x, y), value) in [
            (&lists[0], (m.width / 3, 0), 0x53),
            (&lists[1], (m.width - 1, m.height - 1), 0xa9),
        ] {
            let at = layout.offset(1, x, y, 1).unwrap() as usize;
            let n = layout.desc.element_bytes as usize;
            tiled[at..at + n].fill(value);
            // SAFETY: this element is inside mip 1 of the second array slice.
            unsafe { (memory as *mut u8).add(at).write_bytes(value, n) };
            upload(list, guest, &texture);
            list.close();
        }
        host.execute(&[&lists[0], &lists[1]]);
        assert_eq!(uploads(guest) - before, 1);
        check_readback(host, guest, &texture, &layout, &tiled);

        // The page containing the last visible row extends outside mip 0.
        // Its full-mip fallback must include the short native final row.
        let before = copied_bytes(guest);
        let at = layout
            .offset(0, layout.mips[0].width - 1, layout.mips[0].height - 1, 1)
            .unwrap() as usize;
        tiled[at] ^= 0xff;
        // SAFETY: this is the last visible element of slice 1, mip 0.
        unsafe { (memory as *mut u8).add(at).write(tiled[at]) };
        let last = host.new_list().unwrap();
        upload(&last, guest, &texture);
        last.close();
        host.execute(&[&last]);
        let bytes = copied_bytes(guest) - before;
        let mip0 = layout.mips[0];
        let full = u64::from(mip0.width) * u64::from(mip0.height) * u64::from(layout.desc.element_bytes);
        assert_eq!(bytes, full);
        check_readback(host, guest, &texture, &layout, &tiled);

        // Update the middle of mip 0 in slice 1. RGBA8 can upload a band
        // here; BC1 and BC3 retain complete native subresource footprints.
        let at = layout.offset(0, mip0.width / 2, mip0.height / 2, 1).unwrap() as usize;
        tiled[at] ^= 0x87;
        // SAFETY: the coordinate is inside the tracked mip.
        unsafe { (memory as *mut u8).add(at).write(tiled[at]) };
        let interior = host.new_list().unwrap();
        upload(&interior, guest, &texture);
        interior.close();
        let before = copied_bytes(guest);
        host.execute(&[&interior]);
        if format == 28 {
            let bytes = copied_bytes(guest) - before;
            let stride = (mip0.pitch / layout.block[0]) as usize * layout.alignment as usize;
            let rows = 1 + (crate::watch::PAGE - 1).div_ceil(stride);
            let limit = rows as u64 * u64::from(layout.block[1]) * u64::from(mip0.width) * 4;
            assert!(limit < full && (1..=limit).contains(&bytes), "band {bytes}, limit {limit}");
        }
        check_readback(host, guest, &texture, &layout, &tiled);
    }
}

fn texture_layout(words: &[u32; 14]) -> Layout {
    // SAFETY: all fourteen words include the guest descriptor and padding.
    let desc = unsafe { words.as_ptr().cast::<crate::resource::ResourceDesc>().read_unaligned() };
    crate::resource::texture_layout(&desc).unwrap().planes.remove(0).layout
}

fn submit_texture(host: &crate::host::Host, guest: This, texture: &ID3D12Resource) {
    let list = host.new_list().unwrap();
    upload(&list, guest, texture);
    list.close();
    host.execute(&[&list]);
}

#[test]
fn permanent_detile_error_consumes_dirty_pages_once() {
    let host = host().expect("D3D12");
    let words = [3, 0, 0, 0, 256, 0, 128, 1 | (1 << 16), 28, 1, 0, 0, 0, 0];
    let layout = texture_layout(&words);
    let pages = (layout.size as usize).div_ceil(crate::watch::PAGE);
    let guest = crate::resource::texture_at(crate::watch::tests::pool(pages) as u64, &words).unwrap();
    // Supply a genuine R8 footprint for an RGBA8 guest row. Both resources
    // are valid; this layout can never fit and no GPU copy may be recorded.
    let mut narrow = words;
    narrow[8] = 61;
    let other = crate::resource::texture_at(crate::watch::tests::pool(pages) as u64, &narrow).unwrap();
    let native = crate::resource::host_resource(other).unwrap();
    let state = staging(guest, &native).unwrap();
    assert!(state.tracked.is_some());
    assert!(!state.band_copies);
    assert!(state.maybe_changed());
    let fp = state.footprints[0].Footprint;
    assert!(fp.RowPitch < 4 * layout.mips[0].width);
    submit_texture(host, guest, &native);
    for _ in 0..3 {
        assert!(!state.maybe_changed());
        let (read, copy) = state.refresh_now();
        assert_eq!(read, 0);
        assert!(copy.is_none());
        submit_texture(host, guest, &native);
    }
    assert_eq!(uploads(guest), 0);
    assert_eq!(state.detile_errors.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn non_xor_msaa_does_not_retry_without_a_write() {
    let host = host().expect("D3D12");
    let words = [3, 0, 0, 0, 256, 0, 128, 1 | (1 << 16), 28, 2, 0, 0x105, 1, 0];
    let layout = texture_layout(&words);
    assert!(!layout.addressable() && !layout.desc.swizzle.xor());
    let pages = (layout.size as usize).div_ceil(crate::watch::PAGE);
    let guest = crate::resource::texture_at(crate::watch::tests::pool(pages) as u64, &words).unwrap();
    let texture = crate::resource::host_resource(guest).unwrap();
    let state = staging(guest, &texture).unwrap();
    assert_ne!(state.size, u64::MAX, "invalid native footprint");
    for _ in 0..4 {
        submit_texture(host, guest, &texture);
        assert!(!state.maybe_changed());
    }
    assert_eq!(uploads(guest), 0);
    assert_eq!(state.detile_errors.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn mixed_texture_detects_changes_to_private_pages_alone() {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    let host = host().expect("D3D12");
    let page = crate::watch::PAGE;
    // A normal private allocation directly followed by a write-watched pool.
    // Keep both mappings alive with the placed texture, as in the array test.
    let memory = (0..16).find_map(|_| unsafe {
        let range = VirtualAlloc(std::ptr::null(), 2 * page, MEM_RESERVE, PAGE_READWRITE);
        if range.is_null() { return None }
        VirtualFree(range, 0, MEM_RELEASE);
        let flags = MEM_RESERVE | MEM_COMMIT;
        let private = VirtualAlloc(range, page, flags, PAGE_READWRITE);
        if private.is_null() { return None }
        let watched = VirtualAlloc(
            (private as usize + page) as *const _, page,
            flags | crate::watch::tests::MEM_WRITE_WATCH, PAGE_READWRITE,
        );
        if watched.is_null() {
            VirtualFree(private, 0, MEM_RELEASE);
            return None;
        }
        Some(private as usize)
    }).expect("two adjacent allocations");
    let words = [3, 0, 0, 0, 256, 0, 128, 1 | (1 << 16), 28, 1, 0, 0, 0, 0];
    let layout = texture_layout(&words);
    let mut tiled = vec![0x37; layout.size as usize];
    assert_eq!(tiled.len(), 2 * page);
    // SAFETY: both fresh mappings are committed and writable.
    unsafe { (memory as *mut u8).write_bytes(0x37, page) };
    unsafe { ((memory + page) as *mut u8).write_bytes(0x37, page) };
    let guest = crate::resource::texture_at(memory as u64, &words).unwrap();
    let texture = crate::resource::host_resource(guest).unwrap();
    let state = staging(guest, &texture).unwrap();
    assert!(state.mixed_memory);
    assert!(!state.band_copies);
    submit_texture(host, guest, &texture);
    check_readback(host, guest, &texture, &layout, &tiled);
    for at in [0, page] {
        let before = copied_bytes(guest);
        tiled[at] ^= 0xff;
        // SAFETY: the first byte of the private or watched allocation.
        unsafe { ((memory + at) as *mut u8).write(tiled[at]) };
        submit_texture(host, guest, &texture);
        assert_eq!(copied_bytes(guest) - before, tiled.len() as u64);
        check_readback(host, guest, &texture, &layout, &tiled);
    }
    let before = uploads(guest);
    submit_texture(host, guest, &texture);
    assert_eq!(uploads(guest), before);
}

#[test]
fn tails_small_mips_cube_faces_and_depth_copy_complete_rows() {
    let host = host().expect("D3D12");
    let rgba = [3, 0, 0, 0, 128, 0, 128, 1 | (8 << 16), 28, 1, 0, 0, 0, 0];
    let mut cube = rgba;
    cube[7] = 6 | (1 << 16);
    cube[11] = 1; // D3D12 cube storage is a six-face square array; linear layout.
    let mut depth = rgba;
    depth[7] = 1 | (1 << 16);
    depth[8] = 40; // D32_FLOAT
    depth[11] = 0x105; // explicit non-XOR tiled layout
    depth[12] = 2; // ALLOW_DEPTH_STENCIL
    for words in [rgba, cube, depth] {
        let layout = texture_layout(&words);
        let memory = crate::watch::tests::pool((layout.size as usize).div_ceil(crate::watch::PAGE));
        let mut tiled = vec![0x39; layout.size as usize];
        // SAFETY: the pool fits the entire guest texture.
        unsafe { (memory as *mut u8).write_bytes(0x39, tiled.len()) };
        let guest = crate::resource::texture_at(memory as u64, &words).unwrap();
        let texture = crate::resource::host_resource(guest).unwrap();
        let state = staging(guest, &texture).unwrap();
        submit_texture(host, guest, &texture);
        check_readback(host, guest, &texture, &layout, &tiled);
        let mips = if words == rgba {
            assert!(layout.first_tail_mip < 6);
            assert_eq!((layout.mips[6].width, layout.mips[7].width), (2, 1));
            vec![layout.first_tail_mip, 6, 7]
        } else {
            assert!(!state.band_copies);
            vec![0]
        };
        for mip in mips {
            let slice = layout.slices - 1;
            let at = layout.offset(mip, 0, 0, slice).unwrap() as usize;
            tiled[at] ^= 0xff;
            // SAFETY: at addresses an element in the selected subresource.
            unsafe { (memory as *mut u8).add(at).write(tiled[at]) };
            let (_, copy) = state.refresh_now();
            let copy = copy.expect("changed subresource");
            assert!(copy.1.iter().any(|p| p.index == slice * layout.desc.mips + mip));
            for part in &copy.1 {
                let native = state.footprints[part.index as usize].Footprint;
                assert_eq!(part.y, 0);
                assert_eq!(part.footprint.Footprint.Height, native.Height);
                assert_eq!(part.footprint.Footprint.Width, native.Width);
            }
            let list = host.new_list().unwrap();
            if let Some(barrier) = record_upload(&list, &crate::resource::host_copy(guest).unwrap(), &state, &copy) {
                // SAFETY: record_upload returned the final transition for this resource.
                unsafe { list.list.ResourceBarrier(&[barrier]) };
            }
            list.close();
            host.execute(&[&list]);
            check_readback(host, guest, &texture, &layout, &tiled);
        }
    }
}

fn check_readback(
    host: &crate::host::Host,
    guest: This,
    texture: &ID3D12Resource,
    layout: &Layout,
    tiled: &[u8],
) {
    let list = host.new_list().unwrap();
    crate::list::ensure_state(&list, guest, D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32);
    let count = layout.desc.mips * layout.slices;
    let mut fps = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut total = 0;
    // SAFETY: one output per subresource of this live texture.
    unsafe {
        host.device.GetCopyableFootprints(
            &texture.GetDesc(),
            0,
            count,
            0,
            Some(fps.as_mut_ptr()),
            None,
            None,
            Some(&mut total),
        )
    };
    let (back, data) = list.readback(host, total).unwrap();
    for (i, &fp) in fps.iter().enumerate() {
        // SAFETY: locations borrow live resources through completion of the copy.
        unsafe {
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&back),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: fp,
                },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(texture),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: i as u32,
                },
            };
            list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        }
    }
    list.close();
    host.execute(&[&list]);
    for (i, fp) in fps.iter().enumerate() {
        let mip = i as u32 % layout.desc.mips;
        let m = layout.mips[mip as usize];
        let pitch = m.width as usize * layout.desc.element_bytes as usize;
        let mut reference = vec![0; pitch * m.height as usize];
        layout
            .detile(
                tiled,
                mip,
                i as u32 / layout.desc.mips,
                &mut reference,
                pitch,
                pitch * m.height as usize,
            )
            .unwrap();
        for (y, row) in reference.chunks_exact(pitch).enumerate() {
            let offset = fp.Offset as usize + y * fp.Footprint.RowPitch as usize;
            // SAFETY: the completed readback owns each visible row, including the short last row.
            let actual = unsafe { std::slice::from_raw_parts(data.add(offset), pitch) };
            assert!(
                actual == row,
                "subresource {i}, row {y}, format {:?}",
                fp.Footprint.Format
            );
        }
    }
}
