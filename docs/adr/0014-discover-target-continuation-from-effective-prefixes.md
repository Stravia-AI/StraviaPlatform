---
status: accepted
---

# Discover Target continuation from exact effective prefixes

客户端可能通过 Chat Completions、Open Responses、Anthropic Messages 或 Gemini 重复提交完整历史，而不会显式携带 Stravia response ID。Stravia 决定在一次 Model Turn 的完整 Effective Model Request 已完成历史恢复、Hook、Vendor canonical mutation、effective-profile 归一化与 representability gate 后，从 Response Chain 查询 Reusable Response Prefix。选择只发生在完整 canonical `AiItem` 边界，并要求 Principal、精确 Target、Provider 账号与 credential/config generation、base URL、resolved model、egress protocol、instructions、tools、reasoning、response format 和其它请求控制严格一致。

## Considered options

- 只支持显式 `previous_response_id`：语义最简单，但完整历史客户端无法利用已有上游状态。
- 对文本或原始 wire JSON 做模糊前缀匹配：命中率较高，但会忽略 tool correlation、reasoning、媒体、Unknown block、角色结构和跨协议可表示性，可能静默改变任务。
- 在 Hook 前选择 canonical 前缀：可以减少 Hook 输入，但破坏 Hook 读取完整逻辑历史和 Context Rewrite 语义。
- 在 Effective Model Request 上选择严格 item 前缀：复用范围保守，但保持现有语义边界，采用。

## Consequences

- 显式 continuation 始终优先；自动发现不会推断下游连接、IP 或缺失会话 ID，也不会构造空 delta。
- 只有完整交付、上游 terminal 为 `completed`、存在 upstream response ID 且 UpstreamResponse/ClientOutput Hook 未改变输出的节点进入索引。`incomplete`、`failed`、取消与 delivery failure 可以按既有契约保存审计/历史，但不能成为 Reusable Response Prefix。
- 索引按 Principal 与 hashed effective namespace 隔离；候选按 `item_count DESC, completed_at DESC, node_id DESC` 确定性排序。升级前节点不回填。
- 上游 continuation 只是优化。Target 当前不可续接、`store=false` socket affinity 丢失、配置变化或任何语义不等价时，发送完整 Effective Model Request。
- Hook 始终观察完整逻辑历史；协议转换仍先执行 representability gate。无法等价表达的请求保持 typed reject，不以关闭检查换取前缀命中。
- 该决策与 [`ADR-0013`](0013-own-provider-transport-behind-model-turn-executor.md) 正交：Response Chain 选择 continuation state，Provider Transport 决定通过 HTTP/SSE 或 Responses WebSocket 执行同一 Model Turn。
