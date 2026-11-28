# RTX 5090 第一轮整体验收 · 规模梯度、专项与长时浸泡

**三档回写、刷新、完整纹理上传和局部更新均保留收益。** 两种顺序下，1 GiB 回写快 47.77%–48.06%，64 张纹理局部更新快 46.18%–48.94%。新采的 90 对默认场景中，同页标记和循环查页 dispatch 分别快 19.88% 和 20.52%，两个 19 项家族区间都排除零。16 个热 PSO 预建仍有数微秒的退步；256 次提交准备在两个独立窗口都未检出变化。[大档正序](logs/round1-acceptance-3/large-forward/normalized-timings.txt) · [大档反序](logs/round1-acceptance-3/large-reverse/normalized-timings.txt) · [默认扩样](logs/round1-acceptance-3/dispatch-forward/normalized-timings.txt)

**1 MiB 快照回写的严重退步已修复，最终版通过两臂各 30 分钟的长测与 D3D12 验证。** 在相同的新显卡门槛下，最终 B 的每分钟轮次 p50 中位数为 1.376 ms，A 为 1.346 ms（+2.2%）；p99 分别为 2.047 和 1.907 ms（+7.3%）。两臂均无校验失败、设备移除、持续延迟漂移或资源增长。修复前 A、B 各自的验证层都报过三条错误，修复后为零。[最终 A](logs/round1-acceptance-3/soak-A-newgate/minutes.txt) · [最终 B](logs/round1-acceptance-3/soak-B-final/minutes.txt) · [验证结果](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/validation.json)

## 测量条件与二进制身份

RTX 5090、驱动 621.14、Core Ultra 9 285K、Windows 11 10.0.26300。两臂使用第一段的 Windows 原生 Rust 1.98.1 MSVC 工具链、同一份 `../toolchain/env.ps1` 和各自的 `diag` 产物。每次配对前核对磁盘文件的 SHA-256，驱动脚本还核对清单里的提交、源码差异指纹和可执行文件。以下摘要与第一段正式清单逐字相同。[第一段 A](logs/round1-acceptance-2/formal/A/results.json) · [第一段 B](logs/round1-acceptance-2/formal/B/results.json) · [本次 A](logs/round1-acceptance-3/small-forward/A/results.json) · [本次 B](logs/round1-acceptance-3/small-forward/B/results.json)

| 臂 | 本地分支与身份 | diag 测试程序 SHA-256 |
|---|---|---|
| A，优化前 | `integ/r1-bench-base`，`6b1e408` | `888ed490721961cfa2128e4343f91998d47625995a284b6eb2720ec60f21e4a6` |
| B，合并版 | `integ/r1-accept2`，产品正确性被测版本 `8cdd575`，本次基准脚本 `54c8ae9` | `6d7d89464cdb397d2881c1ca1e08e53dbd6345ac583acded01302e48581cbdc4` |

**显卡入场门槛在验收中途变更。** 三档正反序、专项扩样、原 A/B 长测、首次修复 B 长测及原 A/B 验证使用旧门槛（3%）。原验收只放行低占用的桌面后台引擎，窗口分别记录整卡读数。用户确认日常桌面空闲也会有 3%–6% 占用，随后把规则改为连续 10 次整卡采样均值 ≤8%、任一次 ≤10%，非桌面 3D/Compute 占用须退出。最终 A 与最终 B 各自重新开窗，调查报告中新门槛复测也单独成组。门槛只在开始前取样；长测期间没有持续限制桌面占用。两种门槛下的绝对值分列，仅在同一门槛内比较。[原窗口规则](logs/round1-acceptance-3/window-method.txt) · [首次修复 B 窗口](logs/round1-acceptance-3/soak-B-fixed/window.txt) · [变更与复测](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/gate-change.txt) · [最终 A 窗口](logs/round1-acceptance-3/soak-A-newgate/window.txt) · [最终 B 窗口](logs/round1-acceptance-3/soak-B-final/window.txt)

正式 A/B 入场按第一段检查 P0/P1、核心 ≥2,500 MHz、显存 ≥13,000 MHz、PCIe Gen5 ×16。两次连续达标的相邻样本可复用热状态。[窗口规则与预定样本数](logs/round1-acceptance-3/window-method.txt)

三档各跑两个独立窗口，每窗 30 对，每对内两臂各启一个新进程。正序窗从 AB 开始，反序窗从 BA 开始，各自逐对交换入场顺序。所有变化统一为同对 B/A − 1 的中位数。反序窗按样本身份取回原始时长，用原生 `xtask bench-stats --paired` 重算，两个窗口独立报告。每窗重抽整对 5,000 次，种子 5090。逐项和家族 95% 区间估计中位配对变化，家族大小取套件声明的耗时数，小／中／大为 14／15／14，扩样默认组为 19。家族校正在各窗口内实施。各臂 p95、p99 为最近秩，30 次的 p99 为最大样本。后文只将区间严格排除零的变化计为该窗口检出。

`54c8ae9` 单独修订基准脚本，将四个纹理暂存计数的版本兼容规则扩到三份有名的梯度清单，单臂原始计数完整保留。Windows 脚本还实际请求 ID3D12Debug1 并记录结果。接口查询在计时进程外运行。四份清单各六种字段组合以及未列名套件拒绝放行的检查全部通过。[脚本](../../tools/bench/hot.ps1) · [25 项检查与接口探测](logs/round1-acceptance-3/script-checks.txt)

