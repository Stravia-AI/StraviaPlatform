---
status: accepted
---

# 同一客户端交互允许跨越多个 Generation Chain 根

客户端裁剪图片或以摘要替换历史后，严格历史匹配可能要求创建新的 Generation Chain 根，但这不必然表示开始了新的客户端交互。Stravia 决定：同一 Principal 下，经充分证据确认仍在推进同一任务的 Inference Run 归入同一个 Connect Client Interaction，而不是仅在多个独立 Interaction 之间增加关联导航。

该 Interaction 统一汇总活动状态、Confirmed Upstream Usage，并确定 Interaction Debug Bundle 的导出范围；各 Inference Run 的执行记录和实际 Generation Chain 根、父边保持原样。诊断归并不得恢复客户端已删除的图片或压缩前历史，不授予权限，也不据此启用 Target Continuation；证据不足时保持独立。

保留多个独立 Interaction 的方案更保守，但会让任务状态、用量和最终回复继续分散，仅增加导航无法解决任务归属问题。选择统一归属意味着误合并会同时影响状态、统计和诊断导出，不能用 session 相同、时间接近或模糊文本相似代替跨根续接证据。

已确认的当前工具续接优先于保留尾部归并：请求回传来源 Run 当前待完成工具调用的结果时，即使同时夹带新增 User 输入，也继续原 Interaction。只有未满足当前工具续接条件时，才应用下面的保留尾部、新 User 分支和五分钟窗口规则；历史回放中的旧工具结果不能触发该优先级。

即使没有当前工具结果，唯一且包含完整连续交互的保留尾部精确匹配也可以作为自动归并依据，但必须满足时间窗口约束。时间窗口限制的是这条尾部归并路径；时间接近本身不是任务续接证据，不能放宽唯一性、完整性或 Principal 隔离。

保留尾部路径确认来源后，若匹配区间之后还有新的 User 输入，则创建新的 Connect Client Interaction 节点，并在诊断树中连接到来源 Interaction，而不是归入原 Interaction。两个交互分别汇总状态、Confirmed Upstream Usage 和 Debug Bundle 范围；该连接表达诊断来源，不建立 Generation Chain 执行父边。

时间窗口仅限制归入同一个 Interaction，不限制诊断来源连接。即使没有新增 User 输入，超出归并窗口也创建新的 Interaction；只要来源记录仍在保留期内，且匹配证据完整、唯一，仍连接来源 Interaction。先确认来源，再决定是否归并，不能先按归并窗口排除较旧候选而将歧义误判为唯一。

尾部自动归并窗口为五分钟，包含边界：本次请求入口接收时间减去被匹配来源 Run 完整交付给客户端的时间，须落在 `[0, 300000]` 毫秒内。不使用任务开始时间、Interaction 的可变 `last_active_at` 或数据库写入时间，避免长任务、其他分支活动及持久化排队改变归并资格。

本次修复只改变启用新规则后准入请求的归属判定，不重新分配已有 Run 的 Interaction、不改写其已记录的来源关系，也不提供存量 Observation 重建工具。已发现的两次裁图和一次摘要替换断点用于隔离回归样本，不直接修改 release 数据库；已有错误分组不会因本次修复而自动纠正。

本决策记录已确认的目标归属语义、受时间约束的尾部归并原则和新交互的诊断来源连接，不表示跨根归并已经实现。它扩展 [ADR-0040](0040-own-interaction-observation-outside-generation-chain.md) 的诊断归属语义，保持 [ADR-0020](0020-discover-generation-parents-from-strict-canonical-history.md) 的严格执行历史匹配契约不变。
