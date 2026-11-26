# 候选方案与检查范围

每个社区候选的散列是上游 PR 的末端提交，包含早先的依赖提交。下表记录实际完成的检查或对应的专用用例。正式 A/B 的数值按每个指标的原始报告填写。

| 候选 | 实现或想法 | 检查范围 |
|---|---|---|
| [VIctory #3152](https://github.com/Marvisak/VIctory/pull/3152) · `86eb516` | 同一份 Module 规划供两个 bitcode 阶段使用 | 基线及候选的 DXIL 摘要相同，另有针对活跃性规划的 CPU 对照。 |
| [VIctory #2787](https://github.com/Marvisak/VIctory/pull/2787) · `38614b4` | 预建复用枚举结果与着色器哈希 | 4,000 键的 30 对非独占 Windows 测试中位数为 168.744 → 23.126 ms，键值相同。 |
| [VIctory #3047](https://github.com/Marvisak/VIctory/pull/3047) · `3c4c7d8` | 从寄存器恢复光栅状态 | 在一个已有散列、缺少名称的合成记录上恢复光栅状态，深度状态仍未恢复。 |
| [VIctory #3048](https://github.com/Marvisak/VIctory/pull/3048) · `1d5afa0` | 继续恢复深度状态 | 与 #3047 组合后，同一记录的光栅、深度状态均通过实际 CityHash64 检查。 |
| [VIctory #2828](https://github.com/Marvisak/VIctory/pull/2828) · `b80763c` | 验证争用时实际发布成功的文件 | 基线通用缓存与处理文件内容不一致的上游修复单独构建。平铺候选在实际 Windows 文件共享冲突下能区分有效和损坏的胜出文件。 |
| [VIctory #2614](https://github.com/Marvisak/VIctory/pull/2614) · `8c42f14` | 独立保存已解析的 shader 元数据 | 键只包括容器散列。同一构建的诊断选项变化、翻译器版本变化与记录损坏均须考虑。 |
| [VIctory #2706](https://github.com/Marvisak/VIctory/pull/2706) · `da38ab4` | 每个翻译器分别保存 PSO 库 | 七个新进程的用例里，运行时选项共用同一库，WARP 切换回硬件时重建。 |
| [VIctory #2646](https://github.com/Marvisak/VIctory/pull/2646) · `940d410` | 分离硬件与 WARP 的库 | 七次新进程检查中生成两个库，切回硬件时命中。上游 #2637 是问题报告。 |
| [VIctory #2803](https://github.com/Marvisak/VIctory/pull/2803) · `1226f29` | 库首次打开时固定目录，并独立维护 early list | 同样隔离两个适配器。迟配置场景用实际驱动库重新打开后再执行计算。 |
| [VIctory #3181](https://github.com/Marvisak/VIctory/pull/3181) · `8a216f5` | 只改 early list 时跳过整库序列化 | 专用用例在已有 64 个计算 PSO 的库上只更新下一次应预建的名称。 |
| [VIctory #3188](https://github.com/Marvisak/VIctory/pull/3188) · `d5b24d1` | 另一版独立保存 early list | 与 #3181 共用同一个 64 PSO 及目标文件被占用的重启用例。 |
| [VIctory #3025](https://github.com/Marvisak/VIctory/pull/3025) · `018b56a` | 为图形 PSO 保留工作线程 | 受控队列用例能降低图形等待，原补丁的线程数随启动时序变化。修正方案等待工作线程总数初始化。 |
| [VIctory #2897](https://github.com/Marvisak/VIctory/pull/2897) · `5f82c56` | 让深度绘制更早得到需要的 PSO | 专用场景会验证四个深度像素，并记录从投递背景编译到第一个有效绘制的时间。 |
| [VIctory #2951](https://github.com/Marvisak/VIctory/pull/2951) · `2a7976c` | 分别记录翻译和 PSO 耗时 | 提供诊断可见性，验收其输出与正常运行开销。 |
| [VIctory #2907](https://github.com/Marvisak/VIctory/pull/2907) · `69a0d48` | 自动启动预热流程 | 该实现涉及应用启动、窗口和 host 集成。现有授权合成输入未提供可计时的交互式标题启动。 |

bca3ecd（#2055）和 555c112 已在比较双方的历史中。xcl 的三个思路单独处理：#744 对应跨进程驱动 blob 的实际创建、重新打开和 texel 检查；#2098 需要至少 256 KiB 的单个 DXIL，以真实驱动编译和内存峰值判断；#2371 会重写 DXIL，与逐字节保持相同的验收条件冲突。项目没有移植 xcl 的源代码。
