# 修前 58 条 Clippy 诊断及来源

行号取自提交 `d883dde`。每行对应一次全特性诊断，其中标为“两轮”的 11 条也出现在默认特性检查中。路径均相对仓库根目录。源行由 Git blame 核对，引入提交按 Git 历史与两个基线的实测定位。

| 配置 | 文件与行号 | 诊断 | 引入提交与方向 | 本分支处理 |
|---|---|---|---|---|
| 全特性 | `crates/victory-gpu/src/descriptor/host.rs:2581` | 基准函数位于测试模块后 | `490cf3e` 优化前的第一版热点基准（模块声明源行 `2849477`） | 将基准辅助项移至测试模块前 |
| 两轮 | `crates/victory-gpu/src/descriptor/host.rs:3438` | 固定大小的 chunks_exact | `ef1cdc7` 描述符，指纹碰撞回归测试 | 用 as_chunks 取相同的完整块 |
| 两轮 | `crates/victory-gpu/src/descriptor/host.rs:3686` | 手写整除判断 | `9df0732` 描述符，物理页失效回归测试 | 改用 is_multiple_of(2) |
| 全特性 | `crates/victory-gpu/src/pipeline/hot.rs:60` | unsafe 缺少相邻安全说明 | `490cf3e` 优化前的第一版热点基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot.rs:117` | unsafe 缺少相邻安全说明 | `490cf3e` 优化前的第一版热点基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot.rs:134` | unsafe 缺少相邻安全说明 | `490cf3e` 优化前的第一版热点基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot.rs:148` | unsafe 缺少相邻安全说明 | `490cf3e` 优化前的第一版热点基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot.rs:189` | 固定大小的 chunks_exact | `490cf3e` 优化前的第一版热点基准 | 用 as_chunks 取相同的完整块 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot.rs:420` | unsafe 缺少相邻安全说明 | `490cf3e` 优化前的第一版热点基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot.rs:424` | unsafe 缺少相邻安全说明 | `490cf3e` 优化前的第一版热点基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/contention.rs:27` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/contention.rs:54` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/contention.rs:75` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/contention.rs:149` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/contention.rs:164` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/contention.rs:182` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/contention.rs:183` | 固定大小的 chunks_exact | `3eed7c7` 优化前的压力、并发与浸泡基准 | 用 as_chunks 取相同的完整块 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/contention.rs:187` | 固定大小的 chunks_exact | `3eed7c7` 优化前的压力、并发与浸泡基准 | 用 as_chunks 取相同的完整块 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/contention.rs:259` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/contention.rs:290` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/pressure.rs:13` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/pressure.rs:123` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/pressure.rs:140` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/pressure.rs:170` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/pressure.rs:183` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/pressure.rs:216` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/scale.rs:24` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/soak.rs:13` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/soak.rs:57` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/soak.rs:62` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/soak.rs:71` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/soak.rs:87` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/soak.rs:174` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/soak.rs:201` | unsafe 缺少相邻安全说明 | `3eed7c7` 优化前的压力、并发与浸泡基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/textures.rs:26` | unsafe 缺少相邻安全说明 | `da7f303` 纹理优化，稀疏上传基准 | 交纹理子线程 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/textures.rs:45` | unsafe 缺少相邻安全说明 | `490cf3e` 优化前的第一版热点基准 | 交纹理子线程 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/textures.rs:48` | unsafe 缺少相邻安全说明 | `490cf3e` 优化前的第一版热点基准 | 交纹理子线程 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/textures.rs:153` | unsafe 缺少相邻安全说明 | `490cf3e` 优化前的第一版热点基准 | 交纹理子线程 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/waves.rs:89` | unsafe 缺少相邻安全说明 | `490cf3e` 优化前的第一版热点基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/waves.rs:101` | unsafe 缺少相邻安全说明 | `490cf3e` 优化前的第一版热点基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/waves.rs:131` | unsafe 缺少相邻安全说明 | `392f778` GPU 页标记，分散写入和循环基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/waves.rs:206` | unsafe 缺少相邻安全说明 | `392f778` GPU 页标记，分散写入和循环基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/waves.rs:225` | unsafe 缺少相邻安全说明 | `392f778` GPU 页标记，分散写入和循环基准 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/waves.rs:249` | unsafe 缺少相邻安全说明 | `cb1d22d` GPU 页标记，时间戳和回读核验 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/waves.rs:262` | unsafe 缺少相邻安全说明 | `cb1d22d` GPU 页标记，时间戳和回读核验 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/waves.rs:264` | unsafe 缺少相邻安全说明 | `cb1d22d` GPU 页标记，时间戳和回读核验 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/pipeline/hot/waves.rs:272` | unsafe 缺少相邻安全说明 | `cb1d22d` GPU 页标记，时间戳和回读核验 | 补充或紧贴 unsafe 放置安全说明 |
| 两轮 | `crates/victory-gpu/src/pipeline/library.rs:154` | unsafe 缺少相邻安全说明 | `a3b5d80` PSO 缓存，链接适配器选择 | 补充或紧贴 unsafe 放置安全说明 |
| 两轮 | `crates/victory-gpu/src/pipeline/library.rs:192` | unsafe 缺少相邻安全说明 | `311baca` PSO 缓存，内核适配器信息查询 | 补充或紧贴 unsafe 放置安全说明 |
| 全特性 | `crates/victory-gpu/src/queue.rs:756` | 基准函数位于测试模块后 | `490cf3e` 优化前的第一版热点基准（模块声明源行 `754619f`） | 将基准辅助项移至测试模块前 |
| 两轮 | `crates/victory-gpu/src/shader_cache.rs:199` | 可合并的嵌套 if | `671e508` 着色器缓存，过期暂存文件清理 | 用 let 链保留短路与错误日志 |
| 全特性 | `crates/victory-gpu/src/shadow.rs:1936` | 基准函数位于测试模块后 | `490cf3e` 优化前的第一版热点基准（模块声明源行 `80d8cf5`） | 将基准辅助项移至测试模块前 |
| 两轮 | `crates/victory-gpu/src/texture/readback_tests.rs:187` | unsafe 缺少相邻安全说明 | `bfee212` 纹理优化，混合内存读回测试 | 交纹理子线程 |
| 两轮 | `crates/victory-gpu/src/texture/readback_tests.rs:210` | unsafe 缺少相邻安全说明 | `bfee212` 纹理优化，混合内存读回测试 | 交纹理子线程 |
| 两轮 | `crates/victory-shader/src/emit/lower/marks/tests.rs:85` | unsafe 缺少相邻安全说明 | `613335b` GPU 页标记与 WaveOps 能力测试 | 补充或紧贴 unsafe 放置安全说明 |
| 两轮 | `crates/victory-shader/src/emit/lower/marks/tests.rs:87` | unsafe 缺少相邻安全说明 | `613335b` GPU 页标记与 WaveOps 能力测试 | 补充或紧贴 unsafe 放置安全说明 |
| 两轮 | `crates/victory-shader/src/emit/lower/marks/tests.rs:90` | unsafe 缺少相邻安全说明 | `613335b` GPU 页标记与 WaveOps 能力测试 | 补充或紧贴 unsafe 放置安全说明 |
| 两轮 | `crates/victory-shader/src/emit/lower/marks/tests.rs:95` | unsafe 缺少相邻安全说明 | `613335b` GPU 页标记与 WaveOps 能力测试 | 补充或紧贴 unsafe 放置安全说明 |

三处测试模块的位置早于上游基准提交，触发告警的是 `490cf3e` 在模块后加入的 `bench_slot_keys`、`bench_submit` 和 `bench_mark`。描述符碰撞用例对照了社区指纹算法；纹理方向的 `bfee212` 吸收过社区 #2358、#3136 的上传方案。表中的相关告警均出现在本地新增的验证代码。
