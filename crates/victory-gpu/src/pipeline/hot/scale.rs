use super::*;

#[test]
#[ignore = "one tier per fresh native Windows process"]
fn gradient() {
    let host = crate::host::host().expect("D3D12");
    let tier = std::env::var("VICTORY_BENCH_SCALE").unwrap_or_else(|_| "small".to_owned());
    let (pages, textures, slots, submits, shader_count) = match tier.as_str() {
        "small" => (256, 1, 100, 1, 20),
        "medium" => (4096, 8, 1300, 32, 200),
        "large" => (16384, 64, 100000, 256, 2000),
        _ => panic!("unknown scale {tier}"),
    };
    let cache = tempfile::tempdir().unwrap();
    super::super::library::use_pipeline_cache(cache.path().to_owned());
    writeback(host, pages);
    submissions(host, submits);
    descriptors(slots);
    if tier == "medium" {
        descriptors(14400);
    }
    textures::run(host, textures);
    shaders(cache.path(), shader_count);
    assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    println!("BENCH verified=1");
}
