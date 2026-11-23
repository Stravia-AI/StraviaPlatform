---
status: accepted
---

# Persist Generation Chain independently of upstream `store`

Stravia 将所有完整交付的生成请求与最终响应写入 Principal-scoped、不可变且可分支的 Generation Chain，不依赖 ingress 协议；Response Chain 仅是其中带 Stravia response ID 的 Responses 投影。Open Responses `store` 仅作为发给 Target 的 Upstream Store Hint，不再控制 Stravia 的持久化：即使 Provider 不保存或不能续接上游状态，Stravia 仍可 materialize 完整 canonical 历史。

## Considered options

- 继续让 `store=false` 禁用本地持久化：保留旧的零留存语义，但使 Generation Chain 因协议入口而不完整。
- 只保留摘要：可降低存储量，但不能恢复 canonical 历史、支持分支或保留客户端实际观察到的响应。
- 采用：本地历史独立于上游状态保留；`store` 继续原样影响 Provider continuation 能力。

## Consequences

- 需要本地留存限制时应设置历史保留，而非发送 `store=false`；当前 [Response Chain 与 `store` 契约](../design/open-responses.md) 与存储保留是两个边界。
- Provider continuation 仍是可选优化。上游状态不可用时，Stravia 从 Generation Chain 展开完整历史，不把 upstream response ID 作为公共身份。
- 只有完整交付的 `completed` 与 `incomplete` 终态写入节点；`failed`、取消、客户端断线与 delivery failure 不写入。
