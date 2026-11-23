---
status: accepted
---

# Model Turn 在还原与映射发布后交出唯一成功终态

过去 buffered、live 与内部 Agent 分别解释 Model Turn 的成功点并触发凭据映射发布。Stravia 在共同的 canonical stream 内集中处理：先完成输出还原并交出尾部 delta，再发布本轮所需映射，成功后才交出唯一终态 `Completed`。不等 EOF 来判定成功；完成事件前的 EOF、还原或发布失败都不是成功。空映射可以跳过文本还原，不能跳过终态 gate 或以早期快照代替发布时读取共享引用。

取消与超时可以打断等待，不能倒转已交出的终态，也不能证明持久化尚未提交。映射可能为同一 Principal 的多个请求所用，不在取消时回滚发布。发布不等待客户端交付，也不并入 Generation Chain 提交；否则内部 Agent 无法独立保证终态，且会把真实上游结果与交付混为一谈。Target attempt 与用量仍报告真实上游结果，Model Turn 的成功观测只在 gate 成功后记录。当前执行与凭据生命周期见[整体架构](../design/architecture.md)与[凭据保护设计](../design/credential-protection.md)。
