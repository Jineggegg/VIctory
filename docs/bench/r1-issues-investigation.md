# 第一轮验收排查：1 MiB 浸泡和 D3D12 验证

**建议暂缓合并原 B。** 1 MiB 浸泡每轮产生 263,164 个分散的 GPU 改写段。B 为每段进入一次 Windows 内核，约 97.5% 的整轮时间耗在一个快照回写函数里。三条 D3D12 错误在 A 上逐条复现，验证仍未通过。

两臂在同一台 RTX 5090、驱动 621.14 上运行，使用 Windows x64 的 `diag` 构建。A 为 `6b1e408`，B 验收二进制为 `54c8ae9`，与此前做正确性测试的 `8cdd575` 之间没有 `crates/` 代码差异。原始验收目录是 `docs/bench/logs/round1-acceptance-3/`。本报告引用其中完整的 `soak-A/`、`soak-B/`、`validation/` 和本次新增的 `validation-A/`。

## 1 MiB 浸泡从第一分钟就慢

每轮 CPU 填充 1 MiB，GPU 对全部 262,144 个 32 位字做 XOR，等待提交完成后逐字校验。每分钟独立计算该分钟轮次的 p50。下表两段五分钟窗口取这五个 p50 的中位数。两臂各有一个持续进程。

| 每轮 p50，ms | A | B |
|---|---:|---:|
| 第 1 分钟 | 1.3893 | 99.5450 |
| 第 3–7 分钟 | 1.3804 | 83.3252 |
| 第 26–30 分钟 | 1.3784 | 99.6093 |
| 30 个分钟值的范围 | 1.3566–1.3898 | 82.0144–99.6658 |
| 前后五分钟窗口的漂移 | −0.145% | +19.543% |

按分钟序号对照，全部 30 分钟 B 都慢，耗时为 A 的 59.3–73.4 倍。前段 60.4 倍是跨臂比较。B 自身还有 19.543% 的窗口差异，超过原测试的 10% 判据。第 1 分钟已接近尾段，时间序列有波动。没有逐分钟的子操作计时，无法把这部分波动单独归因。总轮数 A 为 1,261,226，B 为 19,461。所有分钟累计校验失败和设备移除 HRESULT 均为 0，两臂资源增长标志均为 0。[两臂分钟数据](logs/r1-issues-investigation/minutes.txt)

### 引入提交与调用链

`28de15a` 的直接父提交就是 A。它修复了快照范围重叠时，整页回写会覆盖其他线程在读取后刚写入的字节。原实现按 64 KiB 合并写回。新实现保留这条正确性要求，扫描 GPU 输出与提交前快照的差异，对每个需要发布的连续改写段调用一次 `write_mapped`。正常路径通过 Windows `ReadProcessMemory` 把本进程中的源字节复制到目标地址。[快照回写](../../crates/victory-gpu/src/shadow.rs) · [映射写入](../../crates/victory-gpu/src/host.rs)

浸泡在计时循环外注册一次副本，每轮用 `gpu_writes(..., false)` 声明未标记写入，通过根 UAV 直接 dispatch。每个字的最高字节在 1 MiB 负载中保持原值，使差异被拆成大量短段。此处不会逐块注册追踪器，也没有纹理上传。梯度测试的回写走 `marked=true`：小、中、大档回写 15、240、960 MiB，正序和反序 B 都快约 39%–49%。这些数字覆盖另一条回写路径。[浸泡](../../crates/victory-gpu/src/pipeline/hot/soak.rs) · [梯度回写实现](../../crates/victory-gpu/src/pipeline/hot.rs)

### 单项开关对照

在同一临时二进制、同一 gpu-quiet 窗口中，对 1 MiB 负载按旧、新、新、旧的顺序各启一个新进程，每次 8 秒。只切换快照写入实现。在每轮分别计 CPU 填充、命令录制、同步提交和读回校验，在提交内部另计 `store_snapshot_changes` 的时间与旧写法调用 `write_mapped` 的次数。表中子操作是每轮均值，最后一列是整轮 p50。[含二进制指纹、顺序、窗口及逐次读数的探针日志](logs/r1-issues-investigation/probe.txt)

| 1 MiB 顺序 | CPU 填充 ms | 录制 ms | 同步提交 ms | 其中快照写入 ms | 校验 ms | 旧写法调用数/轮 | 整轮 p50 ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| 旧 1 | 0.202 | 0.510 | 76.609 | 75.471 | 0.082 | 263,164 | 87.295 |
| 新 1 | 0.197 | 0.160 | 1.946 | 1.047 | 0.073 | 0 | 2.318 |
| 新 2 | 0.198 | 0.149 | 1.916 | 1.043 | 0.071 | 0 | 2.304 |
| 旧 2 | 0.199 | 0.501 | 75.501 | 74.384 | 0.077 | 263,164 | 68.400 |

两次旧写法中，快照写入各占四个顶层阶段总时间的 97.5%。计数与 shader 数据形状预测一致。换成相同未标记工作负载再改大小，256 KiB 时每轮 65,791 次调用，整轮 p50 从 14.569 降至 0.926 ms。4 MiB 时每轮 1,052,656 次，从 224.038 降至 7.886 ms。两个尺寸各跑 6 秒，两种写法的读回均通过。页跟踪注册、描述符分页、暂存池和 GPU 调度都无需改动即可重现上述差异。

### 原型效果与适用范围

原型在读到的 title 字节与提交前快照相同时，复用标记页路径已有的逐字节掩码和精确写入函数，每 16 字节计算一份掩码，只写 GPU 实际改变的位置。未变化的字节即使在比较后被 CPU 改写也能保留。目标地址变为不可写时沿用已有的故障回退。若读到的 title 已不同于旧快照，继续沿用逐段比较逻辑，这种并发负载仍可能有碎片写入开销。此改动保留了当初引入该提交所要修复的正确性语义。

原型提交为 `5b094f9`。移除开关和计时探针，重新构建同一 profile，在两个独立的短窗口依次测试原 A、原 B（各 8 秒）和原型（60 秒）。验收二进制逐次校验指纹。第一窗的 gpu-quiet 最后采样无桌面 3D 活动，窗口内总 GPU 峰值为 5%。第二窗的峰值为 3%，记录到 Chrome 3D 2%，又额外等到连续十个总 GPU 读数为 0%–1% 才开始。

| 1 MiB 无探针复测 | A p50 ms | 原 B p50 ms | 原型 p50 ms | 原型 60 秒轮数 |
|---|---:|---:|---:|---:|
| 第一窗 | 1.2564 | 98.4996 | 2.1590 | 27,394 |
| 第二窗 | 1.2637 | 95.2759 | 2.2470 | 26,036 |

第二窗原型 p95 为 2.632 ms、p99 为 2.956 ms。p50 比同窗原 B 降低 97.6%，仍比同窗 A 高 0.983 ms（77.8%）。两窗所有轮次校验失败和设备移除均为 0。每窗随后运行的 24 项 shadow 测试全部通过，覆盖比较后发生的 CPU 写入、快照重叠、缺页重试和页边界。上述结果验证了主要回归已消除，原型的长期漂移仍待正式 30 分钟复验。[第一窗完整输出](logs/r1-issues-investigation/final-first.txt) · [第二窗完整输出](logs/r1-issues-investigation/final-strict.txt)

## D3D12 的三条消息在 A、B 逐条一致

用验收脚本的 `validation-A` 阶段运行 A，核验二进制 SHA-256。与 B 一样分别运行普通像素检查和开启 Debug Layer、GPU-based validation 的场景。A 窗口空闲检查最高 2%。B 原始窗口为 0%。错误回调只打印消息，以下两个 1419 的具体绘制位置按测试调用顺序与根签名缓存推定。[两臂逐条诊断、读回、进程状态与指纹](logs/r1-issues-investigation/validation.txt)

| 顺序 / 消息 ID | 对象与调用位置 | A / B 结果 |
|---|---|---|
| 1 / 645 | 计算场景创建 shader 时首次查询分页模式。`descriptor/host.rs` 第 197 行的 `view_pages` 调用 `ID3D12Device::CreateDescriptorHeap` 探测 shader-visible CBV/SRV/UAV 堆，申请 2,359,296 个描述符，该设备的验证层报告上限为 1,000,000。 | 各 1 次 |
| 2 / 1419 | 深度场景新建的图形命令列表首次绘制像素 0（`depth_bounds_test.rs:236`）。`draw.rs` 第 639 行的 `State::bind` 调用 `SetGraphicsRootSignature`，签名声明直接索引 CBV/SRV/UAV 堆，但列表只具备备用采样器堆。 | 各 1 次 |
| 3 / 1419 | 同一命令列表 Reset 后首次绘制像素 3（`depth_bounds_test.rs:246`），根签名缓存重置，同一调用再次缺少直接索引的 CBV/SRV/UAV 堆。 | 各 1 次 |

645 的探测来自 A 之前的 `8ab5679`，申请失败后选择分页。对此场景的直接影响是验证失败，颜色读回正确。建议先通过 `D3D12_OPTIONS19.MaxViewDescriptorHeapSize` 判断容量，旧运行时采用保守分页路径。1419 涉及根签名的标志与实际绑定不一致。无条件直接索引标志可追溯至 `2849477`，A 中选择备用 view heap 的条件来自 `d71c388`。本场景使用无资源的深度着色器，两臂的深度读回均为 `[0.25, 0.5, 0.25, 0.25]`。实际直接索引时可能产生错误绘制或设备移除。建议让生成的签名仅声明实际所需的堆，或在设置该签名前保证所声明的堆均已绑定，并覆盖首次录制和 Reset 后的绘制。[探测](../../crates/victory-gpu/src/descriptor/host.rs) · [签名标志](../../crates/victory-gpu/src/pipeline/host.rs) · [堆绑定](../../crates/victory-gpu/src/draw.rs) · [深度测试顺序](../../crates/victory-gpu/src/pipeline/depth_bounds_test.rs)

