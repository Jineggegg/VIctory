# 纹理并行读回修复验收

尾部 mip 用例在提交锁外提前解平铺，另一张纹理的正常提交复用了它尚未送给 GPU 的上传区域。本次读回失败属于测试干扰。问题由 `bfee212` 新增的尾部 mip 测试引入。

## 归因证据

- 验收版 `d883dde` 使用独立进程运行两个原始测试，指定两个测试线程。此次 RTX 5090 和 WARP 各 5 次通过；第一轮验收记录过硬件 5 次中 4 次失败。随后只在两个用例中临时加入两道屏障：数组用例在首次执行前等待尾部用例取得上传区域，尾部用例等数组首次执行完才继续。硬件与 WARP 各 3 次，全部在尾部 RGBA8 子资源 3、第 0 行失败，数组用例全部通过。[双测日志](logs/texture-parallel/pairs.txt)
- 用两块独立的已提交客体内存进一步追踪字节。先提交 128×128、8 级 mip 的纹理 A，再提交一次干净的 A，让共享上传块从零开始分配。改写尾部第一个字节，录制 A，在执行 A 之前提交整张填有 `a7` 的 256×256 纹理 B。旧测试路径在 A 的子资源 3、第 0 行读回 `a7 a7 a7 a7`，预期为 `c6 39 39 39`。只把 A 的录制方式换成正常的延迟上传，顺序和数据相同，两张纹理逐行校验均通过。[逐次结果](logs/texture-parallel/history.txt)
- 将同一交错用例临时移植到下表四个提交。每个版本、每种写法各跑 RTX 5090 和 WARP 各 3 次，WARP 使用 diagnostics 构建及 `VICTORY_D3D12_WARP=1`。较早版本没有尾部用例，移植只复用了布局解析、提交、读回及交错测试辅助代码。

| 提交 | 版本中的原始尾部用例 | 锁外提前填写 staging | 正常延迟上传 |
|---|---|---|---|
| `2f0d7ea`，`72c2620` 直接父提交 | 无 | 6/6 读到 B | 6/6 通过 |
| `35f1145`，`bfee212` 直接父提交 | 无 | 6/6 读到 B | 6/6 通过 |
| `bfee212` | 首次出现 | 6/6 读到 B | 6/6 通过 |
| `d883dde`，验收副本 | 有 | 6/6 读到 B | 6/6 通过 |

正常录制时，`texture::upload` 只往命令列表登记待上传纹理。进程使用同一个 Host；`Host::execute` 取得提交锁后，`bindless::uploads` 才回收上一批的 staging、并行刷新本批纹理并录制拷贝。工作线程在返回前汇合，每个小纹理从持锁的分配器取得独立区间。正常成功的提交持锁等到 D3D12 fence 完成。原用例直接调用 `refresh_now` 和 `record_upload`，保留的 COM 引用能让缓冲资源存活，却不能阻止另一次提交将该缓冲的偏移归零并重用其内容。[提交与等待](../../crates/victory-gpu/src/host.rs) · [上传批次](../../crates/victory-gpu/src/bindless.rs) · [共享分配器](../../crates/victory-gpu/src/texture.rs)

## 修复与覆盖

本地修复提交为 `eb85408`。尾部 mip、立方体面和深度纹理现在都由正常的录制与提交路径上传。用例对自身纹理启用复制参数记录，提交后校验实际的子资源编号、目标起始行和原生宽高，随后逐行比较每个子资源的 GPU 字节。新增用例 `recorded_tail_update_survives_an_intervening_texture_submission` 固定上述交错并读回两张纹理，无需依赖线程调度，也无需串行化测试套件。[读回测试](../../crates/victory-gpu/src/texture/readback_tests.rs)

正式运行库的上传实现没有代码变化。热点基准以测试二进制运行，在计时范围内的 `record_upload` 新增一次仅在测试构建启用的 `OnceLock::get`。只有尾部读回测试启用记录，常规热点场景不锁定、不复制记录。稀疏上传基准文件另有 4 处 unsafe 安全说明。严谨比较基准读数时应安排正式 A/B。

## Windows 验收结果

| 检查 | 结果 |
|---|---|
| 原两个用例，固定两线程，diagnostics | RTX 5090 连续 20/20、WARP 连续 20/20；每次均执行两个断言完整的用例 |
| 新增确定性交错用例，逐行校验两张纹理 | RTX 5090、WARP 各 3/3 通过 |
| 默认构建，硬件 GPU crate 全量，默认并行度 | 25 次中 11 次全过，最长连续 2 次。全过时 300 通过、3 忽略；14 次失败均出在页保护或回写快照测试，25 次均通过全部 6 个纹理读回用例 |
| diagnostics，GPU crate 全量，默认并行度 | 强制 WARP 316 通过、19 忽略；额外的硬件运行前两次相同，第三次 315 通过、1 失败、19 忽略，失败为页保护的 buffer retake |
| 热点 smoke、合成颜色和深度 | smoke 3/3 均退出 0 并返回 verified=1。原版 run.ps1 的 CheckOnly、SkipValidation 选项按请求完成，颜色和深度校验通过，失败及崩溃计数为 0 |
| Clippy，两组 CI 参数 | 默认、全特性工作区分别有 9、52 条定位到源码的诊断，纹理范围均为 0。单独检查 GPU crate 的全部 targets，两种配置同样没有纹理诊断。工作区余项由另一子线程处理 |
| 仓库资产策略 | Windows 原生 cargo xtask policy 通过，检查 1,428 个文件 |

硬件默认全量的失败集中在 `watch::tests` 的页保护、别名、retake 和 `shadow::tests` 的私有页与快照。单跑 watch 5/5、watch 加 shadow 20/20、watch 加全部纹理读回 12/12 通过。全量跳过全部 6 个纹理读回用例时，15 次中 14 次通过，另 1 次仍在 `shadow::tests::title_write_mid_scan_uses_the_pre_write_snapshot` 失败。第一轮验收的合并父版本也记录过页保护失败。尚未定位这些全量测试的具体干扰源，硬件默认并行连续三次全过的验收项仍未达成。[全量、隔离与静态检查输出](logs/texture-parallel/validation.txt) · [上一轮验收](round1-acceptance.md)

热点冒烟对应源码提交 `eb85408`，源码无差异，3 个样本均有效。像素检查回读首个 RGBA 为 `[1, 0.5, -0.25, 2]`，深度为 `[0.25, 0.5, 0.25, 0.25]`。像素报告的 requested_run_complete 为 true；complete 字段按脚本约定在跳过调试层时为 false。[热点结果](logs/texture-parallel/smoke.json) · [像素结果](logs/texture-parallel/pixels.json)
