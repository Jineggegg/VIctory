# 相距较远的两个视图

以下是本次测量使用的源代码差异。

```diff
diff --git a/crates/victory-gpu/src/host.rs b/crates/victory-gpu/src/host.rs
index 1e62c79..3cf3b51 100644
--- a/crates/victory-gpu/src/host.rs
+++ b/crates/victory-gpu/src/host.rs
@@ -4402,3 +4402,40 @@ pub(crate) fn bench_timestamp_frequency() -> u64 {

 #[cfg(all(test, feature = "diagnostics"))]
 pub(crate) static BENCH_FENCE_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
+
+#[cfg(all(test, feature = "diagnostics"))]
+pub(crate) fn bench_sparse_shadow_views() {
+    use crate::watch::PAGE;
+    const LEN: usize = 256 << 20;
+    const VIEW: usize = 1 << 20;
+    let start = crate::watch::tests::pool(LEN/PAGE);
+    unsafe {
+        std::ptr::write_bytes(start as *mut u8, 0x35, LEN);
+        std::ptr::write_bytes(start as *mut u8, 0x51, VIEW);
+        std::ptr::write_bytes((start+LEN-VIEW) as *mut u8, 0xa4, VIEW);
+    }
+    crate::shadow::register(start,start as u64,LEN as u64);
+    let shadow=crate::shadow::find(start as u64,LEN as u64).unwrap();
+    let mut batch=Batch::default();
+    let time=std::time::Instant::now();
+    batch.add(&shadow,start,VIEW);
+    batch.add(&shadow,start+LEN-VIEW,VIEW);
+    let first=batch.flush();
+    let elapsed=time.elapsed();
+    for (offset,expected) in [(0,0x51515151u32),(LEN-VIEW,0xa4a4a4a4)] {
+        assert!(shadow.read_host((start+offset) as u64,VIEW/4).iter().all(|&x|x==expected));
+    }
+    let gap=start+LEN/2;
+    let second=std::time::Instant::now();
+    batch.add(&shadow,gap,PAGE);
+    let later=batch.flush();
+    let after=second.elapsed();
+    assert!(shadow.read_host(gap as u64,PAGE/4).iter().all(|&x|x==0x35353535));
+    println!("BENCH sparse_views_us={:.3}",elapsed.as_secs_f64()*1e6);
+    println!("BENCH sparse_later_us={:.3}",after.as_secs_f64()*1e6);
+    println!("BENCH sparse_total_us={:.3}",(elapsed+after).as_secs_f64()*1e6);
+    println!("BENCH sparse_views_bytes={first}");
+    println!("BENCH sparse_later_bytes={later}");
+    crate::shadow::unregister(start,start as u64);
+    drop(batch);drop(shadow);crate::watch::tests::free(start);
+}
diff --git a/crates/victory-gpu/src/pipeline/hot.rs b/crates/victory-gpu/src/pipeline/hot.rs
index 93b73f3..5f1dbf2 100644
--- a/crates/victory-gpu/src/pipeline/hot.rs
+++ b/crates/victory-gpu/src/pipeline/hot.rs
@@ -424,3 +424,12 @@ fn hot_paths() {
     println!("BENCH verified=1");
 }
 static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
+
+#[test]
+#[ignore = "run in a fresh process with the paired sampler"]
+fn sparse_shadow_pair() {
+    crate::host::bench_sparse_shadow_views();
+    println!("BENCH sparse_requested_bytes=2097152");
+    println!("BENCH sparse_gap_request_bytes=65536");
+    println!("BENCH verified=1");
+}
```