| 正式配对窗口 | 有效对 | 无效尝试 A / B | 入场核心 MHz | 开窗时的桌面后台 | 配对与逐次数据 |
|---|---:|---:|---:|---|---|
| [小档正序](logs/round1-acceptance-3/small-forward/) | 30 | 0 / 0 | 2,692–2,902 | chrome 3D=1%, dwm 3D=1% | [原始](logs/round1-acceptance-3/small-forward/run.txt) · [配对](logs/round1-acceptance-3/small-forward/paired.json) · [统一方向](logs/round1-acceptance-3/small-forward/normalized-timings.txt) |
| [小档反序](logs/round1-acceptance-3/small-reverse/) | 30 | 0 / 0 | 2,662–2,902 | chrome 3D=1%, dwm 3D=1% | [原始](logs/round1-acceptance-3/small-reverse/run.txt) · [配对](logs/round1-acceptance-3/small-reverse/paired.json) · [统一方向](logs/round1-acceptance-3/small-reverse/normalized-timings.txt) |
| [中档正序](logs/round1-acceptance-3/medium-forward/) | 30 | 0 / 0 | 2,707–2,895 | chrome 3D=1% | [原始](logs/round1-acceptance-3/medium-forward/run.txt) · [配对](logs/round1-acceptance-3/medium-forward/paired.json) · [统一方向](logs/round1-acceptance-3/medium-forward/normalized-timings.txt) |
| [中档反序](logs/round1-acceptance-3/medium-reverse/) | 30 | 0 / 0 | 2,707–2,895 | chrome 3D=1% | [原始](logs/round1-acceptance-3/medium-reverse/run.txt) · [配对](logs/round1-acceptance-3/medium-reverse/paired.json) · [统一方向](logs/round1-acceptance-3/medium-reverse/normalized-timings.txt) |
| [大档正序](logs/round1-acceptance-3/large-forward/) | 30 | 0 / 0 | 2,587–2,925 | chrome 3D=1% | [原始](logs/round1-acceptance-3/large-forward/run.txt) · [配对](logs/round1-acceptance-3/large-forward/paired.json) · [统一方向](logs/round1-acceptance-3/large-forward/normalized-timings.txt) |
| [大档反序](logs/round1-acceptance-3/large-reverse/) | 30 | 0 / 0 | 2,692–2,925 | chrome 3D=2%, dwm 3D=1% | [原始](logs/round1-acceptance-3/large-reverse/run.txt) · [配对](logs/round1-acceptance-3/large-reverse/paired.json) · [统一方向](logs/round1-acceptance-3/large-reverse/normalized-timings.txt) |
| [默认组扩样正序](logs/round1-acceptance-3/dispatch-forward/) | 90 | 0 / 0 | 2,617–2,865 | none | [原始](logs/round1-acceptance-3/dispatch-forward/run.txt) · [配对](logs/round1-acceptance-3/dispatch-forward/paired.json) · [统一方向](logs/round1-acceptance-3/dispatch-forward/normalized-timings.txt) |

六个梯度窗口共 180 对，默认扩样窗口 90 对，合计 270 对、540 个有效计时进程，零输出校验失败、零无效尝试。每个窗口目录均含 A、B 逐次结果和清单；统一方向文件给出该窗口全部声明耗时的中位、p95、p99 和两种区间。

## 规模梯度

| 工作量 | 小档 | 中档 | 大档 |
|---|---:|---:|---:|
| 回写／刷新覆盖的内存 | 16 MiB | 256 MiB | 1 GiB |
| 2048×2048 RGBA8 四 mip 纹理 | 1 张 | 8 张 | 64 张 |
| 同步的脏描述符 | 100 槽 | 1,300、14,400 槽 | 100,000 槽 |
| 已录制列表的串行提交 | 1 次 | 32 次 | 256 次 |

每张纹理修改 mip 1 的 32×8 像素；初传和更新后都逐行读回校验。描述符同步同时校验逻辑堆和 GPU 实际读到的值，大档逻辑堆从 32,768 扩到 131,072 槽。提交准备是各次 CPU 准备阶段的累计时间，与含队列和 fence 等待的总时间分开。[三档工作量与校验](scale-rtx5090-2026-11-26.md) · [三档清单](suites/scale/)

标记页回写和副本刷新在中档获得更大的相对收益，大档保持接近中档的幅度。完整纹理上传从小档约快 38%–39% 收窄到大档约快 10%–12%。局部更新在中档约快 91%，大档仍快 46%–49%。三个规模中，局部实际复制量都降到 A 的 1/32，整批耗时随规模的变化还包含刷新、转换、复制和 fence。

100 槽的两次家族区间都包含零。1,300 槽在两个中档窗口快 9.04%–10.59%，14,400 槽快 15.89%–16.04%，100,000 槽快 16.40%–16.71%。单次提交准备约 4–5 µs，计数分辨率为 1 µs，反序家族区间为 +10.00% 至 +25.00%，正序区间下界恰为零。32 次准备两窗均未检出变化。

下表的覆盖字节数及复制量经两臂逐样本核对，相同档位内各次一致。[小档逐次](logs/round1-acceptance-3/small-forward/run.txt) · [中档逐次](logs/round1-acceptance-3/medium-forward/run.txt) · [大档逐次](logs/round1-acceptance-3/large-forward/run.txt)

| 档位 | 每次刷新 / 回写（MiB） | 初传 A / B（MiB） | 实际局部复制 A / B（MiB） | 游戏侧改写的纹理数据（KiB） |
|---|---:|---:|---:|---:|
| 小 | 16 / 15 | 21.25 / 21.25 | 4 / 0.125 | 1 |
| 中 | 256 / 240 | 170 / 170 | 32 / 1 | 8 |
| 大 | 1024 / 960 | 1360 / 1360 | 256 / 8 | 64 |

### 小档

两窗所有指标的原始分位数和配对区间见[正序统计](logs/round1-acceptance-3/small-forward/normalized-timings.txt)与[反序统计](logs/round1-acceptance-3/small-reverse/normalized-timings.txt)。

