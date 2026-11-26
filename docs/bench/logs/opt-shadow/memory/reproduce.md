# 复核进程寿命测试

在测试用副本中，将下列忽略用例加入 pipeline/hot/contention.rs，使用诊断版构建。它直接调用两个原版基准函数。1000 轮运行以 256 页为单位，VICTORY_BENCH_THREADS 设为 1。规模检查则以 256、4096、16384 页各起新进程重复；每次只运行一轮。记录资源数之前已释放该轮使用的标题映射、影子副本和命令列表。进程句柄数在创建线程清单快照之前获取。

```rust
/// Repeat the production benchmark's refresh, execute, verification and free.
/// The following overlap dispatch also verifies concurrent CPU writes each round.
#[test]
#[ignore = "run in a fresh process in an ordinary build window"]
fn shadow_memory_lifetime() {
    use windows_sys::Win32::System::{
        ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX},
        Threading::{GetCurrentProcess, GetCurrentProcessId, GetProcessHandleCount},
    };
    #[repr(C)]
    struct ThreadEntry { size: u32, usage: u32, id: u32, owner: u32, priority: i32, delta: i32, flags: u32 }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
        fn Thread32First(h: isize, entry: *mut ThreadEntry) -> i32;
        fn Thread32Next(h: isize, entry: *mut ThreadEntry) -> i32;
        fn CloseHandle(h: isize) -> i32;
    }
    fn record(round: usize, pages: usize) {
        // SAFETY: query this process, using initialized Win32 structures and an owned snapshot.
        unsafe {
            let pid = GetCurrentProcessId();
            let handle = GetCurrentProcess();
            let mut c: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
            c.cb = size_of_val(&c) as u32;
            let cb = c.cb;
            assert_ne!(K32GetProcessMemoryInfo(handle, (&raw mut c).cast::<PROCESS_MEMORY_COUNTERS>(), cb), 0);
            let mut handles = 0;
            assert_ne!(GetProcessHandleCount(handle, &mut handles), 0);
            let s = CreateToolhelp32Snapshot(4, 0);
            assert_ne!(s, -1);
            let mut e: ThreadEntry = std::mem::zeroed();
            e.size = size_of_val(&e) as u32;
            let mut more = Thread32First(s, &raw mut e);
            let mut threads = 0;
            while more != 0 {
                if e.owner == pid { threads += 1; }
                e.size = size_of_val(&e) as u32;
                more = Thread32Next(s, &raw mut e);
            }
            CloseHandle(s);
            assert!(threads > 0);
            println!("MEMORY rounds={round} pages={pages} pid={pid} working={} private={} handles={handles} threads={threads} peak={}", c.WorkingSetSize, c.PrivateUsage, c.PeakWorkingSetSize);
        }
    }
    let host = crate::host::host().unwrap();
    let get = |name: &str, default: usize| std::env::var(name).ok().map_or(default, |s| s.parse().unwrap());
    let pages = get("VICTORY_BENCH_MEMORY_PAGES", 256);
    let rounds = get("VICTORY_BENCH_MEMORY_ROUNDS", 1000);
    record(0, pages);
    for i in 1..=rounds {
        super::writeback(host, pages);
        if rounds > 1 { write_overlap(); }
        if i % 50 == 0 || i == rounds { record(i, pages); }
    }
    println!("BENCH verified=1");
}
```