两臂普通检查都以 0 退出。验证场景都拿到了预期颜色和深度，并随后因 `baseline.rs` 断言错误数必须为零，以 Rust 测试失败码 101 退出。两份报告均记为 1 次失败、0 次进程崩溃。三条均为基线已有异常，本次没有针对验证层修改代码。现有记录不能把这两种违规判为在所有驱动上安全。

## 合并判断

原 B 有可重复的严重性能回归，暂缓合并。带原型的候选已能进入下一轮正式验收。短窗足以验证主要耗时的来源，原有 30 分钟时长才覆盖前后五分钟漂移判据。主线程若继续要求验证层零错误，还须处理上述两项基线问题，再决定合并。

## 1 MiB 回写二次优化

**最终版 `feb14ed` 达到 1 MiB 短测目标。** 两个相反顺序的窗口中，p50 为 1.4051 和 1.4027 ms，同窗 A 为 1.4678 和 1.4155 ms。p99 相对 A 分别为 −1.3% 和 +1.6%。默认 19 项的两个复测窗口均未检出通过多重比较校正的退步；中档和大档标记回写较起点 B 分别快 14.9%、13.3%。

### 同窗剖析

原 A 是 `6b1e408`，本节的起点 B 是 `b5c06a7`。先核对两份验收二进制的 SHA-256，在同一 RTX 5090 的 gpu-quiet 窗口内各跑 60 秒。无探针时，A 的 p50 / p95 / p99 为 **1.2879 / 1.4873 / 1.6803 ms**，B 为 **2.2294 / 2.5347 / 2.8395 ms**。同窗 p50 相差 0.9415 ms。此前正式 30 分钟的 0.8518 ms 差距使用各分钟 p50 的中位数。每个短测进程都重新注册一份副本，使用同一个 1 MiB XOR 负载并逐字验算。[原始同窗短测](logs/r1-issues-investigation/optimize-1mib/baseline.txt)

为分别测量 A、B，在本工作目录的临时副本给旧的合并及内核写入、当前掩码和写入、快照读取、页跟踪和锁加上 `Instant` 计时。在另一个窗口用相同诊断构建各跑 60 秒。下表是每轮累计时间的**均值**，单位为微秒。A 的合并同时更新 title 临时缓冲和 host 副本，B 在重新保护页面后另行读回。[逐项读数及二进制指纹](logs/r1-issues-investigation/optimize-1mib/profile.txt)

| 每轮阶段，µs | A | B | B − A |
|---|---:|---:|---:|
| CPU 填充 | 205.56 | 219.15 | +13.59 |
| 命令录制 | 151.07 | 184.97 | +33.90 |
| 提交前保存 GPU 副本 | 38.21 | 49.42 | +11.21 |
| 读取 title 快照 | 71.15 | 91.80 | +20.65 |
| 检查 title 与提交前快照相同 | 0 | 57.57 | +57.57 |
| A 的字节合并 / B 的掩码生成 | 102.23 | 66.51 | −35.72 |
| 打开写保护及退出写入区间 | 29.44 | 33.28 | +3.84 |
| 发布到 title（A 的整块内核写 / B 的混合块写） | 155.64 | 935.70 | **+780.06** |
| 回写后重新保护 | 17.25 | 20.04 | +2.79 |
| 重新保护后同步回 GPU 副本 | 0 | 92.21 | +92.21 |
| 取得本次回写的快照锁 | 0.05 | 0.05 | <0.01 |
| 提交内其余工作，含 GPU 执行、等待和探针开销 | 703.11 | 762.88 | +59.77 |
| 完成后逐字校验 | 51.87 | 57.46 | +5.59 |
| **以上合计** | **1525.58** | **2571.04** | **+1045.46** |

B 每轮恰有 65,536 个 16 字节掩码，全部是混合块。空块、整向量写和用户态写失败后的内核回退各为零。页跟踪注册发生在计时循环之前，每轮零次。另测全局页保护锁和提交队列锁的取得时间，A 为 1.64 和 0.04 µs，B 为 1.72 和 0.08 µs。这两项已包含在上表各阶段。单独给 B 装探针的第一窗也测得混合块写 923.53 µs。桌面占用放行阈值为 3%，每臂开跑前另采样，超限重新等待；窗口日志保留实测占用。短测的绝对延迟随窗口变化，后文按同窗比较。

### 降低未标记快照回写耗时的两项修改

1. `dd4364d` 把“找最低置位字节”和“清掉这个位”解开。清位采用 `mask & (mask - 1)`，下一次位扫描可以在当前字节写入完成前开始。仍只在原来的指令位置写一个字节，故障时仍由原来的索引定位重试起点。新增测试穷举全部 65,536 个向量掩码，覆盖非对齐地址、1–15 字节的尾部、段外哨兵，以及刚好碰到下一页 PAGE_GUARD 时的剩余长度。[第一项修改的回归测试](logs/r1-issues-investigation/optimize-1mib/scalar-shadow-tests.txt)
2. `f907845` 先按 64 KiB 找出 GPU 确实改过且本次提交尚未处理的片段。至少有 16 片、并且 Windows 的无重置 `GetWriteWatch` 确认整个范围是私有写跟踪分配时，才交给已有的进程线程池。1 MiB 全改时分成四组，每组四片。更大范围至多分 16 组，沿用线程池同时最多 8 个执行者的上限。每组独立读取 title、生成掩码、精确写入；所有组结束后，调用线程按地址顺序登记成功的完整页，重新保护，再读入 host 副本。section 映射、混合映射和改写片数少的范围继续按地址顺序发布。[第二项修改的回归测试](logs/r1-issues-investigation/optimize-1mib/private-shadow-tests.txt)

18 页的新增回归同时覆盖被裁剪的首尾、读取后发生在 GPU 写入间隙的 CPU 改写，以及其中一页的完整读取失败。后续较小的声明能补回失败页。测试还核对所有执行线程合计的内核写入次数与 host 副本中的 CPU 字节；已有的 1 MiB 碎片用例继续要求内核目标写入为零。审核时应重点看私有分配的判定、组与组的地址不相交、只登记成功读取的整页，以及所有写入完成后才重新保护和同步的顺序。

试过的其他写法包括 SSE2 掩码写、按四位或八位掩码选用精确的宽写、重复字节位置的专用循环，以及用 AVX2 同时比较两份快照并生成掩码。它们都通过短浸泡的逐字校验。八位分组加 AVX2 的一窗 p50 为 1.5494 ms，对应 A 为 1.5021 ms；另一窗四位分组加 AVX2 为 1.5711 ms，对应 A 为 1.3480 ms。在后一个窗口，用原有精确字节写并行处理私有页的原型为 1.2682 ms，p99 为 1.7063 ms，A 为 1.7248 ms。最终实现沿用原来的故障指令位置。[掩码写与分组尝试](logs/r1-issues-investigation/optimize-1mib/first-variants.txt) · [标量与重复模式](logs/r1-issues-investigation/optimize-1mib/scalar-variants.txt) · [八位分组和 AVX2](logs/r1-issues-investigation/optimize-1mib/eight-byte-plans.txt) · [同窗候选对比](logs/r1-issues-investigation/optimize-1mib/shortlist.txt)

### 前两次提交的 1 MiB 结果

同一窗口依次测起点 B、第一项修改、第二项修改、原 A，每个二进制运行 60 秒。另开一窗先测 A 再测分组版。使用 Windows 原生 `diag` profile、`diagnostics` 特性和仓库的 `pipeline::hot::soak::mixed`，两份提交版二进制均已移除剖析探针。[四臂同窗原始记录](logs/r1-issues-investigation/optimize-1mib/paired.txt) · [反序复测](logs/r1-issues-investigation/optimize-1mib/paired-reverse.txt)

| 窗口与顺序 | 版本 | 轮数 | p50，ms | p95，ms | p99，ms |
|---|---|---:|---:|---:|---:|
| 第一窗 1 | 起点 B，`b5c06a7` | 24,721 | 2.3641 | 2.6774 | 3.0819 |
| 第一窗 2 | 字节清位，`dd4364d` | 32,558 | 1.8086 | 2.0746 | 2.3273 |
| 第一窗 3 | 私有页分组，`f907845` | 41,115 | **1.4221** | 1.6921 | **1.8998** |
| 第一窗 4 | 原 A，`6b1e408` | 39,114 | 1.4781 | 1.7922 | 2.1960 |
| 第二窗 1 | 原 A，`6b1e408` | 40,815 | 1.4197 | 1.6679 | 2.0704 |
| 第二窗 2 | 分组版，`f907845` | 40,608 | **1.4308** | 1.6904 | **2.1460** |

第一项修改降低 p50 0.5555 ms，第二项再降 0.3865 ms，合计较同窗 B 减少 39.8%。两窗分组版 p50 分别为 A 的 **96.2% 和 100.8%**，都在 110% 目标以内。p99 相对 A 分别为 −13.5% 和 +3.7%。全部六臂每轮校验都通过，设备移除计数为零。保留了回写前比较与重新保护后的副本同步，分组让独立私有页的快照读取、比较和写入可以同时进行。反序窗还剩 0.0111 ms 的 p50 差异；两窗的差异方向相反，无法据此确定一个固定下限。30 分钟漂移和资源增长留待正式浸泡。

### 分组版的默认热点与三档规模

旧 B 与分组版各用独立原生进程运行仓库的 `hot_paths` 和 `scale::gradient`。每档先按 B/分组版、分组版/B、B/分组版跑三对，每次核对该场景的输出。为复核桌面负载和显卡频率的影响，又让两臂共用旧 B 的 `warm_adapter` 预热程序，每次从桌面 GPU 占用 ≤3% 放行，频率及 PCIe 链路按仓库 `Warm-Ready` 门槛连续稳定至少三秒，再开始计时。预热窗逐对交换两臂顺序。下列“配对变化”取同一对分组版/B 减一的中位数；“中位数变化”比较两臂各自的中位数。正数表示分组版用时较长。[三对逐次读数](logs/r1-issues-investigation/optimize-1mib/scenario-metrics.txt) · [预热后逐次读数](logs/r1-issues-investigation/optimize-1mib/warm-metrics.txt)

