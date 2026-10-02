---
status: superseded
---

# Replace API-key rate limits with Principal concurrency admission

> **已被取代。** 以下保留原决定的历史，不再描述当前行为。当前决定为 API Key Root RPM 与上游 RPM Pool，见[架构 §8.3–8.4](../design/architecture.md#83-代理请求鉴权与-rpm-准入流程)；冷却隔离见 [ADR-0034](0034-layer-route-target-selection.md)。
>
> 替代决定：API Key 使用 nullable `rpm_limit`，按客户端根请求执行严格滑动 60 秒准入，超限立即返回 429 与 `Retry-After`；同根重试、隐藏轮次与后台执行共享 RootRequest，而不持有并发 lease。每次实际上游发送另按默认目的地或显式共享 RPM Pool 计数；等待有累计预算与队列边界，不限制活跃流。配置持久化，窗口仅属于单实例且重启清空。SQLite/PostgreSQL `0009_rpm_admission` 删除旧字段，将所有新 `rpm_limit` 置为 `NULL`，不换算旧数值；管理员重新配置前不限。三态 patch 改用 `rpm_limit`；不保留任何并发限制。

## Historical decision

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