| 项目 | A 中位 / p95 / p99（ms） | B 中位 / p95 / p99（ms） | 配对变化 | 逐项 95% | 家族 95% |
|---|---:|---:|---:|---:|---:|
| 标记页回写 · 正序 | 5.7815 / 6.6344 / 6.6945 | 3.6414 / 3.8578 / 3.9306 | -38.87% | -39.94% 至 -37.37% | -40.31% 至 -36.91% |
| 标记页回写 · 反序 | 5.7656 / 6.1478 / 6.1902 | 3.4842 / 3.7373 / 3.9099 | -39.98% | -41.07% 至 -38.96% | -41.61% 至 -38.54% |
| 完整纹理上传 · 正序 | 12.6189 / 13.3457 / 13.7707 | 7.8807 / 8.4467 / 18.8548 | -37.39% | -38.74% 至 -35.65% | -38.91% 至 -35.33% |
| 完整纹理上传 · 反序 | 12.8471 / 14.1962 / 20.5330 | 7.7150 / 8.4202 / 8.5474 | -39.42% | -40.57% 至 -37.79% | -41.62% 至 -37.40% |
| 局部纹理更新 · 正序 | 1.1649 / 1.4082 / 1.6403 | 0.3542 / 0.4194 / 0.4237 | -69.04% | -71.21% 至 -67.68% | -72.69% 至 -67.12% |
| 局部纹理更新 · 反序 | 1.1317 / 1.4661 / 1.4813 | 0.3478 / 0.5304 / 0.6074 | -69.19% | -70.43% 至 -67.88% | -71.26% 至 -67.52% |
| 副本刷新 · 正序 | 2.1545 / 2.7790 / 2.8332 | 1.4525 / 1.8333 / 1.9962 | -33.51% | -36.49% 至 -28.83% | -38.58% 至 -27.10% |
| 副本刷新 · 反序 | 2.1284 / 2.3029 / 2.6074 | 1.4054 / 1.6422 / 1.7198 | -33.78% | -35.17% 至 -30.34% | -36.07% 至 -26.56% |
| 描述符 100 槽 · 正序 | 0.1235 / 0.1467 / 0.2351 | 0.1284 / 0.1463 / 0.3065 | +4.83% | +0.00% 至 +7.41% | -3.61% 至 +10.87% |
| 描述符 100 槽 · 反序 | 0.1220 / 0.1306 / 0.1365 | 0.1252 / 0.1378 / 0.1630 | +3.37% | +1.11% 至 +5.51% | -3.84% 至 +7.31% |
| 提交准备 · 正序 | 0.0050 / 0.0050 / 0.0080 | 0.0050 / 0.0060 / 0.0070 | +20.00% | +0.00% 至 +22.50% | +0.00% 至 +25.00% |
| 提交准备 · 反序 | 0.0040 / 0.0050 / 0.0050 | 0.0050 / 0.0070 / 0.0080 | +25.00% | +20.00% 至 +25.00% | +10.00% 至 +25.00% |

### 中档

两窗所有指标的原始分位数和配对区间见[正序统计](logs/round1-acceptance-3/medium-forward/normalized-timings.txt)与[反序统计](logs/round1-acceptance-3/medium-reverse/normalized-timings.txt)。

| 项目 | A 中位 / p95 / p99（ms） | B 中位 / p95 / p99（ms） | 配对变化 | 逐项 95% | 家族 95% |
|---|---:|---:|---:|---:|---:|
| 标记页回写 · 正序 | 81.2244 / 85.8123 / 94.5890 | 41.1216 / 45.9027 / 46.4480 | -49.32% | -50.35% 至 -47.74% | -51.02% 至 -47.08% |
| 标记页回写 · 反序 | 81.3010 / 83.5452 / 84.8730 | 40.6779 / 45.2511 / 45.6592 | -49.36% | -49.91% 至 -48.96% | -50.78% 至 -48.30% |
| 完整纹理上传 · 正序 | 34.9406 / 37.3269 / 45.8865 | 24.8812 / 26.6651 / 27.1598 | -29.60% | -30.69% 至 -27.83% | -31.20% 至 -27.16% |
| 完整纹理上传 · 反序 | 35.1332 / 40.0035 / 46.8363 | 25.4566 / 30.8873 / 38.8386 | -28.31% | -29.53% 至 -26.85% | -30.27% 至 -25.03% |
| 局部纹理更新 · 正序 | 9.3447 / 10.3490 / 10.4721 | 0.8148 / 1.1122 / 1.1436 | -91.02% | -91.59% 至 -90.61% | -91.62% 至 -90.31% |
| 局部纹理更新 · 反序 | 9.1225 / 9.8529 / 9.8601 | 0.8378 / 1.2207 / 1.2784 | -90.67% | -91.20% 至 -90.40% | -91.36% 至 -90.26% |
| 副本刷新 · 正序 | 35.7653 / 49.7034 / 51.5391 | 19.8083 / 21.6817 / 21.7658 | -44.98% | -45.67% 至 -44.38% | -45.83% 至 -43.65% |
| 副本刷新 · 反序 | 35.8315 / 38.2224 / 38.9558 | 19.6071 / 20.7680 / 20.8102 | -45.33% | -46.54% 至 -44.29% | -46.86% 至 -44.20% |
| 描述符 1,300 槽 · 正序 | 0.9367 / 1.1405 / 1.2055 | 0.8452 / 1.0409 / 1.0716 | -10.59% | -12.22% 至 -7.33% | -13.79% 至 -4.51% |
| 描述符 1,300 槽 · 反序 | 0.9294 / 1.0500 / 1.1357 | 0.8393 / 0.9870 / 1.0038 | -9.04% | -11.43% 至 -8.24% | -12.79% 至 -7.55% |
| 描述符 14,400 槽 · 正序 | 9.0855 / 9.6487 / 9.8078 | 7.6562 / 8.1329 / 8.6184 | -16.04% | -17.85% 至 -14.28% | -18.13% 至 -13.52% |
| 描述符 14,400 槽 · 反序 | 9.1653 / 9.6570 / 9.7238 | 7.7148 / 8.4579 / 8.4891 | -15.89% | -17.07% 至 -14.06% | -17.28% 至 -13.42% |
| 提交准备 · 正序 | 0.0365 / 0.0540 / 0.0670 | 0.0375 / 0.0700 / 0.0730 | +13.04% | -12.56% 至 +25.58% | -21.83% 至 +38.92% |
| 提交准备 · 反序 | 0.0375 / 0.0540 / 0.0560 | 0.0400 / 0.0570 / 0.0630 | +5.02% | -10.72% 至 +27.62% | -22.50% 至 +35.62% |