| 默认热点 | 三对中位数变化 | 预热 B 中位，µs | 预热分组版中位，µs | 预热中位数变化 | 预热配对变化 | 分组版较慢对数 |
|---|---:|---:|---:|---:|---:|---:|
| `descriptors_1300` | +0.9% | 815.0 | 807.7 | −0.9% | −1.8% | 5/12 |
| `descriptors_14400` | +9.0% | 7243.4 | 7348.8 | +1.5% | +2.2% | 10/12 |
| `loop_page_lookup_gpu` | +10.4% | 4368.1 | 4478.6 | +2.5% | +16.2% | 7/12 |
| `marked_writeback_frame` | +3.8% | 42902.8 | 41312.9 | −3.7% | −3.4% | 4/12 |
| `prebuild_16_cold` | −1.5% | 8138.6 | 8310.3 | +2.1% | +1.0% | 6/12 |
| `prebuild_16_hot` | +11.3% | 107.0 | 107.2 | +0.1% | +0.1% | 6/12 |
| `pso_cold` | +16.6% | 801.9 | 789.4 | −1.6% | −0.6% | 5/12 |
| `pso_hot` | +2.5% | 58.0 | 61.9 | +6.6% | +5.2% | 9/12 |
| `shader_batch_cold` | +7.6% | 12421.9 | 12638.6 | +1.7% | −0.0% | 6/12 |
| `shader_batch_hot` | +8.7% | 857.0 | 833.0 | −2.8% | −5.9% | 5/12 |
| `shader_batch_translate` | −1.5% | 3167.9 | 3245.9 | +2.5% | +4.6% | 9/12 |
| `shadow_refresh` | +4.4% | 20313.7 | 18623.5 | −8.3% | −4.0% | 2/12 |
| `submit_32` | −7.2% | 4661.6 | 4447.1 | −4.6% | −3.5% | 4/12 |
| `submit_fence_wait` | +9.0% | 1619.4 | 1543.9 | −4.7% | +6.6% | 7/12 |
| `submit_prepare` | +12.8% | 43.5 | 39.0 | −10.3% | −12.8% | 4/12 |
| `submit_queue_and_fence` | −7.9% | 2469.0 | 2286.5 | −7.4% | −2.7% | 5/12 |
| `texture_full` | +29.4% | 23881.4 | 24659.2 | +3.3% | +3.4% | 7/12 |
| `texture_partial` | +9.6% | 843.1 | 863.6 | +2.4% | +3.1% | 8/12 |
| `wave_page_marks_gpu` | +9.0% | 5077.2 | 5154.2 | +1.5% | −4.0% | 5/12 |

三对冒烟中，B 的完整纹理上传从 21.6 到 127.9 ms，两个版本的中位数相差 29.4%。稳定频率后的 12 对中，这项配对差异为 +3.4%，逐项 95% 区间为 −20.6% 至 +10.0%；默认标记回写为 −3.4%，副本刷新为 −4.0%。GPU 页查询的配对点估计为 +16.2%，两臂各自的中位数相差 +2.5%，逐项区间为 −18.1% 至 +34.3%。14,400 个描述符的同步有 10/12 对较慢，配对 +2.2%，逐项区间为 +0.5% 至 +4.6%；19 项同时比较的校正区间为 −0.4% 至 +4.9%。对这 19 项按整对重抽 5,000 次，种子 5090，Bonferroni 校正后的 95% 区间全部跨零。[19 项原始日志](logs/r1-issues-investigation/optimize-1mib/smoke-warm.txt) · [配对区间](logs/r1-issues-investigation/optimize-1mib/warm-paired.txt)

三档规模分别处理 256、4,096、16,384 个 64 KiB 页。默认项和中档的标记场景都包含 4,096 页。表中单位为 ms，预热小档和大档各八对，中档十二对。[小档](logs/r1-issues-investigation/optimize-1mib/scale-small-warm.txt) · [中档](logs/r1-issues-investigation/optimize-1mib/scale-medium-warm.txt) · [大档](logs/r1-issues-investigation/optimize-1mib/scale-large-warm.txt)

| 规模 | 指标 | 三对 B 中位 | 三对分组版中位 | 三对中位数变化 | 预热 B 中位 | 预热分组版中位 | 预热配对变化 | 分组版较慢对数 |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| small | `marked_writeback_frame` | 3.269 | 3.482 | +6.5% | 3.525 | 3.668 | +4.8% | 7/8 |
| small | `shadow_refresh` | 1.449 | 1.391 | −4.0% | 1.415 | 1.481 | +3.6% | 6/8 |
| medium | `marked_writeback_frame` | 41.824 | 40.694 | −2.7% | 39.254 | 39.448 | +1.3% | 10/12 |
| medium | `shadow_refresh` | 18.931 | 18.862 | −0.4% | 16.388 | 16.877 | +3.8% | 9/12 |
| large | `marked_writeback_frame` | 163.476 | 171.913 | +5.2% | 165.396 | 164.996 | +0.3% | 5/8 |
| large | `shadow_refresh` | 64.166 | 77.866 | +21.4% | 69.115 | 66.709 | +0.6% | 4/8 |

大档副本刷新的三对读数一度相差 21.4%，预热后两版各快四对，配对差异为 +0.6%。中档标记回写为 +1.3%，同时比较六项规模指标的区间为 −0.01% 至 +4.35%。小档标记回写首轮预热配对为 +4.8%，单独交换起跑顺序再跑十二对，配对为 +2.9%，逐项区间为 −2.7% 至 +6.3%；小档刷新在反序窗为 +3.9%，区间为 −4.6% 至 +7.0%。中档额外复核的 14,400 描述符同步为 7.1131 → 7.3033 ms，9/12 对较慢，配对 +2.3%，逐项区间为 −0.2% 至 +5.7%。此项计时只包围描述符堆同步，排在本轮快照回写之前。[小档反序原始记录](logs/r1-issues-investigation/optimize-1mib/scale-small-reverse.txt) · [中档逐次日志](logs/r1-issues-investigation/optimize-1mib/scale-medium-warm.txt)

把两臂依次复制到同一目录、同一文件名，逐次重验 SHA-256 后重跑 12 对。默认项的 14,400 描述符同步为 7.0406 → 6.9471 ms，配对 −1.0%，5/12 对较慢，区间 −2.7% 至 +0.9%；前两窗的升幅没有在这个控制条件下重现。完整纹理上传配对 −3.5%，GPU 页查询 −2.2%。第五对切换程序时 Windows 一度占用目标文件，保留出错日志，排除该未配完的单臂，从第五对起重跑。[默认项同路径第一次](logs/r1-issues-investigation/optimize-1mib/controlled-default-initial.txt) · [默认项同路径续测](logs/r1-issues-investigation/optimize-1mib/controlled-default-resume.txt)

小档标记回写在同路径窗为 3.5773 → 3.6848 ms，配对 +3.0%，7/12 对较慢，区间 −7.2% 至 +9.0%。三个预热窗合计 32 对，22 对较慢；按窗口分层重抽，配对中位数 +3.7%，区间 +1.5% 至 +5.5%。另将两个提交中的掩码写汇编一同编进同一 Windows 程序，从同一线程向相同对齐的 16 MiB 缓冲区逐页写入小档的实际字节图案。共 981,760 个全一掩码、66,816 个混合掩码，每轮逐字节验算。除去头两对预热，20 对中汇编单独的配对变化为 +1.5%，区间 −1.7% 至 +4.9%。小档的整帧差异还包含读取 title、生成掩码、建立写保护和同步副本的时间。[小档同路径与汇编隔离读数](logs/r1-issues-investigation/optimize-1mib/controlled-small.txt)

### 标记页读取及最终同窗结果

`feb14ed` 将标记回写读 title 的缓冲区由 8 KiB 扩至至多 32 KiB，放在堆上，每个标记 run 分配一次并复用。完整的 64 KiB 页从八次内核读取降至两次；只要其中一次读取失败，整页在发布前跳过。新增测试让第二个 32 KiB 读取失败，核对边界前后及段外哨兵均保留原值，再验证成功重读能发布相邻两字节。已有的读到一半时 CPU 改写用例继续通过。定向测试共 32 项。[标记回写边界测试](logs/r1-issues-investigation/optimize-1mib/mark-tests.txt)

小档先跑六组三臂，将 B、分组版、扩大读取缓冲区的版本六种次序各用一次。标记回写的中位数分别为 3.4278、3.5881、3.5108 ms。第三项相对分组版五组更快，配对中位数 −1.4%；相对 B 为 +4.4%。默认 4,096 页场景的 12 对里，最终版标记回写每对均快于 B，配对 −14.0%。同一显卡和 `diag` 构建下复测 1 MiB，每臂 60 秒；每个窗口均由 gpu-quiet 验证空闲，每臂另测桌面 GPU 占用 ≤3% 再放行。[三臂小档](logs/r1-issues-investigation/optimize-1mib/mark-pilot.txt) · [1 MiB 四臂同窗](logs/r1-issues-investigation/optimize-1mib/mark-qualifying.txt)

| 窗口 / 顺序 | 版本 | 轮数 | p50，ms | p95，ms | p99，ms |
|---|---|---:|---:|---:|---:|
| 1 / 1 | 分组版 `f907845` | 42,016 | 1.3895 | 1.6403 | 1.8407 |
| 1 / 2 | 最终版 `feb14ed` | 41,542 | 1.4051 | 1.6648 | 1.8509 |
| 1 / 3 | 原 A `6b1e408` | 39,959 | 1.4678 | 1.6968 | 1.8762 |
| 1 / 4 | 起点 B `b5c06a7` | 24,624 | 2.3940 | 2.6835 | 2.9861 |
| 2 / 1 | 原 A `6b1e408` | 41,449 | 1.4155 | 1.6413 | 1.8359 |
| 2 / 2 | 最终版 `feb14ed` | 41,554 | 1.4027 | 1.6793 | 1.8655 |

