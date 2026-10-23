---
status: accepted
---

# 按亲和、优先级与调度分层选择 Target

Stravia 不再把 `Route.balance` 当成互斥的四档选路。Target 选择改为固定分层：Target Continuation 硬约束；Conversation Affinity；无显式身份时的 Cache Affinity；Target Priority；同组 Route Scheduling Strategy。失败、重试与冷却是资格过滤，不是又一种 balance。

Conversation Affinity 使用已有身份，不新建 Session：Generation Chain 父节点，否则 Prompt Cache Directive 路由键。同一身份软粘上次成功 Target，可压过 Target Priority；不同身份不共享；两种都没有才走 ADR-0023 的前缀 Cache Affinity（仍可压过 Priority）。这收窄了 ADR-0023「适用于所有经 Route 的调用」：共享系统提示不得把无关对话粘到同一 Target。

Target Priority 是有符号 32 位分组整数，越大越优先，缺省 0；取消 1|2 上限（合法范围由 ADR-0036 扩展）。旧数据全部迁成 0，不反转旧序号。同组由 Route Scheduling Strategy 二选一，缺省 Traffic Equalization：无亲和命中时先选择在途请求数最少的 Target，再比较滚动 1h 加权 token 流量与在途输入占位，平分时选最久未被选中的 Target。选择、增加在途请求数和输入占位在同一个进程内锁中完成；预先装配好的并发请求也必须在实际选择时重新比较实时负载。正常结束、失败切换与取消均释放预留。合格 Cache Affinity 仍先选择最长精确前缀的 Target，同长度时沿用最近成功记录；不能为了并发或流量均衡改发无缓存 Target。权重组内共用——全组无价时为缓存输入 0.1、未命中输入 1、输出 5、缓存输出 6；有计价（`cost_input > 0` 且有 `cost_output`）时用价格比平均，缺维回退缺省；忽略 reasoning、audio 与 200k 分层。流量按 `provider_id` + upstream model 跨 Route 累计，以 attempt 的 `started_at` 为窗口依据；每个真实 Target attempt 明确上报的 usage（包括失败、重试与 failover）只计一次。各原始维度独立求已知值之和，仅全未知时返回未知，不能让一条未知记录抹掉其余已知流量。未命中输入先逐 attempt 计算，再求和；已知输入但缓存读取未知时保守按全部输入计，不能用覆盖范围不同的两个总和相减。Latency Preference 仍用 1h 成功率 × 输出 tok/s；成功样本 < 20 视为无数据，组内有效 Target < 2 则回退 Traffic Equalization。

普通状态下，同一 `provider_id:model` 的内部重试与跨请求终态上游失败共用连续失败计数；完整成功清零。Target Retry Budget 为 N 时，第 N+1 次连续失败才进入 Target Cooldown，缺省 5 即第 6 次失败后冷却 120s。瞬时失败（含 First Token Timeout）仍可按错误分类在同一 Target 重试，间隔 0.5s 起、×2、封顶 8s、full jitter。

显式 `Retry-After` 不大于 Target First Token Timeout 时，Stravia 内部完整等待后重发同 Target，不提前把该次限流返回客户端，也不按 Target Cooldown 截短等待；QuotaExceeded 带有此类明确恢复时间时也可重试。同 Target 重试仍受共享失败预算、半开／冷却额外尝试的单次限制、Client Output Commit、取消和总 deadline 约束。First Token Timeout 为 0 表示关闭该时长上限。等待大于或等于当前请求剩余 deadline 时，跳过当前 Target 并尝试其余合格 Target；无备用时立即返回原上游错误，不把等待截短后提前重发，也不让等待耗尽窗口后变成 504。

显式 `Retry-After` 大于已启用的 Target First Token Timeout 时，不在当前请求内等待，立即将 Target 冷却 `min(target_cooldown_ms, Retry-After)` 并尝试其他合格 Target，无需等共享失败预算耗尽；共享 Retry-After 门禁使用同一有效冷却截止时间，不残留更长的上游等待。冷却为 0 时不建立共享等待，但本次请求仍跳过该 Target。