### 大档

两窗所有指标的原始分位数和配对区间见[正序统计](logs/round1-acceptance-3/large-forward/normalized-timings.txt)与[反序统计](logs/round1-acceptance-3/large-reverse/normalized-timings.txt)。

| 项目 | A 中位 / p95 / p99（ms） | B 中位 / p95 / p99（ms） | 配对变化 | 逐项 95% | 家族 95% |
|---|---:|---:|---:|---:|---:|
| 标记页回写 · 正序 | 327.8282 / 341.6227 / 346.6127 | 172.3778 / 182.5255 / 186.6005 | -47.77% | -48.07% 至 -47.28% | -48.37% 至 -46.75% |
| 标记页回写 · 反序 | 333.4726 / 349.2683 / 355.2418 | 172.9204 / 179.5810 / 184.5155 | -48.06% | -48.79% 至 -48.00% | -49.33% 至 -47.87% |
| 完整纹理上传 · 正序 | 266.3211 / 342.6533 / 1128.3371 | 237.3439 / 320.6347 / 344.5432 | -12.49% | -15.52% 至 -7.73% | -21.03% 至 -1.32% |
| 完整纹理上传 · 反序 | 280.3715 / 364.0353 / 412.5073 | 248.9530 / 326.0079 / 885.9829 | -9.93% | -14.58% 至 -3.98% | -18.38% 至 -0.76% |
| 局部纹理更新 · 正序 | 254.2965 / 358.2454 / 394.6997 | 132.6721 / 145.0975 / 149.0621 | -48.94% | -50.33% 至 -47.55% | -51.64% 至 -46.86% |
| 局部纹理更新 · 反序 | 247.1160 / 289.2902 / 314.9426 | 134.1743 / 146.0893 / 151.8267 | -46.18% | -49.37% 至 -43.62% | -51.03% 至 -43.17% |
| 副本刷新 · 正序 | 148.0237 / 163.4511 / 166.0390 | 81.8929 / 87.7402 / 89.8751 | -44.27% | -45.05% 至 -43.49% | -45.18% 至 -43.33% |
| 副本刷新 · 反序 | 154.6307 / 164.8828 / 172.3655 | 82.9380 / 90.7250 / 90.7995 | -45.71% | -46.79% 至 -45.37% | -47.40% 至 -44.35% |
| 描述符 100,000 槽 · 正序 | 63.2703 / 65.6490 / 65.7016 | 52.9331 / 56.4656 / 66.5049 | -16.40% | -18.12% 至 -15.22% | -18.50% 至 -14.72% |
| 描述符 100,000 槽 · 反序 | 64.8046 / 67.0628 / 67.6728 | 53.4297 / 58.4815 / 67.3492 | -16.71% | -18.35% 至 -15.75% | -18.87% 至 -14.54% |
| 提交准备 · 正序 | 0.2600 / 0.3990 / 0.4380 | 0.2210 / 0.3890 / 0.4490 | +5.71% | -21.79% 至 +13.88% | -29.68% 至 +34.12% |
| 提交准备 · 反序 | 0.2500 / 0.3960 / 0.4050 | 0.2260 / 0.4070 / 0.4380 | -4.60% | -18.59% 至 +11.29% | -23.68% 至 +28.15% |

## 专项复核

### 256 次提交准备

| 256 次提交准备 | A 中位 / p95 / p99（ms） | B 中位 / p95 / p99（ms） | 配对变化 | 逐项 95% | 大档家族 95% |
|---|---:|---:|---:|---:|---:|
| 正序 | 0.2600 / 0.3990 / 0.4380 | 0.2210 / 0.3890 / 0.4490 | +5.71% | -21.79% 至 +13.88% | -29.68% 至 +34.12% |
| 反序 | 0.2500 / 0.3960 / 0.4050 | 0.2260 / 0.4070 / 0.4380 | -4.60% | -18.59% 至 +11.29% | -23.68% 至 +28.15% |

该项取大档的 CPU 准备计数。两个正反序窗口的点估计方向不同，均保留了从明显加快到明显变慢的可能。A、B 各自的中位数由各自样本排序而得，配对变化另按同一对的比值排序，所以绝对中位数的方向也可能与配对变化不同。[正序统一方向](logs/round1-acceptance-3/large-forward/normalized-timings.txt) · [反序统一方向](logs/round1-acceptance-3/large-reverse/normalized-timings.txt)

### 两个 GPU dispatch

沿用默认场景与 GPU timestamp。新窗口固定采 90 对，完成后一次计算；第一段两次 30 对保留在原报告。默认场景内，同页标记覆盖 262,144 个 lane，循环查页执行 2,097,152 次全局读取。下表附同窗观察到的热预建。[扩样逐对](logs/round1-acceptance-3/dispatch-forward/paired.json) · [A 原始分布](logs/round1-acceptance-3/dispatch-forward/A/results.json) · [B 原始分布](logs/round1-acceptance-3/dispatch-forward/B/results.json)