最终版两窗 p50 分别是 A 的 **95.7% 和 99.1%**，p99 分别比 A 低 1.3% 和高 1.6%。对同窗 B，最终版 p50 少了 **0.9889 ms（41.3%）**。两次最终版一共 83,096 轮，逐轮校验失败和设备移除均为零。短窗中没有测得原先约 0.85 ms 的固定差距。[先 A 后最终版的反序记录](logs/r1-issues-investigation/optimize-1mib/mark-reverse.txt)

最终版在相同可执行文件路径再测小档十二对，以同窗旧 B 为参照。中、大档各六对。每个进程完成回写字节、保留区间、刷新的页和设备状态校验。下表单位为 ms，变化和区间按对计算。[最终版逐次读数](logs/r1-issues-investigation/optimize-1mib/mark-scenario-metrics.txt) · [逐项和多重比较区间](logs/r1-issues-investigation/optimize-1mib/mark-paired.txt)

| 规模 | 指标 | B 中位，ms | 最终版中位，ms | 配对变化 | 逐项 95% 区间 | 最终版较慢对数 |
|---|---|---:|---:|---:|---:|---:|
| small | `marked_writeback_frame` | 3.7753 | 3.4061 | −5.1% | −14.3% 至 +1.2% | 3/12 |
| small | `shadow_refresh` | 1.4399 | 1.5154 | +6.9% | −1.3% 至 +17.7% | 9/12 |
| medium | `marked_writeback_frame` | 40.0876 | 34.2578 | −14.9% | −20.9% 至 −10.4% | 0/6 |
| medium | `shadow_refresh` | 18.8481 | 18.7915 | −1.0% | −2.8% 至 +1.2% | 2/6 |
| large | `marked_writeback_frame` | 162.0910 | 141.2673 | −13.3% | −14.2% 至 −10.6% | 0/6 |
| large | `shadow_refresh` | 63.0903 | 63.2271 | +2.4% | −2.9% 至 +13.6% | 3/6 |

第三项修改后的标记回写，配对变化为小档 −5.1%、中档 −14.9%、大档 −13.3%，中、大档每对均较快。小档刷新点估计为 +6.9%，另一次六组三臂中为 −2.3%；中档为 −1.0%，大档为 +2.4%，这些刷新区间均跨零。

最终版的默认 19 项另做两个预热窗口，每窗 12 对。第二窗把两臂依次放进相同路径并重新核对哈希。每项均通过仓库的工作量和读回断言。[独立路径原始记录](logs/r1-issues-investigation/optimize-1mib/mark-default.txt) · [相同路径原始记录](logs/r1-issues-investigation/optimize-1mib/mark-common-default.txt)

| 默认指标 | 第一窗配对变化 | 同路径 B 中位，ms | 同路径最终版中位，ms | 同路径配对变化 | 同路径逐项 95% 区间 | 最终版较慢对数 |
|---|---:|---:|---:|---:|---:|---:|
| `descriptors_1300` | +3.2% | 0.8101 | 0.8369 | +1.1% | −1.2% 至 +4.2% | 7/12 |
| `descriptors_14400` | +5.4% | 7.1586 | 7.3045 | +1.6% | −2.0% 至 +7.2% | 8/12 |
| `loop_page_lookup_gpu` | −12.8% | 3.8749 | 4.7175 | −0.6% | −23.3% 至 +52.2% | 6/12 |
| `marked_writeback_frame` | −14.0% | 39.4493 | 33.3203 | −16.0% | −16.9% 至 −12.9% | 0/12 |
| `prebuild_16_cold` | −12.3% | 6.7556 | 6.8974 | +3.0% | −6.7% 至 +6.9% | 7/12 |
| `prebuild_16_hot` | −1.8% | 0.1078 | 0.1057 | −1.0% | −5.8% 至 +2.3% | 5/12 |
| `pso_cold` | −6.3% | 0.7815 | 0.8336 | +6.0% | +0.3% 至 +18.8% | 10/12 |
| `pso_hot` | +1.8% | 0.0537 | 0.0579 | +5.1% | +0.6% 至 +14.4% | 9/12 |
| `shader_batch_cold` | +1.3% | 12.6345 | 12.8582 | +1.6% | −1.8% 至 +5.8% | 6/12 |
| `shader_batch_hot` | −12.0% | 0.8034 | 0.8680 | +2.2% | −10.4% 至 +35.6% | 7/12 |
| `shader_batch_translate` | +2.6% | 2.9921 | 3.0869 | +2.3% | +0.1% 至 +7.1% | 9/12 |
| `shadow_refresh` | +1.3% | 19.4312 | 19.7473 | −0.1% | −1.9% 至 +7.8% | 5/12 |
| `submit_32` | −3.0% | 4.4631 | 4.4618 | −1.7% | −7.1% 至 +7.6% | 6/12 |
| `submit_fence_wait` | −3.5% | 1.6637 | 1.5914 | +0.1% | −6.4% 至 +12.6% | 6/12 |
| `submit_prepare` | +9.9% | 0.0340 | 0.0355 | −4.0% | −22.2% 至 +21.5% | 6/12 |
| `submit_queue_and_fence` | −4.2% | 2.2760 | 2.3370 | +0.6% | −7.8% 至 +10.9% | 6/12 |
| `texture_full` | +5.0% | 23.7879 | 24.4031 | +3.1% | −3.0% 至 +8.6% | 8/12 |
| `texture_partial` | −1.4% | 0.8375 | 0.8238 | +1.8% | −1.8% 至 +13.9% | 8/12 |
| `wave_page_marks_gpu` | +1.9% | 3.5621 | 2.7086 | −30.4% | −43.6% 至 −19.5% | 2/12 |

两个窗口各自对 19 项校正后的区间都没有检出变慢的指标，标记回写在两窗分别快 14.0% 和 16.0%。单项仍有波动。同路径窗冷、热 PSO 和 shader 翻译分别慢 6.0%、5.1%、2.3%，各自的逐项区间高于零；校正后均跨零，冷 PSO 在第一窗反而快 6.3%。完整纹理上传两窗均有正点估计，同路径窗为 +3.1%，逐项区间 −3.0% 至 +8.6%。短测可排除开始时 1 MiB 约 62% 的回退，对于几微秒或几个百分点的小幅差异仍有不确定性。

审核重点是只对经过私有写跟踪查询且片段不相交的地址并行、跨页存储的精确故障位置、读取失败后保留后续声明的重试机会，以及重新保护早于最终读回。正式 30 分钟浸泡仍由主线程安排，以覆盖长时间的漂移和资源增长。

### 正确性及交付范围

- 三次代码提交各自运行 Windows x64 原生的工作区 Clippy，包含全部 targets，分别使用默认特性和全部特性，都采用 `--locked -- -D warnings`，六次零诊断。三次的定向 shadow 测试分别通过 30、31、32 项。[第一项默认](logs/r1-issues-investigation/optimize-1mib/scalar-clippy.txt) · [第一项全特性](logs/r1-issues-investigation/optimize-1mib/scalar-clippy_all.txt) · [第二项默认](logs/r1-issues-investigation/optimize-1mib/private-clippy.txt) · [第二项全特性](logs/r1-issues-investigation/optimize-1mib/private-clippy_all.txt) · [第三项默认](logs/r1-issues-investigation/optimize-1mib/mark-clippy.txt) · [第三项全特性](logs/r1-issues-investigation/optimize-1mib/mark-clippy_all.txt)
- 最终版 `cargo test --workspace --locked` 通过 970、失败 0、忽略 12；`cargo test --locked -p victory-gpu` 连续三轮各通过 315、失败 0、忽略 3。只统计每个顶层 harness 的最终结果。首次全量运行的文件策略检查指出新增计时表的 CSV 扩展名不在允许列表内，改用纯文本文件后重新跑完整套测试通过。[测试汇总](logs/r1-issues-investigation/optimize-1mib/mark-tests-summary.txt) · [工作区输出](logs/r1-issues-investigation/optimize-1mib/mark-workspace.txt) · [三轮 GPU 测试记录](logs/r1-issues-investigation/optimize-1mib/mark-ci.txt)
- 在 gpu-quiet 窗口运行 `tools/bench/run.ps1 -CheckOnly`，普通读回和启用 D3D12 Debug Layer、GPU-based validation 的进程均正常退出；验证错误、失败、崩溃均为 0。两次 RGBA 都是 `[1, 0.5, -0.25, 2]` 和 `[0, 0, 0, 0]`，深度都是 `[0.25, 0.5, 0.25, 0.25]`。[验证报告](logs/r1-issues-investigation/optimize-1mib/mark-validation.json)

## 审核修改（并行回写）

独立审核未通过 `f907845` 至 `feb14ed` 的回写优化，提出一项必须修改和三项建议。四项各自单独提交在 `investigate/r1-issues`，最后一个代码提交是 `bf688eb`。1 MiB 短测在正序、反序两个窗口中 p50 分别是同窗 A 的 **96.9% 和 96.0%**。默认 19 项和三档规模与 `679826a` 配对比较，逐项区间都跨零。测试、两套 Clippy 和 Debug Layer 验证全部通过。日志在 [parallel-review-fixes](logs/r1-issues-investigation/parallel-review-fixes/)，各二进制的来源、哈希和对照补丁见 [builds.txt](logs/r1-issues-investigation/parallel-review-fixes/builds.txt)。

### 四项修改

