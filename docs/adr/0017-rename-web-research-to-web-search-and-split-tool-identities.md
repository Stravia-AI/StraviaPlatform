---
status: accepted
---

# Rename Web Research to Web Search and split public and internal tool identities

## 背景与决策

Web Search 对外返回带来源的 Search Report，并支持按 Search Turn 续接；Local Agent 内部的基础索引检索和抓取只是执行叶工具，不能获得公开 composite 的执行权限。产品、运行时和持久化统一使用 Web Search 名称；公开与内部身份由不同 owner 和注册面区分，不靠 wire name 判断。调用面遵循 [StraviaRead 统一读取契约](../design/agent-core.md#straviaread-统一读取契约)，不保留旧工具别名。

同一个 transport-neutral `WebSearchRunner` 服务各个入口，在 Local Agent 和 External Route 两个 backend 中按管理员配置选择其一，不跨 backend fallback。Local Agent 负责本地检索、抓取和有界研究；External Route 将搜索交给配置好的 Route，不套用 Local Agent 的轮次预算。公开 composite 不进入 `AgentToolRegistry`，禁止 Agent→Agent 嵌套；只有基础检索和抓取叶工具可供 Local Agent 使用。

## 权衡

直接复用公开工具作为内部叶工具看似省接口，却会让 Local Agent 递归调用自身，并混淆 Provider 策略与产品执行权限。跨 backend 回退会改变执行边界、数据去向和上游费用，因此宁可明确失败。续接、来源与各 backend 契约见 [Web Search](../design/web-search.md)。