没有上述明确恢复时间的 QuotaExceeded 计数但不在同 Target 空转，直接换 Target；Authentication / Authorization（HTTP 401/403，含 Vendor 直接声明的 Auth 拒绝）与其他非瞬时上游错误（如通用 400、Unknown）同样直接换 Target，不在同 Target 重试；除显式长 Retry-After 外，它们达到共享失败阈值才冷却。凡是上游失败，只要 Client Output Commit 前且不是请求级错误，都可换 Target。ContextLengthExceeded、ContentFiltered 与明确的请求级证据仍结束请求，因为换 Target 只会重复同一失败。确认为请求参数、上下文或内容过滤问题的错误不增加或清零共享失败数，不触发、延长冷却，也不证明恢复；不能仅凭 HTTP 400 或通用 InvalidRequest 判断请求级错误，须保留 Provider 明确的消费上限／过载证据，Claude 529 为过载而非消费额度耗尽。取消、本地准备、Hook 与存储错误不计数。First Token 是上游第一个 canonical 输出，包含 Thinking，缺省 60s。Client Output Commit 之后仍禁止换 Target，只终止当前请求；Commit 本身不计数也不单独触发冷却，其后的健康相关真实上游失败照常计数。Affinity 与 Continuation 遇冷却按下述受控额外尝试规则处理，不再无条件让位冷却。删除 Target.weight。旧 `weighted` / `priority` / `cooldown` 映射为 Traffic Equalization，`latency` 映射为 Latency Preference。

Model Turn 中 Vendor 返回明确的 `Unsupported` 时，视为当前 Target 不具备所需操作或能力，直接尝试后续合格 Target；不在同 Target 重试，也不计共享健康失败。通用 `Invalid` 或 HTTP 400 不据此放宽。Native Compaction、不可安全跨 Target 回放的不透明 Conversation，以及首个 canonical 输出后的切换限制继续生效。

本段明确替换本 ADR 此前的「重试预算用尽、QuotaExceeded 或请求放弃当前 Target 即立即冷却」规则：除上述显式长 Retry-After 外，普通 Target 只在共享连续失败数达到统一阈值时冷却，单次流式失败、QuotaExceeded 或已提交输出均不会仅凭自身触发冷却。Target Cooldown 到期后进入半开，而不是全量恢复；下一个实际符合选路条件的请求独占一次探测机会。探测关闭同 Target 预算重试和 ProviderCall 内部回退，完整成功清零并恢复正常；任何上游探测失败（含已经发出上游请求后的超时）立即重新冷却，显式长 Retry-After 仍取上述有界时长，否则等待完整配置冷却。用户取消、本地准备失败和消费者断开仅释放名额。半开失败后的请求是否切换 Target，仍遵守错误分类与 Client Output Commit。冷却为 0 时仅关闭冷却调度门禁；失败仍计数并在达到阈值后按错误分类更换或停止 Target，完整成功仍清零。恢复状态、共享计数与进行中占位统一由 `RoutePolicyState` 拥有，以世代隔离迟到结果，不再叠加独立的固定失败次数健康过滤。已经开始执行的请求不因其他请求触发冷却而被取消。

Latency Preference 的输出 tok/s 为一小时内成功 attempt 的 `Σoutput_tokens / (Σduration_ms / 1000)`，包含首 Token 等待，不减 `first_token_ms`，也不平均单次 TPS。管理面 Provider TPS 使用同一完整 attempt 耗时；Run TPS 评价客户端完整请求，不把其他 Target、平台工具或客户端请求间隔的耗时归到该 Target。成功率权重、有效样本门槛、亲和与优先级顺序保持不变。

## Root-scoped cooldown attempts

