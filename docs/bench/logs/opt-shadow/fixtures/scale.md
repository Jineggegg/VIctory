# 初筛用的三档测量入口

以下是本次测量使用的源代码差异。

```diff
diff --git a/crates/victory-gpu/src/pipeline/hot.rs b/crates/victory-gpu/src/pipeline/hot.rs
index 93b73f3..99fd71c 100644
--- a/crates/victory-gpu/src/pipeline/hot.rs
+++ b/crates/victory-gpu/src/pipeline/hot.rs
@@ -424,3 +424,20 @@ fn hot_paths() {
     println!("BENCH verified=1");
 }
 static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
+
+#[test]
+#[ignore = "one shadow tier per fresh native Windows process"]
+fn shadow_scale_pair() {
+    let host = crate::host::host().expect("D3D12");
+    let pages = match std::env::var("VICTORY_BENCH_SCALE").as_deref() {
+        Ok("small") => 256,
+        Ok("medium") => 4096,
+        Ok("large") => 16384,
+        _ => panic!("expected small, medium, or large"),
+    };
+    let cache = tempfile::tempdir().unwrap();
+    super::library::use_pipeline_cache(cache.path().to_owned());
+    writeback(host, pages);
+    assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
+    println!("BENCH verified=1");
+}
```
