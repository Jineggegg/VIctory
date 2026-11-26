# RTX 5090 第一轮整体验收

**验收未通过。** 五项优化合并后，两种后端的工作区测试都在 RGBA8 纹理读回处发现了内容不一致。两轮上游 CI 的 Clippy 检查也失败。按本轮遇到大问题即停的约定，正式 A/B 和浸泡暂缓。

## 测试版本与环境

| 用途 | 本地副本与分支 | 被测提交 |
|---|---|---|
| A，第一轮优化前 | `/mnt/e/Projects/GTA-build/integ-base`，`integ/r1-bench-base` | `6b1e408` |
| B，五项优化合并后 | `/mnt/e/Projects/GTA-build/integ-r1`，`integ/r1-acceptance` | `38d674b`，两父提交为 `73570ff`、`7878982` |

Windows 原生 MSVC 工具链，rustc 1.98.1，Windows 11 10.0.26300。处理器为 Core Ultra 9 285K，系统报告 136,871,063,552 字节物理内存。NVIDIA GeForce RTX 5090 报告显存 32,607 MiB，驱动 621.14。每个副本使用自己的 Cargo target。硬件像素与冒烟结果均记录了合并提交，源代码无工作区改动。[系统及像素记录](logs/round1-acceptance/hardware-pixels/results.json) · [冒烟记录](logs/round1-acceptance/hardware-smoke/results.json)

## 合并版正确性

| Windows 检查 | RTX 5090 | 强制 WARP 环境 |
|---|---:|---:|
| CI 默认 Clippy | 失败，11 条诊断 | 失败，11 条诊断 |
| CI 全特性 Clippy | 失败，58 条诊断 | 失败，58 条诊断 |
| CI 默认工作区测试，已结算通过 / 失败 / 忽略 | 395 / 1 / 8 | 394 / 2 / 8 |
| diagnostics 工作区测试，已结算通过 / 失败 / 忽略 | 409 / 3 / 24 | 410 / 2 / 24 |
| 资产策略 | 通过 | 通过 |
| 合成像素与深度 | 两项断言均通过 | 两个场景的底层测试均通过，原包装入口限制为硬件 |
| 三次热点冒烟 | 3 / 3 通过，每次都校验输出 | 入口拒绝软件适配器，正式冒烟次数为 0 |
| D3D12 Debug Layer 与 GPU-based validation | 待补，系统缺少 Windows Graphics Tools | 待补 |

默认工作区命令保留 CI 的遇错即停行为，计数仅含 Cargo 已结算的顶层测试目标，排除同一用例启动子进程时重复打印的结果。WARP 环境同时设置了 `VICTORY_WARP=1` 与 `VICTORY_D3D12_WARP=1`，其中 GPU 库仅在 diagnostics 构建读取后者。默认工作区这列有部分 GPU 覆盖仍在硬件上。Clippy 和资产策略均为静态检查。[十项原始输出](logs/round1-acceptance/correctness/)

RTX 5090 的计算着色器写出两个 RGBA 值 `[1, 0.5, -0.25, 2]`、`[0, 0, 0, 0]`，深度读回为 `[0.25, 0.5, 0.25, 0.25]`。像素检查进程退出 0，崩溃数为 0。WARP 下直接运行同一像素与深度断言，深度用例记录的适配器为 Microsoft Basic Render Driver。原版 `run.ps1` 会将 GPU 库的 WARP 开关重置为 0，`hot.ps1` 会清除继承的 VIctory 环境变量。因此另用相同诊断二进制直接强制 WARP，两个原始入口都在软件适配器标志处中止。[硬件读回](logs/round1-acceptance/hardware-pixels/results.json) · [WARP 像素](logs/round1-acceptance/triage/B-WARP-placed_uav_dispatch_writes_its_texture.txt) · [WARP 深度](logs/round1-acceptance/triage/B-WARP-depth_bounds_control_a_title_draw.txt) · [WARP 冒烟入口](logs/round1-acceptance/triage/B-WARP-hot_paths.txt)

## CI 阻断与归因

默认 Clippy 在 A 为 0 条，在 B 新出现 11 条。其中 GPU 页标记测试缺少 4 处 unsafe 安全说明，着色器及 PSO 缓存有 3 条，描述符测试有 2 条，纹理读回测试有 2 条。全特性检查在 A 有 45 条、B 有 58 条。按文件、诊断文本和 Git 原始行匹配，39 条两边相同，19 条只在 B 匹配到，6 条只在 A 匹配到。两种配置有重叠，不将 11 和 58 相加。源行归属指向各方向提交；现有证据显示这些静态诊断分散在多个改动中。[逐条对照](logs/round1-acceptance/triage/clippy-attribution.txt) · [A 默认检查](logs/round1-acceptance/triage/A-clippy-default.txt) · [A 全特性检查](logs/round1-acceptance/triage/A-clippy-all.txt)

