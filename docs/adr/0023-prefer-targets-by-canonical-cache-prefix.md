---
status: accepted
---

# Prefer Route Targets by canonical cache prefix

## 背景与决策

对不具备 Conversation Affinity 的 Route 请求，在 Request Hook 后以有序 canonical item 摘要匹配最长精确前缀。Cache Affinity 隔离在 Principal 与 Route 内；只有成功报告 `prompt_tokens >= 20,000` 的历史可建立候选。命中的健康 Target 可优先于常规 Priority 和组内调度，但不绕过资格、冷却或重试边界；有 Conversation Affinity 身份时由其接管（见 [ADR-0034](0034-layer-route-target-selection.md)）。

## 权衡

全请求 fingerprint 无法帮助增量前缀；按连接或 Principal 粘滞会把无关请求聚到同一 Target；上游缓存指标又往往在选路后才可得。摘要索引不保留原文，只提高缓存命中机会，不证明上游一定命中。索引丢失时直接使用 Route 策略，不改变任务、鉴权或历史。完整选路契约见 [系统架构](../design/architecture.md)。
