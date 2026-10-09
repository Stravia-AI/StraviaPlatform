---
status: accepted
---

# Materialize Generation Chains from deltas

Stravia 将 Generation Chain 的持久节点限定为 canonical 输入 delta、最终输出和 resolved profile delta；Gateway 使用可丢弃的 Generation Materialization Cache 加速精确重建。完整 durable Checkpoint 会持续复制增长中的历史，且现有逐祖先物化接口不会从中获益，因此不作为当前存储格式。

当前缓存后端修正原来的进程内 LRU 实现：物化对象及按 ingress 隔离的祖先引用目录进入同一 Gateway 的统一派生缓存，Memory / SQLite 使用 TinyUFO，PostgreSQL 使用 Redis，共享逻辑字节预算。后端变化不改变本 ADR 的 delta 存储决定或 SQL 历史事实源；配置与单实例边界见[存储架构](../design/architecture.md#统一派生缓存与-server-配置)。

## Consequences

- Cache key 以 Principal、不可变节点 ID 与 payload version 隔离，引用目录另按 ingress 隔离；它不是历史事实源，重启、淘汰、超出预算、读取失败或不一致时必须从 immutable delta 按父节点顺序重放，绝不重跑 Hook。
- Target Continuation、Automatic Parent Discovery 与 Cache Affinity 只使用精确物化的历史，不能依赖当前 Hook 或 Provider mutation 重新计算。
- `TurnChainStore` 必须以批量或递归查询读取祖先链，避免把一次冷物化变成每个祖先节点一次数据库往返。
- 只有基准测试证明冷链物化尾延迟不可接受时，才考虑带结构共享和 GC 的持久 checkpoint；周期性复制完整 JSON 不再是候选方案。