1. **并行回写期间的重映射，`b6dc581`。** 原先 `GetWriteWatch` 成功后，没有机制保证这块私有分配在各组写完前仍然存在。另一个线程可以在这期间释放它，再把两个组的地址映射到同一物理页，两组随后向同一字节写入不同的值，结果取决于线程调度。`crate::watch` 新增一个门，记录正在进行的重映射数和持有映射的回写数。并行回写在查询前取得持有，所有组结束后才放开。`Writing::before_remap`（32 位释放、原地提交）和运行时映射物理页视图时取得 `Remapping`：先等持有者归零，存在期间阻止新的并行回写开始。回写遇到正在进行的重映射时不等待，改走按地址顺序的路径，提交线程不会因此阻塞在 title 上。多个重映射之间互不等待。门用互斥量加计数实现，没有用读写锁的守卫，这样 `Writing` 仍可跨线程传递。也考虑过只对生命周期有保证的分配并行，但那需要跟踪每个池的归属；现方案每次并行回写只多一次无竞争的加锁。映射视图也要等，即使 title 绕过运行时直接通过主机释放了分配，也不会在回写期间出现别名。新测试在第一组即将读取页面时从另一线程释放整个池，并把同一 section 页映射到第一组和第三组的两个地址。断言释放被挡到回写结束、section 原值不变、写回字节数与“顺序路径回写后再做同一重映射”的参照一致。带门的版本 3/3 通过。去掉等待的对照版本先后 5 次全部失败：前 3 次按当时的断言顺序报告“释放在组写入期间完成”，调整断言顺序后的 2 次报告“有组写进了重映射后的物理页”。[竞争测试与对照](logs/r1-issues-investigation/parallel-review-fixes/race-test-gate-and-control.txt) · [调整断言顺序后的对照](logs/r1-issues-investigation/parallel-review-fixes/race-test-control-reordered.txt)
2. **读取半页失败时旧快照被提前消费，`72eb73e`。** 标记页分两个 32 KiB 块读取。前半页读完后 CPU 写入会触发旧页快照，后半页读取失败时，原代码已先 `unarm` 并取走快照，再因读取失败跳过整页。重试时这一页已脏但没有快照，GPU 改动就再也发布不出去。这个问题在两块读取之前就存在：单次读取失败加上并发写入同样会触发。现在读取失败时页面保持已武装、快照留在原处；只有整页读取成功才 `unarm` 并取走快照。`copy_back_marked_ranges` 的顺序相同，一并修正。新增组合用例覆盖“前半页读完后另一线程写入 → 后半页读取失败 → 重试”：要求重试发布边界两侧的 GPU 字节，并保留 CPU 字节。修改前重试写回 0 字节（应为 65,536），修改后通过。[修改前的失败记录](logs/r1-issues-investigation/parallel-review-fixes/marked-retry-before-fix.txt)
3. **一组走回退、另一组正常完成，`cd7ed42`。** 16 个私有页分为四组。第二组读完一页后，测试在 GPU 改写段两侧写入 CPU 字节（第三组的一页也写），再给改写段所在的 4 KiB 块加上一次性 `PAGE_GUARD`。这一组会等到第三组写完那一页、开始读下一页再继续，两组确实同时进行。断言回写保持并行；每页都有 GPU 改写段；两页的 CPU 字节在 title 内存和重新同步后的副本中都保留；内核写入恰好一次，即被保护的那一段。
4. **稀疏负载，`bf688eb`。** 新增两个忽略测试。`pipeline::hot::soak::sparse` 与 `soak::mixed` 用同一个 1 MiB 池，着色器只把每个 64 KiB 页改 1 个字节，正好 16 页，刚好触发并行，每轮逐字节校验。`shadow::tests::sparse_write_back_timing` 在同一进程里只计时 `copy_back`，按 50 轮一块、并行/顺序/顺序/并行（ABBA）交替，给出块配对比。对照二进制关闭并行分组，其余相同。结果见下表，并行不慢，**门槛保持按改动的 64 KiB 片数判断**。耗时主要在读取和比较 16 个整页，不在 16 次写入。

| 稀疏负载，同一 gpu-quiet 窗口 | 并行版 | 关闭并行的对照 |
|---|---:|---:|
| 进程内回写 p50，µs（ABBA 两臂） | 170.6 / 170.5 | 228.4 / 226.5（同进程顺序路径） |
| 进程内块配对比中位（并行 ÷ 顺序） | 0.740 / 0.726，并行较慢 5/200、10/200 块 | 1.001 / 0.999（两边都是顺序，方法本身无偏） |
| 端到端每轮 p50，µs | 1448.5 / 1492.4 | 1570.1 / 1522.5 |
| 端到端每轮 p99，µs | 1873.2 / 2174.7 | 2014.7 / 1994.3 |

端到端四臂按并行、对照、对照、并行的次序各跑 60 秒，共 155,310 轮，逐轮校验失败和设备移除均为零。端到端 p50 两对分别快 7.7% 和 2.0%；p99 一对较快，另一对慢 9.0%，短测不足以判断尾部。[进程内计时](logs/r1-issues-investigation/parallel-review-fixes/sparse-writeback-timing.txt) · [端到端浸泡](logs/r1-issues-investigation/parallel-review-fixes/sparse-soak.txt)

### 1 MiB 同窗短测

`diag` 构建，每臂 60 秒，A 是 `integ-base` 清单中的 `6b1e408`，本次是干净的 `bf688eb`，`679826a` 臂用上一节 `feb14ed` 的同一二进制（`679826a` 只增加文档）。每个窗口由 gpu-quiet 确认空闲，每臂开跑前再连续采样，桌面 GPU 占用全部 ≤3%。窗口记录的后台占用只有 chrome 和 dwm 各 1% 或没有。[正序窗口](logs/r1-issues-investigation/parallel-review-fixes/mib-forward.txt) · [反序窗口](logs/r1-issues-investigation/parallel-review-fixes/mib-reverse.txt)

| 窗口 / 顺序 | 版本 | 轮数 | p50，ms | p95，ms | p99，ms |
|---|---|---:|---:|---:|---:|
| 正序 1 | 原 A `6b1e408` | 42,076 | 1.3861 | 1.6343 | 1.8116 |
| 正序 2 | 本次 `bf688eb` | 43,099 | **1.3433** | 1.6326 | 1.8037 |
| 正序 3 | `679826a` | 43,137 | 1.3401 | 1.6439 | 1.8331 |
| 反序 1 | `679826a` | 43,228 | 1.3365 | 1.6422 | 1.8204 |
| 反序 2 | 本次 `bf688eb` | 43,282 | **1.3326** | 1.6406 | 1.8505 |
| 反序 3 | 原 A `6b1e408` | 42,030 | 1.3882 | 1.6364 | 1.8159 |

本次 p50 是 A 的 96.9% 和 96.0%，在 110% 目标以内；对 `679826a` 为 +0.2% 和 −0.3%。p99 相对 A 为 −0.4% 和 +1.9%。六臂共 256,852 轮，逐轮校验失败和设备移除均为零。门控没有带来可测的 1 MiB 开销。

### 默认热点与三档规模

两臂为 `679826a`（同上，`feb14ed` 二进制）和本次 `bf688eb`。每次运行前把二进制复制到同一路径并重新核对哈希，用旧 B 的 `warm_adapter` 预热，频率和 PCIe 链路按 `Warm-Ready` 连续稳定三秒以上才开始计时，逐对交换起跑顺序。下表按对计算本次 ÷ `679826a` − 1 的中位数，正数表示本次较慢。区间按对重抽 5,000 次，种子 5090。默认项 10 对，小档 12 对，中档 8 对，大档 6 对。每次运行都通过仓库的工作量和读回断言。[默认 19 项](logs/r1-issues-investigation/parallel-review-fixes/default-hotspots.txt) · [小档](logs/r1-issues-investigation/parallel-review-fixes/scale-small.txt) · [中档](logs/r1-issues-investigation/parallel-review-fixes/scale-medium.txt) · [大档](logs/r1-issues-investigation/parallel-review-fixes/scale-large.txt) · [全部配对统计](logs/r1-issues-investigation/parallel-review-fixes/paired-stats.txt)

| 指标 | 配对变化 | 逐项 95% 区间 | 本次较慢对数 |
|---|---:|---:|---:|
| 默认 `marked_writeback_frame` | −0.8% | −3.6% 至 +1.0% | 4/10 |
| 默认 `shadow_refresh` | +1.8% | −9.1% 至 +4.7% | 6/10 |
| 默认 `descriptors_14400` | −0.3% | −1.7% 至 +2.8% | 4/10 |
| 默认 `texture_full` | −2.7% | −5.9% 至 +11.1% | 4/10 |
| 小档 `marked_writeback_frame` | +4.8% | −3.3% 至 +9.2% | 7/12 |
| 小档 `shadow_refresh` | +2.0% | −4.3% 至 +5.2% | 7/12 |
| 中档 `marked_writeback_frame` | −2.5% | −4.6% 至 +0.9% | 2/8 |
| 中档 `shadow_refresh` | −6.6% | −18.3% 至 +0.1% | 2/8 |
| 大档 `marked_writeback_frame` | −0.2% | −3.1% 至 +0.8% | 3/6 |
| 大档 `shadow_refresh` | −1.3% | −3.4% 至 +3.9% | 2/6 |

默认 19 项的逐项区间全部跨零，完整表格在配对统计文件中。点估计最大的是 `submit_queue_and_fence` +10.3%（−13.3% 至 +16.1%）和 `submit_fence_wait` +8.5%，这两项是提交与围栏等待，与本次改动的代码无关。10 对时，校正 19 项的重抽样分位点只落在第 6 个样本附近，这一列在文件里仅作参考。

