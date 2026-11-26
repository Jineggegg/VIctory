两臂使用了下面相同的测试源码。保留代码块中的换行即可复现源码增量摘要。

```rust
use super::gfx10;
use crate::pipeline::{self, Prebuild};
use std::{path::Path, time::Instant};

fn pso_phase(root: &Path, hit: bool) {
    pipeline::use_pipeline_cache(root.to_owned());
    let host=crate::host::host().unwrap();
    let mut started=None;
    let mut request=std::time::Duration::ZERO;
    let texels=pipeline::placed_uav_tests::run_store(host, |phase| match phase {
        "shader_us" => started=Some(Instant::now()),
        "pso_ready_us" => request=started.unwrap().elapsed(),
        _ => {}
    }).unwrap();
    assert_eq!(texels, [[1.0,0.5,-0.25,2.0],[0.0;4]]);
    assert_eq!(pipeline::pipeline_counts(),(u64::from(!hit),u64::from(hit)));
    if hit {
        pipeline::library::note_used("synthetic-new-draw");
        let start=Instant::now();
        pipeline::save_pipeline_cache();
        let elapsed=start.elapsed();
        println!("CHILD early_only_save_us={:.3}",elapsed.as_secs_f64()*1e6);
    } else {
        for width in 1..=64 {Prebuild::compute(&gfx10::compute_container(width*128)).make().unwrap();}
        assert_eq!(pipeline::pipeline_counts().0,65);
        pipeline::save_pipeline_cache();
    }
    let label=if hit {"disk_hit"} else {"fresh"};
    println!("CHILD pso_{label}_us={:.3}",request.as_secs_f64()*1e6);
}

#[test]
#[ignore]
fn phase() {
    let root=std::path::PathBuf::from(std::env::var_os("VICTORY_TEST_PHASE_ROOT").unwrap());
    match std::env::var("VICTORY_TEST_PHASE").unwrap().as_str() {
        "pso-miss" => pso_phase(&root,false),
        "pso-hit" => pso_phase(&root,true),
        s => panic!("unknown phase {s}"),
    }
}

fn run_phase(root:&Path,phase:&str,expected:usize) {
    let child=std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact","pipeline::hot::shader_bench::phase","--ignored","--nocapture"])
        .env("VICTORY_TEST_PHASE_ROOT",root).env("VICTORY_TEST_PHASE",phase)
        .output().unwrap();
    assert!(child.status.success(),"child {phase}: {}",String::from_utf8_lossy(&child.stderr));
    let stdout=String::from_utf8(child.stdout).unwrap();
    let written:Vec<_>=stdout.lines().filter_map(|s|s.split_once("CHILD ").map(|(_,v)|v)).collect();
    assert_eq!(written.len(),expected,"child {phase}: {stdout}");
    for value in written {println!("BENCH {value}");}
}

#[test]
#[ignore]
fn disk() {
    let root = tempfile::tempdir().unwrap();
    let phase = std::env::var("VICTORY_BENCH_DISK").unwrap_or_else(|_| "all".to_owned());
    assert!(matches!(phase.as_str(), "all" | "shaders" | "pipelines"));
    assert_eq!(phase, "pipelines");
    if phase != "shaders" {
        let dir = root.path().join("pipelines");
        run_phase(&dir, "pso-miss", 1);
        run_phase(&dir, "pso-hit", 2);
        println!("BENCH prebuild_count=64");
    }
    println!("BENCH verified=1");
}

```
