---
status: accepted
---

# 守护条目耗尽时持久化暂停 Provider 路由资格

本决策部分修正 ADR 0029 与 ADR 0032 中“Provider Allowance 不改变路由资格”的约定；Monitor registry、凭据边界、官方额度端点、TTL 缓存与 Allowance Sample 语义保持不变。

管理员需要让订阅额度耗尽的上游账户自动退出路由，并在额度重置后自动回归。因此 Stravia 允许实例管理员为每个具备 Monitor 能力的 Provider 选定守护条目（仅限账户级 Allowance Item）。一次成功的 fresh 读取显示任一守护条目达到现有“耗尽”阈值（已用 ≥ 100% 或剩余 ≤ 0）时，Provider 进入持久化的额度暂停；该 Provider 在所有 Route 上的 Target 在调度快照装配时失去资格，判定方式与凭据失效相同。只有新的成功读取显示全部守护条目都已不再耗尽，或管理员取消相关守护，才解除暂停。到达 `reset_at` 只触发一次强制读取，不直接解除；读取失败或守护条目缺失时维持原状态。没有 `reset_at` 的余额类条目只能通过读取结果恢复。

额度暂停与 Target `enabled`、Provider `is_enabled` 和 `credential_status` 互相正交。守护配置独立于 Provider 连接配置保存，修改它不会改变 Provider revision，也不会复位凭据失效。暂停状态写入共享数据库：只接受读取完成时间比已记录证据更新的结果，并以读取发起时的凭据版本为写入条件，与 ADR 0073 同构。以下时机会对相关 Provider 发起强制读取：守护配置保存、Provider 非展示字段变更、到达 `reset_at`、手动刷新、Gateway 启动、Provider 重新启用。其余时候沿用 30 分钟一次的采样周期，不因守护而缩短，上游 `QuotaExceeded` 也不触发读取。

## Considered Options

- 自动改写 Target `enabled`：会违反“Route 至少有一个已启用 Target”的约束；恢复时会把管理员在暂停期间手动禁用的 Target 重新启用；也会混淆管理员意图与上游证据，与 ADR 0073 的取舍相矛盾。
- 按进程派生、不落盘：重启后在首次读取前会放行，请求先撞上上游 429；多个实例的状态也可能不一致。
- 到达 `reset_at` 即乐观恢复：重置时刻并不等于上游实际恢复的时刻，余额类条目也没有这个值。
- 把上游 `QuotaExceeded` 直接当作暂停证据，或缩短读取周期：前者会把单个模型或单个请求的错误放大成整个 Provider 的状态；后者会增加上游额度接口的调用量。最终接受最长约 30 分钟的检测延迟。
- 按 API Key 配置守护：路由资格是 Provider/Target 级别的，所有 Principal 共享同一条 Route，按 Principal 区分需要额外拆出一层路由资格。

## Consequences

- 选路、Conversation Affinity、Cache Affinity 和 Target Cooldown 都要跳过被暂停 Provider 的 Target；暂停不计入 Target 的失败次数，进行中的请求不中断。
- 只有当所有候选都因额度暂停而不可选时，客户端才收到额度暂停专用错误：HTTP 429，按各 ingress 协议原生的额度耗尽错误形态返回（OpenAI 兼容协议使用 `insufficient_quota` 风格），不带 Retry-After，不承诺恢复时间。原因混合时沿用现有的 `provider_unavailable`。这类请求属于失败的请求。
- 模型发现、Supported Thinking Levels、客户端配置导出以及 Route 的“至少一个已启用 Target”约束不受暂停影响。
- 需要在 SQLite 与 PostgreSQL 中新增守护配置和暂停状态的存储，并重新生成参考 schema。
- 守护配置在额度页按条目设置；额度页、Provider 列表与详情页、Target 徽标三处展示暂停状态，徽标优先级为：凭据失效 > 额度暂停 > 冷却。不提供“强制恢复”，也不做外部通知。