**小档副本刷新复核。** 上一节小档副本刷新的 +6.9% 是 `feb14ed` 对旧 B `b5c06a7` 的读数。为此另开一窗，把旧 B、`679826a`、本次三臂的六种次序各跑两遍，共 12 组。`679826a` 对旧 B 为 +1.2%（−5.4% 至 +5.7%，6/12），上一节的 +6.9% 没有重现。本次对旧 B 为 +8.6%（−3.3% 至 +24.2%，8/12），对 `679826a` 为 +6.1%（−2.6% 至 +22.0%）。与两臂窗口合并、按窗口分层后，本次对 `679826a` 共 24 对，配对 +3.2%（−2.3% 至 +7.4%，15/24 对较慢）。这项计时只包含一次新注册副本的首次 `update`，即小档 16 MiB 的首次复制和触页缺页。所走的 `update_tracked`、`take_split`、`refresh_run` 本次没有修改，三臂窗口的读数宽度也说明单次计时噪声较大。现有数据不能确认退步，也不能排除 +7% 以内的小幅差异。小档标记回写合并 24 对为 +3.5%（−2.5% 至 +7.1%）；三臂窗口中本次对旧 B 为 −3.9%。[三臂小档](logs/r1-issues-investigation/parallel-review-fixes/scale-small-three-arm.txt)

### 正确性

- 四个提交各自在 Windows x64 原生运行工作区 Clippy，包含全部 targets，默认特性和全部特性各一次，均使用 `--locked -- -D warnings`，八次零诊断。第三项提交前第一次运行指出新测试缺少 `SAFETY` 注释，补上后重跑通过；日志是通过的那次。[第一项](logs/r1-issues-investigation/parallel-review-fixes/c1-b6dc581-clippy.txt) · [全特性](logs/r1-issues-investigation/parallel-review-fixes/c1-b6dc581-clippy_all.txt) · [第二项](logs/r1-issues-investigation/parallel-review-fixes/c2-72eb73e-clippy.txt) · [全特性](logs/r1-issues-investigation/parallel-review-fixes/c2-72eb73e-clippy_all.txt) · [第三项](logs/r1-issues-investigation/parallel-review-fixes/c3-cd7ed42-clippy.txt) · [全特性](logs/r1-issues-investigation/parallel-review-fixes/c3-cd7ed42-clippy_all.txt) · [第四项](logs/r1-issues-investigation/parallel-review-fixes/c4-bf688eb-clippy.txt) · [全特性](logs/r1-issues-investigation/parallel-review-fixes/c4-bf688eb-clippy_all.txt)
- 在 `bf688eb` 上，`cargo test --workspace --locked` 通过 973、失败 0、忽略 13；`cargo test --locked -p victory-gpu` 连续三轮各通过 318、失败 0、忽略 4，只统计各顶层 harness 的最终结果。相较上一节，新增三个测试，新增的两个忽略基准中，`pipeline::hot` 只在 `diagnostics` 特性下编译，所以忽略项只多一个。开发中定向的 `shadow::` 35 项连续三轮通过，`watch::` 20 项通过。[工作区](logs/r1-issues-investigation/parallel-review-fixes/workspace.txt) · [GPU 第一轮](logs/r1-issues-investigation/parallel-review-fixes/gpu1.txt) · [第二轮](logs/r1-issues-investigation/parallel-review-fixes/gpu2.txt) · [第三轮](logs/r1-issues-investigation/parallel-review-fixes/gpu3.txt) · [窗口记录](logs/r1-issues-investigation/parallel-review-fixes/ci-window.txt) · [定向 shadow](logs/r1-issues-investigation/parallel-review-fixes/shadow-tests.txt) · [定向 watch](logs/r1-issues-investigation/parallel-review-fixes/watch-tests.txt)
- 在 gpu-quiet 窗口对 `bf688eb` 运行 `tools/bench/run.ps1 -CheckOnly`。普通读回和启用 D3D12 Debug Layer、GPU-based validation 的进程均正常退出，validation_errors=0，失败和崩溃均为 0。两次 RGBA 都是 `[1, 0.5, -0.25, 2]` 和 `[0, 0, 0, 0]`，深度都是 `[0.25, 0.5, 0.25, 0.25]`。[验证报告](logs/r1-issues-investigation/parallel-review-fixes/validation.json)

### 审核重点

- 门只覆盖运行时看得见的释放和映射：`XMemFreeDefault` 的 32 位释放、`XMemVirtualAlloc` 的原地提交和物理页视图映射。title 直接调用主机 `VirtualFree` 释放、再用自己创建的 section 在同一地址做别名，仍然看不见；这与 `watch.rs` 开头记录的已有限制相同。
- 物理页映射现在可能等待一次正在进行的并行回写，时长与回写本身相当，1 MiB 时估计为几百微秒，范围更大时更长。GTA 的流式映射线程因此可能偶尔被拖慢，这一点短测没有覆盖，需要在正式浸泡或实机中观察。
- 锁顺序是“门 → 守卫锁”：`before_remap` 在取守卫锁之前等门；回写持门期间会取守卫锁和副本快照锁，但从不等门。新代码若在持有守卫锁或 `physical()` 锁时进入并行回写，就会破坏这个顺序。
- 读取失败后页面保持已武装，直到下一次成功读取或 `disarm`。在这期间，这一页的首次 CPU 写入会多保存一份 64 KiB 快照，与读取成功前的行为一致。
- 小档副本刷新合并点估计 +3.2%，区间上界 +7.4%，代码路径没有改动，建议在正式浸泡中顺带观察。

## 复审修改（并行回写）

复审仍未通过，剩两项必须修改、一项建议修改，三项各自单独提交在 `investigate/r1-issues`，最后一个代码提交是 `4c8ce3f`。日志在 [parallel-review-fixes-2](logs/r1-issues-investigation/parallel-review-fixes-2/)，二进制来源、哈希和两个对照补丁见 [builds.txt](logs/r1-issues-investigation/parallel-review-fixes-2/builds.txt)。

**显卡空闲门槛在本节中途改过。** 从「三项修改」到「默认热点与三档规模」的数据，以及本文此前所有章节，都按旧门槛测：每次开跑前整卡占用连续 ≤3%。桌面空闲时整卡本来就在 3%–6% 之间（35 °C、约 62 W，没有程序跑显卡计算），旧门槛下本轮的 CI 和 Validation 窗口被连续拒绝 21 次，一直没有跑成。按用户要求放宽后，从「按新显卡门槛复测」一小节起全部按新门槛测：10 次采样均值 ≤8%、单次 ≤10%，dwm、浏览器、Edge WebView、Teams 等桌面后台放行并记进日志。新门槛下的数据都放在 [new-gate](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/) 子目录。

### 三项修改

1. **标记页入口的并行回写也持门，`044a462`（必须修）。** `copy_back_marks` 在同一副本有至少 128 个有效标记页时，直接做写变更查询并按 8 页一段并行写回，没有持重映射门。这条路径在 `679826a` 就有。通过正常的 `XMemFreeDefault` 释放私有池，再把两段的地址映射到同一物理页，就会出现与快照入口相同的数据错误。现在它与快照入口一样，在查询前取得持有，直到所有段结束才放开；遇到已登记的重映射就按地址顺序写回。门本身没有改动。竞争测试改写为一个公共辅助函数，覆盖快照入口（16 页）和标记页入口（128 页）两种入口，以及两种释放：一种经 `before_remap`，与 `XMemFreeDefault` 相同；另一种绕过运行时直接释放，这时只有映射自己的等待能挡住别名。每个别名地址都在各自的 `crate::watch::remapping()` 下映射，这就是运行时 `map()` 调用的等门函数，不再与释放包在同一个持有期里。测试判断“这次回写确实并行”的依据改为按回写起点登记，不再问“是否有人持门”。四个竞争用例互相串行，因为一个用例的重映射会让其他用例退回顺序路径。同一 gpu-quiet 窗口里每个版本各跑 5 轮：带门版四个用例 20/20 通过；`remapping()` 不等待的对照版 20/20 失败；只去掉标记页入口持门的对照版（即 `679826a` 的标记页行为），标记页两个用例 10/10 失败、快照两个用例 10/10 通过。失败原因都是“有组写进了重映射后的物理页”或“重映射在组写入期间完成”。[带门版与两个对照](logs/r1-issues-investigation/parallel-review-fixes-2/alias-tests-gate-and-controls.txt)
2. **别名测试只清理自己映射的视图，`93f38d8`（必须修）。** 原测试在某个别名地址映射失败（地址已被别的测试占用）时，清理仍对两个预期地址都调用 `UnmapViewOfFile`，可能解除别的测试建立的视图，让整个测试进程崩溃。现在映射逐个记下本次真正取得的视图，遇到第一次失败就停止，清理只上报并解除这些视图。读取 section 前先读完再解除视图，读取用的临时视图先检查映射成功再从指针构造切片。修改后该用例 3 轮通过。[修改后的运行](logs/r1-issues-investigation/parallel-review-fixes-2/alias-test-after-cleanup-fix.txt)
3. **故障组测试确实证明两组同时在场，`4c8ce3f`（建议修）。** 原先正常组可能在故障组装上保护之前就置好完成标志，“发生重叠”的断言证明不了两组同时在场；只有一个执行线程时，故障组会空等两秒后失败。现在故障组装好保护后先发出“已就绪”，正常组收到后才写自己的页；故障组仍等正常组写完该页、开始读下一页再继续。两边各自记录看到的信号，分别断言。`parallel_items` 只有一个执行线程（`available_parallelism` 为 1）时，两组只能先后运行，不等待、不断言重叠，只检查结果。这个用例同样依赖并行路径，现与别名竞争用例共用串行锁。`shadow::` 全部 38 项连续 3 轮通过，该用例单独 10 轮通过。单执行线程分支没有在本机实际触发过。[shadow 全部用例](logs/r1-issues-investigation/parallel-review-fixes-2/shadow-tests.txt) · [故障组用例 10 轮](logs/r1-issues-investigation/parallel-review-fixes-2/guarded-group-test.txt)

### 1 MiB 同窗短测

做法与上一节相同，本次是干净的 `4c8ce3f`。[正序](logs/r1-issues-investigation/parallel-review-fixes-2/mib-forward.txt) · [反序](logs/r1-issues-investigation/parallel-review-fixes-2/mib-reverse.txt)

