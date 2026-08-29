---
status: accepted
---

# Replace API-key rate limits with Principal concurrency admission

Stravia 删除 API Key 的 RPM、RPD、TPM 与 TPD，用 nullable `concurrency_limit` 定义 Principal Concurrency Limit。该限制按有效 API Key 建立的 Principal 计数：每个 Proxy Inference Run 与每次 MCP `tools/call` 都是一个根请求；认证成功后、Request Hook 或 MCP 工具执行开始前获取一个名额，直到完整交付或终止清理才释放。根请求内的重试、隐藏 Model Turn、透明 Platform Tool call、透明 function call 与嵌套执行复用同一名额，因此不会重复消耗并发。

## Considered options

- 保留 RPM/RPD/TPM/TPD 并叠加并发：把互不等价的吞吐预算与并发准入混在同一 API Key policy，且未完成移除旧限制的目标。
- 按每个内部 Model Turn 或上游连接计数：透明 function call 和重试会重复占用名额，无法表达一个客户端根请求只占一个并发。
- 在 Gateway 内排队或阻塞：会额外持有客户端连接，并要求定义容量、超时、取消与公平性；当前选择立即拒绝。

## Consequences

- `NULL` 表示不限，正整数表示上限；零和负数是无效配置。即使当前不限，活跃根请求仍被计数，使改为有限后立即对后续准入有效；已开始执行不会因更新被取消。
- 名额耗尽时，Proxy 与 MCP `tools/call` 都在入口返回 HTTP 429，并使用 `ConcurrencyLimitExceeded` / `STRAVIA_CONCURRENCY_LIMIT`；没有可预测的名额释放时间，因此不发送 `Retry-After`，也不建立等待队列。MCP session、发现、工具列表与订阅不执行模型，不占名额。
- 这是一次干净切换：删除旧字段及其用量窗口检查；现有 API Key 的新 `concurrency_limit` 一律为 `NULL`，管理员必须按新语义重新配置。
- 更新 API 对 `concurrency_limit` 使用三态 patch：字段缺失保持不变，`null` 清除为不限，正整数设置上限。
