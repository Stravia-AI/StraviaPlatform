---
status: accepted
---

# Unify web research behind one WebResearchRunner

Stravia 以一个 transport-neutral `WebResearchRunner` 拥有 Web Research，并在其后提供互不 fallback 的 Local Agent 与 Codex Agentic Search adapters。公开工具继续命名为 `web_search`，但 clean cutover 为返回强校验 Research Report；旧 Web Search/Web Fetch 只保留为 Local backend 的隐藏 leaves，Codex 则从 WebProvider 移为管理员显式选择、固定 Provider/upstream model 的 Research backend。这样 PlatformTool、ToolContinuation、MCP、Responses、principal authorization 与 TurnChain 仍各有单一 owner，同时 public composite 不进入 AgentToolRegistry，避免 Agent → Agent 嵌套。

当前 interface、状态、迁移与安全边界见 [`docs/design/web-search.md`](../design/web-search.md)。OpenAI 一手研究见 [`docs/research/openai-web-research.md`](../research/openai-web-research.md)。

> ADR-0009 曾部分取代本决策的 Local backend authorization；该授权字段现已由 [ADR-0016](0016-gate-advanced-capabilities-and-separate-transparent-injection.md) 删除。有效 API Key 的显式使用由平台 Gate 授权，透明注入是独立设置。

## Considered options

- 把多步研究塞回旧 `WebSearchTool`：会让一个 leaf 同时承担 provider selection、agent loop、continuation、Report validation 与 transport adapters，形成浅而混杂的 interface。
- 只注册一个 public Agent Definition：Local 路径简单，但 Codex 一次性 hosted Agentic Search 不是 AgentRunner loop；强套 AgentRunner 会形成本地 loop 包 hosted loop，并重复预算与会话语义。
- Local/Codex 各暴露一个工具：实现直接，但调用方必须理解两套输入、Turn、Report、错误与权限，无法保持 provider-neutral 产品语义。
- 把 public Research 注册成 AgentTool：Local backend 会递归启动另一个 AgentRunner，违反 Agent Core 禁止 nesting 的约束。
- 保留旧 public `web_fetch` 或 legacy aliases：降低 cutover 冲击，但长期维护两套不同深度的 Web contract，并让模型难以判断何时搜索、抓取或研究。
- 使用 OpenAI Deep Research/background：能力更深，但与本期一次性 Codex Agentic Search、request-bound lifecycle、Codex OAuth model availability 和 Stravia-owned TurnChain 不一致。

## Consequences

- `web_search` 名称保持，输入/输出语义发生 breaking change；`web_fetch` 从 Hook/MCP discovery 删除，不提供 shim。
- Local backend 使用 internal-only、ephemeral Agent Run；网页正文不进入持久化 Agent Turn。Local/Codex 都只提交外层 principal-scoped Research Turn。
- `WebResearchConfig` 单独拥有 enabled、backend binding、turns/time；Local Agent Definition 只拥有 code revision、instructions、hidden leaves 与 validator。
- PlatformTool/AgentTool 增加共享 `parallel_safe` metadata，Builder 通过显式 Surface Plan 分离 public composite 与 internal leaves；Agent model turns 使用 caller-owned tool exposure，Hook 不再动态注入 Research。
- Agent Core 支持 optional tool/token/parallel/concurrency budgets。只有 internal Web Research 使用无这些上限的配置；其他 Definitions 行为保持不变。
- Codex backend 固定 Provider/upstream model、使用 live external web、只信 URL annotations，不自动 fallback。该路径是管理员明确授权的数据出境例外。
- Report provenance 是 Web Research 的终止契约；父模型读取 Report 后如何生成或展示最终答案不属于该模块的保证。
- 本决策接受无 parent PlatformTool call/round/bytes budget、无 Research semaphore、Codex blocked domains 仅靠 prompt、OpenAI 非 domain controls 静默忽略、全 ancestor context 无 compaction/preflight，以及 destructive clean cutover 的风险。
