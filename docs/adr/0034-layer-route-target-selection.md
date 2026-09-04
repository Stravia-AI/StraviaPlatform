---
status: accepted
---

# 按亲和、优先级与调度分层选择 Target

Stravia 不再把 `Route.balance` 当成互斥的四档选路。Target 选择改为固定分层：Target Continuation 硬约束；Conversation Affinity；无显式身份时的 Cache Affinity；Target Priority；同组 Route Scheduling Strategy。失败、重试与冷却是资格过滤，不是又一种 balance。

Conversation Affinity 使用已有身份，不新建 Session：Generation Chain 父节点，否则 Prompt Cache Directive 路由键。同一身份软粘上次成功 Target，可压过 Target Priority；不同身份不共享；两种都没有才走 ADR-0023 的前缀 Cache Affinity（仍可压过 Priority）。这收窄了 ADR-0023「适用于所有经 Route 的调用」：共享系统提示不得把无关对话粘到同一 Target。

Target Priority 是 0–100000 的分组整数，越大越优先，缺省 0；取消 1|2 上限。旧数据全部迁成 0，不反转旧序号。同组由 Route Scheduling Strategy 二选一，缺省 Traffic Equalization：把下一个请求给 24h 加权 token 流量最低的 Target。权重组内共用——全组无价时为缓存输入 0.1、未命中输入 1、输出 5、缓存输出 6；有计价（`cost_input > 0` 且有 `cost_output`）时用价格比平均，缺维回退缺省；忽略 reasoning、audio 与 200k 分层。流量按 `provider_id` + upstream model 跨 Route 累计，只计成功，进行中占位，失败只冷却。Latency Preference 用 1h 成功率 × 输出 tok/s；成功样本 < 20 视为无数据，组内有效 Target < 2 则回退 Traffic Equalization。

瞬时失败（含 First Token Timeout）可在同一 Target 上额外重试 5 次（共 6 次），间隔 0.5s 起、×2、封顶 8s、full jitter；429 `Retry-After` 优先。用尽后换 Target 并进入 120s Target Cooldown。QuotaExceeded 不在同 Target 空转，只换 Target 并冷却，偏离「quota 不换 Provider」的旧文档。Auth、InvalidRequest、ContextLength、ContentFiltered 立刻失败整次请求。First Token 是上游第一个 canonical 输出，包含 Thinking，缺省 60s。Client Output Commit 之后仍禁止换 Target。Affinity 让位冷却；Continuation 目标在冷却中则放弃续接、完整重放。删除 Target.weight。旧 `weighted` / `priority` / `cooldown` 映射为 Traffic Equalization，`latency` 映射为 Latency Preference。

## Considered options

- 新建客户端 Session 并硬粘 Target：与 glossary 冲突，且 ADR-0023 已拒绝按连接或 Session 固定 Target。
- Cache Affinity 只在同 Priority 组内提权：保住主/备，但放弃 20k+ 前缀的跨组缓存命中。选择维持 ADR-0023 的全列表提权，并用 Conversation Affinity 避免跨对话误粘。
- 按美元成本均衡：request_logs 没有实付；目标是加权流量，不是账单。
- 保留旧四档 balance，只在 priority 策略内部再分调度：Priority、冷却与抽样会继续打架。
- 额度耗尽也不换 Target：多账户备份无法消化 QuotaExceeded。

## Consequences

- 管理面必须能显式设置 Target Priority 与失败旋钮，不能再用列表 `index+1` 当优先级。
- 选路要读 24h/1h 的 request_logs 聚合与进行中占位；冷却保持进程内。
- 529 / QuotaExceeded 与 HTTP `is_retryable(status)` 必须对齐到三档处置，不能再两套判定。
- Target Priority 的合法范围由 ADR-0036 改为有符号 32 位全区间；本 ADR 的分层选择顺序不变。
- 管理面用优先级泳道表达这些组，见 ADR-0037；已禁用 Target 见 ADR-0035。