| 窗口 / 顺序 | 版本 | 轮数 | p50，ms | p95，ms | p99，ms |
|---|---|---:|---:|---:|---:|
| 正序 1 | 原 A `6b1e408` | 43,960 | 1.3134 | 1.6012 | 1.7672 |
| 正序 2 | 本次 `4c8ce3f` | 43,248 | **1.3347** | 1.6433 | 1.8378 |
| 正序 3 | `679826a` | 42,871 | 1.3482 | 1.6606 | 1.8935 |
| 反序 1 | `679826a` | 42,884 | 1.3461 | 1.6440 | 1.8187 |
| 反序 2 | 本次 `4c8ce3f` | 43,527 | **1.3212** | 1.6360 | 1.8378 |
| 反序 3 | 原 A `6b1e408` | 42,212 | 1.3819 | 1.6282 | 1.7746 |

本次 p50 是 A 的 **101.6% 和 95.6%**，在 110% 目标以内；对 `679826a` 为 −1.0% 和 −1.8%。p99 相对 A 为 +4.0% 和 +3.6%，相对 `679826a` 为 −2.9% 和 +1.1%。六臂共 258,702 轮，逐轮校验失败和设备移除均为零。

### 默认热点与三档规模

做法与上一节相同（同路径、预热到频率稳定、逐对交换顺序），两臂为 `679826a` 和 `4c8ce3f`。默认项 10 对，小档 12 对，中档 8 对，大档 6 对，每次都通过仓库的工作量和读回断言。[默认 19 项](logs/r1-issues-investigation/parallel-review-fixes-2/default-hotspots.txt) · [小档](logs/r1-issues-investigation/parallel-review-fixes-2/scale-small.txt) · [中档](logs/r1-issues-investigation/parallel-review-fixes-2/scale-medium.txt) · [大档](logs/r1-issues-investigation/parallel-review-fixes-2/scale-large.txt) · [全部配对统计](logs/r1-issues-investigation/parallel-review-fixes-2/paired-stats.txt)

| 指标 | 配对变化 | 逐项 95% 区间 | 本次较慢对数 |
|---|---:|---:|---:|
| 默认 `marked_writeback_frame` | +0.9% | −2.5% 至 +3.3% | 6/10 |
| 默认 `shadow_refresh` | +2.0% | −5.4% 至 +9.5% | 5/10 |
| 小档 `marked_writeback_frame` | −4.5% | −8.3% 至 +7.4% | 5/12 |
| 小档 `shadow_refresh` | −0.9% | −4.3% 至 +9.4% | 5/12 |
| 中档 `marked_writeback_frame` | −1.8% | −13.7% 至 +3.0% | 3/8 |
| 中档 `shadow_refresh` | −2.1% | −6.6% 至 +19.3% | 3/8 |
| 大档 `marked_writeback_frame` | +2.9% | −2.3% 至 +5.1% | 4/6 |
| 大档 `shadow_refresh` | +1.1% | −4.7% 至 +16.8% | 3/6 |

默认 19 项的逐项区间全部跨零，完整表格见配对统计文件。中、大档的标记回写有 4,096 和 16,384 页，走的正是本次加门的标记页并行入口；每次调用只多一次无竞争的加锁，没有测出差异。上一节关注的小档副本刷新这次为 −0.9%。

### 按新显卡门槛复测

以下全部按新门槛测，代码仍是 `4c8ce3f`，基准二进制与上文相同（`final.exe` `ab91eaeb…`、`679826a` 臂 `1b21ccf6…`、原 A `888ed490…`）。测试和 Validation 在 `dc725af` 的工作树上编译，它在 `4c8ce3f` 之上只改了 `tools/bench`。

**新门槛。** 判断写在一份共用的 PowerShell 门槛库里，gpu-quiet 开窗和窗口内每次开跑前的检查都调用它。放行条件：最近 10 次采样（跨度至少 10 秒）整卡均值 ≤8%、单次 ≤10%；没有桌面后台以外的进程使用 3D/Compute/CUDA 引擎；没有挖矿进程。桌面后台指 dwm、Windows 外壳组件、Chrome/Edge/Firefox 等浏览器、Edge WebView、Teams 和网易云音乐，放行时逐个记下进程和窗口内的引擎峰值。不满足条件时只等待，从不结束或通知任何进程。改动的文件、旧新规则对照和门槛库全文见 [gate-change.txt](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/gate-change.txt)。仓库内只有一个提交 `dc725af`：`hot.ps1` 把 gpu-quiet 的放行摘要记为 `quiet_window.admission`，`hot.sh` 可用 `VICTORY_GPU_QUIET` 指定 gpu-quiet。`run.ps1` 默认单次 ≤10%，本来就是这个值，没有改；窗口脚本不再传 `-MaxGpuUtilization 3`。全局的 `~/.local/bin/gpu-quiet` 没有改动。

**放行情况。** 本轮 10 个 gpu-quiet 窗口、129 次放行，放行窗口内整卡均值的中位数为 3.6%（最高 6.2%），单次最高 10%。每次检查的中位耗时 12.1 秒，其中约 11 秒是 10 次采样本身。记录到的桌面后台只有 chrome、dwm、explorer、网易云音乐和 Edge WebView，全程没有出现桌面以外的显卡用户，没有结束过任何进程。大档反序补测中有一次等了 18 分钟：那段时间整卡一直在 9%–10%，只有上述桌面进程各占 2%–3%。[全部放行记录](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/gate-admissions.txt)

#### 1 MiB 同窗短测

做法与上文相同。[正序](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/mib-forward.txt) · [反序](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/mib-reverse.txt)

| 窗口 / 顺序 | 版本 | 轮数 | p50，ms | p95，ms | p99，ms |
|---|---|---:|---:|---:|---:|
| 正序 1 | 原 A `6b1e408` | 43,299 | 1.3607 | 1.6320 | 1.9319 |
| 正序 2 | 本次 `4c8ce3f` | 41,993 | **1.4066** | 1.7585 | 2.0341 |
| 正序 3 | `679826a` | 40,723 | 1.4285 | 1.8816 | 2.5836 |
| 反序 1 | `679826a` | 41,492 | 1.4044 | 1.8260 | 2.3488 |
| 反序 2 | 本次 `4c8ce3f` | 43,918 | **1.3292** | 1.6932 | 1.9728 |
| 反序 3 | 原 A `6b1e408` | 43,826 | 1.3380 | 1.6128 | 1.9881 |

本次 p50 是同窗 A 的 **103.4% 和 99.3%**，在 110% 目标以内；3% 门槛下是 101.6% 和 95.6%。对 `679826a` 为 −1.5% 和 −5.4%。p99 相对 A 为 +5.3% 和 −0.8%（3% 门槛下 +4.0% 和 +3.6%）。六臂共 255,251 轮，逐轮校验失败和设备移除均为零。三臂的 p95、p99 都比 3% 门槛时高一些，桌面后台的占用会进入尾延迟。

#### 默认热点与三档规模

做法与上文相同，两臂仍为 `679826a` 和 `4c8ce3f`，默认项 10 对，小档 12 对，中档 8 对，大档 6 对，每次都通过仓库的工作量和读回断言。小档的标记回写在正序窗出现逐项区间不跨零的变慢，大档的副本刷新也接近，所以小档和大档各补了一个先跑另一臂的窗口（12 对和 6 对），下表的小档、大档是两窗合计。[默认 19 项](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/default-hotspots.txt) · [小档](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/scale-small.txt) · [小档反序](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/scale-small-reverse.txt) · [中档](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/scale-medium.txt) · [大档](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/scale-large.txt) · [大档反序](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/scale-large-reverse.txt) · [全部配对统计](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/paired-stats.txt)

| 指标 | 配对变化 | 逐项 95% 区间 | 校正区间 | 本次较慢对数 | 3% 门槛下 |
|---|---:|---:|---:|---:|---:|
| 默认 `marked_writeback_frame` | −2.3% | −4.2% 至 +2.0% | −5.1% 至 +3.5% | 4/10 | +0.9% |
| 默认 `shadow_refresh` | +1.4% | −1.7% 至 +2.9% | −2.6% 至 +3.5% | 7/10 | +2.0% |
| 小档 `marked_writeback_frame` | +1.8% | −3.8% 至 +4.7% | −4.5% 至 +6.1% | 13/24 | −4.5% |
| 小档 `shadow_refresh` | +3.9% | −0.2% 至 +6.1% | −2.8% 至 +8.1% | 16/24 | −0.9% |
| 中档 `marked_writeback_frame` | −3.7% | −7.1% 至 +2.7% | −7.8% 至 +3.7% | 3/8 | −1.8% |
| 中档 `shadow_refresh` | −1.9% | −5.2% 至 +0.4% | −6.7% 至 +2.5% | 2/8 | −2.1% |
| 大档 `marked_writeback_frame` | +0.3% | −1.6% 至 +1.4% | −1.8% 至 +1.8% | 8/12 | +2.9% |
| 大档 `shadow_refresh` | **+2.1%** | **+0.9% 至 +3.2%** | −0.3% 至 +5.3% | 10/12 | +1.1% |

默认项的校正区间按 19 项计，三档按 6 项计。默认 19 项的逐项区间全部跨零；提交与围栏等待类（`submit_*`）的区间比 3% 门槛时宽得多，例如 `submit_fence_wait` 为 −13.0% 至 +38.8%，这几项不经过回写代码，宽度来自桌面后台。小档标记回写正序窗为 +4.7%（+1.8% 至 +8.6%，10/12 对较慢），反序窗为 −3.1%（−5.7% 至 +1.3%，3/12），合计 +1.8% 且区间跨零，正序窗的变慢没有复现。更早的「分组版」一节也见过同样的现象：首轮 +4.8%，交换起跑顺序后 +2.9%。大档副本刷新两窗分别为 +2.3% 和 +1.8%，合计逐项区间不跨零，校正后跨零。副本刷新走 `Shadow::update`，这两臂之间没有改动它，也不经过本轮加门的函数；3% 门槛下这项为 +1.1%。

