---
status: accepted
---

# Emulate Responses web_search through Web Access

> 部分被 [`ADR-0011`](0011-own-open-responses-as-a-dated-protocol.md) 取代：Web Access ownership、隐藏 Platform Tool round 与 Provider selection 继续有效；OpenAI-native Responses tool identity 和旧 wire output 不再适用。能力 Gate、API Key 透明注入字段和公开 composite identity 后续分别由 [`ADR-0016`](0016-gate-advanced-capabilities-and-separate-transparent-injection.md) 与 [`ADR-0017`](0017-rename-web-research-to-web-search-and-split-tool-identities.md) 取代。

Open Responses ingress 中显式声明的 `stravia:web_search` tool 不直接决定 Web Provider；Stravia 将该声明适配为同一 Inference Run 内的隐藏 Platform Tool 轮次，让当前模型产生统一 Search 请求，再由 Web Access 的管理员策略选择 Codex、Exa、Brave、Tavily 或智谱 Coding Plan。这样保留平台故障切换，同时不冒充 OpenAI Hosted Tool wire contract。

## Considered options

- 原样透传给 Codex/OpenAI：能获得原生事件与引用，但绕过统一 Web Provider 选择，Exa、Brave、Tavily 和智谱 Coding Plan 无法同级参与。
- 内嵌独立研究 Agent：能合成统一回答，但引入额外模型、递归、成本和生命周期；一期复用现有 Inference Run，未来以 `web_research` 表达不同语义。
- 完整模拟 OpenAI `web_search_call`、sources 与 `url_citation`：兼容面最大，但引用字符区间无法从任意 Provider 可靠重建；采用最终文本兼容和尽力活动项。

## Consequences

- 只接受当前 `web_search` 及其当前 dated alias，不接入旧 `web_search_preview`，也不伪造 Responses `web_fetch`。domain filters 映射到统一 Search 请求；其余 hosted-tool 选项接受但忽略，包括 `external_web_access`、`search_context_size`、`user_location` 与内容类型提示。
- 显式 Responses Search 只受全局 `web_access_enabled` 约束，不读取 API key 的 MCP 或透明注入权限。透明 Platform Tool 注入还要求调用 API key 的 `web_search_injection_enabled` 为真。显式 Responses Search 遇到不支持 function tools 的模型 Target 时返回 `web_search_unsupported`；自动 Platform 注入遇到同类 Target 时不注入并继续普通推理。
- 客户端同时声明原生 `web_search` 与同名 function tool，或与注入的 `web_search`/`web_fetch` 保留名冲突时，返回 400，不覆盖或重命名客户端工具。
- Codex 客户端兼容底线是正常的最终 message 与 `response.completed`。Stravia 尽力产生 `web_search_call` activity item，但不保证 sources、citation annotations 或完整 Hosted Tool 生命周期；Codex 当前只把 activity item 用于 UI，不依赖它推进工具控制流。
- Codex agentic Web Provider 引用现有 Codex Provider 的 OAuth runtime，并以一次内部 Responses native web search 归一出 answer 与来源；不复制 refresh token，不新增用户风险或测试费用提示。