| 项目 | A 中位 / p95 / p99（ms） | B 中位 / p95 / p99（ms） | 配对变化 | 逐项 95% | 家族 95% |
|---|---:|---:|---:|---:|---:|
| 同页标记 dispatch | 6.3418 / 8.6779 / 9.7822 | 5.0760 / 6.3818 / 7.5837 | -19.88% | -27.30% 至 -12.10% | -31.74% 至 -3.71% |
| 循环查页 dispatch | 5.8902 / 7.5351 / 8.0311 | 4.2899 / 5.7580 / 6.0448 | -20.52% | -27.15% 至 -14.54% | -29.23% 至 -2.12% |
| 16 个热 PSO 预建 | 0.1050 / 0.1475 / 0.2033 | 0.1085 / 0.1682 / 0.2288 | +3.43% | +2.01% 至 +4.83% | +0.76% 至 +6.34% |

第一段两个窗口里，同页标记的点估计分别为 −10.48%、−25.20%，循环查页为 −10.33%、−10.40%，家族区间都跨零。本次两个家族区间的上界分别为 −3.71%、−2.12%。据此两项在该默认工作量上均确认加快；换用其他 dispatch 形状或密度时，仍需使用相应负载的实测。[第一段正反序对照](round1-acceptance-2.md) · [扩充负载的边界](gpu-marks/复现与取舍.md)

### 16 个热 PSO 预建

第一段直接比较优化前与合并版，正、反序窗口分别慢 4.33%、5.17%；反序的绝对中位差为每批 5.40 µs。下表沿用已有父提交 `7878982` 的两组直接配对，各 30 对，保留原先的 19 项校正。[基线对 shader 父提交](logs/round1-acceptance-2/shader-parent/) · [shader 父提交对最终版](logs/round1-acceptance-2/parent-to-merged/)

| 方向 | 左臂中位 / p95 / p99（ms） | 右臂中位 / p95 / p99（ms） | 配对变化 | 逐项 95% | 家族 95% |
|---|---:|---:|---:|---:|---:|
| 基线 → shader 父提交 | 0.1069 / 0.1509 / 0.5078 | 0.1090 / 0.1540 / 0.2226 | +4.40% | -0.39% 至 +8.47% | -1.05% 至 +17.16% |
| shader 父提交 → 合并版 | 0.1058 / 0.1358 / 0.1419 | 0.1082 / 0.2283 / 0.2961 | +3.10% | +0.19% 至 +5.02% | -0.32% 至 +6.18% |

父提交相对基线已出现同向点估计，最终版相对父提交的逐项区间也排除了零，但这两组父提交对照的家族区间均跨零。可将最终版 16 个热预建的小幅退步列入验收；现有数据无法把开销拆给合并或某个修复。若后续处理，优先直接给缓存命中路径分段计时。此处归因只使用已有父提交对照。

### 16 个冷 PSO 首样本与缓存状态

| 窗口 | 首个有效样本 A / B（ms） | A 中位 / p95 / p99（ms） | B 中位 / p95 / p99（ms） | 配对变化 | 逐项 95% | 家族 95% |
|---|---:|---:|---:|---:|---:|---:|
| [第一段默认正序](logs/round1-acceptance-2/formal/) | 7.7148 / 7.3051 | 7.5777 / 9.0540 / 9.6104 | 7.1563 / 7.8074 / 8.0513 | -5.51% | -9.59% 至 -2.91% | -11.71% 至 -1.95% |
| [第一段默认反序](logs/round1-acceptance-2/formal-reverse/) | 7.3397 / 7.3171 | 7.1258 / 8.1858 / 8.2057 | 6.9716 / 7.9966 / 8.1195 | -0.29% | -4.96% 至 +2.45% | -6.72% 至 +3.77% |
| [第二段小档正序](logs/round1-acceptance-3/small-forward/) | 12.9545 / 12.0544 | 8.8163 / 9.8911 / 12.9545 | 8.1405 / 8.9771 / 12.0544 | -9.36% | -11.28% 至 -6.99% | -13.70% 至 -5.90% |
| [第二段小档反序](logs/round1-acceptance-3/small-reverse/) | 8.9773 / 8.5467 | 8.7641 / 9.6913 / 9.9179 | 7.9379 / 9.2512 / 9.2579 | -8.30% | -10.62% 至 -4.96% | -11.71% 至 -2.20% |
| [第二段默认扩样](logs/round1-acceptance-3/dispatch-forward/) | 8.2683 / 7.0929 | 7.8291 / 9.0314 / 9.6279 | 7.7693 / 8.8850 / 9.1130 | -0.20% | -2.52% 至 +1.73% | -3.35% 至 +2.47% |

“冷”指本进程新建的 VIctory 管线库和本批首次驱动创建。保留了两臂各窗口首个有效样本。驱动缓存沿用机器状态，未清空，缺少可证实逐项命中或未命中的驱动遥测，因此驱动冷热记为未知。小档两次均检出收益，默认组的反序和 90 对窗口均包含零。大档另有 1,996 个 PSO，正序窗 A、B 首样本分别为 29.431 s、32.661 s，反序窗的 A、B 首样本为 1.000 s、0.854 s。两轮首样本都进入分位数，未将较慢的值筛掉，也未从耗时推断驱动缓存状态。[小档正序](logs/round1-acceptance-3/small-forward/run.txt) · [大档正序](logs/round1-acceptance-3/large-forward/run.txt) · [大档反序](logs/round1-acceptance-3/large-reverse/run.txt)

