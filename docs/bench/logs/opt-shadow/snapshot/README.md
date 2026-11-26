# 重叠快照回写中的 CPU 写入

上游主分支 `63bcca4` 在 RTX 5090 和 WARP 上都丢失了同一个 CPU 字节。两次测试均创建 D3D12 设备和共享缓冲区，在缓冲区中设置着色器输出，再让一个 Windows 线程向真实的受跟踪内存写一次。这是对回写时序的定点测试，GPU 执行时间不在测量范围内。

| 实际 DXGI 适配器 | 上游主分支 | 独立修复 `b1fb182` |
|---|---|---|
| NVIDIA GeForce RTX 5090，硬件 | [失败，读回 00](main-hardware.txt) | [通过，保住两次 CPU 写入](standalone-hardware.txt) |
| Microsoft Basic Render Driver，软件 | [失败，读回 00](main-warp.txt) | [通过，保住两次 CPU 写入](standalone-warp.txt) |

两个声明覆盖同一页内的第 2–5 和 3–7 字节。模拟的 GPU 输出将第 2、5 字节写为 A1、A2。第一次回写已经读完页内数据时，CPU 线程把第 4 字节写为 C1，仅写一次。上游读回 A1 00 00 A2，期望 A1 00 C1 A2。随后 CPU 把第 4 字节写为 C2，第二次重叠回写也必须保留 C2。证据程序同时检查 DXGI 返回的实际适配器名和软件标记。每个适配器、每个版本使用独立的 Windows 原生进程。

修复比较提交前的快照和 GPU 输出，只向发生变化的连续字节区间发起写入。完整覆盖的页先恢复写保护，再将当时的实际内存读回副本。已处理的局部快照也更新基线，后续重叠声明可以区分这些 CPU 字节和 GPU 的修改。该修改使用既有的受检内存写入接口，独立于标记页调度、工作线程数和显卡型号。

副本中可向上游单独提交的本地分支是 `fix/shadow-snapshot-race`，以 `63bcca4` 为父提交。开发分支在 `6b1e408` 后的第一个提交是 `28de15a`。这两个提交只改 `crates/victory-gpu/src/shadow.rs`，同时包含用例与修复，稳定补丁标识均为 `1f50dbf216532c3a4f798adbc198cbf4eda412e6`。矩阵中的“测试提交”加入了适配器记录以及等价的定点读后回调。[逐版本证据](matrix.json)

社区已经报告过读取与写入之间的覆盖和未及时刷新副本两个问题。关闭的 #2009 将快照回写限制在连续的差异字节，适合用来检验第一次写入时的竞争。#2242 和后续的 #2280、#2409 会把间隔小于 256 字节的差异段合并，因而一次短小的 CPU 写入仍可能落在跨度之内。社区提交列的源码来自原 PR 头，实际运行列使用移植到同一开发基线的代码。[覆盖问题 #721](https://github.com/Marvisak/VIctory/issues/721) · [副本过期问题 #722](https://github.com/Marvisak/VIctory/issues/722) · [旧修复 #2009](https://github.com/Marvisak/VIctory/pull/2009) · [差异区间实现 #2242](https://github.com/Marvisak/VIctory/pull/2242)

在 RTX 5090 上增加一次前置 CPU 写入后，可以区分 #2009 与本修复。CPU 先写 B1，第一次快照读取后再写 C1，第二次回写前写 C2。#2009 保住 C1，却在第二次回写把 C2 改回第一次读到的 B1。本修复两次都通过。这个例子同时检查是否写了正确的字节，以及重叠声明是否仍在使用过期的比较基线。[开发基线实测](prior-base.txt) · [#2009 实测](prior-2009.txt) · [独立修复实测](prior-standalone.txt)

也直接测试了待提交的用例：在上游未修复的路径中只加入该用例及其读后回调，再与独立修复的原始提交比较，硬件和 WARP 都从失败变为通过。[硬件修复前](clean-snapshot-red-hardware.txt) · [硬件修复后](clean-snapshot-green-hardware.txt) · [WARP 修复前](clean-snapshot-red-warp.txt) · [WARP 修复后](clean-snapshot-green-warp.txt)
