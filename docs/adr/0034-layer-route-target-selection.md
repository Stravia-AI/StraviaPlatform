---
status: accepted
---

# 按亲和、优先级与调度分层选择 Target

Stravia 不再把 `Route.balance` 当成互斥的四档选路。Target 选择改为固定分层：Target Continuation 硬约束；Conversation Affinity；无显式身份时的 Cache Affinity；Target Priority；同组 Route Scheduling Strategy。失败、重试与冷却是资格过滤，不是又一种 balance。

Conversation Affinity 使用已有身份，不新建 Session：Generation Chain 父节点，否则 Prompt Cache Directive 路由键。同一身份软粘上次成功 Target，可压过 Target Priority；不同身份不共享；两种都没有才走 ADR-0023 的前缀 Cache Affinity（仍可压过 Priority）。这收窄了 ADR-0023「适用于所有经 Route 的调用」：共享系统提示不得把无关对话粘到同一 Target。

Target Priority 是有符号 32 位分组整数，越大越优先，缺省 0；取消 1|2 上限（合法范围由 ADR-0036 扩展）。旧数据全部迁成 0，不反转旧序号。同组由 Route Scheduling Strategy 二选一，缺省 Traffic Equalization：把下一个请求给 24h 加权 token 流量最低的 Target。权重组内共用——全组无价时为缓存输入 0.1、未命中输入 1、输出 5、缓存输出 6；有计价（`cost_input > 0` 且有 `cost_output`）时用价格比平均，缺维回退缺省；忽略 reasoning、audio 与 200k 分层。流量按 `provider_id` + upstream model 跨 Route 累计；每个真实 Target attempt 明确上报的 usage（包括失败、重试与 failover）只计一次，未知维度保持未知，进行中另加占位，上游失败另按下述统一规则计数。Latency Preference 用 1h 成功率 × 输出 tok/s；成功样本 < 20 视为无数据，组内有效 Target < 2 则回退 Traffic Equalization。

普通状态下，同一 `provider_id:model` 的内部重试与跨请求终态上游失败共用连续失败计数；完整成功清零。Target Retry Budget 为 N 时，第 N+1 次连续失败才进入 Target Cooldown，缺省 5 即第 6 次失败后冷却 120s。瞬时失败（含 First Token Timeout）仍可按错误分类在同一 Target 重试，间隔 0.5s 起、×2、封顶 8s、full jitter；429 `Retry-After` 优先。QuotaExceeded 计数但不在同 Target 空转，直接换 Target；Auth、InvalidRequest、ContextLength、ContentFiltered 等终态上游错误也计数，但仍立刻失败整次请求。取消、本地准备、Hook 与存储错误不计数。First Token 是上游第一个 canonical 输出，包含 Thinking，缺省 60s。Client Output Commit 之后仍禁止换 Target，只终止当前请求；Commit 本身不计数也不单独触发冷却，其后的真实上游失败照常计数。Affinity 让位冷却；Continuation 目标在冷却中则放弃续接、完整重放。删除 Target.weight。旧 `weighted` / `priority` / `cooldown` 映射为 Traffic Equalization，`latency` 映射为 Latency Preference。

本段明确替换本 ADR 此前的「重试预算用尽、QuotaExceeded 或请求放弃当前 Target 即立即冷却」规则：普通 Target 只在共享连续失败数达到统一阈值时冷却，单次流式失败、QuotaExceeded 或已提交输出均不会仅凭自身触发冷却。Target Cooldown 到期后进入半开，而不是全量恢复；下一个实际符合选路条件的请求独占一次探测机会。探测关闭同 Target 预算重试和 ProviderCall 内部回退，完整成功清零并恢复正常；任何上游探测失败（含已经发出上游请求后的超时）立即重新等待完整冷却。用户取消、本地准备失败和消费者断开仅释放名额。半开失败后的请求是否切换 Target，仍遵守错误分类与 Client Output Commit。冷却为 0 时仅关闭冷却调度门禁；失败仍计数并在达到阈值后按错误分类更换或停止 Target，完整成功仍清零。恢复状态、共享计数与进行中占位统一由 `RoutePolicyState` 拥有，以世代隔离迟到结果，不再叠加独立的固定失败次数健康过滤。已经开始执行的请求不因其他请求触发冷却而被取消。

## Considered options

- 新建客户端 Session 并硬粘 Target：与 glossary 冲突，且 ADR-0023 已拒绝按连接或 Session 固定 Target。
- Cache Affinity 只在同 Priority 组内提权：保住主/备，但放弃 20k+ 前缀的跨组缓存命中。选择维持 ADR-0023 的全列表提权，并用 Conversation Affinity 避免跨对话误粘。
- 按美元成本均衡：Observation 的 Confirmed Upstream Usage 没有实付；目标是加权流量，不是账单。
- 保留旧四档 balance，只在 priority 策略内部再分调度：Priority、冷却与抽样会继续打架。
- 额度耗尽也不换 Target：多账户备份无法消化 QuotaExceeded。

## Consequences

- 管理面必须能显式设置 Target Priority 与失败旋钮，不能再用列表 `index+1` 当优先级。
- 选路通过 `UsageStatsStore` 读取 `target_attempt_observations` 的 24h/1h Confirmed Upstream Usage 聚合，并叠加进行中占位；共享连续失败计数与冷却状态保持进程内。存储查询失败时沿用进程内最后一次成功快照并标记 stale；尚无快照或 Observation gap 导致数据缺失时，以无历史用量的既有调度 fallback 继续选路，不能让观测失败阻断推理。
- 529 / QuotaExceeded 与 HTTP `is_retryable(status)` 必须对齐到三档处置，不能再两套判定。
- Target Priority 的合法范围由 ADR-0036 改为有符号 32 位全区间；本 ADR 的分层选择顺序不变。
- 管理面用优先级泳道表达这些组，见 ADR-0037；已禁用 Target 见 ADR-0035。