## A、B 各 30 分钟连续浸泡

首次 A 臂运行因用户暂停而中断，只记录到第 4 个完整分钟。本报告将这次运行作废，所有浸泡统计仅采用随后各自完整的 30 分钟窗口。[中断的原生输出](logs/round1-acceptance-3/soak-A-aborted/run.txt) · [中断的线程采样](logs/round1-acceptance-3/soak-A-aborted/threads.txt)

每臂在独立正式窗口中启动一次原生 `pipeline::hot::soak::mixed`，设置 `VICTORY_BENCH_SECONDS=1800`。同一测试进程和同一个 1 MiB 副本持续工作，每轮包含 CPU 改写、刷新、GPU dispatch、fence、回写和读回校验。每分钟原生进程给出轮数、工作集、DXGI 本地／非本地显存、句柄、该分钟的轮次 p50/p95/p99、累计校验失败和当时的设备移除 HRESULT。监控器持有启动时取得的同一 Windows 进程对象，刷新其线程数，并把最新读数与原生分钟输出的时刻配对。原始记录保留 PID、两次 UTC 时间和读数年龄。[原生浸泡与判据](../../crates/victory-gpu/src/pipeline/hot/soak.rs) · [A 分钟表](logs/round1-acceptance-3/soak-A/minutes.txt) · [B 分钟表](logs/round1-acceptance-3/soak-B/minutes.txt) · [A 线程](logs/round1-acceptance-3/soak-A/threads.txt) · [B 线程](logs/round1-acceptance-3/soak-B/threads.txt)

沿用基准内置判据，排除头两分钟，比较第 3–7 分钟与第 26–30 分钟的中位数。工作集增量超过 32 MiB、本地或非本地显存增量超过 32 MB、句柄增加超过 16 个会触发资源增长标志；每轮 p50 相对变化绝对值超过 10% 会触发延迟漂移标志。线程数及 p95/p99 直接给出观测量。浸泡每臂各一个连续进程，两组分钟数据存在时间相关性，故不据此构造独立 A/B 配对区间。下表各列分别取 30 个分钟 p50、分钟 p95、分钟 p99 的中位数。原生输出另列出前后两段的变化。

| 门槛与版本 | 轮次 p50（ms） | 轮次 p95（ms） | 轮次 p99（ms） | 前后 p50 漂移 | 完成轮数 | 原始记录 |
|---|---:|---:|---:|---:|---:|---|
| 3% · A | 1.378 | 1.665 | 1.816 | -0.14% | 1,261,226 | [分钟表](logs/round1-acceptance-3/soak-A/minutes.txt) · [判据](logs/round1-acceptance-3/soak-A/run.txt) · [开窗](logs/round1-acceptance-3/soak-A/window.txt) |
| 3% · 修复前 B | 99.189 | 100.742 | 101.836 | +19.54% | 19,461 | [分钟表](logs/round1-acceptance-3/soak-B/minutes.txt) · [判据](logs/round1-acceptance-3/soak-B/run.txt) · [开窗](logs/round1-acceptance-3/soak-B/window.txt) |
| 3% · 首次修复 B | 2.229 | 2.484 | 2.696 | -0.29% | 794,910 | [分钟表](logs/round1-acceptance-3/soak-B-fixed/minutes.txt) · [判据](logs/round1-acceptance-3/soak-B-fixed/run.txt) · [开窗](logs/round1-acceptance-3/soak-B-fixed/window.txt) |
| 8%/10% · A | 1.346 | 1.587 | 1.907 | +1.08% | 1,303,180 | [分钟表](logs/round1-acceptance-3/soak-A-newgate/minutes.txt) · [判据](logs/round1-acceptance-3/soak-A-newgate/run.txt) · [开窗](logs/round1-acceptance-3/soak-A-newgate/window.txt) |
| 8%/10% · 最终 B | 1.376 | 1.701 | 2.047 | -3.82% | 1,282,954 | [分钟表](logs/round1-acceptance-3/soak-B-final/minutes.txt) · [判据](logs/round1-acceptance-3/soak-B-final/run.txt) · [开窗](logs/round1-acceptance-3/soak-B-final/window.txt) |

旧门槛下，原 B 第 3–7 分钟的 p50 为 83.325 ms，A 为 1.380 ms，相差约 60 倍。原 B 后五分钟升到 99.609 ms，触发漂移。首次修复 B 的 30 分钟 p50 降到 2.229 ms，漂移消失，但仍比同门槛 A 多约 0.852 ms，由此启动二次优化。[原 A](logs/round1-acceptance-3/soak-A/run.txt) · [原 B](logs/round1-acceptance-3/soak-B/run.txt) · [首次修复](logs/round1-acceptance-3/soak-B-fixed/run.txt)

最终两臂均无校验失败、设备移除或资源增长。前后五分钟相比，A 的工作集增加 4 KiB、句柄减少 2 个，B 的工作集减少 172 KiB、句柄数持平；两臂本地及非本地显存的中位数均无变化。[A 原生判据](logs/round1-acceptance-3/soak-A-newgate/run.txt) · [B 原生判据](logs/round1-acceptance-3/soak-B-final/run.txt)

最终 B 第 18 分钟（约 01:15 UTC）的 p99 升至 2.695 ms，第 19 分钟为 2.291 ms，第 20 分钟回到 1.921 ms。用户回忆当时可能在看视频，运行中没有逐分钟的视频或整卡占用记录，无法确认尖峰原因。[B 分钟表](logs/round1-acceptance-3/soak-B-final/minutes.txt) · [B 分钟时刻](logs/round1-acceptance-3/soak-B-final/threads.txt)

### 1 MiB 回写为何变慢，如何验证修复

