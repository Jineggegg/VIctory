# 回写审核复测

同一 shadow 的同一页若同时被普通位图和 withheld 位图标记，位图无法区分该页内两个程序各自写过哪些字节。现在先从普通标记中移除两张位图的交集，再发布其余普通页，最后处理 held 页的失效。每个同时存活的 shadow 占用独立的位号，因此两个虚拟地址映射同一物理页时，其普通副本仍可先回写。标记集合按位号升序产生，交集扫描使用单向游标，无需再分配内存。

三项定点场景均在独立 Windows 原生进程里运行。测试版额外读取实际 D3D12 设备的 DXGI 适配器名称，并输出断言所检查的字节。以下测试的修复前版本只添加用例与诊断输出。完整工作区使用提交中的源文件。[原始输出清单](matrix.json)

| 定点场景 | 修复前 | 修复后 | 独立适配器与输出 |
|---|---|---|---|
| 未声明范围，两个程序分别写同一页偏移 100、200，旁页另有普通写 | 标题内存和下次刷新均得到 A1 B2 C3，应保留的两个字节被写回 | 标题内存和刷新后的副本均为 00 00 C3；只重取 64 KiB，旁页照常回写 | [5090 修前](opt-red-hardware.txt) · [修后](opt-green-hardware.txt) · [WARP 修前](opt-red-warp.txt) · [修后](opt-green-warp.txt) |
| 整片快照先回写 A1 A2 A3 A4，CPU 写 C2，随后回写较小声明 | b1fb182 将 C2 恢复成旧 A2 | 新整片检查保留 A1 C2 A3 A4 | [5090 修前](up-red-hardware.txt) · [修后](up-green-hardware.txt) · [WARP 修前](up-red-warp.txt) · [修后](up-green-warp.txt) |

另在真实的 64 KiB Windows 分配中释放一个 4 KiB 子块，保留它之后的四字节声明可读。开发分支旧代码在整片读取失败后仍记为已完成，四字节保持 00 00 00 00。修复后子范围重试，读回 A1 A2 A3 A4。独立分支旧版原本允许该子范围重试，移植完成位检查后继续通过。[开发版 5090 修前](opt-red-hardware-hole.txt) · [修后](opt-green-hardware-hole.txt) · [开发版 WARP 修前](opt-red-warp-hole.txt) · [修后](opt-green-warp-hole.txt) · [独立版 5090](up-green-hardware-hole.txt) · [独立版 WARP](up-green-warp-hole.txt)

开发分支已有整片完成位检查，本次加上能直接检验它的用例。独立上游分支同步修复了局部声明对已完成整片的查询。两个分支的相关测试均在创建设备失败时按原文件惯例返回。之前的局部声明重叠修复及逐候选比较见[原始记录](../snapshot/README.md)。

| 交付代码 | Windows 完整工作区（通过 / 失败 / 按配置忽略） | 诊断版 victory-gpu（通过 / 失败 / 按配置忽略） |
|---|---|---|
| ffe6a6f | [938 / 0 / 10](opt-workspace.txt) | [305 / 0 / 14](opt-gpu.txt) |
| 31626ec | [868 / 6 / 10](up-workspace.txt) | [247 / 2 / 1](up-gpu.txt) |

工作区依次覆盖非 VALU、非 normalized 的 VALU、normalized VALU 三组互斥的测试名；GPU 和其他默认包串行，后两组各用四个测试线程。统计按各测试程序的最终汇总计数，HLE 自行重跑的一个子进程用例只计一次。若按此前报告直接累加 libtest 各行，开发分支会多计这一次，得到 939 次通过。

独立上游基线在这台 5090 上有以下既有失败，均在父提交 b1fb182 另行复现。开发分支全测包含这些用例。

- 描述符堆容量小于预期。[父提交原始输出](parent-descriptor-host-tests-dynamic_slots_are_held_until_released.txt)
- 测试直接从堆读取纹理时，Windows 着色器进程以访问冲突退出。[父提交原始输出](parent-emit-memory_tests-heap_direct_texture_reads_mark_their_heap_index.txt)
- 使用视图步长读结构化缓冲时，硬件读回数据与预期不符。[父提交原始输出](parent-emit-stride_tests-a_heap_buffer_read_by_element_takes_the_views_stride.txt)
- 缺少缓冲视图时，硬件拒绝分派。[父提交原始输出](parent-emit-stride_tests-a_heap_buffer_read_where_no_buffer_view_is_reads_zeros.txt)
- 在指定层级聚集纹理样本时，硬件返回值与预期不符。[父提交原始输出](parent-emit-synthetic_tests-gathers_at_a_level_match_the_hardware.txt)
- 动态槽之后的视图缺失。[父提交原始输出](parent-memory-tests-lists_share_a_table_and_copies_keep_their_views.txt)

上游着色器测试进程中止后，按原生测试程序列出的用例名逐批补跑剩余项。[按名称核对的结果](up-shader-cases.json) · [中止后的原始输出](up-shader-continuation.txt)

两个分支的资产策略均通过。[开发分支](opt-policy.txt) · [独立分支](up-policy.txt)

原性能基准只向普通位图设置位号，withheld 声明另由区间处理。新增交集扫描仅在 held 位号非空时进入，普通基准每次提交只增加一次空集合判断。没有重跑正式性能采样，原 19 项数字仍对应此前的三臂终选。
