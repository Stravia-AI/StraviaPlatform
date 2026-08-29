---
status: accepted
---

# Discover Target Continuation from exact effective prefixes

Stravia 允许没有显式 response ID 的客户端重发完整历史，因此只支持显式父节点会让 Chat Completions、Anthropic Messages 与 Google Gemini 客户端反复提交已完成上下文。Stravia 决定在完整历史恢复、Hook 与 Protocol Conversion 完成后，从同一 Principal 和等价 Target 语义的 Response Chain 节点中选择最长 Reusable Response Prefix，并以对应 Target Continuation 替换该前缀；这样减少重复 token，同时不把客户端连接或相似文本误当成 Session 身份。

## Considered options

- 只支持客户端显式 response ID：身份最明确，但不能帮助只会重发完整历史的客户端协议。
- 匹配客户端原始 wire、文本或 content block 前缀：命中更多，但会绕过 Representability、Hook 与 tool/reasoning/media 语义，可能续接到不等价任务。
- 先选择本地最长历史节点再尝试 upstream ID：会反复探测已过期、断线或已被 Provider 驱逐的状态。
- 采用精确 Effective Model Request 前缀：只选择完整 item 前缀、兼容语义和当前可用的 Target Continuation，采用。

## Consequences

- 客户端显式 response ID 始终优先；自动发现只在没有显式父节点时运行。
- Hook 始终读取完整逻辑历史。前缀替换只影响最终 Target request，不把 delta 暴露成另一种 Hook 语义。
- Reusable Response Prefix 必须对应成功终态且完整客户端交付的 Response Chain 节点，并在 Principal、Target、model、instructions、tools、reasoning、response format 与其他任务语义上等价。
- 只在完整 canonical item 边界切分；自动续接必须留下新 item，不根据连接推断父节点，也不把空 delta 当作自动 continuation。
- 在当前可用的 Reusable Response Prefix 中先按前缀 item 数取最长，再按完成时间取最近。Fingerprint 只负责索引，复用前必须 materialize 并完整比较语义。
- Target Continuation 的可用性服从 Provider 的 store、连接亲和、保留期与失效契约；本地 Response Chain 存在不表示 upstream state 仍可用。
- Reusable Response Prefix 的索引元数据随 Turn Chain 节点原子持久化并共享七天保留期；升级前节点不回填自动索引，但显式 response ID 继续有效。