### 纹理尾部 mip 与并行读回

B 的四次工作区运行都在 `tails_small_mips_cube_faces_and_depth_copy_complete_rows` 的逐行校验处失败。格式均为 RGBA8（DXGI 28），子资源 3、第 0 行。按 Cargo 的精确测试名在新进程内重跑，RTX 5090 和强制 WARP 各 5 / 5 通过。A 没有这一纹理用例。[硬件单项五次](logs/round1-acceptance/triage/B-exact-texture-0.txt) · [WARP 单项五次](logs/round1-acceptance/triage/B-exact-texture-1.txt)

| 硬件诊断对照版本 | GPU crate 并行，通过 / 失败 / 忽略 | 单独运行该纹理用例 | 并行时的结果 |
|---|---:|---:|---|
| `dde121f`，纹理优化已合入 | 286 / 1 / 14 | 1 / 1 通过 | 子资源 3、第 0 行不符 |
| `73570ff`，最后一次合并的第一父提交 | 302 / 3 / 14 | 1 / 1 通过 | 同一行不符，另有两个页跟踪用例失败 |
| `7878982`，最后一次合并的第二父提交 | 296 / 1 / 19 | 1 / 1 通过 | 同一行不符 |

该断言在纹理方向交付时已经存在，最后一次合并前双方都能触发。用例直接调用 `refresh_now` 后，才记录并提交复制命令；生产路径则在 `Host::execute` 的提交锁内准备上传，`bindless::uploads` 会回收暂存池。并行用例是否在这个窗口复用了暂存区，仍需用记录暂存分配与回收顺序的证据确认。当前证据可以判定 CI 的读回失败，尚不足以认定游戏画面也会损坏。[纹理交付版](logs/round1-acceptance/triage/dde121f-gpu.txt) · [第一父提交](logs/round1-acceptance/triage/73570ff-gpu.txt) · [第二父提交](logs/round1-acceptance/triage/7878982-gpu.txt) · [读回测试](../../crates/victory-gpu/src/texture/readback_tests.rs)

在同一份工作区 diagnostics 测试二进制中，将并发缩小到两个用例：尾部 mip 用例单独运行 5 / 5 通过；与数组 mip 反复更新用例并发时，5 次中有 4 次在尾部 mip 用例读回失败。两次为 RGBA8 的子资源 3，另两次为 DXGI 39 的子资源 0，均为第 0 行。和另外三个纹理用例逐一并发，各 5 次共 15 次全部通过。这组实验给出了可复查的测试组合。[同一二进制的 25 次输出](logs/round1-acceptance/triage/B-texture-pairs.txt)

### 页跟踪与基线

B 的并行测试还出现了三种页跟踪失败。硬件诊断的两个 retake 用例分别出现脏页漏报和保护值 4 / 期望 2，WARP 环境默认测试出现其中一个 retake 失败，WARP 诊断则在另一映射的写入处得到空集合。两个 retake 用例单线程复跑，两后端都通过。页跟踪源文件相对 A 没有改动；`73570ff` 独立出现了同类的守卫与取消映射用例失败，具体触发者仍未确定。[硬件并行](logs/round1-acceptance/correctness/hardware-test-diag.txt) · [WARP 并行](logs/round1-acceptance/correctness/warp-test-diag.txt) · [两项单线程](logs/round1-acceptance/triage/B-watch-0.txt)

优化前的 A 在额外运行的硬件默认工作区检查中，继续执行各目标后共 903 通过、2 失败、10 忽略。失败项为帧等待计数和 DirectStorage 页表请求，均与 B 本轮最先报告的纹理读回位置不同。[A 全量输出](logs/round1-acceptance/triage/A-test.txt)

### 串行全工作区与 WARP 特有失败

为检查 CI 遇错停止后尚未执行的组件，B 用 diagnostics、单测试线程和 Cargo 的 `--no-fail-fast` 补跑完整工作区。RTX 5090 为 **971 通过、0 失败、28 忽略**；强制 WARP 为 **968 通过、3 失败、28 忽略**。其中 GPU crate 在两边均为 315 通过、0 失败、19 忽略。页跟踪及纹理用例在两种后端的串行运行中全部通过。[硬件串行全测](logs/round1-acceptance/triage/B-serial-workspace-0.txt) · [WARP 串行全测](logs/round1-acceptance/triage/B-serial-workspace-1.txt)

WARP 的三个失败位于 shader 的合成测试，涉及一项 uniform loop 掩码和两项 lane permute。三项都在创建 32-lane wave 管线时失败。相同三项在 A 上逐名复现。测试内部先用名为 hardware 的分支创建设备，环境强制 WARP 后，这一分支实际也使用软件适配器，而失败处理仍按硬件断言。建议同时记录实际适配器及 wave 能力，再决定硬件与 WARP 的测试覆盖。[A 的 WARP 对照](logs/round1-acceptance/triage/A-WARP-synthetic.txt) · [设备选择代码](../../crates/victory-shader/src/emit/host.rs)

