# Windows 原生验证

正式性能用的三臂来自 177ee68 开发基线。交付验证的 B1 和 B2 又分别合并了描述符改动 4e3d95e。每个版本使用自己的 Windows target、构建队列和原生 MSVC 工具链。[正式测量版本](../final-arms.json) · [交付验证汇总](delivery-candidates.json)

| 项目 | B1 本地掩码与 Windows 池 | B2 社区存储与 #2051 池 |
|---|---|---|
| 启用诊断的 victory-gpu 库测试 | 297 通过，0 失败，13 忽略 | 300 通过，0 失败，13 忽略 |
| 工作区默认测试，先运行除 victory-shader 以外的所有包与文档测试 | 620 通过，0 失败，6 忽略 | 623 通过，0 失败，6 忽略 |
| 正常开发配置的 victory-shader 库 | 310 个用例，307 通过，0 失败，3 忽略 | 与 B1 使用相同源码和依赖，复用左侧结果 |
| victory-shader 文档测试 | 0 个用例，退出成功 | 0 个用例，退出成功 |
| contention::write_overlap | 1、4、8、16 个 CPU 写线程逐字节通过 | 同四档逐字节通过 |
| 合成热点 smoke | 3 次均核验成功 | 3 次均核验成功 |
| 合成渲染像素与深度检查 | 通过 | 通过 |
| 含 diagnostics 的 victory-gpu Clippy | 通过 | 通过 |
| D3D12 Debug Layer | 待安装 Windows 图形工具后补跑 | 同左 |

着色器测试使用工作区统一依赖配置编出的原生测试程序。为配合独占显卡时段，将程序列出的 310 个测试名分为 24 组，每个名字恰好分到一组。记录包含每组测试名、通过或忽略计数及原生输出。[310 个用例](B1/shader-coverage.json) · [原生输出](B1/shader-batches.txt)

两套候选都有各自的 GPU 测试、像素、smoke、并发、Clippy 和默认工作区日志。像素运行启用了 CheckOnly 与 SkipValidation。[B1 原始结果](B1/) · [B2 原始结果](B2/)

随后又将纹理优化 dde121f 合入本地方案，得到 99cce47。在该提交重新执行 Windows 诊断版 victory-gpu，302 个通过、0 失败、14 个按配置忽略。其余工作区默认包和文档（victory-shader 另列）合计 626 个通过、0 失败、6 个忽略。四个线程档位的写竞争检查、三次热点 smoke、像素与深度检查和 Clippy 均通过。[此次原始结果](delivery-99cce4752c76/)

最后又直接执行完整工作区入口 cargo test --locked --workspace --no-fail-fast，逐项运行含 victory-shader 在内的所有默认包及文档测试。Windows 原生结果为 936 个通过，0 个失败，10 个按配置忽略。[完整原生输出](delivery-99cce4752c76/workspace.txt)

Windows 原生 cargo xtask policy 在包含完整测试和基准日志的交付目录中通过，检查 1132 个文件。
