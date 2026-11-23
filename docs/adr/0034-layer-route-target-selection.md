---
status: accepted
---

# 按亲和、优先级与调度分层选择 Target

不再让互斥的 `Route.balance` 档位决定是否考虑优先级、亲和或健康。Target 选择依次应用 Target Continuation 硬约束、Conversation Affinity、无对话身份时的 Cache Affinity、Target Priority 和同组 Route Scheduling Strategy。失败、重试和冷却决定资格，不是额外的调度策略。

Conversation Affinity 优先使用 Generation Chain 父节点，其次使用 Prompt Cache Directive 路由键，软性复用该身份上次成功的 Target，不新建 Session 或共享不同身份的亲和。无这两种身份时才用前缀 Cache Affinity，避免共享系统提示把无关对话粘在一起。两类软亲和都可以压过 Priority：将缓存限制在同组虽可保住主备顺序，却会放弃昂贵前缀的跨组复用。无亲和命中时，同组默认按实时在途数和加权 Token 流量均衡，而不是按无法从观测用量推断的实付成本；选择和在途预留必须原子完成。

输出提交前，瞬时上游失败可重试；可切换的上游错误（含无明确恢复时间的额度耗尽）改试其他合格 Target。明确的请求级错误既不切换也不计入共享健康；当前 Target 明确不支持该操作则可跳过，但不把它计作健康失败。共享连续失败预算把内部重试与跨请求失败纳入同一阈值；明确的长 `Retry-After` 可提前触发有界冷却。冷却结束用独占半开尝试验证恢复，已有选路依据的 RootRequest 在冷却中各有一次受限额外尝试，避免全局冷却立即切断所有复用机会。只凭完整成功恢复健康，不能以部分输出解封或让过时成功覆盖新一代冷却。

只有宿主能在确认续接未执行、且输出尚未提交时恢复完整历史；不将普通重试当作重放许可，也不为 Compact 重放。调度、错误分类、冷却、RPM 与宿主重放契约由[架构设计](../design/architecture.md)维护。