该负载用 XOR 改写 1 MiB，每轮产生 263,164 个分散的改写段。原 B 为保住其他 CPU 线程刚写入的字节，对每一段调用一次映射写入，正常路径逐段进入 Windows 内核。剖析测得约 97.5% 的轮次时间落在快照回写。首次修复保留逐字节判定，正常路径改为受保护的用户态掩码写，发生访问故障时从失败位置回退，避免重写已完成字节。[排查、开关对照及故障处理](r1-issues-investigation.md) · [原始探针](logs/r1-issues-investigation/probe.txt) · [首次修复验证](logs/r1-issues-investigation/review-fixes/)

第二次剖析定位到混合掩码写。实现解开位扫描与清位的依赖，对满足条件的私有页分组并行，保留顺序回退，同时扩大标记页的读取缓冲。审核又覆盖物理页重映射、并行组故障与稀疏写；最后把别名竞态测试隔离到独立进程。五个用例各跑十次全部通过，移除映射等待的对照版四个别名用例全部按预期失败。四轮审核最终通过，末批提交仅调整测试和记录。[分段计时](logs/r1-issues-investigation/optimize-1mib/profile.txt) · [两轮同窗短测](logs/r1-issues-investigation/optimize-1mib/mark-qualifying.txt) · [复审与对照](logs/r1-issues-investigation/parallel-review-fixes-3/summary.txt) · [完整调查](r1-issues-investigation.md)

## D3D12 Debug Layer 与 GPU-based validation

| 场景 | 普通像素检查 | 开启两层验证 | 错误数 | 失败 / 崩溃 | 日志 |
|---|---|---|---:|---:|---|
| 旧门槛 A | 通过 | 失败 | 3 | 1 / 0 | [结果](logs/round1-acceptance-3/validation-A/results-redacted.json) · [原生输出（脱敏）](logs/round1-acceptance-3/validation-A/run-redacted.txt) |
| 旧门槛原 B | 通过 | 失败 | 3 | 1 / 0 | [结果](logs/round1-acceptance-3/validation/results-redacted.json) · [原生输出（脱敏）](logs/round1-acceptance-3/validation/run-redacted.txt) |
| 修复后，新门槛复测 | 通过 | 通过 | 0 | 0 / 0 | [结果与读回](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/validation.json) · [开窗](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/validation-window.txt) |

两臂原先都出现一次 645（探测用的描述符堆超过设备上限）、两次 1419（首次绘制和 Reset 后，设置直接索引根签名时缺少已绑定的堆）。修复在调试设备上先查询 OPTIONS19 的容量，并先绑定根签名要求的两个堆。修复后颜色、深度读回与预期一致。较旧运行时缺少 OPTIONS19 时，回退探测仍可能触发 645。[错误与读回对照](logs/r1-issues-investigation/validation.txt) · [描述符容量检查](../../crates/victory-gpu/src/descriptor/host.rs) · [修复后验证](logs/r1-issues-investigation/final-validation/validation.json)

## 集成分支合并后检查

Windows 原生 MSVC 加载同一份 `toolchain/env.ps1`。合入调查分支的五个测试与记录提交后，工作区与 GPU crate 经同一显卡排队窗口依次运行。计数只取每个顶层测试程序的最后一条结果，过滤用例启动的子进程。[显卡窗口](logs/round1-acceptance-3/post-merge-checks-3/window.txt) · [提交与计数](logs/round1-acceptance-3/post-merge-checks-3/summary.txt)

| 检查 | 结果 |
|---|---|
| [Clippy，默认特性](logs/round1-acceptance-3/post-merge-checks-3/clippy.txt) · [全部特性](logs/round1-acceptance-3/post-merge-checks-3/clippy_all.txt) | 两套均零诊断 |
| [工作区测试](logs/round1-acceptance-3/post-merge-checks-3/workspace.txt) | 29 个顶层测试程序，976 项通过，0 失败，13 项忽略 |
| [GPU crate 测试](logs/round1-acceptance-3/post-merge-checks-3/gpu.txt) | 2 个顶层测试程序，321 项通过，0 失败，4 项忽略 |
| [仓库资源策略](logs/round1-acceptance-3/post-merge-checks-3/policy.txt) | 通过 |

## 第一轮整体验收总汇总

