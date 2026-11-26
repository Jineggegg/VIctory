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

        // Only the final visible row changes. The enclosing tile row can end
        // partway through a block, and the footprint has a short final row.
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
        assert!(bytes > 0);
        check_readback(host, guest, &texture, &layout, &tiled);

        // A private page strictly inside mip 0 exercises a nonzero
        // destination Y even when its last page was shared with another mip.
        let at = layout.offset(0, m.width / 2, m.height / 2, 1).unwrap() as usize;
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
            assert!((1..u64::from(772 * 516 * 4u32)).contains(&bytes));
        }
        check_readback(host, guest, &texture, &layout, &tiled);
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
