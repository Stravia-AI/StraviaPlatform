---
status: superseded by ADR-0022
---

# Checkpoint Generation Chains without duplicating history

Generation Chain 节点始终保存本轮 canonical 输入 delta、最终 canonical 输出和 resolved profile delta；Stravia 只在固定间隔、分支根或 Hook/Provider 改写既有历史时写入完整 effective execution context 的 Generation Checkpoint。物化从最近 Checkpoint 重放后续增量，既不重跑可变 Hook，也不让每个节点复制完整祖先历史。

## Considered options

- 每节点保存完整 effective request：读取简单，但长链每次追加复制祖先上下文，存储写放大随链长度平方增长。
- 只保存增量并重跑 Hook/Vendor mutation：空间最小，但升级、配置或 Hook 行为变化会改写已执行历史。
- 采用增量加条件 Checkpoint：把精确重建和历史不变性收进 Generation Chain，并减少相对每节点完整快照的写放大。

## Consequences

- Checkpoint 是物化加速器而不是第二个事实源；输入 delta、最终输出与 profile delta 仍定义历史。
- Target Continuation 和 Cache Affinity 只使用由节点及 Checkpoint 精确物化的 Effective Model Request，不能以当前 Hook 重算的结果替代。
- 完整 Checkpoint 会随历史增长而变大；固定间隔只能限制重放，并不能使长期持久化的总写入保持线性。若长期链的写放大不可接受，应改用不持久化完整快照的物化缓存，或使用结构共享的 checkpoint 表示。
