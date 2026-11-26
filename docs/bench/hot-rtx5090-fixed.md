# RTX 5090 正确性修复后的热点基线

正式采样尚未开始。当前记录来自修复分支 `e6ed615` 上的预检，分页回读适配提交为 `451227b`。等待最终步长修复合入后，需要在最终代码上重新验证并采集 30 个新进程的样本。

## Windows 预检

| 项目 | 结果 |
|---|---|
| 原生 Windows 全工作区、全目标、diagnostics 特性 | 编译成功 |
| diag 配置热点测试可执行文件 | 编译成功 |
| 默认并行度工作区测试，首轮 | 900 通过、2 失败、10 忽略 |
| 相同工作区命令，第二轮 | 901 通过、1 失败、10 忽略 |
| 像素与深度回读 | 通过，按要求跳过 Debug Layer 和 GPU-based validation |
| 热点冒烟 | 3 个新进程均通过全部断言 |

首轮分页测试要求重新分配到原物理页，实际为 260，预期为 259。单独运行该用例通过，第二轮完整工作区测试也通过了这项断言。两轮都在资产策略测试中发现已跟踪的两个 Python 基准工具扩展名不在许可清单内。[首轮完整日志](logs/hot-rtx5090-fixed/workspace-initial.txt) · [第二轮完整日志](logs/hot-rtx5090-fixed/workspace-initial-repeat.txt)

## 基准适配

描述符负载仍在 32,768 个逻辑槽中分散改写 1,300 或 14,400 个槽。计时始末仍是调用并返回 `descriptor::sync_heaps`，分页同步成本在该区间内。计时结束后校验全部逻辑槽的记录 ID 和代数，GPU 从物理堆起点按页表定位每一个改写槽，逐个核对其常量。整个进程的内存峰值会覆盖校验资源和分页实现。

三次冒烟中 20 个 shader 的 DXIL 集合均为 88,177 字节，BLAKE3 为 `a0c629eda000e64f6a98240935c1cda672a72df20184c884832d2da2831b69f8`。旧基线为 77,333 字节，BLAKE3 为 `6a3b8c756f04aa62ade1d92871430c8a1326354061bb5d52a9266ddb2936090f`。最终代码的 DXIL 和性能变化需以正式采样为准。[冒烟结果](logs/hot-rtx5090-fixed/initial-smoke.json) · [像素检查](logs/hot-rtx5090-fixed/initial-pixel.json) · [构建记录](logs/hot-rtx5090-fixed/initial-build.txt)
