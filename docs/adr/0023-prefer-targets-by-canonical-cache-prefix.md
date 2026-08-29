---
status: accepted
---

# Prefer Route Targets by canonical cache prefix

Stravia 在 Request Hook 完成后、首次选择 Target 前，为所有经 Route 的调用计算 ordered Canonical Item Hash 前缀。Gateway-local Cache Affinity 索引按 Principal 与 Route 隔离；某个 Target 成功响应并报告 `prompt_tokens >= 20,000` 后，最长精确前缀命中的健康候选仅被提到 RouteAttemptPolicy 的首位。无命中、不健康、Target 已不在 Route 中或可重试失败时，现有 Route 策略与重试顺序保持不变。

## Considered options

- 客户端请求全量 fingerprint：跨协议不一致，客户端可操纵，且已取消该层需求。
- 按 Principal、连接或 Session 固定 Target：不能证明 Prompt Cache 前缀相同，且会把无关请求黏到一个 Target。
- Provider 的缓存命中指标驱动：多数 Provider 不提供可用于选路的稳定命中信息，且信息在选路后才可获得。
- 使用精确 canonical 前缀和成功后的 `prompt_tokens`：不读取或保证 Provider cache，但能以最小语义风险提高同一 Target 的概率。

## Consequences

- 每条 `AiItem` 只以 canonical SHA-256 摘要记录在内存索引；不得持久化 raw 内容或在日志中输出 Hash。
- 这是纯性能优化，不影响 Effective Model Request、Response Chain、Target Continuation、鉴权或故障切换。
- 首次访问、Gateway 重启、索引淘汰或 Provider cache 丢失会回退 Route 策略，不产生错误或客户端可见语义变化。
