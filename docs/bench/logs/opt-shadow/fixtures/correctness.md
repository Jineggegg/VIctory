# 并发与范围定点用例

以下是本次测量使用的源代码差异。

```diff
diff --git a/crates/victory-gpu/src/host.rs b/crates/victory-gpu/src/host.rs
index 1e62c79..4997b86 100644
--- a/crates/victory-gpu/src/host.rs
+++ b/crates/victory-gpu/src/host.rs
@@ -2790,7 +2790,10 @@ pub(crate) fn copy_mapped(dst: *mut u8, src: *const u8, len: usize, title_dst: b
 /// Reads title memory at `src` into `dst`; false if a page of it is not
 /// readable.
 pub(crate) fn read_into(src: *const u8, dst: &mut [u8]) -> bool {
-    read(src, dst.as_mut_ptr(), dst.len())
+    let ok=read(src, dst.as_mut_ptr(), dst.len());
+    #[cfg(test)]
+    if ok { crate::shadow::candidate_contract::after_read(src as usize); }
+    ok
 }

 /// Copies `len` bytes from `src` to `dst` in the kernel: false if a source
diff --git a/crates/victory-gpu/src/shadow.rs b/crates/victory-gpu/src/shadow.rs
index 7aaec0d..86f634b 100644
--- a/crates/victory-gpu/src/shadow.rs
+++ b/crates/victory-gpu/src/shadow.rs
@@ -1752,3 +1752,221 @@ pub(crate) fn bench_mark(first: usize, pages: usize) {
         unsafe { *bits.add(bit / 32) |= 1 << (bit % 32) };
     }
 }
+
+#[cfg(test)]
+pub(crate) mod candidate_contract {
+    use super::*;
+    use std::cell::RefCell;
+    thread_local! {
+        static RACE: RefCell<Option<(usize, Box<dyn FnOnce()>)>> = const { RefCell::new(None) };
+    }
+    pub(crate) fn after_read(src: usize) {
+        let callback = RACE.with(|r| {
+            let mut slot = r.borrow_mut();
+            if slot.as_ref().is_some_and(|(at, _)| *at == src) { slot.take() } else { None }
+        });
+        if let Some((_, f)) = callback { f(); }
+    }
+    fn marked(copy: &Shadow, start: usize, count: usize) {
+        for i in 0..count { copy.copy_back_marked(start + i * PAGE); }
+    }
+    #[test]
+    fn cpu_byte_inside_changed_span_survives() {
+        let h = host().unwrap();
+        let p = crate::watch::tests::pool(1);
+        let (resource, data) = h.shared_buffer(PAGE as u64).unwrap();
+        let copy = Shadow::new(resource, data, 0, p as u64, PAGE as u64);
+        copy.update(p, PAGE);
+        copy.writing(p, PAGE, true);
+        for i in [8, 12] { copy.write_host((p + i) as u64, &[0xa1]); }
+        RACE.with(|r| *r.borrow_mut() = Some((p, Box::new(move || {
+            std::thread::spawn(move || unsafe { ((p + 10) as *mut u8).write_volatile(0xc1) }).join().unwrap();
+        }))));
+        marked(&copy, p, 1);
+        assert!(RACE.with(|r| r.borrow().is_none()));
+        let mut bytes = [0; 5];
+        assert!(crate::host::read_into((p + 8) as *const u8, &mut bytes));
+        drop(copy);
+        crate::watch::tests::free(p);
+        assert_eq!(bytes, [0xa1, 0, 0xc1, 0, 0xa1]);
+    }
+
+    #[test]
+    fn real_owner_in_last_bitmap_word() {
+        let h=host().unwrap();
+        let p=crate::watch::tests::pool(1);
+        let (resource, data)=h.shared_buffer(PAGE as u64).unwrap();
+        let last= victory_shader::emit::VIEW_WRITE_BITS-2;
+        let (old_end, old_free)={
+            let mut registry=marks();
+            (std::mem::replace(&mut registry.end,last),std::mem::take(&mut registry.free))
+        };
+        let copy=Shadow::new(resource,data,0,p as u64,PAGE as u64);
+        assert_eq!(copy.marks,Some(last));
+        copy.update(p,PAGE);
+        copy.writing(p,PAGE,true);
+        copy.write_host(p as u64,&[0xa5]);
+        let map=bitmap(false).unwrap();
+        // Set both the valid owner's bit and the shader overflow sink.
+        unsafe { *((map.data as *mut u32).add(last as usize / 32)) |= 3 << 30; }
+        copy_back_marks(&[],false);
+        // For a lazy implementation, a real CPU read faults the data back.
+        let value=unsafe { (p as *const u8).read_volatile() };
+        drop(copy);
+        {
+            let mut registry=marks();registry.end=old_end;registry.free=old_free;
+        }
+        crate::watch::tests::free(p);
+        assert_eq!(value,0xa5);
+    }
+    #[test]
+    fn overlapping_snapshot_does_not_replay_cpu_bytes_from_a_marked_page() {
+        let Some(h) = host() else { return };
+        let p = crate::watch::tests::pool(1);
+        let (resource, data) = h.shared_buffer(PAGE as u64).unwrap();
+        let copy = Shadow::new(resource, data, 0, p as u64, PAGE as u64);
+        copy.update(p, PAGE);
+        copy.writing(p + 2, 8, false);
+        copy.writing(p, PAGE, true);
+        copy.write_host((p + 2) as u64, &[0xa1]);
+        // SAFETY: this byte is inside the live title allocation.
+        unsafe { ((p + 3) as *mut u8).write_volatile(0xc1) };
+        marked(&copy, p, 1);
+        // The marked pass reconciled 0xc1 into the copy. The snapshot must
+        // not misidentify it as a GPU write after a second CPU store.
+        // SAFETY: the same live byte, guarded again after the marked pass.
+        unsafe { ((p + 3) as *mut u8).write_volatile(0xc2) };
+        copy.copy_back(p + 2, 8);
+        let mut bytes = [0; 2];
+        assert!(crate::host::read_into((p + 2) as *const u8, &mut bytes));
+        assert_eq!(bytes, [0xa1, 0xc2]);
+        drop(copy);
+        crate::watch::tests::free(p);
+    }
+
+    #[test]
+    fn withheld_byte_on_each_side_of_a_boundary_blocks_both_marked_pages() {
+        let Some(h) = host() else { return };
+        let p = crate::watch::tests::pool(4);
+        let len = 4 * PAGE;
+        register(p, p as u64, len as u64);
+        let copy = find(p as u64, len as u64).unwrap();
+        copy.update(p, len);
+        let list = h.new_list().unwrap();
+        {
+            let _writer = list.writer_uses_memory(true);
+            copy.gpu_writes(&list, (p + 2 * PAGE - 1) as u64, 2, true);
+        }
+        let shader = copy.clone();
+        list.defer_host(move || {
+            let map = bitmap(false).unwrap();
+            for i in 0..4 {
+                shader.write_host((p + i * PAGE) as u64, &[0x78]);
+                let bit = shader.marks.unwrap() as usize + i;
+                // SAFETY: a valid bitmap word under the host submission lock.
+                unsafe { *(map.data as *mut u32).add(bit / 32) |= 1 << (bit % 32) };
+            }
+        });
+        list.defer_missed(|| true);
+        list.close();
+        h.execute(&[&list]);
+        for (i, expected) in [0x78, 0, 0, 0x78].into_iter().enumerate() {
+            let mut value = [0];
+            assert!(crate::host::read_into(
+                (p + i * PAGE) as *const u8,
+                &mut value
+            ));
+            assert_eq!(value[0], expected);
+        }
+        drop(list);
+        unregister(p, p as u64);
+        drop(copy);
+        crate::watch::tests::free(p);
+    }
+
+    #[test]
+    fn physical_aliases_merge_cross_page_gpu_and_cpu_stores() {
+        let Some(h) = host() else { return };
+        let (a, b) = crate::watch::tests::twin_views(2);
+        let len = 2 * PAGE;
+        let make = |at| {
+            let (resource, data) = h.shared_buffer(len as u64).unwrap();
+            Shadow::new(resource, data, 0, at as u64, len as u64)
+        };
+        let (one, two) = (make(a), make(b));
+        for (shadow, at) in [(&one, a), (&two, b)] {
+            shadow.update(at, len);
+            shadow.writing(at, len, true);
+        }
+        one.write_host((a + PAGE - 2) as u64, &[1, 2, 3, 4]);
+        two.write_host((b + PAGE + 1) as u64, &[5, 6]);
+        // A title thread writes through the second virtual mapping while the
+        // submitted shadows contain different writes to the same physical pages.
+        // SAFETY: both addresses are bytes of the second live mapped view.
+        std::thread::spawn(move || unsafe {
+            ((b + PAGE - 3) as *mut u8).write_volatile(7);
+            ((b + PAGE + 3) as *mut u8).write_volatile(8);
+        })
+        .join()
+        .unwrap();
+        // Deterministically land another CPU write between the page read and
+        // the GPU stores, inside the gap between two shader writes.
+        one.write_host((a + PAGE + 5) as u64, &[11]);
+        RACE.with(|slot| {
+            *slot.borrow_mut() = Some((a+PAGE, Box::new(move || {
+                // SAFETY: the second view stays mapped until this thread joins.
+                std::thread::spawn(move || unsafe {
+                    ((b + PAGE + 4) as *mut u8).write_volatile(9)
+                })
+                .join()
+                .unwrap();
+            })));
+        });
+        marked(&one, a, 2);
+        marked(&two, b, 2);
+        let mut bytes = [0; 9];
+        assert!(crate::host::read_into(
+            (a + PAGE - 3) as *const u8,
+            &mut bytes
+        ));
+        assert_eq!(bytes, [7, 1, 2, 3, 5, 6, 8, 9, 11]);
+        // A missed program also writes through the second mapping. Its
+        // invalidation must happen after the ordinary copy's mark goes back.
+        one.copy_back(a, len);
+        two.copy_back(b, len);
+        let list = h.new_list().unwrap();
+        for (shadow, at, memory, offset) in [(&one, a, false, 32), (&two, b, true, 33)] {
+            let _writer = list.writer_uses_memory(memory);
+            shadow.refresh(&list, at as u64, PAGE as u64);
+            shadow.gpu_writes(&list, at as u64, PAGE as u64, true);
+            let shadow = shadow.clone();
+            list.defer_host(move || {
+                shadow.write_host((at + offset) as u64, &[0xbb]);
+                let bit = shadow.marks.unwrap() as usize;
+                let bits = bitmap(memory).unwrap().data as *mut u32;
+                // SAFETY: the submission lock excludes another bitmap writer.
+                unsafe { *bits.add(bit / 32) |= 1 << (bit % 32) };
+            });
+        }
+        list.defer_missed(|| true);
+        list.close();
+        h.execute(&[&list]);
+        let mut results = [0; 2];
+        assert!(crate::host::read_into((a + 32) as *const u8, &mut results));
+        assert_eq!(results, [0xbb, 0]);
+        drop(list);
+        drop((one, two));
+        for at in [a, b] {
+            crate::watch::title_memory_remapped(at, len);
+            // SAFETY: the shadows have released these mapped section views.
+            unsafe {
+                windows_sys::Win32::System::Memory::UnmapViewOfFile(
+                    windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
+                        Value: at as *mut _,
+                    },
+                )
+            };
+        }
+    }
+
+}
```
