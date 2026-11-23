---
status: accepted
---

# 同一客户端交互允许跨越多个 Generation Chain 根

客户端裁剪图片或以摘要替换历史后，严格历史匹配可能要求创建新的 Generation Chain 根，但这不必然表示开始了新的客户端交互。Stravia 决定：同一 Principal 下，经充分证据确认仍在推进同一任务的 Inference Run 归入同一个 Connect Client Interaction，而不是仅在多个独立 Interaction 之间增加关联导航。

该 Interaction 统一汇总活动状态、Confirmed Upstream Usage，并确定 Interaction Debug Bundle 的导出范围；各 Inference Run 的执行记录和实际 Generation Chain 根、父边保持原样。诊断归并不得恢复客户端已删除的图片或压缩前历史，不授予权限，也不据此启用 Target Continuation；证据不足时保持独立。

保留多个独立 Interaction 的方案更保守，但会让任务状态、用量和最终回复继续分散，仅增加导航无法解决任务归属问题。选择统一归属意味着误合并会同时影响状态、统计和诊断导出，不能用 session 相同、时间接近或模糊文本相似代替跨根续接证据。

归属评估先核验同 Principal 已交付来源的当前工具续接，即使请求已有较早的 Generation parent 或夹带 User 输入；旧工具结果的回放或编辑不等同于完成当前待处理工具。没有当前工具来源时，沿用已确认的 Generation parent；无执行父边且存在唯一、完整的保留尾部匹配时，才考虑按尾部归属。只以实际完整交付到本次入口的时间计五分钟归并窗；尾后有新增 User 或超出窗口则保留独立的 Interaction，只连接诊断来源。证据不完整或来源不唯一时不猜测。

本决策不重算既有 Run 的分组。判定优先级、重启和裁剪下的证据、分支与保留尾部见[请求记录设计](../design/interaction-observation.md)；[Open Responses 设计](../design/open-responses.md)仍拥有严格执行父边语义。
