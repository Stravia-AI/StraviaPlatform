---
status: accepted
---

# Put Web Search and Web Fetch behind one Web Access seam

## 背景与决策

上层需要统一的 Search 和 Fetch；具体 Web Provider 的凭据、协议和部署方式不应成为客户端契约。Core 拥有 Web Access 中立接口、验证、能力匹配、独立的有序故障切换与不可变运行配置快照。宿主通过 `WebProviderFactory` 注入适配器，OSS Core 不内置 LocalWeb 或 Chromium。Provider 实现不接管平台鉴权、工具身份或客户端传输。

Search 与 Fetch 不共用一条优先级队列，因为不是每个 Provider 都实现两种能力。按供应商提供独立客户端工具虽能暴露更多参数，却会让模型感知部署拓扑并自行故障切换；把 Agent 循环放进 Web Access 又会给基础检索强加模型调用。公开 composite 与 Local Agent 内部检索的关系见 [ADR-0017](0017-rename-web-research-to-web-search-and-split-tool-identities.md)。

适配器、代理与 Fetch 安全规则见 [Web Access Provider](../design/web-access-providers.md)；调用契约见 [Web Search](../design/web-search.md)。