## 第一轮性能与长时稳定性

正式窗口共采集 **0 对**。默认 19 个计时指标和两项内存峰值均缺 A/B 配对变化及其置信区间；小档、中档、大档与 GPU 页标记扩充组也都未采样。1,300 / 14,400 槽描述符的约 3% 减速、256 次提交准备阶段的约 20% 增幅，仍待正式复核。本次无合并版整体加速或退步的估计。

下表保留 B 在 3 次冒烟中的现场读数，便于安排复测。p95 和 p99 均采用基准脚本的最近秩，因此在 3 次样本中均为最大值。首个样本入场时显卡处于 P3，后两次为 P1。不同测试的工作量及计时边界不同，不可相加为帧耗时。显存 MB 沿用场景输出，工作集为字节。[原始三次读数](logs/round1-acceptance/hardware-smoke/results.json)

| B 冒烟指标 | 中位数 | p95 | p99 | 正式配对变化及区间 |
|---|---:|---:|---:|---|
| `pso_cold_us` | 777.8 | 829.6 | 829.6 | 未采样 |
| `pso_hot_us` | 60.3 | 60.9 | 60.9 | 未采样 |
| `prebuild_16_cold_us` | 7,728.8 | 247,261.1 | 247,261.1 | 未采样 |
| `prebuild_16_hot_us` | 114.2 | 135.1 | 135.1 | 未采样 |
| `shader_batch_translate_us` | 3,124.7 | 3,384.7 | 3,384.7 | 未采样 |
| `shader_batch_cold_us` | 12,054.6 | 15,457.3 | 15,457.3 | 未采样 |
| `shader_batch_hot_us` | 785.9 | 4,777.3 | 4,777.3 | 未采样 |
| `descriptors_1300_us` | 836.9 | 844.6 | 844.6 | 未采样 |
| `descriptors_14400_us` | 7,703.6 | 8,229 | 8,229 | 未采样 |
| `submit_32_us` | 4,468.3 | 4,736.1 | 4,736.1 | 未采样 |
| `submit_fence_wait_us` | 1,578.6 | 1,592.4 | 1,592.4 | 未采样 |
| `submit_prepare_us` | 39 | 49 | 49 | 未采样 |
| `submit_queue_and_fence_us` | 2,316 | 2,334 | 2,334 | 未采样 |
| `texture_full_us` | 22,863.2 | 23,476.4 | 23,476.4 | 未采样 |
| `texture_partial_us` | 865.6 | 884.7 | 884.7 | 未采样 |
| `wave_page_marks_gpu_us` | 5,496.992 | 5,907.328 | 5,907.328 | 未采样 |
| `loop_page_lookup_gpu_us` | 4,442.56 | 5,235.584 | 5,235.584 | 未采样 |
| `shadow_refresh_us` | 20,620.2 | 21,312.6 | 21,312.6 | 未采样 |
| `marked_writeback_frame_us` | 41,622.6 | 42,025 | 42,025 | 未采样 |
| `peak_working_set_bytes` | 1,169,367,040 | 1,248,899,072 | 1,248,899,072 | 未采样 |
| `sampled_local_peak_mb` | 232.542 | 232.542 | 232.542 | 未采样 |

A、B 的 30 分钟浸泡均未启动，分钟样本为 0。工作集、显存、句柄、线程的长期增长，延迟漂移、校验失败率与设备移除率都无法判定。此次仅使用普通构建队列，未进入 gpu-quiet 正式窗口，无 tolerated background 进程记录。

## 复测顺序与第二轮重点

1. 先处理并行运行时能读回错误纹理的问题，保留现有逐行校验。对映射别名和页保护用例独立复核。对照默认并行与单线程结果，以原 CI 命令通过为验收条件。
2. 按逐条对照表处理 Clippy，先修优化分支新增项，再清理全特性基线遗留项。将报告和脱敏日志加入暂存区后，Windows 原生资产策略复核通过，共 1,422 个文件。
3. 正确性恢复后，先测 30 对默认场景。在三次硬件冒烟中，标记页回写中位 41.62 ms、完整纹理上传 22.86 ms、副本刷新 20.62 ms。可按这个顺序在合并版做正式剖析，再用小、中、大档确认规模变化；这些合成耗时不能推出整款游戏的优化优先级。
4. 单列描述符两个槽位规模和 256 次提交准备的配对区间；冷 PSO 的 16 项预建在三次冒烟中最大值达 247.26 ms，正式测试需保留首样本和驱动缓存状态。完成正式组后再跑每分钟记录线程数的浸泡，并补装图形工具后复查 D3D12 验证层。
