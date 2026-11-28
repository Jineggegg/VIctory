# 合入纹理更新后的 Windows 检查

测试提交 4a899adb94f656ba32e6712c0d17165a8860f349，含 perf/rtx5090 的 dde121f。热点测试程序 SHA-256 为 f658a08ad62d7bd76f258ad163ae7e9b45df8bb6455bfaa31817645c60a8d4b2。Windows 11，RTX 5090，621.14。

| 五个 crate 的功能配置 | 通过 | 失败 | 默认忽略 | shader 单测程序 SHA-256 |
|---|---:|---:|---:|---|
| GPU diagnostics | 648 | 0 | 23 | 87d08bdda2bd29dc218faf037fe58f12dbc98bd847a4ac85afb82ea27b0f5df9 |
| 默认 | 632 | 0 | 7 | 87d08bdda2bd29dc218faf037fe58f12dbc98bd847a4ac85afb82ea27b0f5df9 |

受测 crate 为 victory、victory-shader、victory-game、victory-gpu 与 victory-pipeline。两种配置生成的 shader 单元测试二进制相同，35 个 VALU 测试在该二进制上逐项重跑，其他测试分别运行。

| 补充检查 | 结果 |
|---|---|
| 三个独立 Windows 进程的热点冒烟 | 3/3，通过 20 个固定 shader 的 BLAKE3 校验 5f1614fabbcaa1b0fc0784e7a184e3ce0ad26948ec42a3b685d429e5f226fad2 |
| 大档 2,000 个固定 shader 和 1,996 个 PSO | 通过，BLAKE3 为 20473c42839d9df07d82a5489a8d023920ae7fb2d74ebe660a8f6591f065ef8b |
| 随机常量的独立进程重放 | 两个进程各创建 128 个 PSO，无应用库命中，DXIL 摘要相同，GPU 像素回读一致 |
| run.ps1 -CheckOnly -SkipValidation | 通过，零失败、零进程崩溃 |
| cargo xtask policy | asset policy: 775 files ok |
| hardware 驱动库拒绝后重建 | 7 个测试进程全部通过 |
| hardware WaveOps 缓存隔离 | 1 个测试进程全部通过 |
| warp 驱动库拒绝后重建 | 7 个测试进程全部通过 |
| warp WaveOps 缓存隔离 | 1 个测试进程全部通过 |

本机缺 Windows 图形工具，D3D12 Debug Layer 和 GPU-based validation 待补。