#### 测试与 Validation

- `cargo test --workspace --locked`：29 个顶层测试程序，976 项通过，0 失败，13 项忽略。（初稿写成 33 个程序、980 项，是把用例自己启动的子进程的结果也算了进去；本文此后都只按顶层测试程序计数。）[日志](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/workspace.txt)
- `cargo test --locked -p victory-gpu` 连跑 3 轮：每轮 321 项通过，0 失败，4 项忽略（初稿的 324 同样多算了子进程）。[第 1 轮](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/gpu1.txt) · [第 2 轮](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/gpu2.txt) · [第 3 轮](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/gpu3.txt) · [窗口](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/ci-window.txt)
- `tools/bench/run.ps1 -CheckOnly`，开启 D3D12 Debug Layer 和 GPU-based validation：`validation_status` 为 passed，`validation_errors` = 0，失败和崩溃均为 0。颜色读回 `[[1, 0.5, -0.25, 2], [0, 0, 0, 0]]`，深度 `[0.25, 0.5, 0.25, 0.25]`，普通检查与验证场景一致。[窗口](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/validation-window.txt) · [结果](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/validation.json)
- 两套 Clippy 已在三个代码提交上跑过（见上文），之后代码没有改动，没有重跑。

#### 审核关注

- 新门槛允许桌面后台占用最多到 8% 均值，1 MiB 尾延迟和提交类指标的离散度都比 3% 门槛时大。跨门槛的数据不宜直接比较，本节只比同窗、同门槛的两臂。
- 大档副本刷新 +2.1% 落在本轮没有改动的路径上，上文已建议在正式浸泡中观察小档副本刷新，这项也一并观察。
- 放行名单比用户点名的多了 Windows 外壳组件（explorer、SearchHost 等）、登录代理 `Microsoft.AAD.BrokerPlugin` 和网易云音乐，它们都只绘制桌面界面。名单之外的进程只会让门槛等待。整卡均值和单次上限对名单内的进程同样有效。
- 桌面占用持续在 8%–10% 时门槛会一直等，本轮最长等了 18 分钟。gpu-quiet 默认等 180 秒后放弃，由调用方重试；本轮的窗口脚本设为 1800 秒。

## 第三轮复审：隔离别名测试

第三轮复审结论是有条件通过：生产代码没有问题，合并前要把测试隔离开。本节只改测试和报告，每项单独提交在 `investigate/r1-issues`。日志在 [parallel-review-fixes-3](logs/r1-issues-investigation/parallel-review-fixes-3/)，结果汇总见 [summary.txt](logs/r1-issues-investigation/parallel-review-fixes-3/summary.txt)。本节的测试都按新显卡门槛排队（见上一节），计数一律按顶层测试程序算，用例自己启动的子进程打印的结果不计入。

### 四项修改

1. **别名竞争用例在独立子进程里运行，`a72ee69`（必须修）。** 原测试解除自己映射的视图之后，仍对已经释放的整个池调用 `title_memory_remapped(pool, len)`。如果同进程的另一个测试已经复用了其中某一页，这个通知会把新持有者的页标脏、重新打开写保护，并删掉它的别名关系。现在四个别名竞争用例各自用 `--exact` 重新启动测试程序本身，在子进程里运行（与 `shader_cache`、`pipeline::library` 已有的子进程用例同一做法）；子进程的输出只在失败时转给父进程，因此顶层结果里每个用例只算一次。测试只对自己仍然映射着的视图发重映射通知，去掉了整池通知。
2. **别名用例先确认重映射线程已经停在门前，`41ed1e9`（建议修）。** 原先给重映射线程 300 ms，只看它有没有完成，分不清“还没走到门”和“被门挡住”。现在钩子先等（最多 5 秒）重映射在 `remapping()` 里登记自己：登记和等待在同一把锁下先后发生，而运行钩子的组正持有映射，所以登记之后它一定在门前等待。之后才给 300 ms，并要求它没有完成。5 秒内没走到门就判测试失败。为此在 `watch::tests` 里加了只读的测试辅助函数 `remaps_at_gate()`，生产代码没有改。
3. **故障组用例按本次实际的执行线程判断能否重叠，`407c51d`（建议修）。** 原先按 CPU 数推断有没有第二个执行者，但线程池创建失败时，`parallel_items` 会合法地全部在调用线程上运行。现在每页回调记下自己所在的线程；故障组装好保护后，等待（最多 5 秒）任一页在别的线程上被读到。看到了，才等待正常组、断言两组在两个线程上重叠；看不到，就只检查结果，并打印“只有一个执行者”。
4. **报告计数更正，`1d286c6`。** 上一节初稿写的 980 项和 324 项多算了子进程的结果，按顶层测试程序应为工作区 976 通过、13 忽略，victory-gpu 每轮 321 通过、4 忽略。

### 验证

- 两套 Clippy（`--workspace --all-targets`，其中一套加 `--all-features`，均 `-D warnings`）在 `a72ee69`、`41ed1e9`、`407c51d` 上逐个运行，全部通过。[c1](logs/r1-issues-investigation/parallel-review-fixes-3/c1-clippy.txt) · [c1 全特性](logs/r1-issues-investigation/parallel-review-fixes-3/c1-clippy_all.txt) · [c2](logs/r1-issues-investigation/parallel-review-fixes-3/c2-clippy.txt) · [c2 全特性](logs/r1-issues-investigation/parallel-review-fixes-3/c2-clippy_all.txt) · [c3](logs/r1-issues-investigation/parallel-review-fixes-3/c3-clippy.txt) · [c3 全特性](logs/r1-issues-investigation/parallel-review-fixes-3/c3-clippy_all.txt)
- `407c51d` 的测试程序在同一个 gpu-quiet 窗口里，五个用例各单独连跑 10 轮，**50/50 通过**：快照入口经登记释放、快照入口未登记释放、标记页入口经登记释放、标记页入口未登记释放、故障组。故障组 10 轮都看到了第二个执行者，并断言了两组重叠。[逐轮记录](logs/r1-issues-investigation/parallel-review-fixes-3/repeat-tests.txt)
- 对照版：在 `407c51d` 上去掉 `remapping()` 的等待（[补丁](logs/r1-issues-investigation/parallel-review-fixes-3/nogate.patch.txt)），四个别名用例各跑一次，**4/4 失败**。快照两个用例和标记页“经登记释放”报“有组写进了重映射后的物理页”；标记页“未登记释放”报“重映射在组写入期间完成”。失败都来自子进程，由父进程转出。[对照版记录](logs/r1-issues-investigation/parallel-review-fixes-3/nogate-control.txt)
- `cargo test --workspace --locked`：第一次在 `victory-hle` 的 `modules::xasync::tests::run_returns_the_work_result` 失败，期望 `E_ABORT`（0x80004004），实际 `E_PENDING`（0x8000000A）；cargo 在这个程序后停下，之前 9 个程序 570 项通过。[第一次](logs/r1-issues-investigation/parallel-review-fixes-3/workspace-run1.txt) · [窗口](logs/r1-issues-investigation/parallel-review-fixes-3/ci-window-run1.txt) 第二次 29 个顶层测试程序 **976 项通过，0 失败，13 项忽略**。[第二次](logs/r1-issues-investigation/parallel-review-fixes-3/workspace.txt)
- `cargo test --locked -p victory-gpu` 在同一窗口里连跑 3 轮：每轮 2 个顶层测试程序，**321 项通过，0 失败，4 项忽略**。第二次 CI 窗口在第 2 轮前因门槛计数器的异常值中止（见下文），所以三轮整体重跑了一遍。[第 1 轮](logs/r1-issues-investigation/parallel-review-fixes-3/gpu1.txt) · [第 2 轮](logs/r1-issues-investigation/parallel-review-fixes-3/gpu2.txt) · [第 3 轮](logs/r1-issues-investigation/parallel-review-fixes-3/gpu3.txt) · [三轮窗口](logs/r1-issues-investigation/parallel-review-fixes-3/ci-window.txt) · [中止的第二次 CI 窗口](logs/r1-issues-investigation/parallel-review-fixes-3/ci-window-run2.txt)

**xasync 失败的原因（与本轮改动无关）。** `complete_call` 先 `release(block)`，把调用从登记表里移除，之后才发布状态并置完成标志。`XAsyncGetStatus(block, true)` 如果正好在这两步之间进来，就查不到调用，不等待，直接读到尚未发布的 `E_PENDING`，与失败值完全一致。这段顺序来自 `30a5202`，优化前的 A（`6b1e408`）同样包含。本轮只改了 victory-gpu 的测试代码，没有改动 victory-hle。该用例在空闲机器上单独连跑 300 次全部通过，[记录](logs/r1-issues-investigation/parallel-review-fixes-3/xasync-alone.txt)：它只在整套测试并发运行时偶发。这是生产代码里的竞态，不在本轮“只改测试”的范围内，需要另行决定是否修复。

**显卡排队。** 本节的测试一律经 gpu-quiet 开窗，门槛规则与上一节相同，另有三处补充（[说明](logs/r1-issues-investigation/parallel-review-fixes-3/gate-change.txt)）：
- Discord 加入桌面后台名单，它和 Teams 一样常驻 2%–3% 的 3D 占用。
- 进 gpu-quiet 之前先做一次只读采样，非桌面程序在用显卡时在外面等，不反复暂停、重启桌面服务。
- Windows 引擎计数器偶尔读出离谱的数值（见过 711049543019964），原先转换时会抛异常，让第二次 CI 窗口在 victory-gpu 第 2 轮之前中止；现在把这种采样当作“读不出”，记进日志后继续等待。

本节的测试从 21:33 起排队，用户从 21:23 起在玩《Phantom Blade Zero》（PBZ，3D 占用 80%–90%），门槛一直等到 00:24 游戏结束才放行。之后还让过另一个子线程的 30 分钟长测，也等过 Chrome 放视频（3D 19%，整卡 24%）。全程没有结束或通知任何进程。