| 类别 | 两段验收合起来的结论 |
|---|---|
| 保留的收益 | 第一段默认组的 19 项耗时中，10 项在主窗口的家族 95% 区间内确认加快。20 个 shader 的缓存冷写和热命中分别快 18.38%、33.87%，256 MiB 副本刷新和标记页回写分别快 43.12%、48.25%。第二段的三档回写、刷新、完整纹理上传和局部更新在正反序窗口均保留收益，1,300 槽及以上的描述符同步也在两种顺序下加快。90 对默认负载下，两个 dispatch 分别快 19.88% 和 20.52%。[第一段正式组](logs/round1-acceptance-2/formal/paired.json) · [第二段扩样](logs/round1-acceptance-3/dispatch-forward/normalized-timings.txt) |
| 确认的退步 | 16 个热 PSO 预建在第一段反序窗慢 5.17%，一批的中位耗时多 5.40 µs；第二段 90 对窗口慢 3.43%，家族区间为 +0.76% 至 +6.34%。第一段默认工作集峰值配对增加 0.12%，约 1.25 MiB，交换顺序后增加 0.14%。这是单次运行的峰值；持续运行时的增长另按上节的分钟序列判定。[第一段反序窗](logs/round1-acceptance-2/correctness/normalized-reverse.txt) · [第二段热预建](logs/round1-acceptance-3/dispatch-forward/normalized-timings.txt) |
| 未定项 | 256 次提交准备在两个窗口分别为 +5.71%、−4.60%，两个区间都跨零；100 槽描述符同步也未检出稳定变化。单个热 PSO 在第一段两个窗口的家族区间结论不一致。热预建退步在父提交对照中的归因仍不确定。冷预建受益程度随窗口变化，驱动缓存状态未知。[大档正序](logs/round1-acceptance-3/large-forward/normalized-timings.txt) · [大档反序](logs/round1-acceptance-3/large-reverse/normalized-timings.txt) · [父提交对照](round1-acceptance-2.md) |
| 正确性与待补项 | 第一段硬件 diagnostics 为 979 通过、0 失败；默认工作区通过 3 轮，GPU crate 通过 10 轮。强制 WARP 仍有 3 个要求固定 32-lane wave 的用例在创建管线时失败，均在优化前复现。待补的是这三个用例在 WARP 上的实际执行；合成场景也无法给出整款游戏的帧率变化。[硬件和 WARP 结果](round1-acceptance-2.md) |
| 修复后的 1 MiB 长测与验证层 | 同门槛复测的 p50 / p99 为 A 的 +2.2% / +7.3%，均无漂移、资源增长、校验失败或设备移除；验证层零错误。[A 分钟表](logs/round1-acceptance-3/soak-A-newgate/minutes.txt) · [B 分钟表](logs/round1-acceptance-3/soak-B-final/minutes.txt) · [验证结果](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/validation.json) |
| 修复前后仍待观察的差异 | 小档和大档原始 A/B 显示副本刷新整体加快。另以修复版本相互比较，小档刷新 +3.9%，家族 95% 区间 −2.8% 至 +8.1%；大档 +2.1%，逐项区间 +0.9% 至 +3.2%，家族区间 −0.3% 至 +5.3%。这些读数仍需在实机跟踪。[分窗口统计](logs/r1-issues-investigation/parallel-review-fixes-2/new-gate/paired-stats.txt) · [合并两窗的区间](r1-issues-investigation.md) |
| 已知遗留与后续 | 单独修复 victory-hle 的 xasync 竞态。自 `30a5202` 起，`complete_call` 先移除登记，再发布状态，导致等待者偶尔读到 E_PENDING；审核判定不阻塞本轮。实机观察物理页重映射等待并行回写的时长；旧运行时保留 645 的兼容性复测。热 PSO 的后续剖析优先看缓存命中路径。[偶发失败](logs/r1-issues-investigation/parallel-review-fixes-3/workspace-run1.txt) · [单独复跑](logs/r1-issues-investigation/parallel-review-fixes-3/xasync-alone.txt) · [调查与复审](r1-issues-investigation.md) |

## 干净复测

同一份 B 二进制在 RTX 5090 上重跑 30 分钟的 1 MiB 混合轮次，SHA-256 为 `08193123c00bf833443259d42acf6960f0872419e7ed740a9f7038a12de3dac9`。持有原 GPU 互斥锁，保留运行中的服务，用原门槛等待自然空闲。开跑前 10 次整卡占用均为 0%，沿用原预热、测试入口和按 PID 采集线程数的方式。[开跑记录](logs/round1-acceptance-3/soak-B-clean/window.txt) · [原生输出](logs/round1-acceptance-3/soak-B-clean/run.txt) · [线程采样](logs/round1-acceptance-3/soak-B-clean/threads.txt)

下表的 p50、p95、p99 各取 30 个分钟值的中位数，单位为 ms。p50 漂移比较第 3–7 分钟和第 26–30 分钟的中位数。

| 测量 | p50 | p95 | p99 | 第 18 分钟 p99 | 最高分钟 p99 | p50 漂移 | 完成轮数 | 分钟记录 |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| A | 1.346 | 1.587 | 1.907 | 1.969 | 2.169（第 19 分钟） | +1.08% | 1,303,180 | [A](logs/round1-acceptance-3/soak-A-newgate/minutes.txt) |
| 上次 B | 1.376 | 1.701 | 2.047 | 2.695 | 2.695（第 18 分钟） | −3.82% | 1,282,954 | [上次 B](logs/round1-acceptance-3/soak-B-final/minutes.txt) |
| 复测 B | 1.218 | 1.660 | 2.021 | 2.012 | 4.189（第 1 分钟） | −3.48% | 1,340,256 | [复测 B](logs/round1-acceptance-3/soak-B-clean/minutes.txt) |

第 18–20 分钟的 p99 为 2.012、1.978、2.255 ms，旧尖峰没有在同一分钟复现。不过第 1、8、10、11 分钟分别达到 4.189、3.043、2.864、3.457 ms，都超过上次 B 的峰值。外部每 5 秒采样一次，361 次均有效，整卡利用率中位数 6%，范围 5%–22%。所有采样中仅测试进程出现过非零 3D/Compute 占用，另有两次系统 Copy 引擎活动，最高 3%。第 18 分钟的 12 次采样也只见测试进程使用 3D。两次采样之间的短时活动仍可能漏过，上次运行缺少同期记录，尖峰与视频播放的因果关系仍未确定。[逐分钟延迟](logs/round1-acceptance-3/soak-B-clean/minutes.txt) · [整卡及各进程引擎采样](logs/round1-acceptance-3/soak-B-clean/gpu-during.txt)

三次运行的校验失败和设备移除均为 0，均未触发延迟漂移或资源增长判据。复测 B 前后两段的工作集、句柄、本地及非本地显存中位数均持平，全程句柄为 370–378 个，线程为 15–17 个，与上次 B 相同。[A 判据](logs/round1-acceptance-3/soak-A-newgate/run.txt) · [上次 B 判据](logs/round1-acceptance-3/soak-B-final/run.txt) · [复测 B 判据](logs/round1-acceptance-3/soak-B-clean/run.txt)
