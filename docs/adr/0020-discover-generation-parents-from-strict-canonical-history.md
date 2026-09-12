---
status: accepted
---

# Discover Generation Chain parents from strict canonical history

Open Responses 可以显式提交 `previous_response_id`，但 Chat Completions、Anthropic Messages 与 Gemini generation 不携带 Stravia 父节点。Stravia 决定：显式父节点始终优先；未提供时，只在同一 Principal 内以完整 canonical 历史精确匹配选择最长父链，且匹配后必须仍有至少一个新 input item；没有候选或存在任一语义差异就创建新根。

## Considered options

- 每次创建根：不会错链，但使自动留存无法形成协议无关历史。
- 按连接、网络或 Session 归链：重连、代理、共享出口与并发分支会错链，并引入可变 Session 身份。
- 文本模糊匹配：会丢失角色、工具、reasoning、媒体与请求控制差异。
- 采用严格 canonical 历史匹配：保守，但与 Hook 前完整逻辑历史、Principal 隔离及可分支 Turn Chain 契约一致。

## Consequences

- 自动发现仅建立 Stravia 历史父链；它不保证 Target Continuation。后者仍要求既有的 Target、Provider、配置和 egress 语义等价性。
- 未带父节点的协议仍必须提交完整历史才能命中；Stravia 不从连接推测缺失历史，也不把完整相同的请求变成空增量。
- 严格匹配针对完整消息语义，而不是原始 wire 对象相等。应用 `metadata`、`internal_chat_message_metadata_passthrough` 及交付身份字段不决定历史身份；原始字段仍保留供同协议转发和 Debug 使用。角色、顺序、指令、内容块、工具调用及其结果关联、受保护推理、原生压缩状态和未分类的协议扩展仍严格比较。
- 指纹筛选与候选精确核验使用同一语义投影。投影变更通过版本化派生索引受控重建，不改写历史节点 ID、原始 payload 或父边；不可物化的过期历史不作为续接候选。
