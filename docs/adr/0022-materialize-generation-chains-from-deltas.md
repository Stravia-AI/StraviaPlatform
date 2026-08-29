---
status: accepted
---

# Materialize Generation Chains from deltas

Stravia 将 Generation Chain 的持久节点限定为 canonical 输入 delta、最终输出和 resolved profile delta；Gateway 使用按字节上限淘汰的进程内 LRU Generation Materialization Cache 加速精确重建。完整 durable Checkpoint 会持续复制增长中的历史，且现有逐祖先物化接口不会从中获益，因此不作为当前存储格式。

## Consequences

- Cache key 以 Principal、不可变节点 ID 与 payload version 隔离；它不是历史事实源，重启、淘汰或不一致时必须从 immutable delta 按父节点顺序重放，绝不重跑 Hook。
- Target Continuation、Automatic Parent Discovery 与 Cache Affinity 只使用精确物化的历史，不能依赖当前 Hook 或 Provider mutation 重新计算。
- `TurnChainStore` 必须以批量或递归查询读取祖先链，避免把一次冷物化变成每个祖先节点一次数据库往返。
- 只有基准测试证明冷链物化尾延迟不可接受时，才考虑带结构共享和 GC 的持久 checkpoint；周期性复制完整 JSON 不再是候选方案。