本次替代原有「请求级终态错误也计共享失败」与「Affinity 一律让位冷却」决定：冷却前已选中 Target，或具有合法 Target Continuation、Conversation Affinity、Cache Affinity 的请求，每个 RootRequest 对同一冷却 Target 最多领取一次额外实际上游尝试。每个独立合格根各自有机会，不是每轮冷却全局仅一次；内部重试、隐藏轮次、后台执行与切回原 Target 不重置机会。不合格新请求仍走普通健康选路。额外尝试关闭同 Target 预算重试与 ProviderCall 内部回退，必须经过实际发送目的地 RPM、按上述规则计算的 Retry-After 等待／有界冷却、授权、能力、禁用、Credential Invalid、deadline 和取消门禁；不放宽 Continuation 的执行状态不确定性或 Client Output Commit。

额外尝试只有完整成功且冷却世代仍匹配才能清零并恢复；HTTP 200、首 token 或部分输出不算恢复，旧世代迟到成功不得覆盖新冷却。健康相关上游失败沿既有分类和冷却规则处理，请求级错误与取消不改变健康状态也不遗留探测占用。普通冷却到期的独占半开探测继续有效，已在执行的流不因别根触发冷却被取消。目的地 RPM 不改变健康身份；根累计等待及额度配置见[架构 §8.4](../design/architecture.md#84-上游目的地-rpm-发送门禁与配置)。

远端 Continuation 不存在属于请求级错误，不为取得恢复预算而增加共享健康失败。完整历史回放仍受原 Target Retry Budget 限制；预算为 0 时不回放，已执行的认证恢复占用同一预算，完整回放最多一次。宿主尚未发送请求的本地连接 miss 仍可免费回退；半开探测与冷却额外尝试不获得内部回放许可。

上述请求级隔离也适用于半开探测：此前「任何上游探测失败即重新冷却」现仅指健康相关失败；明确请求级错误只释放探测占用，不改变既有冷却世代或失败数。

## Considered options

- 新建客户端 Session 并硬粘 Target：与 glossary 冲突，且 ADR-0023 已拒绝按连接或 Session 固定 Target。
- Cache Affinity 只在同 Priority 组内提权：保住主/备，但放弃 20k+ 前缀的跨组缓存命中。选择维持 ADR-0023 的全列表提权，并用 Conversation Affinity 避免跨对话误粘。
- 按美元成本均衡：Observation 的 Confirmed Upstream Usage 没有实付；目标是加权流量，不是账单。
- 保留旧四档 balance，只在 priority 策略内部再分调度：Priority、冷却与抽样会继续打架。
- 额度耗尽也不换 Target：多账户备份无法消化 QuotaExceeded。

## Consequences

- 管理面必须能显式设置 Target Priority 与失败旋钮，不能再用列表 `index+1` 当优先级。
- 选路通过 `UsageStatsStore` 读取 `target_attempt_observations` 的滚动 1h Confirmed Upstream Usage 聚合，并叠加进行中占位；在途请求数、最近选择顺序、共享连续失败计数与冷却状态保持进程内，不协调独立实例。存储普通查询错误时沿用进程内最后一次成功快照并标记 stale；尚无快照或 Observation gap 导致数据缺失时，以无历史用量继续按实时在途数分流，不能让普通观测失败阻断推理。存储失败按用户修订的 ADR-0060 只影响所属请求或任务，不再触发实例终止；这里已有有效缓存与 stale/fallback 的正常调度语义保持不变，不新增存储健康模式或改变选路、重试预算。长期管理 Stats Usage 的 15 分钟汇总不替代选路所需的原始 1h 短窗口；观测保留期、缓存 TTL 和数据库结构均不因此改变。
- 529 / QuotaExceeded 与 HTTP `is_retryable(status)` 必须对齐到三档处置，不能再两套判定。
- Target Priority 的合法范围由 ADR-0036 改为有符号 32 位全区间；本 ADR 的分层选择顺序不变。
- 管理面用优先级泳道表达这些组，见 ADR-0037；已禁用 Target 见 ADR-0035。
